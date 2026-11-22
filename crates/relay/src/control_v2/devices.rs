//! ADR 0015 **S4b2**：`DeviceApprove`（kind 4）/`DeviceRevoke`（kind 5）/
//! `DeviceRightsChange`（kind 16）。
//!
//! 三个 kind 都走 [`super::submit`] 的同一套链/CAS/时间窗/幂等/rev 绑定，本模块只负责
//! **kind 专属校验与投影**（全部在 handler 的 `BEGIN IMMEDIATE` 事务内）：
//!
//! | kind | payload | 门限 | 效果 |
//! | --- | --- | --- | --- |
//! | 4 DeviceApprove | `{0: cert, 1: grants[]}` | 2× APPROVE +（仅首次准入）目标 PoP | 凭证历史 + `devices` ACTIVE + grant；authz/auth +1 |
//! | 5 DeviceRevoke | `{0: sorted_device_ids[]}` | 2× REVOKE | `devices` REVOKED + 目标全部 cap 失效；authz/auth +1 |
//! | 16 DeviceRightsChange | `{0: cert, 1: grants[]}` | 2× APPROVE | 新 serial 凭证 + grant；authz/auth +1 |
//!
//! # 设备凭证（`device_credentials`）
//!
//! 每次批准/改权写一行 `(device_id, serial)`，**只增不改**（ADR：「旧 cert 历史保留」）：
//! 首份凭证 serial=1（与 S4b1 的 Upgrade manifest 同口径），改权必须 `serial = max+1`。
//! `approval_control_rev` 指向**本次**控制提交（复合 FK 保证同 vault、同事务、真实存在），
//! `issued_authz_rev` 是提交后的 authz rev（历史证据，不要求等于当前值）。
//! V2 证书的授权事实就是这条两签控制记录，因此没有 v1 那种整份证书的额外签名
//! （`certificate_signature = NULL`）。
//!
//! # grant 投影
//!
//! grant 包 `{aad, enc, ciphertext}` 被两签 `OperationV2` 摘要承诺（换包必然验签失败），
//! 这里再按 DB 事实校验：`recipient` = 目标、`recipient_hpke_pub` = 证书 hpke 公钥、
//! `epoch` = 当前活动 epoch、`authority_rev` = 当前 authority 代、
//! `issued_authz_rev` = 提交后的 authz rev、`grant_id` 在全表内不复用。
//! 落库复用 `device_grants`（`signed_header` = grant AAD 字节、`hpke_enc`/`hpke_ciphertext`
//! = 包内字段、`signature` = NULL）：V2 的授权不是管理钥的独立签名，而是控制记录本身。
//!
//! S4b2 只接受**当前 epoch** 的一个 grant 包（多 epoch 属于轮换/激活，S4c）。
//!
//! # 撤销级联
//!
//! `DeviceRevoke` 不是 v1 那种"只改 access 状态"的旁路（ADR 0015 前提纠正：v1 独立 revoke
//! 不写控制链）：它是**签名撤销事实**——`devices.status = revoked`、目标名下全部
//! `management_caps.revoked_control_rev` 落点、`authz_rev/auth_rev` 同事务 +1。
//! 后者让所有旧 bearer（含 realm 3 管理 token）立即失效（`auth::authenticate` 逐请求
//! 重验），WS 在下一帧重验时 close 1008；提交成功后 handler 的 `state.notify()` 兜底
//! 立即触发一次重验。**已经发出去的字节无法收回**（ADR 明示）。
//!
//! 绑定撤销 cap 的**未兑换邀请**不需要改行：`pairing::redeem_invite` 在兑换时重验
//! `issuer_cap_id` 对应 cap（撤销/到期/authority 变更 → 403），因此"cap 失效则未兑换邀请
//! 失效"是**派生**事实，不额外改写邀请历史。
//!
//! # quorum 保护
//!
//! - 撤销后剩余 ACTIVE 设备不得少于 `PolicyV2.min_quorum`（=2）；
//! - 任何高危 scope（APPROVE/REVOKE/CAP_ADMIN/ROTATE/POLICY）**本来可用两设备**时，
//!   撤销不得把它降到两台以下（ADR：「普通变更不得让任一高危 scope 的可用两设备 quorum
//!   低于 2」）。已经不足两台的情况不因本次撤销变得更差，不额外拦截；
//! - 改权不得把**可写**设备降到 `min_quorum` 以下（数据权限 quorum）。
//!
//! # 幂等与不可复活
//!
//! - 已 ACTIVE 设备收到**逐字节相同**的证书 + grant 包：视为同一次批准的重放，只追加
//!   控制记录（链要求），不再推进 authz/auth；证书不同 → 409 `INVALID_STATE`
//!   （改权必须走 kind 16 并递增 serial）；
//! - 已 revoked 设备收到批准/改权 → 403，**永不复活**；
//! - 目标全部已 revoked 的重复撤销 → 成功但不推进 authz/auth（changed = false）。
//!
//! 失败语义：[`super`] 的约定（400/403/404/409）；本模块所有门限/状态/PoP/证书-grant
//! 不符都返回 403，不给"哪一项不符"的细分 oracle；只有目标设备不存在是 404。

