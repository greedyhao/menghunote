//! 配额与维护 reservation（ADR 0006 §7 + §13.2）。
//!
//! 两个预算：
//! - 正常预算 `Q = normal_limit`：日常 update/asset/snapshot 密文预算；写满返回 507，**不删旧数据腾空间**；
//! - 维护预算 `R = maintenance_limit - normal_limit` 的余量（`maintenance_limit` 是 `Q+R` 的峰值上限）：
//!   只用于 checkpoint 替换、轮换和宽限保留。
//!
//! 可用维护额度公式（§13.2）：
//! `R_available = R − maintenance_used − Σ(active.reserved_bytes − consumed_bytes)`
//! 其中 `R = maintenance_limit − normal_limit`、`maintenance_used = usage.maintenance_bytes`。
//! 注意该公式下"在 reservation 内写一片"是**不变量**：`maintenance_bytes += len` 与
//! `Σ(reserved−consumed) -= len` 相抵，所以 reservation 本身就是预留的额度。
//!
//! **峰值不变量（评审 0003 P1-E 残余）**：`maintenance_limit` 是 `normal + maintenance`
//! 的**真实总量**峰值 `Q+R`，不是 maintenance 桶自己的上限。两个 `charge_*` 都必须同时
//! 验证分类桶与真实总量；只验分类桶会被 `recompute` 的归类（把退下 pair 的 normal 降为
//! maintenance）绕过：normal 桶仍装得下，但真实密文已推过峰值。
//!
//! 逐片原子扣减：本模块的 `charge_*` 必须在**写入 object_parts 的同一个事务**里调用
//! （调用方 `objects::put_part` 保证）；超限返回 507 且同事务零效应（错误传播即回滚）。

use rusqlite::types::Value;
use rusqlite::OptionalExtension;

use crate::error::{Error, ErrorCode, Result};
use crate::ids::{random_id16, Id16};
use crate::scope::ScopeConn;
use crate::time::Clock;
use crate::vault;

/// 预算用途。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationPurpose {
    /// checkpoint 上传/替换。
    Checkpoint,
    /// key epoch 轮换（新基线复制 + 两 checkpoint）。
    Rotation,
}

impl ReservationPurpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Checkpoint => "checkpoint",
            Self::Rotation => "rotation",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "checkpoint" => Some(Self::Checkpoint),
            "rotation" => Some(Self::Rotation),
            _ => None,
        }
    }
}

/// reservation 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationState {
    Active,
    Complete,
    Expired,
}

impl ReservationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Complete => "complete",
            Self::Expired => "expired",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "complete" => Some(Self::Complete),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

/// 用量账本。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// 正常对象密文字节数。
    pub normal_bytes: i64,
    /// 维护对象密文字节数（checkpoint/轮换期间写入）。
    pub maintenance_bytes: i64,
}

impl Usage {
    /// 真实密文总量（= 仍持有 parts 的全部对象字节和）。
    pub fn total(self) -> i64 {
        self.normal_bytes.saturating_add(self.maintenance_bytes)
    }

    /// 是否仍在峰值 `Q+R`（`maintenance_limit`）以内。
    ///
    /// 注意门禁是**总量**：`normal ≤ Q` 与 `maintenance + reserved ≤ R` 都不足以单独
    /// 保证峰值（§7；评审 0003 P1-E 残余）。
    pub fn within_peak(self, peak_limit: i64) -> bool {
        self.total() <= peak_limit
    }
}

/// 一条维护 reservation。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub reservation_id: Id16,
    pub purpose: ReservationPurpose,
    pub resource_id: Vec<u8>,
    pub owner_device: Id16,
    pub reserved_bytes: i64,
    pub consumed_bytes: i64,
    pub state: ReservationState,
    pub expires_at: i64,
}

impl Reservation {
    /// 剩余可写字节。
    pub fn remaining(&self) -> i64 {
        self.reserved_bytes.saturating_sub(self.consumed_bytes)
    }
}

/// 读账本。
pub fn usage(conn: &ScopeConn) -> Result<Usage> {
    let row = conn
        .query_row(
            "SELECT normal_bytes, maintenance_bytes FROM usage WHERE vault_id = ?1",
            [conn.scope().as_slice()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let (normal_bytes, maintenance_bytes) = row.ok_or(Error::Invariant("usage row missing"))?;
    Ok(Usage {
        normal_bytes,
        maintenance_bytes,
    })
}

/// `Σ(active 且未过期).(reserved − consumed)`。
pub fn active_reserved_remaining(conn: &ScopeConn, now: i64) -> Result<i64> {
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(reserved_bytes - consumed_bytes), 0)
         FROM maintenance_reservations
         WHERE state = 'active' AND expires_at > ?1 AND vault_id = ?",
        (now, conn.scope().as_slice()),
        |row| row.get(0),
    )?;
    Ok(total)
}

