//! vault 生命周期注册表 + 票据 reservation/claim/cancel（ADR 0018 §2）。
//!
//! 语义边界（S2）：
//! - `vault_registry` 是生命周期**权威**；`vaults` 只是租户内容表。`ACTIVE` 必须对应
//!   `vaults` 行；`RESERVED` 只有 registry 行 + 票据（还没有租户内容）。
//! - 票据绑定 registry：只有 `RESERVED` 能被 claim。claim 在**同一个写事务**里完成
//!   票据消费、`RESERVED → ACTIVE` CAS、genesis/租户行与兄弟票据失效；任一步失败整体回滚
//!   （票据不消费、不留半个 vault）。
//! - `cancel_reservation` 与 claim 竞争只有一个赢：claim 赢后取消返回 409（**绝不**删除
//!   已创建的 vault）；取消赢后 claim 的票据已被作废，只能 403。
//! - 列表/详情是 S4 Web 的**只读**入口：只读库、不迁移、不写库。`RESERVED` 的票据过期
//!   只在读时**推导**为 `EXPIRED`；把 registry 真正落成 `EXPIRED` 只发生在 cancel 与迁移里。
//! - 状态机（ADR 0018 §2）：`RESERVED → ACTIVE | EXPIRED | CANCELLED`；
//!   `ACTIVE → DELETING → DELETED`（失败 `DELETE_FAILED`）属于 S3，本切片只保留状态列。

use rusqlite::{Connection, OptionalExtension};

use crate::admin;
use crate::config::RelayConfig;
use crate::db::Db;
use crate::error::{Error, ErrorCode, Result};
use crate::ids::Id16;
use crate::scope::ScopeConn;
use crate::sig::SignatureHook;
use crate::time::Clock;
use crate::vault::{self, InitOutcome, InitRequest, Vault};

/// 生命周期状态（`vault_registry.state`，ADR 0018 §2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// 已发票据、尚未 genesis（无 `vaults` 行）。
    Reserved,
    /// 已完成 genesis（必须有 `vaults` 行）。
    Active,
    /// 预留的票据在有效期内未被领取。
    Expired,
    /// 运维显式取消（票据已作废；ID 禁止复用）。
    Cancelled,
    /// S3：删除作业进行中（fenced）。
    Deleting,
    /// S3：终态墓碑（只留最小管理元数据）。
    Deleted,
    /// S3：删除失败（只能继续删除，永不重开 active）。
    DeleteFailed,
}

impl LifecycleState {
    /// wire/DB 文本（不得静默改名：它是迁移与运维口径的一部分）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "RESERVED",
            Self::Active => "ACTIVE",
            Self::Expired => "EXPIRED",
            Self::Cancelled => "CANCELLED",
            Self::Deleting => "DELETING",
            Self::Deleted => "DELETED",
            Self::DeleteFailed => "DELETE_FAILED",
        }
    }

    /// 严格解析（未知值 = 矛盾状态，调用方必须 fail closed）。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "RESERVED" => Some(Self::Reserved),
            "ACTIVE" => Some(Self::Active),
            "EXPIRED" => Some(Self::Expired),
            "CANCELLED" => Some(Self::Cancelled),
            "DELETING" => Some(Self::Deleting),
            "DELETED" => Some(Self::Deleted),
            "DELETE_FAILED" => Some(Self::DeleteFailed),
            _ => None,
        }
    }

    /// 活状态：`RESERVED`（可领取）与 `ACTIVE`（可服务）。其余都是终态/删除作业状态。
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Reserved | Self::Active)
    }

    /// 在线删除作业状态（ADR 0018 §3）：数据面 admission 一律关闭；**永不重开** `ACTIVE`。
    pub const fn is_delete_state(self) -> bool {
        matches!(self, Self::Deleting | Self::DeleteFailed | Self::Deleted)
    }
}

/// `vault_registry` 一行（运维/测试/详情视图；**不含**租户内容/密钥）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryRow {
    /// 128-bit 公开路由标识（全局唯一含墓碑）。
    pub vault_id: Id16,
    /// 生命周期状态。
    pub state: LifecycleState,
    /// 生命周期开始时刻（预留创建时刻；迁移的旧 vault 取它既有 `created_at`）。
    pub created_at: i64,
    /// 生命周期结束时刻（取消/过期/删除；未知则 `None`，不虚构）。
    pub deleted_at: Option<i64>,
    /// S3：删除作业 id。
    pub delete_job_id: Option<Id16>,
    /// S3：删除进度（已清理租户表数/字节口径由 S3 定义，这里只保证非负）。
    pub delete_progress: u64,
    /// S3：删除失败原因（只存运维可读的短文本，不含租户内容）。
    pub delete_error: Option<String>,
}