use rusqlite::OptionalExtension;

use super::{
    bump_authz_and_auth, forbidden, i64_of, invalid, load_policy, proto_sha256, signature_invalid,
    two_scope_signers, SigPolicy,
};
use crate::error::{Error, ErrorCode, Result};
use crate::ids::Id16;
use crate::scope::ScopeConn;
use crate::vault::Vault;
use menghunote_sync_proto as proto;

/// 高危 scope：撤销设备时这些 scope 的两设备 quorum 不得被降破（ADR 0015）。
const HIGH_RISK_SCOPES: [u32; 5] = [
    proto::CapScope::APPROVE,
    proto::CapScope::REVOKE,
    proto::CapScope::CAP_ADMIN,
    proto::CapScope::ROTATE,
    proto::CapScope::POLICY,
];

/// 已校验、待执行的一个设备管理记录。
pub(super) enum DevicePlan {
    Approve(ApprovePlan),
    Revoke(RevokePlan),
    Rights(RightsPlan),
}

/// 已校验的批准/改权（两者共享 wire 形状，效果不同）。
pub(super) struct ApprovePlan {
    cert: proto::DeviceCertV2,
    /// 证书的 canonical CDE（逐字节等于 payload 里的 key 0）。
    cert_bytes: Vec<u8>,
    grants: Vec<proto::GrantPackageV2>,
    /// 目标已 ACTIVE 且证书/grant 与库内逐字节一致：只追加控制记录，不再改投影/rev。
    idempotent: bool,
}

pub(super) struct RightsPlan {
    cert: proto::DeviceCertV2,
    cert_bytes: Vec<u8>,
    grants: Vec<proto::GrantPackageV2>,
}

pub(super) struct RevokePlan {
    targets: Vec<Id16>,
    /// 本次是否真的有设备从 ACTIVE 变成 REVOKED；全部已 revoked → false（幂等空操作）。
    changed: bool,
}

// ---------------------------------------------------------------- 设备行

/// `devices.status`（DDL 的 CHECK 已排除其它取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceStatus {
    Pending,
    Active,
    Revoked,
}

struct DeviceRow {
    status: DeviceStatus,
    sign_pk: [u8; 32],
    hpke_pk: [u8; 32],
    certificate: Vec<u8>,
}

fn device_row(conn: &ScopeConn, device_id: &Id16) -> Result<Option<DeviceRow>> {
    let mut stmt = conn.prepare(
        "SELECT status, sign_pk, hpke_pk, certificate FROM devices
         WHERE vault_id = ?1 AND device_id = ?2",
    )?;
    let row = stmt
        .query_row((conn.scope().as_slice(), device_id.as_slice()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .optional()?;
    let Some((status, sign_pk, hpke_pk, certificate)) = row else {
        return Ok(None);
    };
    let status = match status.as_str() {
        "pending" => DeviceStatus::Pending,
        "active" => DeviceStatus::Active,
        "revoked" => DeviceStatus::Revoked,
        _ => return Err(Error::Invariant("devices.status")),
    };
    Ok(Some(DeviceRow {
        status,
        sign_pk: sign_pk
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("devices.sign_pk"))?,
        hpke_pk: hpke_pk
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("devices.hpke_pk"))?,
        certificate,
    }))
}

// ---------------------------------------------------------------- 门限签名者