/// 正常预算剩余。
pub fn normal_available(conn: &ScopeConn) -> Result<i64> {
    let vault = vault::load(conn)?;
    Ok(vault.normal_limit - usage(conn)?.normal_bytes)
}

/// 一次"未来 live"计量的基线集合：`incoming` 加上同 epoch 最新若干份 healthy published
/// （最多补到两份）。publish 传即将发布的 1 份（补 1 份已发布），activate 传被指名的
/// 两份候选（不再补：激活后它们就是最新两份）。
fn live_pair_ids(conn: &ScopeConn, epoch: u64, incoming: &[Id16]) -> Result<Vec<Id16>> {
    let mut pair = incoming.to_vec();
    let fill = 2usize.saturating_sub(pair.len());
    if fill > 0 {
        let mut stmt = conn.prepare(
            "SELECT checkpoint_id FROM checkpoints
              WHERE state = 'published' AND epoch = ?1 AND health != 'unhealthy'
                AND vault_id = ?3
              ORDER BY published_at DESC, rowid DESC
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            (
                i64::try_from(epoch).map_err(|_| Error::Invariant("epoch"))?,
                i64::try_from(fill).map_err(|_| Error::Invariant("live pair limit"))?,
                conn.scope().as_slice(),
            ),
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        for row in rows {
            pair.push(
                row?.as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("checkpoint_id"))?,
            );
        }
    }
    Ok(pair)
}

/// live 集合的 CTE（`pair` + `live`）。口径与 [`future_live_bytes`]/[`recompute`] 完全一致，
/// 两处必须共用这一份 SQL，避免"门禁算一套、账本算另一套"。
///
/// `pair` 是本次计量视为"最新 pair"的 checkpoint 集合（0–2 个，绑定为 `?1..?n`）；
/// `epoch` 是 orphan ASSET 的归属 epoch（绑定为 `?n+1`）。集合为：
/// - pair 的 `role IN ('data','manifest')` refs（MANIFEST 密文也是真实保留的 metadata）；
/// - `log` 仍保留（尚未被 GC 前缀删除）的 UPDATE 对象；
/// - 同 epoch、未 retired、且**未被 pair 的 data refs 引用**的 finalized ASSET
///   （v1 不删除资产孤儿，轮换必须复制它们，因此它们同样是 live 密文）。
///
/// 最后按 `object_id` 去重（UNION），密文只计一次。
fn live_cte_sql(pair_len: usize) -> String {
    let pair_source = if pair_len == 0 {
        // 空 pair：fresh vault 还没有 published 基线，`pair` 里放一行 NULL 即可让
        // `IN (SELECT checkpoint_id FROM pair)` 恒为空。
        "SELECT NULL".to_string()
    } else {
        let values: Vec<String> = (1..=pair_len).map(|index| format!("(?{index})")).collect();
        format!("VALUES {}", values.join(", "))
    };
    let epoch_param = pair_len + 1;
    // vault_id 在 CTE 的每个分支都要过滤：同一个显式编号复用（参数按编号绑定）。
    let vault_param = pair_len + 2;
    format!(
        "WITH pair(checkpoint_id) AS ({pair_source}),\n\
         live(object_id) AS (\n\
             SELECT r.object_id FROM checkpoint_refs r\n\
              WHERE r.role IN ('data','manifest')\n\
                AND r.checkpoint_id IN (SELECT checkpoint_id FROM pair)\n\
                AND r.vault_id = ?{vault_param}\n\
             UNION\n\
             SELECT l.object_id FROM log l\n\
              WHERE l.kind = 'update' AND l.object_id IS NOT NULL\n\
                AND l.vault_id = ?{vault_param}\n\
             UNION\n\
             SELECT a.object_id FROM asset_objects a\n\
              WHERE a.epoch = ?{epoch_param} AND a.state != 'retired'\n\
                AND a.vault_id = ?{vault_param}\n\
                AND a.object_id NOT IN (\n\
                    SELECT r.object_id FROM checkpoint_refs r\n\
                     WHERE r.role = 'data'\n\
                       AND r.checkpoint_id IN (SELECT checkpoint_id FROM pair)\n\
                       AND r.vault_id = ?{vault_param}\n\
                )\n\
         )"
    )
}

fn pair_params(pair: &[Id16], epoch: u64, conn: &ScopeConn) -> Result<Vec<Value>> {
    let mut params: Vec<Value> = pair.iter().map(|id| Value::Blob(id.to_vec())).collect();
    params.push(Value::Integer(
        i64::try_from(epoch).map_err(|_| Error::Invariant("epoch"))?,
    ));
    params.push(Value::Blob(conn.scope().as_slice().to_vec()));
    Ok(params)
}