fn id16_from_blob(bytes: Vec<u8>, field: &'static str) -> Result<Id16> {
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Invariant(field))
}

/// 读一行 registry（**不解作用域**：registry 是全局运维表，调用方自带 `vault_id`）。
pub fn load_raw(conn: &Connection, vault_id: &Id16) -> Result<Option<RegistryRow>> {
    let row = conn
        .query_row(
            "SELECT vault_id, state, created_at, deleted_at, delete_job_id,
                    delete_progress, delete_error
               FROM vault_registry WHERE vault_id = ?1",
            [vault_id.as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((
        vault_id,
        state,
        created_at,
        deleted_at,
        delete_job_id,
        delete_progress,
        delete_error,
    )) = row
    else {
        return Ok(None);
    };
    let delete_job_id = match delete_job_id {
        None => None,
        Some(bytes) => Some(id16_from_blob(bytes, "vault_registry.delete_job_id")?),
    };
    Ok(Some(RegistryRow {
        vault_id: id16_from_blob(vault_id, "vault_registry.vault_id")?,
        state: LifecycleState::parse(&state).ok_or(Error::Invariant("vault_registry.state"))?,
        created_at,
        deleted_at,
        delete_job_id,
        delete_progress: u64::try_from(delete_progress)
            .map_err(|_| Error::Invariant("vault_registry.delete_progress"))?,
        delete_error,
    }))
}

/// 读当前作用域的 registry 行（不存在 = `None`）。
pub fn load(conn: &ScopeConn) -> Result<Option<RegistryRow>> {
    load_raw(conn.raw(), conn.vault_id())
}

/// 读状态（不存在 = `None`）。
pub fn state_raw(conn: &Connection, vault_id: &Id16) -> Result<Option<LifecycleState>> {
    Ok(load_raw(conn, vault_id)?.map(|row| row.state))
}

/// **事务级 fence 复核**（ADR 0018 §3）：删除态的 vault 拒绝一切 scoped 读写。
///
/// 为什么放在 DB 层：HTTP 取消/排队不会取消已入队 job，admission 的内存检查存在
/// "已通过检查、事务稍后才执行"的窗口；每个写事务在拿到写锁后**再次**查权威 registry
/// （单 writer 串行化 ⇒ 与 `ACTIVE → DELETING` CAS 有全序），因此不能被绕过。
/// 读路径同样复核：删除作业提交过 purge 后不得再返回旧数据。
///
/// 只拒绝删除三态；`RESERVED/EXPIRED/CANCELLED` 仍走各自的状态机（claim/cancel 语义不变）。
pub fn ensure_not_fenced(conn: &Connection, vault_id: &Id16) -> Result<()> {
    if let Some(state) = state_raw(conn, vault_id)? {
        if state.is_delete_state() {
            return Err(Error::api(ErrorCode::VaultDeleted));
        }
    }
    Ok(())
}

/// 启动恢复用：全部删除态 registry 行（`DELETING/DELETE_FAILED/DELETED`）。
///
/// 只读；顺序稳定（`created_at, vault_id`），便于日志/测试断言。状态缺失或矛盾由调用方
/// fail closed（ADR 0018 §3：`DELETING` 缺 `delete_job_id` 等）。
pub fn load_delete_rows(conn: &Connection) -> Result<Vec<RegistryRow>> {
    let mut stmt = conn.prepare(
        "SELECT vault_id, state, created_at, deleted_at, delete_job_id,
                delete_progress, delete_error
           FROM vault_registry
          WHERE state IN ('DELETING','DELETE_FAILED','DELETED')
          ORDER BY created_at, vault_id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<Vec<u8>>>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (vault_id, state, created_at, deleted_at, delete_job_id, delete_progress, delete_error) =
            row?;
        let state =
            LifecycleState::parse(&state).ok_or(Error::Invariant("vault_registry.state"))?;
        if !state.is_delete_state() {
            return Err(Error::Invariant("vault_registry query returned live state"));
        }
        let delete_job_id = match delete_job_id {
            None => None,
            Some(bytes) => Some(id16_from_blob(bytes, "vault_registry.delete_job_id")?),
        };
        out.push(RegistryRow {
            vault_id: id16_from_blob(vault_id, "vault_registry.vault_id")?,
            state,
            created_at,
            deleted_at,
            delete_job_id,
            delete_progress: u64::try_from(delete_progress)
                .map_err(|_| Error::Invariant("vault_registry.delete_progress"))?,
            delete_error,
        });
    }
    Ok(out)
}

/// `vaults` 是否已有该行（租户内容表；与 registry 的一致性由本模块维护）。
pub(crate) fn vault_row_exists(conn: &Connection, vault_id: &Id16) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM vaults WHERE vault_id = ?1",
        [vault_id.as_slice()],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// genesis 落 `vaults` 行时的 registry 收口（ADR 0018 §2："ACTIVE 必须对应 vaults 行"）。
///
/// - 没有 registry 行（v1 legacy/直接 setup-secret genesis）→ 插 `ACTIVE`（`created_at=now`）；
/// - `RESERVED`（票据 claim 的 CAS 之外的兜底路径）→ CAS 到 `ACTIVE`；
/// - `ACTIVE`（claim 已在同事务里 CAS 过）→ 幂等 no-op；
/// - 其它终态 → 409：**终态 ID 禁止复用**，绝不把墓碑重新激活。
pub fn ensure_active_on_genesis(conn: &Connection, vault_id: &Id16, now: i64) -> Result<()> {
    match state_raw(conn, vault_id)? {
        None => {
            conn.execute(
                "INSERT INTO vault_registry(vault_id, state, created_at)
                 VALUES (?1, 'ACTIVE', ?2)",
                (vault_id.as_slice(), now),
            )?;
            Ok(())
        }
        Some(LifecycleState::Active) => Ok(()),
        Some(LifecycleState::Reserved) => {
            let changed = conn.execute(
                "UPDATE vault_registry SET state = 'ACTIVE'
                  WHERE vault_id = ?1 AND state = 'RESERVED'",
                [vault_id.as_slice()],
            )?;
            if changed != 1 {
                return Err(Error::api(ErrorCode::CasConflict));
            }
            Ok(())
        }
        Some(_) => Err(Error::api(ErrorCode::CasConflict)),
    }
}

/// `RESERVED → ACTIVE` 的 CAS（claim 用；失败说明状态已被别人改掉）。
fn cas_reserved_to_active(conn: &Connection, vault_id: &Id16) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE vault_registry SET state = 'ACTIVE'
          WHERE vault_id = ?1 AND state = 'RESERVED'",
        [vault_id.as_slice()],
    )?;
    Ok(changed == 1)
}