/// 目标设备 PoP：role3 签名者必须是目标设备、不带 cap、pk = 目标已登记公钥，
/// 并对 `MN/v2/pop\0 || h || pk32` 验签（ADR 0015 S4b 冻结 §5）。
fn verify_target_pop(
    signed: &proto::SignedControlV2,
    target: &Id16,
    expected_pk: &[u8; 32],
    policy: SigPolicy<'_>,
) -> Result<()> {
    let mut seen = false;
    for (index, signer) in signed.operation.signers.iter().enumerate() {
        if signer.role != proto::SignerRole::TargetPop {
            continue;
        }
        if seen {
            return Err(forbidden());
        }
        seen = true;
        if signer.device_id != Some(*target) || signer.cap_id.is_some() || signer.pk != *expected_pk
        {
            return Err(forbidden());
        }
        let input = proto::pop_signing_input(&signed.operation, &signer.pk, proto_sha256)
            .map_err(|_| invalid())?;
        if policy
            .verify(index, &input, &signer.pk, &signed.signatures[index])
            .is_err()
        {
            return Err(signature_invalid());
        }
    }
    if !seen {
        return Err(forbidden());
    }
    Ok(())
}

// ---------------------------------------------------------------- grant / 凭证校验

/// 校验 kind 4/16 的 grant 包，返回唯一的包（S4b2 只接受当前 epoch 的一份）。
fn validate_grants(
    conn: &ScopeConn,
    vault: &Vault,
    target: &Id16,
    recipient_hpke: &[u8; 32],
    issued_authz_rev: u64,
    grants: &[proto::GrantPackageV2],
) -> Result<()> {
    // 一个 epoch 一台设备只需要一份 grant（PK `(vault, epoch, device)`）；多 epoch
    // 属于轮换/激活流程（S4c），S4b2 不接受。
    if grants.len() != 1 {
        return Err(forbidden());
    }
    let package = &grants[0];
    let aad = &package.aad;
    if aad.instance_id != vault.instance_id
        || aad.vault_id != vault.vault_id
        || aad.epoch != vault.active_epoch
        || aad.recipient_device_id != *target
        || aad.recipient_hpke_pub != *recipient_hpke
        || aad.authority_rev != vault.authority_rev
        || aad.issued_authz_rev != issued_authz_rev
    {
        return Err(forbidden());
    }
    // 授权事实的 ID 不复用：同 grant_id 出现在别的包/设备上说明包被搬移。
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM device_grants WHERE vault_id = ?1 AND grant_id = ?2",
        (conn.scope().as_slice(), aad.grant_id.as_slice()),
        |row| row.get(0),
    )?;
    if count != 0 {
        return Err(Error::api(ErrorCode::IdConflict));
    }
    Ok(())
}

/// 已 ACTIVE 的幂等重放：提交的 grant 包必须与库内那一行逐字节一致。
fn require_identical_grant(
    conn: &ScopeConn,
    vault: &Vault,
    target: &Id16,
    grants: &[proto::GrantPackageV2],
) -> Result<()> {
    if grants.len() != 1 {
        return Err(forbidden());
    }
    let package = &grants[0];
    let aad = &package.aad;
    let mut stmt = conn.prepare(
        "SELECT hpke_enc, hpke_ciphertext, signed_header, grant_id FROM device_grants
         WHERE vault_id = ?1 AND epoch = ?2 AND device_id = ?3",
    )?;
    let row = stmt
        .query_row(
            (
                conn.scope().as_slice(),
                i64_of(vault.active_epoch, "epoch")?,
                target.as_slice(),
            ),
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((enc, ciphertext, signed_header, grant_id)) = row else {
        return Err(Error::api(ErrorCode::InvalidState));
    };
    let aad_bytes = aad.aad_bytes().map_err(|_| invalid())?;
    let identical = aad.epoch == vault.active_epoch
        && enc.as_slice() == package.enc
        && ciphertext == package.ciphertext
        && signed_header == aad_bytes
        && grant_id.as_deref() == Some(aad.grant_id.as_slice());
    if !identical {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    Ok(())
}

/// 设备当前凭证 serial（无凭证历史时按 Upgrade manifest 的 serial=1 起算）。
fn current_serial(conn: &ScopeConn, device_id: &Id16) -> Result<u64> {
    let max: Option<i64> = conn.query_row(
        "SELECT MAX(serial) FROM device_credentials WHERE vault_id = ?1 AND device_id = ?2",
        (conn.scope().as_slice(), device_id.as_slice()),
        |row| row.get(0),
    )?;
    match max {
        None => Ok(1),
        Some(value) => {
            u64::try_from(value).map_err(|_| Error::Invariant("device_credentials.serial"))
        }
    }
}

fn credential_count(conn: &ScopeConn, device_id: &Id16) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM device_credentials WHERE vault_id = ?1 AND device_id = ?2",
        (conn.scope().as_slice(), device_id.as_slice()),
        |row| row.get(0),
    )?)
}