fn live_bytes(conn: &ScopeConn, epoch: u64, pair: &[Id16]) -> Result<i64> {
    let vault_param = pair.len() + 2;
    let sql = format!(
        "{} SELECT COALESCE(SUM(length(p.envelope)), 0)\n\
           FROM object_parts p\n\
          WHERE p.object_id IN (SELECT object_id FROM live)\n\
            AND p.vault_id = ?{vault_param}",
        live_cte_sql(pair.len())
    );
    let bytes = conn.query_row(
        &sql,
        rusqlite::params_from_iter(pair_params(pair, epoch, conn)?),
        |row| row.get(0),
    )?;
    Ok(bytes)
}

/// 发布/激活 checkpoint 后的"未来 live"密文字节数（ADR §7；评审 0002 P1-5 / T20、P1-E）。
///
/// 只读计量，不新增 schema；计量口径与 [`usage`]/[`recompute`] 一致
/// （`SUM(length(object_parts.envelope))`，同一 object_id 只计一次）。集合见
/// [`live_cte_sql`]：未来最新两份基线的 **data + MANIFEST** refs、`log` 仍保留的 UPDATE、
/// 以及同 epoch 未 retired 且未被 pair 引用的 finalized ASSET。
///
/// publish 传即将发布的 1 份（函数再补当前同 epoch 最新一份 published）；activate 传
/// 被指名的两份候选（此刻它们尚未 published，不会再补旧 epoch 的基线）。超预算只拒绝
/// （507），不删除任何安全根、不建 GC 计划。
pub fn future_live_bytes(conn: &ScopeConn, epoch: u64, incoming: &[Id16]) -> Result<i64> {
    let pair = live_pair_ids(conn, epoch, incoming)?;
    live_bytes(conn, epoch, &pair)
}

/// 维护额度剩余（§13.2 公式）。**过期的 reservation 不再占额度，但已写入的数据仍在**。
///
/// 口径：`vault.maintenance_limit` 是**峰值上限 `Q+R`**（ADR §6），因此维护预算
/// `R = maintenance_limit - normal_limit`；`R_available = R - maintenance_used - Σ(active.reserved-consumed)`。
pub fn maintenance_budget(vault: &vault::Vault) -> i64 {
    (vault.maintenance_limit - vault.normal_limit).max(0)
}

/// 维护额度剩余。
pub fn maintenance_available(conn: &ScopeConn, now: i64) -> Result<i64> {
    let vault = vault::load(conn)?;
    let used = usage(conn)?.maintenance_bytes;
    let reserved = active_reserved_remaining(conn, now)?;
    Ok(maintenance_budget(&vault) - used - reserved)
}

/// 账本是否已超过峰值 `Q+R`（只读诊断，不改任何状态）。
///
/// 超峰状态只允许**读**与**降级回收**（GC/`cleanup_staging` 只会让真实 bytes 单调下降）；
/// 一切增量写入都由 [`charge_normal`] / [`charge_maintenance`] 的峰值门禁拒绝。
/// 历史数据（旧版本超峰、或运维缩容 `maintenance_limit`）不会被 [`recompute`] 抹平，
/// 因此恢复写入必须先把真实 bytes 降回峰值以内。
pub fn peak_exceeded(conn: &ScopeConn) -> Result<bool> {
    let vault = vault::load(conn)?;
    Ok(!usage(conn)?.within_peak(vault.maintenance_limit))
}

/// 一片密文的峰值准入：`normal + maintenance + bytes ≤ Q+R`（`maintenance_limit`）。
///
/// 这是**真实总量**门禁，与分类桶无关：`recompute` 可以把退下 pair 的 normal 归类为
/// maintenance，让 `normal + bytes ≤ Q` 重新通过；若只验分类桶，一次 normal 写入就能把
/// 真实密文推过峰值（评审 0003 P1-E 残余）。所有 `charge_*` 必须走这里。
fn ensure_peak_charge(vault: &vault::Vault, current: Usage, bytes: i64) -> Result<()> {
    if current.total().saturating_add(bytes) > vault.maintenance_limit {
        return Err(Error::Api(
            crate::error::ApiError::new(ErrorCode::QuotaExceeded).epoch(vault.active_epoch),
        ));
    }
    Ok(())
}

