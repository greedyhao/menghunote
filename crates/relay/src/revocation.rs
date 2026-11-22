//! 设备访问吊销（ADR 0010）：管理端**独立**吊销入口的领域逻辑。
//!
//! 定位：同步是端到端加密，relay 只存密文，因此删设备凭证只能阻断**访问**，不能让已持有
//! 旧 root 的设备失去解密能力（ADR 0010「安全边界」）。本模块只做这件"诚实 relay 上的
//! 设备数据访问撤销"，不冒充签名成员移除/密码学撤销。
//!
//! 三条硬约束（违反其一都会破坏既有客户端）：
//!
//! 1. **不动控制链**：独立吊销是 relay 侧访问状态，不是签名控制事实。客户端要求 `/control`
//!    的签名链尾精确等于 `control_rev`，所以这里只推进 `auth_rev`，`control_rev` 不变，
//!    也不追加占位控制记录；
//! 2. **pending 不能直接置 revoked**：pending 成员是空证书占位；变成 revoked 会让现有
//!    客户端按"非待批准成员证书为空"整条控制链 fail-stop（取消待批准设备另做）；
//! 3. **不能吊销到 0 台 ACTIVE**：数 ACTIVE 与更新必须在同一个 `BEGIN IMMEDIATE` 事务里
//!    （handler 只在 `Db::write` 内调用本模块），否则两台设备并发互删会留下"空 vault"。
//!
//! 幂等：目标已 revoked 时返回 `changed=false` 且**不推进任何版本**——该判断必须早于
//! rotating/initializing 守卫，否则轮换期间的重复请求会变成 409。

use rusqlite::OptionalExtension;

use crate::auth;
use crate::error::{Error, ErrorCode, Result};
use crate::ids::Id16;
use crate::scope::ScopeConn;
use crate::time::Clock;
use crate::vault::{self, VaultState};

/// 吊销结果（wire 响应体由 API 层编码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeOutcome {
    /// 被吊销的目标设备（同时是 canonical body 的字段 0）。
    pub target: Id16,
    /// 本次请求是否真的改变了状态；已 revoked → false（幂等空操作）。
    pub changed: bool,
    /// 吊销后的 `vault.auth_rev`（旧 token 已全部失效）。
    pub auth_rev: u64,
    /// `vault.control_rev`（本端点**不变**）。
    pub control_rev: u64,
}

/// `devices.status` 的三种取值（DDL 的 CHECK 已排除其它值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceStatus {
    Pending,
    Active,
    Revoked,
}

fn device_status(conn: &ScopeConn, device_id: &Id16) -> Result<Option<DeviceStatus>> {
    let status: Option<String> = conn
        .query_row(
            "SELECT status FROM devices WHERE device_id = ?1 AND vault_id = ?",
            (device_id.as_slice(), conn.scope().as_slice()),
            |row| row.get(0),
        )
        .optional()?;
    match status.as_deref() {
        None => Ok(None),
        Some("pending") => Ok(Some(DeviceStatus::Pending)),
        Some("active") => Ok(Some(DeviceStatus::Active)),
        Some("revoked") => Ok(Some(DeviceStatus::Revoked)),
        Some(_) => Err(Error::Invariant("devices.status")),
    }
}

/// 吊销目标设备的访问权（调用方保证在**单个写事务**内）。
///
/// 顺序是语义相关的：存在性（404）→ 已 revoked（幂等 200）→ pending（409）→
/// vault 状态（rotating/initializing）→ ACTIVE 计数守卫 → 状态迁移 + `auth_rev`。
pub fn revoke_access(conn: &ScopeConn, clock: &dyn Clock, target: &Id16) -> Result<RevokeOutcome> {
    let vault = vault::load(conn)?;
    let status = device_status(conn, target)?.ok_or_else(|| Error::api(ErrorCode::NotFound))?;

    // ① 已 revoked：幂等空操作，**不推进任何版本**（ADR 0010）。这一步必须在轮换守卫之前，
    //    否则"响应丢失 + 轮换期间重试"会得到 409 而不是原结果。
    if status == DeviceStatus::Revoked {
        return Ok(RevokeOutcome {
            target: *target,
            changed: false,
            auth_rev: vault.auth_rev,
            control_rev: vault.control_rev,
        });
    }
    // ② pending 是空证书占位：置 revoked 会让既有客户端整条控制链 fail-stop。
    if status == DeviceStatus::Pending {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    // ③ ACTIVE 目标：轮换期间访问状态由轮换流程统一管（避免与 freeze/grant 计划打架）。
    match vault.state {
        VaultState::Rotating => return Err(Error::api(ErrorCode::Rotating)),
        VaultState::Initializing => return Err(Error::api(ErrorCode::InvalidState)),
        VaultState::Active => {}
    }

    // ④ 同一事务内数 ACTIVE：并发互删最多一个成功，且永远留一台 ACTIVE。
    //    （relay 无法识别哪台设备持有管理私钥，所以这里不能做"最后一个管理设备"守卫；
    //    保留一台 ACTIVE 也不证明管理密钥仍被保存——ADR 0010 要求用户保留最新恢复码。）
    let active: i64 = conn.query_row(
        "SELECT COUNT(*) FROM devices WHERE status = 'active' AND vault_id = ?1",
        [conn.scope().as_slice()],
        |row| row.get(0),
    )?;
    if active <= 1 {
        return Err(Error::api(ErrorCode::InvalidState));
    }

    // ⑤ 真变化：只改 status/revoked_at，并由 `auth::revoke_device` 推进 `auth_rev`
    //    （证书、grant、控制链历史全部保留；不换 root/epoch、不冻结）。
    let changed = auth::revoke_device(conn, clock.now_secs(), target)?;
    let vault = vault::load(conn)?;
    Ok(RevokeOutcome {
        target: *target,
        changed,
        auth_rev: vault.auth_rev,
        control_rev: vault.control_rev,
    })
}