/// 设备当前的数据权限：优先最新 `device_credentials` 行，其次 `devices.certificate`
/// （V2 证书或升级前的 `DeviceCertV1`）。
fn rights_from_certificate(bytes: &[u8], field: &'static str) -> Result<proto::Rights> {
    if let Ok(cert) = proto::DeviceCertV2::decode(bytes) {
        return Ok(cert.data_rights);
    }
    let cert = proto::DeviceCertV1::decode(bytes).map_err(|_| Error::Invariant(field))?;
    Ok(cert.rights)
}

fn current_data_rights(conn: &ScopeConn, device_id: &Id16) -> Result<proto::Rights> {
    let latest: Option<Vec<u8>> = conn
        .query_row(
            "SELECT certificate FROM device_credentials
             WHERE vault_id = ?1 AND device_id = ?2 ORDER BY serial DESC LIMIT 1",
            (conn.scope().as_slice(), device_id.as_slice()),
            |row| row.get(0),
        )
        .optional()?;
    if let Some(bytes) = latest {
        return rights_from_certificate(&bytes, "device_credentials.certificate");
    }
    let certificate: Vec<u8> = conn.query_row(
        "SELECT certificate FROM devices WHERE vault_id = ?1 AND device_id = ?2",
        (conn.scope().as_slice(), device_id.as_slice()),
        |row| row.get(0),
    )?;
    rights_from_certificate(&certificate, "devices.certificate")
}

/// 全部 ACTIVE 设备的当前数据权限（改权的可写 quorum 守卫用）。
fn active_device_rights(conn: &ScopeConn) -> Result<Vec<(Id16, proto::Rights)>> {
    let mut stmt = conn.prepare(
        "SELECT d.device_id, d.certificate,
                (SELECT c.certificate FROM device_credentials c
                  WHERE c.vault_id = d.vault_id AND c.device_id = d.device_id
                  ORDER BY c.serial DESC LIMIT 1)
         FROM devices d
         WHERE d.vault_id = ?1 AND d.status = 'active'
         ORDER BY d.device_id ASC",
    )?;
    let rows = stmt.query_map([conn.scope().as_slice()], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Option<Vec<u8>>>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (device_id, certificate, latest) = row?;
        let bytes = latest.as_deref().unwrap_or(certificate.as_slice());
        out.push((
            device_id
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("devices.device_id"))?,
            rights_from_certificate(bytes, "devices.certificate")?,
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------- quorum 守卫

/// 某 scope 当前可用的 ACTIVE 设备数（cap 未撤销/authority 当前/时间窗内/公钥匹配）。
fn scope_quorum(
    conn: &ScopeConn,
    vault: &Vault,
    now: u64,
    scope: u32,
    excluded: &[Id16],
) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT c.subject_device, c.scopes
         FROM management_caps c
         JOIN devices d ON d.vault_id = c.vault_id AND d.device_id = c.subject_device
         WHERE c.vault_id = ?1
           AND c.revoked_control_rev IS NULL
           AND c.issuer_authority_rev = ?2
           AND c.not_before <= ?3
           AND c.expires_at > ?3
           AND d.status = 'active'
           AND d.sign_pk = c.subject_pk",
    )?;
    let rows = stmt.query_map(
        (
            conn.scope().as_slice(),
            i64_of(vault.authority_rev, "authority_rev")?,
            i64_of(now, "now")?,
        ),
        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let mut devices: Vec<Id16> = Vec::new();
    for row in rows {
        let (device_id, scopes) = row?;
        let scopes =
            u32::try_from(scopes).map_err(|_| Error::Invariant("management_caps.scopes"))?;
        if scopes & scope == 0 {
            continue;
        }
        let device_id: Id16 = device_id
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("management_caps.subject_device"))?;
        if excluded.contains(&device_id) || devices.contains(&device_id) {
            continue;
        }
        devices.push(device_id);
    }
    Ok(devices.len())
}

/// 当前 ACTIVE 设备 ID（撤销 quorum 守卫用）。
fn active_device_ids(conn: &ScopeConn) -> Result<Vec<Id16>> {
    let mut stmt = conn.prepare(
        "SELECT device_id FROM devices WHERE vault_id = ?1 AND status = 'active'
         ORDER BY device_id ASC",
    )?;
    let rows = stmt.query_map([conn.scope().as_slice()], |row| row.get::<_, Vec<u8>>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(
            row?.as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("devices.device_id"))?,
        );
    }
    Ok(out)
}