/// `/v2/vaults` 的 claim 事务（调用方已经包在 `Db::write` 的 `BEGIN IMMEDIATE` 里）。
///
/// 顺序（每一步都在同一事务里，失败整体回滚）：
/// 1. **票据先验**（有效/未消费/未过期/绑定本 vault）——失败统一 403，不泄露 vault 是否存在；
/// 2. registry 状态：`ACTIVE` → 409 `ALREADY_INITIALIZED`（不消费票据，旧行为不变）；
///    `RESERVED` → 继续；其它终态/删除状态 → 403（票据已在取消时作废）；
/// 3. 消费票据（`quota=0` + `consumed_at` CAS）；
/// 4. `RESERVED → ACTIVE` CAS；
/// 5. genesis/租户行（`vault::create_vault_with_ticket`，含 registry 幂等收口）；
/// 6. 同 vault 兄弟票据全部作废；
/// 7. 读回 vault 供响应使用。
///
/// 成功后 registry/vaults/control/票据四者一致；claim 赢后 cancel 只能得到 409。
pub fn claim_reserved(
    conn: &ScopeConn,
    clock: &dyn Clock,
    cfg: &RelayConfig,
    hook: &dyn SignatureHook,
    request: &InitRequest,
    ticket: &[u8],
) -> Result<(InitOutcome, Vault)> {
    let now = clock.now_secs();
    // 1) 票据先验：不存在/过期/已消费/错 vault 一律同一个 403。
    admin::validate_create_ticket(conn, now, ticket)?;
    // 2) registry 状态。
    match state_raw(conn.raw(), conn.vault_id())? {
        None => {
            // 票据存在却没有 registry 行 = 损坏（新签发与迁移都会建行）：fail closed。
            return Err(Error::Invariant(
                "claim: create ticket without vault_registry row",
            ));
        }
        Some(LifecycleState::Active) => {
            return Err(Error::api(ErrorCode::AlreadyInitialized));
        }
        Some(LifecycleState::Reserved) => {}
        // EXPIRED/CANCELLED：票据理论上已被作废（上一步 403）；若仍有效，按"票据无效"拒绝，
        // 不泄露"这个 vault 存在过"的更多信息。DELETING/DELETED/DELETE_FAILED 同理。
        Some(_) => return Err(admin::ticket_rejected()),
    }
    // 3) 消费票据（CAS）。
    admin::claim_create_ticket(conn, now, ticket)?;
    // 4) RESERVED → ACTIVE（CAS；失败 = 状态被并发改掉，整体回滚）。
    if !cas_reserved_to_active(conn.raw(), conn.vault_id())? {
        return Err(Error::api(ErrorCode::CasConflict));
    }
    // 5) genesis：vaults 行/首个 epoch/设备/manager key/控制链/usage（同事务）。
    let outcome = vault::create_vault_with_ticket(conn, clock, cfg, hook, request)?;
    // 6) 兄弟票据失效：同一 vault 的其它未消费票据一律作废（含已过期但尚未标记的）。
    admin::invalidate_sibling_tickets(conn, now, &crate::ids::sha256(ticket))?;
    // 7) 读回（响应与调用方断言用）。
    let vault = vault::load(conn)?;
    Ok((outcome, vault))
}