/// 写一片**正常**对象的密文：原子扣预算，超限返回 507。
///
/// 两个条件同时成立才放行（缺一不可）：
/// - `normal + bytes ≤ Q`（正常桶本身不超预算）；
/// - `normal + maintenance + bytes ≤ Q+R`（真实总量不超峰值；否则归类后的旧 maintenance
///   会让"Q 还装得下"的写入击穿峰值）。
pub fn charge_normal(conn: &ScopeConn, bytes: i64) -> Result<()> {
    if bytes < 0 {
        return Err(Error::Invariant("negative charge"));
    }
    let vault = vault::load(conn)?;
    let current = usage(conn)?;
    if current.normal_bytes.saturating_add(bytes) > vault.normal_limit {
        return Err(Error::Api(
            crate::error::ApiError::new(ErrorCode::QuotaExceeded).epoch(vault.active_epoch),
        ));
    }
    ensure_peak_charge(&vault, current, bytes)?;
    conn.execute(
        "UPDATE usage SET normal_bytes = normal_bytes + ?1 WHERE vault_id = ?",
        (bytes, conn.scope().as_slice()),
    )?;
    Ok(())
}

/// 写一片**维护**对象的密文：必须落在 active reservation 内，且不超过 `Q+R` 峰值。
/// **逐片原子扣减**（与 part 写入同一事务）。
pub fn charge_maintenance(
    conn: &ScopeConn,
    now: i64,
    reservation_id: &Id16,
    bytes: i64,
) -> Result<()> {
    if bytes < 0 {
        return Err(Error::Invariant("negative charge"));
    }
    let reservation =
        reservation(conn, reservation_id)?.ok_or_else(|| Error::api(ErrorCode::ReserveExceeded))?;
    if reservation.state != ReservationState::Active {
        return Err(Error::api(ErrorCode::ReserveExceeded));
    }
    if reservation.expires_at <= now {
        // 过期即失效：不删除已发布数据（§13.2），但不能再往里写。
        expire_reservation(conn, reservation_id)?;
        return Err(Error::api(ErrorCode::ReserveExceeded));
    }
    if reservation.remaining() < bytes {
        return Err(Error::api(ErrorCode::ReserveExceeded));
    }
    let vault = vault::load(conn)?;
    let current = usage(conn)?;
    // 峰值预算 Q+R（ADR §7：maintenance_limit 就是 normal+maintenance 的峰值上限）。
    ensure_peak_charge(&vault, current, bytes)?;
    conn.execute(
        "UPDATE maintenance_reservations SET consumed_bytes = consumed_bytes + ?1
         WHERE reservation_id = ?2 AND vault_id = ?",
        (bytes, reservation_id.as_slice(), conn.scope().as_slice()),
    )?;
    conn.execute(
        "UPDATE usage SET maintenance_bytes = maintenance_bytes + ?1 WHERE vault_id = ?",
        (bytes, conn.scope().as_slice()),
    )?;
    Ok(())
}

/// 读一条 reservation。
pub fn reservation(conn: &ScopeConn, id: &Id16) -> Result<Option<Reservation>> {
    let mut stmt = conn.prepare(
        "SELECT reservation_id, purpose, resource_id, owner_device, reserved_bytes,
                consumed_bytes, state, expires_at
         FROM maintenance_reservations WHERE reservation_id = ?1 AND vault_id = ?",
    )?;
    let row = stmt
        .query_row((id.as_slice(), conn.scope().as_slice()), |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })
        .optional()?;
    let Some((
        id_bytes,
        purpose,
        resource_id,
        owner,
        reserved_bytes,
        consumed_bytes,
        state,
        expires_at,
    )) = row
    else {
        return Ok(None);
    };
    Ok(Some(Reservation {
        reservation_id: id_bytes
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("reservation_id"))?,
        purpose: ReservationPurpose::parse(&purpose)
            .ok_or(Error::Invariant("reservation purpose"))?,
        resource_id,
        owner_device: owner
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("reservation owner"))?,
        reserved_bytes,
        consumed_bytes,
        state: ReservationState::parse(&state).ok_or(Error::Invariant("reservation state"))?,
        expires_at,
    }))
}

/// 建立维护 reservation：额度不足返回 507 RESERVE（**不删旧数据**）。
pub fn create_reservation(
    conn: &ScopeConn,
    now: i64,
    purpose: ReservationPurpose,
    resource_id: &[u8],
    owner_device: &Id16,
    requested_bytes: i64,
    expires_at: i64,
) -> Result<Id16> {
    if requested_bytes < 0 {
        return Err(Error::Invariant("negative reservation"));
    }
    let available = maintenance_available(conn, now)?;
    if requested_bytes > available {
        return Err(Error::Api(crate::error::ApiError::new(
            ErrorCode::ReserveExceeded,
        )));
    }
    let id = random_id16()?;
    conn.execute(
        "INSERT INTO maintenance_reservations(vault_id, reservation_id, purpose, resource_id,
                                              owner_device, reserved_bytes, consumed_bytes,
                                              state, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 'active', ?7)",
        (
            conn.scope().as_slice(),
            id.as_slice(),
            purpose.as_str(),
            resource_id,
            owner_device.as_slice(),
            requested_bytes,
            expires_at,
        ),
    )?;
    Ok(id)
}