/// 撤销不得把任一高危 scope 的两设备 quorum 降到 2 以下，也不得让 ACTIVE 设备少于
/// `PolicyV2.min_quorum`（ADR 0015「能力、门限与恢复」）。
pub(super) fn ensure_revoke_quorum(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    targets: &[Id16],
) -> Result<()> {
    let policy = load_policy(vault)?;
    let remaining = active_device_ids(conn)?
        .into_iter()
        .filter(|device_id| !targets.contains(device_id))
        .count();
    if remaining < policy.min_quorum as usize {
        return Err(forbidden());
    }
    for scope in HIGH_RISK_SCOPES {
        let before = scope_quorum(conn, vault, now, scope, &[])?;
        if before < proto::PolicyV2::MIN_QUORUM as usize {
            continue;
        }
        let after = scope_quorum(conn, vault, now, scope, targets)?;
        if after < proto::PolicyV2::MIN_QUORUM as usize {
            return Err(forbidden());
        }
    }
    Ok(())
}

/// 改权不得把可写设备降到 `min_quorum` 以下（数据权限 quorum）。
fn ensure_writable_quorum(
    conn: &ScopeConn,
    vault: &Vault,
    target: &Id16,
    new_rights: proto::Rights,
) -> Result<()> {
    let policy = load_policy(vault)?;
    let mut before = 0usize;
    let mut after = 0usize;
    for (device_id, rights) in active_device_rights(conn)? {
        if rights.can_write() {
            before += 1;
        }
        let effective = if device_id == *target {
            new_rights
        } else {
            rights
        };
        if effective.can_write() {
            after += 1;
        }
    }
    if before >= proto::PolicyV2::MIN_QUORUM as usize && after < policy.min_quorum as usize {
        return Err(forbidden());
    }
    Ok(())
}

// ---------------------------------------------------------------- kind 4：DeviceApprove

/// 校验 `DeviceApprove`（kind 4）。
///
/// - PENDING 目标：两签 APPROVE + 目标 PoP；证书必须绑定兑换时提交的 `sign_pk`/`hpke_pk`，
///   `serial=1`（首份 V2 凭证，不复用），`issued_authz_rev` = 提交后的 authz rev；
///   grant 包只指向当前 epoch、本人 hpke 公钥与提交后 rev；
/// - ACTIVE 目标：证书与 grant 必须与库内逐字节相同（同一次批准的重放），只追加控制记录；
/// - REVOKED 目标：403，永不复活。
pub(super) fn validate_approve(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
    policy: SigPolicy<'_>,
) -> Result<DevicePlan> {
    let operation = &signed.operation;
    let payload =
        proto::DeviceCredentialPayloadV2::decode(&operation.payload).map_err(|_| invalid())?;
    let cert = payload.cert;
    let cert_bytes = cert.encode().map_err(|_| invalid())?;
    if cert.instance_id != vault.instance_id || cert.vault_id != vault.vault_id {
        return Err(forbidden());
    }
    let row = device_row(conn, &cert.device_id)?.ok_or_else(|| Error::api(ErrorCode::NotFound))?;
    let digest_input =
        proto::control_signing_input(operation, proto_sha256).map_err(|_| invalid())?;
    let issued_authz = vault.authz_rev.checked_add(1).ok_or_else(invalid)?;
    match row.status {
        DeviceStatus::Revoked => Err(forbidden()),
        DeviceStatus::Pending => {
            two_scope_signers(
                conn,
                now,
                signed,
                &digest_input,
                &[proto::CapScope::APPROVE],
                &[proto::SignerRole::TargetPop],
                policy,
            )?;
            // 绑定 PENDING 已提交的两把公钥（防 approve 换钥匙）。
            if row.sign_pk != cert.sign_ed25519_pub || row.hpke_pk != cert.hpke_x25519_pub {
                return Err(forbidden());
            }
            verify_target_pop(signed, &cert.device_id, &row.sign_pk, policy)?;
            // 首份 V2 凭证：serial=1 且该设备没有任何历史凭证（serial 不复用）。
            if cert.cert_serial != 1 || credential_count(conn, &cert.device_id)? != 0 {
                return Err(forbidden());
            }
            if !cert.data_rights.can_read() {
                return Err(forbidden());
            }
            if cert.issued_authz_rev != issued_authz {
                return Err(forbidden());
            }
            validate_grants(
                conn,
                vault,
                &cert.device_id,
                &cert.hpke_x25519_pub,
                issued_authz,
                &payload.grants,
            )?;
            Ok(DevicePlan::Approve(ApprovePlan {
                cert,
                cert_bytes,
                grants: payload.grants,
                idempotent: false,
            }))
        }
        DeviceStatus::Active => {
            // 幂等：同一次批准的重放（证书与 grant 包都逐字节相同）只追加控制记录。
            two_scope_signers(
                conn,
                now,
                signed,
                &digest_input,
                &[proto::CapScope::APPROVE],
                &[],
                policy,
            )?;
            if row.certificate != cert_bytes {
                return Err(Error::api(ErrorCode::InvalidState));
            }
            require_identical_grant(conn, vault, &cert.device_id, &payload.grants)?;
            Ok(DevicePlan::Approve(ApprovePlan {
                cert,
                cert_bytes,
                grants: payload.grants,
                idempotent: true,
            }))
        }
    }
}