/// 取消一个预留的结果（重复取消幂等；已过期只报事实，不伪造"取消成功"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    /// 本次真正把 `RESERVED` 改成 `CANCELLED`，并作废了所有未消费票据。
    Cancelled {
        /// 被取消的 vault。
        vault_id: Id16,
        /// 取消时刻（同事务采样的 relay 时钟）。
        cancelled_at: i64,
        /// 本次作废的票据数（`consumed_at` 由 NULL 改为时刻的行数）。
        invalidated_tickets: u64,
    },
    /// 之前已经取消过（幂等，不再改库）。
    AlreadyCancelled,
    /// 预留已过期：本次把它显式标成 `EXPIRED` 并作废票据（或它本来就是 `EXPIRED`）。
    Expired,
}

/// `RESERVED → CANCELLED`（或已过期 → `EXPIRED`）并作废该 vault 的全部未消费票据。
///
/// 与 claim 的竞争由 writer 串行保证：谁先提交谁赢。
/// - claim 赢（registry `ACTIVE`）→ 409（**绝不**删除已创建的 vault）；
/// - cancel 赢 → claim 的票据已被作废，只能 403。
pub fn cancel_reservation(db: &Db, vault_id: Id16) -> Result<CancelOutcome> {
    let clock = db.clock_handle();
    db.write_instance_blocking(move |conn| {
        let now = clock.now_secs();
        let Some(row) = load_raw(conn, &vault_id)? else {
            return Err(Error::api(ErrorCode::NotFound));
        };
        match row.state {
            LifecycleState::Reserved => {
                // "已过期可标 EXPIRED"：没有仍有效的票据（或全部过期）时按过期处理。
                let valid_expiry = outstanding_ticket_expiry(conn, &vault_id)?;
                if valid_expiry.is_none_or(|expiry| expiry <= now) {
                    let changed = conn.execute(
                        "UPDATE vault_registry SET state = 'EXPIRED', deleted_at = ?1
                          WHERE vault_id = ?2 AND state = 'RESERVED'",
                        (now, vault_id.as_slice()),
                    )?;
                    if changed != 1 {
                        return Err(Error::api(ErrorCode::CasConflict));
                    }
                    invalidate_all_tickets(conn, &vault_id, now)?;
                    return Ok(CancelOutcome::Expired);
                }
                let changed = conn.execute(
                    "UPDATE vault_registry SET state = 'CANCELLED', deleted_at = ?1
                      WHERE vault_id = ?2 AND state = 'RESERVED'",
                    (now, vault_id.as_slice()),
                )?;
                if changed != 1 {
                    return Err(Error::api(ErrorCode::CasConflict));
                }
                let invalidated_tickets = invalidate_all_tickets(conn, &vault_id, now)?;
                Ok(CancelOutcome::Cancelled {
                    vault_id,
                    cancelled_at: now,
                    invalidated_tickets,
                })
            }
            LifecycleState::Cancelled => Ok(CancelOutcome::AlreadyCancelled),
            LifecycleState::Expired => Ok(CancelOutcome::Expired),
            // claim 已赢（或 S3 已开始删除）：冲突，不删已创建/已删除的 vault。
            LifecycleState::Active
            | LifecycleState::Deleting
            | LifecycleState::Deleted
            | LifecycleState::DeleteFailed => Err(Error::api(ErrorCode::CasConflict)),
        }
    })
}

