//! 配额与维护 reservation（ADR 0006 §7 + §13.2）。
//!
//! 两个预算：
//! - 正常预算 `Q = normal_limit`：日常 update/asset/snapshot 密文预算；写满返回 507，**不删旧数据腾空间**；
//! - 维护预算 `R = maintenance_limit - normal_limit` 的余量（`maintenance_limit` 是 `Q+R` 的峰值上限）：
//!   只用于 checkpoint 替换、轮换和宽限保留。
//!
//! 可用维护额度公式（§13.2）：
//! `R_available = R − maintenance_used − Σ(active.reserved_bytes − consumed_bytes)`
//! 其中 `R = maintenance_limit`、`maintenance_used = usage.maintenance_bytes`。
//! 注意该公式下"在 reservation 内写一片"是**不变量**：`maintenance_bytes += len` 与
//! `Σ(reserved−consumed) -= len` 相抵，所以 reservation 本身就是预留的额度。
//!
//! 逐片原子扣减：本模块的 `charge_*` 必须在**写入 object_parts 的同一个事务**里调用
//! （调用方 `objects::put_part` 保证）。

use rusqlite::{Connection, OptionalExtension};

use crate::error::{Error, ErrorCode, Result};
use crate::ids::{random_id16, Id16};
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
pub fn usage(conn: &Connection) -> Result<Usage> {
    let row = conn
        .query_row(
            "SELECT normal_bytes, maintenance_bytes FROM usage WHERE singleton = 1",
            [],
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
pub fn active_reserved_remaining(conn: &Connection, now: i64) -> Result<i64> {
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(reserved_bytes - consumed_bytes), 0)
         FROM maintenance_reservations
         WHERE state = 'active' AND expires_at > ?1",
        [now],
        |row| row.get(0),
    )?;
    Ok(total)
}

/// 正常预算剩余。
pub fn normal_available(conn: &Connection) -> Result<i64> {
    let vault = vault::load(conn)?;
    Ok(vault.normal_limit - usage(conn)?.normal_bytes)
}

/// 维护额度剩余（§13.2 公式）。**过期的 reservation 不再占额度，但已写入的数据仍在**。
///
/// 口径：`vault.maintenance_limit` 是**峰值上限 `Q+R`**（ADR §6），因此维护预算
/// `R = maintenance_limit - normal_limit`；`R_available = R - maintenance_used - Σ(active.reserved-consumed)`。
pub fn maintenance_budget(vault: &vault::Vault) -> i64 {
    (vault.maintenance_limit - vault.normal_limit).max(0)
}

/// 维护额度剩余。
pub fn maintenance_available(conn: &Connection, now: i64) -> Result<i64> {
    let vault = vault::load(conn)?;
    let used = usage(conn)?.maintenance_bytes;
    let reserved = active_reserved_remaining(conn, now)?;
    Ok(maintenance_budget(&vault) - used - reserved)
}

/// 写一片**正常**对象的密文：原子扣预算，超限返回 507。
pub fn charge_normal(conn: &Connection, bytes: i64) -> Result<()> {
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
    conn.execute(
        "UPDATE usage SET normal_bytes = normal_bytes + ?1 WHERE singleton = 1",
        [bytes],
    )?;
    Ok(())
}

/// 写一片**维护**对象的密文：必须落在 active reservation 内，且不超过 `Q+R` 峰值。
/// **逐片原子扣减**（与 part 写入同一事务）。
pub fn charge_maintenance(
    conn: &Connection,
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
    // 峰值预算 Q+R（ADR §7：maintenance_limit 就是峰值上限）。
    if current
        .normal_bytes
        .saturating_add(current.maintenance_bytes)
        .saturating_add(bytes)
        > vault.maintenance_limit
    {
        return Err(Error::api(ErrorCode::QuotaExceeded));
    }
    conn.execute(
        "UPDATE maintenance_reservations SET consumed_bytes = consumed_bytes + ?1
         WHERE reservation_id = ?2",
        (bytes, reservation_id.as_slice()),
    )?;
    conn.execute(
        "UPDATE usage SET maintenance_bytes = maintenance_bytes + ?1 WHERE singleton = 1",
        [bytes],
    )?;
    Ok(())
}

/// 读一条 reservation。
pub fn reservation(conn: &Connection, id: &Id16) -> Result<Option<Reservation>> {
    let mut stmt = conn.prepare(
        "SELECT reservation_id, purpose, resource_id, owner_device, reserved_bytes,
                consumed_bytes, state, expires_at
         FROM maintenance_reservations WHERE reservation_id = ?1",
    )?;
    let row = stmt
        .query_row([id.as_slice()], |row| {
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
    conn: &Connection,
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
        "INSERT INTO maintenance_reservations(reservation_id, purpose, resource_id, owner_device,
                                              reserved_bytes, consumed_bytes, state, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, 'active', ?6)",
        (
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
pub fn active_checkpoint_lease(conn: &Connection, now: i64) -> Result<Option<Id16>> {
    let mut stmt = conn.prepare(
        "SELECT resource_id FROM maintenance_reservations
         WHERE purpose = 'checkpoint' AND state = 'active' AND expires_at > ?1
         LIMIT 1",
    )?;
    let row = stmt
        .query_row([now], |row| row.get::<_, Vec<u8>>(0))
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
pub fn complete_reservation(conn: &Connection, id: &Id16) -> Result<()> {
    conn.execute(
        "UPDATE maintenance_reservations SET state = 'complete'
         WHERE reservation_id = ?1 AND state = 'active'",
        [id.as_slice()],
    )?;
    Ok(())
}

/// 标记过期。
pub fn expire_reservation(conn: &Connection, id: &Id16) -> Result<()> {
    conn.execute(
        "UPDATE maintenance_reservations SET state = 'expired'
         WHERE reservation_id = ?1 AND state = 'active'",
        [id.as_slice()],
    )?;
    Ok(())
}

/// 清扫过期 reservation（只改状态，不删数据；ADR §13.2）。
pub fn expire_stale_reservations(conn: &Connection, now: i64) -> Result<u64> {
    let changed = conn.execute(
        "UPDATE maintenance_reservations SET state = 'expired'
         WHERE state = 'active' AND expires_at <= ?1",
        [now],
    )?;
    Ok(changed as u64)
}

/// 从 `object_parts` 重算账本（GC 删除分片后对账；幂等、自愈）。
pub fn recompute(conn: &Connection) -> Result<Usage> {
    let (normal, maintenance): (i64, i64) = conn.query_row(
        "SELECT
            COALESCE(SUM(CASE WHEN o.budget_class = 'normal' THEN length(p.envelope) ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN o.budget_class = 'maintenance' THEN length(p.envelope) ELSE 0 END), 0)
         FROM object_parts p JOIN objects o ON o.object_id = p.object_id",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    conn.execute(
        "UPDATE usage SET normal_bytes = ?1, maintenance_bytes = ?2 WHERE singleton = 1",
        (normal, maintenance),
    )?;
    Ok(Usage {
        normal_bytes: normal,
        maintenance_bytes: maintenance,
    })
}

/// 便捷：当前时刻（测试里是 FakeClock）。
pub fn now(clock: &dyn Clock) -> i64 {
    clock.now_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