// ---------------------------------------------------------------- kind 5：DeviceRevoke

/// 校验 `DeviceRevoke`（kind 5）：两签 REVOKE；目标必须存在且全部 ACTIVE（或已 REVOKED
/// 的幂等重复）；PENDING 目标拒绝（v1 语义：pending 是空证书占位，不能置 revoked）。
pub(super) fn validate_revoke(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
    policy: SigPolicy<'_>,
) -> Result<DevicePlan> {
    let operation = &signed.operation;
    let payload =
        proto::DeviceRevokePayloadV2::decode(&operation.payload).map_err(|_| invalid())?;
    let digest_input =
        proto::control_signing_input(operation, proto_sha256).map_err(|_| invalid())?;
    two_scope_signers(
        conn,
        now,
        signed,
        &digest_input,
        &[proto::CapScope::REVOKE],
        &[],
        policy,
    )?;
    let mut targets = Vec::with_capacity(payload.device_ids.len());
    let mut changed = false;
    for device_id in &payload.device_ids {
        let row = device_row(conn, device_id)?.ok_or_else(|| Error::api(ErrorCode::NotFound))?;
        match row.status {
            DeviceStatus::Pending => return Err(forbidden()),
            DeviceStatus::Active => changed = true,
            DeviceStatus::Revoked => {}
        }
        targets.push(*device_id);
    }
    if changed {
        ensure_revoke_quorum(conn, now, vault, &targets)?;
    }
    Ok(DevicePlan::Revoke(RevokePlan { targets, changed }))
}

// ---------------------------------------------------------------- kind 16：DeviceRightsChange