/// 该 vault 仍可领取的票据的最晚过期时刻（没有 = `None`）。
fn outstanding_ticket_expiry(conn: &Connection, vault_id: &Id16) -> Result<Option<i64>> {
    let value: Option<i64> = conn.query_row(
        "SELECT MAX(expires_at) FROM create_tickets
          WHERE vault_id = ?1 AND consumed_at IS NULL AND quota > 0",
        [vault_id.as_slice()],
        |row| row.get(0),
    )?;
    Ok(value)
}

/// 作废该 vault 全部未消费票据（`quota=0` + `consumed_at=now`）。
fn invalidate_all_tickets(conn: &Connection, vault_id: &Id16, now: i64) -> Result<u64> {
    let changed = conn.execute(
        "UPDATE create_tickets SET quota = 0, consumed_at = ?1
          WHERE vault_id = ?2 AND consumed_at IS NULL",
        (now, vault_id.as_slice()),
    )?;
    Ok(u64::try_from(changed).unwrap_or(u64::MAX))
}

/// 列表项（S4 Web 的只读投影；不含票据明文/租户内容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultListEntry {
    /// 128-bit 公开路由标识。
    pub vault_id: Id16,
    /// 有效状态：`RESERVED` 的票据过期/不存在时读时推导为 `EXPIRED`（不写库）。
    pub state: LifecycleState,
    /// registry 里实际存的状态（诊断/S3 口径）。
    pub stored_state: LifecycleState,
    /// 生命周期开始时刻。
    pub created_at: i64,
    /// `RESERVED` 的有效票据最晚过期时刻（无有效票据 = `None`）。
    pub expires_at: Option<i64>,
    /// 仍可领取的票据数（未消费且 quota > 0）。
    pub outstanding_tickets: u64,
    /// `vaults` 行是否存在（`ACTIVE` 必须为 true；违例时 S4 显示告警）。
    pub vault_exists: bool,
    /// 设备行数（`devices`；`RESERVED`/墓碑 = 0）。S4 列表/详情只显示计数。
    pub device_count: u64,
    /// S3 删除进度（已清空的租户表数；非删除态 = 0）。S4 列表显示 `n/26`。
    pub delete_progress: u64,
}

/// 详情（S4 Web；同样只读、不迁移）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultDetail {
    /// 列表口径的公共字段。
    pub entry: VaultListEntry,
    /// 生命周期结束时刻（取消/过期/删除；未知则 `None`）。
    pub deleted_at: Option<i64>,
    /// S3：删除作业 id。
    pub delete_job_id: Option<Id16>,
    /// S3：删除进度。
    pub delete_progress: u64,
    /// S3：删除失败原因。
    pub delete_error: Option<String>,
    /// v1 legacy 默认 vault 标记（`vaults.legacy_default`）。
    pub legacy_default: bool,
    /// 授权状态（`vaults.authz_state`；`legacy/active/recovery_only`；无 vault 行 = `None`）。
    pub authz_state: Option<String>,
    /// 活动 epoch（无 vault 行 = `None`）。
    pub active_epoch: Option<u64>,
    /// 日志头水位（无 vault 行 = `None`）。
    pub head_seq: Option<u64>,
    /// 票据总数（含历史）。
    pub tickets_total: u64,
    /// 已消费/已作废票据数。
    pub tickets_consumed: u64,
    /// 仍可领取的票据数。
    pub tickets_outstanding: u64,
    /// 运行状态（`vaults.state`：`initializing`/`active`/`rotating`；无 vault 行 = `None`）。
    ///
    /// 与生命周期（registry）/授权状态（`authz_state`）**独立**（ADR 0018 §2）。
    pub runtime_state: Option<String>,
    /// 控制提交 rev（`vaults.control_rev`；无 vault 行 = `None`）。
    pub control_rev: Option<u64>,
    /// 普通配额已用字节（`usage.normal_bytes`；无行 = `None`）。
    pub usage_normal_bytes: Option<u64>,
    /// 维护配额已用字节（`usage.maintenance_bytes`；无行 = `None`）。
    pub usage_maintenance_bytes: Option<u64>,
}