/// 有没有 active（未过期）的 checkpoint 类 reservation——用于"单一 checkpoint writer lease"。
pub fn active_checkpoint_lease(conn: &ScopeConn, now: i64) -> Result<Option<Id16>> {
    let mut stmt = conn.prepare(
        "SELECT resource_id FROM maintenance_reservations
         WHERE purpose = 'checkpoint' AND state = 'active' AND expires_at > ?1
           AND vault_id = ?
         LIMIT 1",
    )?;
    let row = stmt
        .query_row((now, conn.scope().as_slice()), |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .optional()?;
    match row {
        None => Ok(None),
        Some(bytes) => {
            Ok(Some(bytes.as_slice().try_into().map_err(|_| {
                Error::Invariant("reservation resource_id")
            })?))
        }
    }
}

/// 标记完成（未用完的额度立即归还 R_available；**不删除任何已写数据**）。
pub fn complete_reservation(conn: &ScopeConn, id: &Id16) -> Result<()> {
    conn.execute(
        "UPDATE maintenance_reservations SET state = 'complete'
         WHERE reservation_id = ?1 AND state = 'active' AND vault_id = ?",
        (id.as_slice(), conn.scope().as_slice()),
    )?;
    Ok(())
}

/// 标记过期。
pub fn expire_reservation(conn: &ScopeConn, id: &Id16) -> Result<()> {
    conn.execute(
        "UPDATE maintenance_reservations SET state = 'expired'
         WHERE reservation_id = ?1 AND state = 'active' AND vault_id = ?",
        (id.as_slice(), conn.scope().as_slice()),
    )?;
    Ok(())
}

/// 清扫过期 reservation（只改状态，不删数据；ADR §13.2）。
pub fn expire_stale_reservations(conn: &ScopeConn, now: i64) -> Result<u64> {
    let changed = conn.execute(
        "UPDATE maintenance_reservations SET state = 'expired'
         WHERE state = 'active' AND expires_at <= ?1 AND vault_id = ?",
        (now, conn.scope().as_slice()),
    )?;
    Ok(changed as u64)
}

/// 按**真实 parts** 重算账本，并把对象归到正确预算类别（激活/GC 后对账；评审 0002 P1-E）。
///
/// 归类规则（与 [`future_live_bytes`] 同一 live 集合，pair = active epoch 最新两份 healthy
/// published）：
/// - live 对象（最新两份基线的 data + MANIFEST、log 仍保留的 UPDATE、同 epoch 未 retired
///   且未被基线引用的 finalized ASSET）→ `normal`；
/// - 其余**仍持有 parts** 的对象（旧 epoch 宽限基线、staging 候选、退役中的槽等）→
///   `maintenance`；
/// - 已无 parts 的 retired tombstone 行不动（不能被"复活"）。
///
/// 这样 `usage.normal_bytes/maintenance_bytes` 永远等于各类对象真实 parts 之和：检查点
/// 发布/轮换激活后，维护 reservation 写下的活对象从 `maintenance` 升到 `normal`，旧 epoch
/// 副本降为 `maintenance`，下一轮维护预算不会被已 live 的对象永久吃掉。幂等、自愈。
///
/// **不得绕过峰值**：归类后仍按真实 parts 落账，绝不把 `normal + maintenance` 调小来
/// 恢复可写空间（见 [`peak_exceeded`]）。总量超 `Q+R` 的历史数据保持"只读/可降级"，
/// 写入由 [`charge_normal`]/[`charge_maintenance`] 的 `ensure_peak_charge` 拒绝。
pub fn recompute(conn: &ScopeConn) -> Result<Usage> {
    let vault = vault::load(conn)?;
    let pair = live_pair_ids(conn, vault.active_epoch, &[])?;
    let params = pair_params(&pair, vault.active_epoch, conn)?;
    let vault_param = pair.len() + 2;
    // 仍有 parts 的对象：live → normal，其余 → maintenance（即使历史类别是 retired，
    // 只要 parts 还在就必须进账本，保证 ledger = 真实 parts）。
    let classify_sql = format!(
        "{} UPDATE objects SET budget_class = CASE\n\
              WHEN object_id IN (SELECT object_id FROM live) THEN 'normal'\n\
              ELSE 'maintenance'\n\
          END\n\
          WHERE vault_id = ?{vault_param}\n\
            AND EXISTS (SELECT 1 FROM object_parts p\n\
                        WHERE p.object_id = objects.object_id\n\
                          AND p.vault_id = ?{vault_param})",
        live_cte_sql(pair.len())
    );
    conn.execute(&classify_sql, rusqlite::params_from_iter(params))?;

    let (normal, maintenance): (i64, i64) = conn.query_row(
        "SELECT
            COALESCE(SUM(CASE WHEN o.budget_class = 'normal' THEN length(p.envelope) ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN o.budget_class = 'maintenance' THEN length(p.envelope) ELSE 0 END), 0)
         FROM object_parts p JOIN objects o ON o.object_id = p.object_id
         WHERE p.vault_id = ?1 AND o.vault_id = ?1",
        [conn.scope().as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let next = Usage {
        normal_bytes: normal,
        maintenance_bytes: maintenance,
    };
    // 归类只改变两桶之间的**分布**，不改变真实总量（= 全部仍持有 parts 的字节和）。
    // 因此这里绝不能"为了让下一次写入通过"而抹平或限额：写入侧的真实总量门禁
    // （`charge_*` 的 `Q+R`）使用的就是这份账本。若历史数据已超峰（旧版本或缩容），
    // 超峰事实必须原样落账——读与回收（GC 只会让 total 下降）继续可用，增量写入被
    // 507 拒绝；recompute 本身不返回错误，否则 GC/降级的回收事务会被整体回滚。
    conn.execute(
        "UPDATE usage SET normal_bytes = ?1, maintenance_bytes = ?2 WHERE vault_id = ?",
        (
            next.normal_bytes,
            next.maintenance_bytes,
            conn.scope().as_slice(),
        ),
    )?;
    Ok(next)
}

/// 便捷：当前时刻（测试里是 FakeClock）。
pub fn now(clock: &dyn Clock) -> i64 {
    clock.now_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::envelope::ObjectType;
    use crate::ids::{Hash32, Id16};
    use crate::scope::{ScopeConn, VaultScope};
    use rusqlite::Connection;

    const TEST_VAULT: Id16 = [0xA1u8; 16];

    /// 建 schema + 一个 vault 行 + usage 行；返回作用域连接。
    fn fixture(conn: &Connection) -> ScopeConn<'_> {
        conn.execute(
            "INSERT INTO vaults(vault_id, instance_id, log_id, log_incarnation, cursor_key,
                                state, active_epoch, next_epoch, manager_rev, control_rev,
                                auth_rev, head_seq, gc_floor, normal_limit, maintenance_limit,
                                legacy_default, created_at)
             VALUES (?1, ?2, ?2, ?2, ?3, 'active', 1, 2, 1, 1, 1, 0, 0, 1048576, 2097152, 1, 0)",
            (
                TEST_VAULT.as_slice(),
                [0u8; 16].as_slice(),
                [0u8; 32].as_slice(),
            ),
        )
        .expect("vault row");
        conn.execute(
            "INSERT INTO usage(vault_id, normal_bytes, maintenance_bytes) VALUES (?1, 0, 0)",
            [TEST_VAULT.as_slice()],
        )
        .expect("usage row");
        ScopeConn::new(conn, VaultScope::new(TEST_VAULT))
    }

    /// 插入一行 complete 对象 + 单个分片；返回 object_id。
    fn insert_object(conn: &ScopeConn, seed: u8, kind: ObjectType, envelope_len: usize) -> Hash32 {
        let object_id: Hash32 = [seed; 32];
        conn.execute(
            "INSERT INTO objects(vault_id, object_id, message_id, producer, epoch, type, generation,
                                 sender_seq, part_count, plain_len, state, budget_class,
                                 created_at, touched_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, 0, 0, 1, 1, 'complete', 'maintenance', 0, 0)",
            (
                conn.scope().as_slice(),
                object_id.as_slice(),
                [seed; 16].as_slice(),
                [0u8; 16].as_slice(),
                kind.as_i64(),
            ),
        )
        .expect("insert object");
        conn.execute(
            "INSERT INTO object_parts(vault_id, object_id, part_no, envelope, digest)
             VALUES (?1, ?2, 0, ?3, ?4)",
            (
                conn.scope().as_slice(),
                object_id.as_slice(),
                vec![seed; envelope_len],
                [seed; 32].as_slice(),
            ),
        )
        .expect("insert part");
        object_id
    }

    fn insert_checkpoint(conn: &ScopeConn, seed: u8, state: &str, published_at: Option<i64>) {
        conn.execute(
            "INSERT INTO checkpoints(vault_id, checkpoint_id, epoch, covered_seq,
                                     validation_version, health, state, created_at, published_at)
             VALUES (?1, ?2, 1, 0, 1, 'healthy', ?3, 0, ?4)",
            (
                conn.scope().as_slice(),
                [seed; 16].as_slice(),
                state,
                published_at,
            ),
        )
        .expect("insert checkpoint");
    }

    fn add_ref(conn: &ScopeConn, checkpoint: u8, object_id: &Hash32, role: &str) {
        conn.execute(
            "INSERT INTO checkpoint_refs(vault_id, checkpoint_id, object_id, role)
             VALUES (?1, ?2, ?3, ?4)",
            (
                conn.scope().as_slice(),
                [checkpoint; 16].as_slice(),
                object_id.as_slice(),
                role,
            ),
        )
        .expect("insert ref");
    }

    fn add_log(conn: &ScopeConn, relay_seq: i64, object_id: &Hash32, kind: &str) {
        conn.execute(
            "INSERT INTO log(vault_id, relay_seq, kind, object_id, epoch, event, created_at)
             VALUES (?1, ?2, ?3, ?4, 1, x'00', 0)",
            (
                conn.scope().as_slice(),
                relay_seq,
                kind,
                object_id.as_slice(),
            ),
        )
        .expect("insert log");
    }

    fn open() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open");
        schema::apply(&mut conn).expect("schema");
        conn
    }

    #[test]
    fn future_live_counts_latest_pair_data_manifest_and_retained_updates_once() {
        let conn = open();
        let scoped = fixture(&conn);

        let old_snap = insert_object(&scoped, 1, ObjectType::Snapshot, 300);
        let new_snap = insert_object(&scoped, 2, ObjectType::Snapshot, 400);
        let asset_a = insert_object(&scoped, 3, ObjectType::Asset, 500);
        let asset_b = insert_object(&scoped, 4, ObjectType::Asset, 600);
        let shared = insert_object(&scoped, 5, ObjectType::Asset, 700);
        let manifest = insert_object(&scoped, 6, ObjectType::Manifest, 212);
        let logged_update = insert_object(&scoped, 7, ObjectType::Update, 250);
        let unlogged_update = insert_object(&scoped, 8, ObjectType::Update, 260);

        // 两份已发布基线：只有"最新一份"进入未来 live；更旧的 baseline 及其独有对象不计。
        insert_checkpoint(&scoped, 11, "published", Some(10));
        insert_checkpoint(&scoped, 12, "published", Some(20));
        insert_checkpoint(&scoped, 13, "staging", None);
        add_ref(&scoped, 11, &old_snap, "data");
        add_ref(&scoped, 12, &asset_a, "data");
        add_ref(&scoped, 12, &shared, "data");
        // incoming：新 snapshot + 与最新 published 共享的 asset + manifest（MANIFEST 密文
        // 同样是真实保留的 metadata，必须计入 live）。
        add_ref(&scoped, 13, &new_snap, "data");
        add_ref(&scoped, 13, &asset_b, "data");
        add_ref(&scoped, 13, &shared, "data");
        add_ref(&scoped, 13, &manifest, "manifest");
        // log 仍保留的 UPDATE 计入；未记日志的 UPDATE 不是 live。
        add_log(&scoped, 1, &logged_update, "update");
        add_log(&scoped, 2, &unlogged_update, "control");

        let live = future_live_bytes(&scoped, 1, &[[13u8; 16]]).expect("live");
        assert_eq!(
            live,
            400 + 500 + 600 + 700 + 212 + 250,
            "最新一份 published + incoming 的 data/manifest refs；共享对象只计一次；log UPDATE 计入；更旧基线独有对象 / 未记日志 UPDATE 不计"
        );

        // 只有 incoming 一份基线时，同样只算它自己的 data refs + log 仍保留的 UPDATE。
        scoped
            .execute(
                "DELETE FROM checkpoint_refs
                 WHERE vault_id = ?1 AND checkpoint_id IN (
                   SELECT checkpoint_id FROM checkpoints
                   WHERE state = 'published' AND vault_id = ?1
                 )",
                [scoped.scope().as_slice()],
            )
            .expect("delete refs");
        scoped
            .execute(
                "DELETE FROM checkpoints WHERE state = 'published' AND vault_id = ?1",
                [scoped.scope().as_slice()],
            )
            .expect("delete published");
        let live_first = future_live_bytes(&scoped, 1, &[[13u8; 16]]).expect("live first");
        assert_eq!(
            live_first,
            400 + 600 + 700 + 212 + 250,
            "第一份基线只计自身 data/manifest refs 与 log UPDATE"
        );
    }

    #[test]
    fn future_live_counts_only_active_assets_unreferenced_by_pair() {
        let conn = open();
        let scoped = fixture(&conn);

        let snap = insert_object(&scoped, 21, ObjectType::Snapshot, 400);
        let referenced_asset = insert_object(&scoped, 22, ObjectType::Asset, 300);
        let orphan_asset = insert_object(&scoped, 23, ObjectType::Asset, 500);
        let retired_asset = insert_object(&scoped, 24, ObjectType::Asset, 600);
        let other_epoch_asset = insert_object(&scoped, 25, ObjectType::Asset, 700);

        insert_checkpoint(&scoped, 31, "staging", None);
        add_ref(&scoped, 31, &snap, "data");
        add_ref(&scoped, 31, &referenced_asset, "data");
        let register = |object_id: &Hash32, epoch: u64, state: &str| {
            scoped
                .execute(
                    "INSERT INTO asset_objects(vault_id, epoch, object_id, state, finalized_seq)
                     VALUES (?1, ?2, ?3, ?4, 0)",
                    (
                        scoped.scope().as_slice(),
                        i64::try_from(epoch).expect("epoch"),
                        object_id.as_slice(),
                        state,
                    ),
                )
                .expect("asset registry");
        };
        register(&referenced_asset, 1, "active");
        register(&orphan_asset, 1, "active");
        register(&retired_asset, 1, "retired");
        register(&other_epoch_asset, 2, "active");

        let live = future_live_bytes(&scoped, 1, &[[31u8; 16]]).expect("live");
        assert_eq!(
            live,
            400 + 300 + 500,
            "pair data refs + 同 epoch 未 retired 且未被 pair 引用的 ASSET；retired / 其他 epoch 的 ASSET 不计，pair 已引用的 ASSET 去重"
        );

        // 登记为 retired（密文已回收）的孤儿资产不再占 live。
        scoped
            .execute(
                "UPDATE asset_objects SET state = 'retired'
                  WHERE epoch = 1 AND object_id = ?1 AND vault_id = ?",
                (orphan_asset.as_slice(), scoped.scope().as_slice()),
            )
            .expect("retire asset");
        let after = future_live_bytes(&scoped, 1, &[[31u8; 16]]).expect("live after retire");
        assert_eq!(after, 400 + 300);
    }

    #[test]
    fn usage_total_saturates_and_peak_boundary_is_inclusive() {
        let usage = Usage {
            normal_bytes: 6,
            maintenance_bytes: 4,
        };
        assert_eq!(usage.total(), 10);
        assert!(usage.within_peak(10), "峰值边界必须包含等号（≤ Q+R）");
        assert!(!usage.within_peak(9));

        let saturated = Usage {
            normal_bytes: i64::MAX,
            maintenance_bytes: i64::MAX,
        };
        assert_eq!(saturated.total(), i64::MAX, "total 必须饱和，不得溢出");

        assert_eq!(Usage::default().total(), 0);
        assert!(Usage::default().within_peak(0));
    }

    #[test]
    fn reservation_remaining_saturates() {
        let reservation = Reservation {
            reservation_id: [1u8; 16],
            purpose: ReservationPurpose::Checkpoint,
            resource_id: vec![1, 2, 3],
            owner_device: [2u8; 16],
            reserved_bytes: 100,
            consumed_bytes: 100,
            state: ReservationState::Active,
            expires_at: 0,
        };
        assert_eq!(reservation.remaining(), 0);
    }

    #[test]
    fn purpose_and_state_roundtrip() {
        assert_eq!(
            ReservationPurpose::parse(ReservationPurpose::Rotation.as_str()),
            Some(ReservationPurpose::Rotation)
        );
        assert_eq!(
            ReservationState::parse(ReservationState::Expired.as_str()),
            Some(ReservationState::Expired)
        );
        assert_eq!(ReservationState::parse("bogus"), None);
    }

    /// 隔离：另一个 vault 的 usage/live 数字绝不进入本作用域。
    #[test]
    fn usage_is_scoped_per_vault() {
        let conn = open();
        let scoped = fixture(&conn);
        let other_scope = VaultScope::new([0xB2u8; 16]);
        conn.execute(
            "INSERT INTO vaults(vault_id, instance_id, log_id, log_incarnation, cursor_key,
                                state, active_epoch, next_epoch, manager_rev, control_rev,
                                auth_rev, head_seq, gc_floor, normal_limit, maintenance_limit,
                                legacy_default, created_at)
             VALUES (?1, ?2, ?2, ?2, ?3, 'active', 1, 2, 1, 1, 1, 0, 0, 1048576, 2097152, 0, 0)",
            (
                other_scope.vault_id().as_slice(),
                [0u8; 16].as_slice(),
                [0u8; 32].as_slice(),
            ),
        )
        .expect("other vault");
        conn.execute(
            "INSERT INTO usage(vault_id, normal_bytes, maintenance_bytes) VALUES (?1, 777, 888)",
            [other_scope.vault_id().as_slice()],
        )
        .expect("other usage");
        assert_eq!(usage(&scoped).expect("usage"), Usage::default());
        let other = ScopeConn::new(&conn, other_scope);
        assert_eq!(
            usage(&other).expect("other usage"),
            Usage {
                normal_bytes: 777,
                maintenance_bytes: 888
            }
        );
    }
}