/// 校验 `DeviceRightsChange`（kind 16）。
///
/// 目标必须 ACTIVE；两签 APPROVE；**不改钥匙**（换钥属轮换，S4c）；`serial` 必须是
/// `max(serial)+1`；数据权限必须真的变化（READ 必须在，任何设备都不能变成"只写"）；
/// grant 包与批准同口径；可写 quorum 不得被降到 2 台以下。
pub(super) fn validate_rights_change(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
    policy: SigPolicy<'_>,
) -> Result<DevicePlan> {
    let operation = &signed.operation;
    let payload =
        proto::DeviceCredentialPayloadV2::decode(&operation.payload).map_err(|_| invalid())?;
    let cert = payload.cert;
    let cert_bytes = cert.encode().map_err(|_| invalid())?;
    if cert.instance_id != vault.instance_id || cert.vault_id != vault.vault_id {
        return Err(forbidden());
    }
    let row = device_row(conn, &cert.device_id)?.ok_or_else(|| Error::api(ErrorCode::NotFound))?;
    if row.status != DeviceStatus::Active {
        // REVOKED 永不复活；PENDING 只能走 kind 4。
        return Err(forbidden());
    }
    let digest_input =
        proto::control_signing_input(operation, proto_sha256).map_err(|_| invalid())?;
    two_scope_signers(
        conn,
        now,
        signed,
        &digest_input,
        &[proto::CapScope::APPROVE],
        &[],
        policy,
    )?;
    // 改权不改钥：证书里的两把公钥必须仍是登记值（换钥是轮换/激活，S4c）。
    if row.sign_pk != cert.sign_ed25519_pub || row.hpke_pk != cert.hpke_x25519_pub {
        return Err(forbidden());
    }
    // serial 严格递增：旧凭证（含证书）永久保留，新事实必须换 serial。
    let serial = current_serial(conn, &cert.device_id)?
        .checked_add(1)
        .ok_or_else(invalid)?;
    if cert.cert_serial != serial {
        return Err(forbidden());
    }
    // 权限必须真的变化，且不能丢掉 READ（只写设备无法同步/无法读回自己的数据）。
    if !cert.data_rights.can_read() {
        return Err(forbidden());
    }
    if cert.data_rights == current_data_rights(conn, &cert.device_id)? {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    let issued_authz = vault.authz_rev.checked_add(1).ok_or_else(invalid)?;
    if cert.issued_authz_rev != issued_authz {
        return Err(forbidden());
    }
    ensure_writable_quorum(conn, vault, &cert.device_id, cert.data_rights)?;
    validate_grants(
        conn,
        vault,
        &cert.device_id,
        &cert.hpke_x25519_pub,
        issued_authz,
        &payload.grants,
    )?;
    Ok(DevicePlan::Rights(RightsPlan {
        cert,
        cert_bytes,
        grants: payload.grants,
    }))
}

// ---------------------------------------------------------------- 效果

/// 写投影与 rev，返回提交后的 `(authz_rev, authority_rev)`。
pub(super) fn apply(
    conn: &ScopeConn,
    now: i64,
    vault: &Vault,
    rev: u64,
    plan: DevicePlan,
) -> Result<(u64, u64)> {
    match plan {
        DevicePlan::Approve(plan) => {
            if plan.idempotent {
                // 同一次批准的重放：链上仍追加记录（`super::submit` 已做），授权状态不变。
                return Ok((vault.authz_rev, vault.authority_rev));
            }
            let issued_authz = vault.authz_rev.checked_add(1).ok_or_else(invalid)?;
            insert_credential(conn, now, rev, &plan.cert, &plan.cert_bytes)?;
            let changed = conn.execute(
                "UPDATE devices SET status = 'active',
                                    certificate = ?1,
                                    certificate_signature = NULL,
                                    approval_control_rev = ?2,
                                    issued_authz_rev = ?3
                 WHERE vault_id = ?4 AND device_id = ?5 AND status = 'pending'",
                (
                    plan.cert_bytes.as_slice(),
                    i64_of(rev, "devices.approval_control_rev")?,
                    i64_of(issued_authz, "devices.issued_authz_rev")?,
                    conn.scope().as_slice(),
                    plan.cert.device_id.as_slice(),
                ),
            )?;
            if changed != 1 {
                return Err(Error::Invariant("device approve projection"));
            }
            let package = plan.grants.first().ok_or_else(invalid)?;
            upsert_grant(conn, rev, vault, &plan.cert, package, issued_authz)?;
            bump_authz_and_auth(conn)?;
            Ok((issued_authz, vault.authority_rev))
        }
        DevicePlan::Revoke(plan) => {
            if !plan.changed {
                // 全部目标已 revoked：签名事实仍上链，但授权/鉴权状态不变。
                return Ok((vault.authz_rev, vault.authority_rev));
            }
            for target in &plan.targets {
                conn.execute(
                    "UPDATE devices SET status = 'revoked', revoked_at = ?1
                     WHERE vault_id = ?2 AND device_id = ?3 AND status = 'active'",
                    (now, conn.scope().as_slice(), target.as_slice()),
                )?;
                // 目标名下**全部** cap 失效（含它作为 subject 的每一条），
                // 未兑换邀请在兑换时按 `issuer_cap_id` 重验，因此一并失效。
                conn.execute(
                    "UPDATE management_caps SET revoked_control_rev = ?1
                     WHERE vault_id = ?2 AND subject_device = ?3
                       AND revoked_control_rev IS NULL",
                    (
                        i64_of(rev, "management_caps.revoked_control_rev")?,
                        conn.scope().as_slice(),
                        target.as_slice(),
                    ),
                )?;
            }
            let issued_authz = vault.authz_rev.checked_add(1).ok_or_else(invalid)?;
            bump_authz_and_auth(conn)?;
            Ok((issued_authz, vault.authority_rev))
        }
        DevicePlan::Rights(plan) => {
            let issued_authz = vault.authz_rev.checked_add(1).ok_or_else(invalid)?;
            // 新 serial 追加一行，旧证书行不覆盖（历史可验）。
            insert_credential(conn, now, rev, &plan.cert, &plan.cert_bytes)?;
            let changed = conn.execute(
                "UPDATE devices SET certificate = ?1,
                                    certificate_signature = NULL,
                                    approval_control_rev = ?2,
                                    issued_authz_rev = ?3
                 WHERE vault_id = ?4 AND device_id = ?5 AND status = 'active'",
                (
                    plan.cert_bytes.as_slice(),
                    i64_of(rev, "devices.approval_control_rev")?,
                    i64_of(issued_authz, "devices.issued_authz_rev")?,
                    conn.scope().as_slice(),
                    plan.cert.device_id.as_slice(),
                ),
            )?;
            if changed != 1 {
                return Err(Error::Invariant("device rights projection"));
            }
            let package = plan.grants.first().ok_or_else(invalid)?;
            upsert_grant(conn, rev, vault, &plan.cert, package, issued_authz)?;
            bump_authz_and_auth(conn)?;
            Ok((issued_authz, vault.authority_rev))
        }
    }
}

/// 追加一份 V2 设备凭证历史（`approval_control_rev` 指向本次提交）。
pub(super) fn insert_credential(
    conn: &ScopeConn,
    now: i64,
    rev: u64,
    cert: &proto::DeviceCertV2,
    cert_bytes: &[u8],
) -> Result<()> {
    conn.execute(
        "INSERT INTO device_credentials(vault_id, device_id, serial, authz_version, sign_pk,
                                        hpke_pk, certificate, certificate_signature,
                                        issued_authz_rev, approval_control_rev, created_at)
         VALUES (?1, ?2, ?3, 2, ?4, ?5, ?6, NULL, ?7, ?8, ?9)",
        (
            conn.scope().as_slice(),
            cert.device_id.as_slice(),
            i64_of(cert.cert_serial, "device_credentials.serial")?,
            cert.sign_ed25519_pub.as_slice(),
            cert.hpke_x25519_pub.as_slice(),
            cert_bytes,
            i64_of(cert.issued_authz_rev, "device_credentials.issued_authz_rev")?,
            i64_of(rev, "device_credentials.approval_control_rev")?,
            now,
        ),
    )?;
    Ok(())
}