/// `RESERVED` 的有效状态推导（读时；不写库）。
fn effective_state(
    stored: LifecycleState,
    expires_at: Option<i64>,
    outstanding: u64,
    now: i64,
) -> LifecycleState {
    if stored != LifecycleState::Reserved {
        return stored;
    }
    match expires_at {
        Some(expiry) if outstanding > 0 && expiry > now => LifecycleState::Reserved,
        _ => LifecycleState::Expired,
    }
}

/// 只读：列出所有 registry 行（S4 Web 列表；**不**触发迁移、不写库）。
pub fn list_vaults(conn: &Connection, now: i64) -> Result<Vec<VaultListEntry>> {
    let mut stmt = conn.prepare(
        "SELECT r.vault_id, r.state, r.created_at,
                (SELECT MAX(t.expires_at) FROM create_tickets t
                  WHERE t.vault_id = r.vault_id AND t.consumed_at IS NULL AND t.quota > 0),
                (SELECT COUNT(*) FROM create_tickets t
                  WHERE t.vault_id = r.vault_id AND t.consumed_at IS NULL AND t.quota > 0),
                EXISTS(SELECT 1 FROM vaults v WHERE v.vault_id = r.vault_id),
                (SELECT COUNT(*) FROM devices d WHERE d.vault_id = r.vault_id),
                r.delete_progress
           FROM vault_registry r
          ORDER BY r.created_at, r.vault_id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (vault_id, state, created_at, expires_at, outstanding, exists, devices, progress) =
            row?;
        let stored_state =
            LifecycleState::parse(&state).ok_or(Error::Invariant("vault_registry.state"))?;
        let outstanding_tickets = u64::try_from(outstanding)
            .map_err(|_| Error::Invariant("vault_registry.outstanding"))?;
        out.push(VaultListEntry {
            vault_id: id16_from_blob(vault_id, "vault_registry.vault_id")?,
            state: effective_state(stored_state, expires_at, outstanding_tickets, now),
            stored_state,
            created_at,
            expires_at,
            outstanding_tickets,
            vault_exists: exists != 0,
            device_count: u64::try_from(devices)
                .map_err(|_| Error::Invariant("vault_registry.devices"))?,
            delete_progress: u64::try_from(progress)
                .map_err(|_| Error::Invariant("vault_registry.delete_progress"))?,
        });
    }
    Ok(out)
}