/// 写 grant 投影：`signed_header` = grant AAD 字节，`signature` = NULL（V2 由控制记录授权）。
///
/// `manager_rev` 是 v1 遗留列（NOT NULL、≥1）：V2 里没有管理钥参与，保留当前
/// `vaults.manager_rev` 作为历史取值，仅用于投影，不参与任何 V2 校验。
pub(super) fn upsert_grant(
    conn: &ScopeConn,
    rev: u64,
    vault: &Vault,
    cert: &proto::DeviceCertV2,
    package: &proto::GrantPackageV2,
    issued_authz_rev: u64,
) -> Result<()> {
    let aad_bytes = package.aad.aad_bytes().map_err(|_| invalid())?;
    conn.execute(
        "INSERT INTO device_grants(vault_id, epoch, device_id, manager_rev, hpke_enc,
                                   hpke_ciphertext, signed_header, signature, grant_id,
                                   activation_ref, approval_control_rev, issued_authz_rev)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, NULL, ?9, ?10)
         ON CONFLICT(vault_id, epoch, device_id) DO UPDATE SET
           manager_rev = excluded.manager_rev,
           hpke_enc = excluded.hpke_enc,
           hpke_ciphertext = excluded.hpke_ciphertext,
           signed_header = excluded.signed_header,
           signature = NULL,
           grant_id = excluded.grant_id,
           approval_control_rev = excluded.approval_control_rev,
           issued_authz_rev = excluded.issued_authz_rev",
        (
            conn.scope().as_slice(),
            i64_of(package.aad.epoch, "device_grants.epoch")?,
            cert.device_id.as_slice(),
            i64_of(vault.manager_rev, "device_grants.manager_rev")?,
            package.enc.as_slice(),
            package.ciphertext.as_slice(),
            aad_bytes.as_slice(),
            package.aad.grant_id.as_slice(),
            i64_of(rev, "device_grants.approval_control_rev")?,
            i64_of(issued_authz_rev, "device_grants.issued_authz_rev")?,
        ),
    )?;
    Ok(())
}