/// 只读：单个 vault 的详情（不存在 = `None`；不迁移、不写库）。
pub fn vault_detail(conn: &Connection, vault_id: &Id16, now: i64) -> Result<Option<VaultDetail>> {
    let row = conn
        .query_row(
            "SELECT r.vault_id, r.state, r.created_at, r.deleted_at, r.delete_job_id,
                    r.delete_progress, r.delete_error,
                    (SELECT COUNT(*) FROM devices d WHERE d.vault_id = r.vault_id),
                    COALESCE(v.legacy_default, 0), v.state, v.control_rev,
                    v.authz_state, v.active_epoch, v.head_seq,
                    u.normal_bytes, u.maintenance_bytes,
                    (SELECT MAX(t.expires_at) FROM create_tickets t
                      WHERE t.vault_id = r.vault_id AND t.consumed_at IS NULL AND t.quota > 0),
                    (SELECT COUNT(*) FROM create_tickets t
                      WHERE t.vault_id = r.vault_id AND t.consumed_at IS NULL AND t.quota > 0),
                    (SELECT COUNT(*) FROM create_tickets t WHERE t.vault_id = r.vault_id),
                    (SELECT COUNT(*) FROM create_tickets t
                      WHERE t.vault_id = r.vault_id AND t.consumed_at IS NOT NULL)
               FROM vault_registry r
               LEFT JOIN vaults v ON v.vault_id = r.vault_id
               LEFT JOIN usage u ON u.vault_id = r.vault_id
              WHERE r.vault_id = ?1",
            [vault_id.as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<i64>>(12)?,
                    row.get::<_, Option<i64>>(13)?,
                    row.get::<_, Option<i64>>(14)?,
                    row.get::<_, Option<i64>>(15)?,
                    row.get::<_, Option<i64>>(16)?,
                    row.get::<_, i64>(17)?,
                    row.get::<_, i64>(18)?,
                    row.get::<_, i64>(19)?,
                ))
            },
        )
        .optional()?;
    let Some((
        vault_id_blob,
        state,
        created_at,
        deleted_at,
        delete_job_id,
        delete_progress,
        delete_error,
        devices,
        legacy_default,
        runtime_state,
        control_rev,
        authz_state,
        active_epoch,
        head_seq,
        usage_normal_bytes,
        usage_maintenance_bytes,
        expires_at,
        outstanding,
        tickets_total,
        tickets_consumed,
    )) = row
    else {
        return Ok(None);
    };
    let stored_state =
        LifecycleState::parse(&state).ok_or(Error::Invariant("vault_registry.state"))?;
    let outstanding_tickets =
        u64::try_from(outstanding).map_err(|_| Error::Invariant("vault_registry.outstanding"))?;
    let vault_id = id16_from_blob(vault_id_blob, "vault_registry.vault_id")?;
    let delete_job_id = match delete_job_id {
        None => None,
        Some(bytes) => Some(id16_from_blob(bytes, "vault_registry.delete_job_id")?),
    };
    let active_epoch = match active_epoch {
        None => None,
        Some(value) => {
            Some(u64::try_from(value).map_err(|_| Error::Invariant("vaults.active_epoch"))?)
        }
    };
    let head_seq = match head_seq {
        None => None,
        Some(value) => Some(u64::try_from(value).map_err(|_| Error::Invariant("vaults.head_seq"))?),
    };
    let control_rev = match control_rev {
        None => None,
        Some(value) => {
            Some(u64::try_from(value).map_err(|_| Error::Invariant("vaults.control_rev"))?)
        }
    };
    let usage_normal_bytes = match usage_normal_bytes {
        None => None,
        Some(value) => {
            Some(u64::try_from(value).map_err(|_| Error::Invariant("usage.normal_bytes"))?)
        }
    };
    let usage_maintenance_bytes = match usage_maintenance_bytes {
        None => None,
        Some(value) => {
            Some(u64::try_from(value).map_err(|_| Error::Invariant("usage.maintenance_bytes"))?)
        }
    };
    Ok(Some(VaultDetail {
        entry: VaultListEntry {
            vault_id,
            state: effective_state(stored_state, expires_at, outstanding_tickets, now),
            stored_state,
            created_at,
            expires_at,
            outstanding_tickets,
            vault_exists: authz_state.is_some(),
            device_count: u64::try_from(devices)
                .map_err(|_| Error::Invariant("vault_registry.devices"))?,
            delete_progress: u64::try_from(delete_progress)
                .map_err(|_| Error::Invariant("vault_registry.delete_progress"))?,
        },
        deleted_at,
        delete_job_id,
        delete_progress: u64::try_from(delete_progress)
            .map_err(|_| Error::Invariant("vault_registry.delete_progress"))?,
        delete_error,
        legacy_default: legacy_default != 0,
        authz_state,
        active_epoch,
        head_seq,
        tickets_total: u64::try_from(tickets_total)
            .map_err(|_| Error::Invariant("create_tickets.count"))?,
        tickets_consumed: u64::try_from(tickets_consumed)
            .map_err(|_| Error::Invariant("create_tickets.consumed"))?,
        tickets_outstanding: outstanding_tickets,
        runtime_state,
        control_rev,
        usage_normal_bytes,
        usage_maintenance_bytes,
    }))
}

/// 只读：`vault_id` 是否在 registry（含墓碑）里——ID 禁止复用的判定入口。
pub fn id_taken(conn: &Connection, vault_id: &Id16) -> Result<bool> {
    Ok(load_raw(conn, vault_id)?.is_some())
}
