//! ADR 0015 **S4c**：`RecoveryReset`（13）/`RecoveryKeyChange`（14）。
//!
//! 两个 kind 都走 [`super::submit`] 的同一套链/CAS/时间窗/幂等/rev 绑定，本模块只负责
//! **kind 专属校验与投影**（全部在 handler 的 `BEGIN IMMEDIATE` 事务内）。
//!
//! | kind | payload | 门限 | 效果 |
//! | --- | --- | --- | --- |
//! | 13 RecoveryReset | `{0:policy,1:fresh_recovery_pk,2:caps[],3:adds[],4:revokes[]}` | 旧恢复根 ×1（`recover`）+ 新钥 PoP ×1（`recover`）+ 每个 add 的设备 PoP（`pop`） | 换 authority 代、原子装 policy/caps/设备增删；不足两台 → `recovery_only` |
//! | 14 RecoveryKeyChange | `{0:fresh_recovery_pk,1:caps[]}` | 2× POLICY + 新钥 PoP ×1（`recover`） | 换 authority 代与恢复钥、原子装新代 caps；不改 policy/成员 |
//!
//! # 为什么两个 kind 都带 bootstrap caps
//!
//! ADR：「换恢复钥/Reset 时 `authority_rev+1`，旧代全部 cap 失效并**原子配置新 cap**」。
//! `effective_cap` 逐请求要求 `cap.issuer_authority_rev == vault.authority_rev`，所以
//! 代一换，旧 cap 自动失效；如果同一提交里不装新 cap，管理面会死锁（没有有效 cap 就
//! 无法再签任何管理记录）。payload 因此都带 `bootstrap_caps`。
//!
//! # 恢复根与 PoP
//!
//! - **旧根签**：`role2`（`OfflineRecovery`）+ `pk = vault.recovery_pub`，签 `recover` 域；
//! - **新钥 PoP**：`role2` + `pk = payload.fresh_recovery_pk`，同样签 `recover` 域
//!   （只有持有新私钥的一方才能给出这个签名；与 S4b1 UpgradePolicy 的 role2 同口径）。
//!   两颗钥的 pk 必须不同（signer 严格升序结构上就拒绝重复 pk），因此"新钥 = 旧钥"
//!   既不能复用也不能假装换钥；
//! - **设备 add PoP**：`role3`（`TargetPop`）+ `device_id` = 目标设备 +
//!   `pk = cert.sign_ed25519_pub`，签 `MN/v2/pop\0 || h || pk32`。
//!
//! # 不能复活 / 不能降门限
//!
//! - 目标设备已 `revoked` → 403（**永不复活**）；PENDING 才能被 add（`serial=1`、无凭证
//!   历史）；ACTIVE 不能被 add（换钥属轮换/激活，不走恢复）；
//! - `PolicyV2` 在 proto 层固定 `highrisk_threshold=2`/`min_quorum=2`，恢复不能降门限；
//! - 结果仍有 ≥2 台 ACTIVE 时必须保持 `active` 且至少两台不同设备持 `CAP_ADMIN`
//!   （高危 scope 的两设备 quorum 不能被恢复过程绕开）；不足两台才进入 `recovery_only`。
//!
//! # `recovery_only`
//!
//! 进入后 [`super::submit`] 只放行 kind 13（普通管理写 403），`/authz` 管理 realm 也不再
//! 签出有效 cap bearer；设备 realm 与恢复 realm 仍可读证据/同步数据（撤权不等于删数据）。
//! 恢复成功且成员 ≥2 时回到 `active`。

use rusqlite::OptionalExtension;

use super::{
    forbidden, i64_of, insert_cap, invalid, proto_sha256, signature_invalid, two_scope_signers,
    SigPolicy,
};
use crate::error::{Error, ErrorCode, Result};
use crate::ids::Id16;
use crate::scope::ScopeConn;
use crate::vault::{AuthzState, Vault};
use menghunote_sync_proto as proto;

/// 已校验、待执行的一条恢复记录。
pub(super) enum RecoveryPlan {
    Reset(ResetPlan),
    KeyChange(KeyChangePlan),
}

/// 一份新代 cap 的公共校验结果（Reset/KeyChange 共用）。
struct CapPlan {
    caps: Vec<proto::ManagementCapV2>,
}

pub(super) struct ResetPlan {
    policy_bytes: Vec<u8>,
    recovery_pk: [u8; 32],
    caps: Vec<proto::ManagementCapV2>,
    adds: Vec<AddPlan>,
    revoke_ids: Vec<Id16>,
    /// 提交后是否仍有 ≥2 台 ACTIVE（否则 `recovery_only`）。
    keep_active: bool,
    post_authz_rev: u64,
    post_authority_rev: u64,
}

pub(super) struct AddPlan {
    cert: proto::DeviceCertV2,
    cert_bytes: Vec<u8>,
    grant: proto::GrantPackageV2,
}

pub(super) struct KeyChangePlan {
    recovery_pk: [u8; 32],
    #[allow(dead_code)]
    keep_active: bool,
    caps: Vec<proto::ManagementCapV2>,
    post_authz_rev: u64,
    post_authority_rev: u64,
}

// ---------------------------------------------------------------- 公共校验

fn require_v2(vault: &Vault) -> Result<[u8; 32]> {
    if vault.authz_state == AuthzState::Legacy || vault.authz_version != 2 {
        // 旧 vault 没有恢复根，不能走 V2 恢复（必须先 Upgrade）。
        return Err(Error::api(ErrorCode::InvalidState));
    }
    vault
        .recovery_pub
        .ok_or(Error::Invariant("vaults.recovery_pub"))
}

/// 新恢复公钥必须是非全零、且**不等于**当前恢复根（换钥是事实，不是重放）。
fn ensure_fresh_recovery_key(vault: &Vault, fresh: &[u8; 32]) -> Result<()> {
    if fresh.iter().all(|byte| *byte == 0) {
        return Err(invalid());
    }
    if vault.recovery_pub.as_ref() == Some(fresh) {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    Ok(())
}

/// 提交后的 rev 三元组（`control`/`authz`/`authority`）：恢复记录的新代 cap 必须绑定它们。
#[derive(Debug, Clone, Copy)]
struct PostRevs {
    control: u64,
    authz: u64,
    authority: u64,
}

impl PostRevs {
    fn of(vault: &Vault) -> Result<Self> {
        Ok(Self {
            control: vault.control_rev.checked_add(1).ok_or_else(invalid)?,
            authz: vault.authz_rev.checked_add(1).ok_or_else(invalid)?,
            authority: vault.authority_rev.checked_add(1).ok_or_else(invalid)?,
        })
    }
}

/// 提交后仍是 ACTIVE 的设备与登记公钥（`adds` 的证书公钥优先于库内登记值）。
type ActiveAfter = (Vec<(Id16, [u8; 32])>, bool);

/// 新代 cap 的公共校验：主体必须**提交后**仍是 ACTIVE、key 匹配、rev/TTL 绑定提交后的值。
///
/// `active_after` 是提交后 ACTIVE 设备到登记公钥的映射（含本次 adds、排除本次 revokes）。
fn validate_caps(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    policy: &proto::PolicyV2,
    caps: &[proto::ManagementCapV2],
    active_after: &[(Id16, [u8; 32])],
    post: PostRevs,
) -> Result<CapPlan> {
    let mut cap_admin_devices: Vec<Id16> = Vec::new();
    for cap in caps {
        if cap.instance_id != vault.instance_id || cap.vault_id != vault.vault_id {
            return Err(forbidden());
        }
        if cap.issued_control_rev != post.control
            || cap.issued_authz_rev != post.authz
            || cap.issuer_authority_rev != post.authority
        {
            return Err(forbidden());
        }
        let Some((_, sign_pk)) = active_after
            .iter()
            .find(|(device_id, _)| *device_id == cap.subject_device_id)
        else {
            return Err(forbidden());
        };
        if *sign_pk != cap.subject_ed_pk {
            return Err(forbidden());
        }
        if cap.not_before > now || cap.expires_at <= now {
            return Err(forbidden());
        }
        if cap.expires_at.saturating_sub(cap.not_before) > policy.max_cap_ttl {
            return Err(forbidden());
        }
        if cap.scopes.bits() & proto::CapScope::CAP_ADMIN != 0
            && !cap_admin_devices.contains(&cap.subject_device_id)
        {
            cap_admin_devices.push(cap.subject_device_id);
        }
        if super::cap_exists(conn, &cap.cap_id)? {
            return Err(Error::api(ErrorCode::IdConflict));
        }
    }
    // 保持 active 时不得把任一高危 scope 的两设备 quorum 降为 1。
    if active_after.len() >= proto::PolicyV2::MIN_QUORUM as usize
        && cap_admin_devices.len() < proto::PolicyV2::MIN_QUORUM as usize
    {
        return Err(forbidden());
    }
    Ok(CapPlan {
        caps: caps.to_vec(),
    })
}

/// 提交后仍是 ACTIVE 的设备与登记公钥（`adds` 的证书公钥优先于库内登记值）。
fn active_after(conn: &ScopeConn, adds: &[AddPlan], revoke_ids: &[Id16]) -> Result<ActiveAfter> {
    let mut stmt = conn.prepare(
        "SELECT device_id, sign_pk FROM devices
         WHERE vault_id = ?1 AND status = 'active' ORDER BY device_id ASC",
    )?;
    let rows = stmt.query_map([conn.scope().as_slice()], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut out: Vec<(Id16, [u8; 32])> = Vec::new();
    for row in rows {
        let (device_id, sign_pk) = row?;
        let device_id: Id16 = device_id
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("devices.device_id"))?;
        if revoke_ids.contains(&device_id) {
            continue;
        }
        out.push((
            device_id,
            sign_pk
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("devices.sign_pk"))?,
        ));
    }
    for add in adds {
        out.push((add.cert.device_id, add.cert.sign_ed25519_pub));
    }
    out.sort_by_key(|(device_id, _)| *device_id);
    let keep_active = out.len() >= proto::PolicyV2::MIN_QUORUM as usize;
    Ok((out, keep_active))
}

/// 目标设备状态（adds 只接受 PENDING，revokes 只接受 ACTIVE/已 revoked 幂等）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceStatus {
    Pending,
    Active,
    Revoked,
}

fn device_status(conn: &ScopeConn, device_id: &Id16) -> Result<Option<DeviceStatus>> {
    let status: Option<String> = conn
        .query_row(
            "SELECT status FROM devices WHERE vault_id = ?1 AND device_id = ?2",
            (conn.scope().as_slice(), device_id.as_slice()),
            |row| row.get(0),
        )
        .optional()?;
    Ok(match status.as_deref() {
        None => None,
        Some("pending") => Some(DeviceStatus::Pending),
        Some("active") => Some(DeviceStatus::Active),
        Some("revoked") => Some(DeviceStatus::Revoked),
        Some(_) => return Err(Error::Invariant("devices.status")),
    })
}

fn device_keys(conn: &ScopeConn, device_id: &Id16) -> Result<([u8; 32], [u8; 32])> {
    let (sign_pk, hpke_pk): (Vec<u8>, Vec<u8>) = conn.query_row(
        "SELECT sign_pk, hpke_pk FROM devices WHERE vault_id = ?1 AND device_id = ?2",
        (conn.scope().as_slice(), device_id.as_slice()),
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((
        sign_pk
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("devices.sign_pk"))?,
        hpke_pk
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("devices.hpke_pk"))?,
    ))
}

fn credential_count(conn: &ScopeConn, device_id: &Id16) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM device_credentials WHERE vault_id = ?1 AND device_id = ?2",
        (conn.scope().as_slice(), device_id.as_slice()),
        |row| row.get(0),
    )?)
}

/// 设备 add 的 PoP：role3 signer 必须是目标设备、pk = 证书签名公钥，签 `pop` 域。
fn verify_add_pop(
    signed: &proto::SignedControlV2,
    device_id: &Id16,
    expected_pk: &[u8; 32],
    policy: SigPolicy<'_>,
) -> Result<()> {
    let mut seen = 0usize;
    for (index, signer) in signed.operation.signers.iter().enumerate() {
        if signer.role != proto::SignerRole::TargetPop {
            continue;
        }
        seen += 1;
        if signer.device_id != Some(*device_id)
            || signer.cap_id.is_some()
            || signer.pk != *expected_pk
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
    if seen != 1 {
        return Err(forbidden());
    }
    Ok(())
}

/// 恢复根签名（`recover` 域）：role2 + 指定 pk，必须恰好一个。
fn verify_recovery_signer(
    signed: &proto::SignedControlV2,
    expected_pk: &[u8; 32],
    policy: SigPolicy<'_>,
) -> Result<()> {
    let input =
        proto::recover_signing_input(&signed.operation, proto_sha256).map_err(|_| invalid())?;
    let mut seen = 0usize;
    for (index, signer) in signed.operation.signers.iter().enumerate() {
        if signer.role != proto::SignerRole::OfflineRecovery {
            continue;
        }
        if signer.pk != *expected_pk {
            continue;
        }
        seen += 1;
        if signer.device_id.is_some() || signer.cap_id.is_some() {
            return Err(forbidden());
        }
        if policy
            .verify(index, &input, &signer.pk, &signed.signatures[index])
            .is_err()
        {
            return Err(signature_invalid());
        }
    }
    if seen != 1 {
        return Err(forbidden());
    }
    Ok(())
}

/// role2 签名者的数量/角色约束（Reset 允许两颗不同的恢复钥签名，Change 只允许新钥一颗）。
fn ensure_role2_count(signed: &proto::SignedControlV2, expected: usize) -> Result<()> {
    let count = signed
        .operation
        .signers
        .iter()
        .filter(|signer| signer.role == proto::SignerRole::OfflineRecovery)
        .count();
    if count != expected {
        return Err(forbidden());
    }
    Ok(())
}

// ---------------------------------------------------------------- kind 13：RecoveryReset

pub(super) fn validate_reset(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
    policy: SigPolicy<'_>,
) -> Result<RecoveryPlan> {
    let current_recovery_pk = require_v2(vault)?;
    let payload =
        proto::RecoveryResetPayloadV2::decode(&signed.operation.payload).map_err(|_| invalid())?;
    ensure_fresh_recovery_key(vault, &payload.fresh_recovery_pk)?;
    let policy_bytes = payload.policy.encode().map_err(|_| invalid())?;
    let post = PostRevs::of(vault)?;
    let post_authz_rev = post.authz;
    let post_authority_rev = post.authority;

    // adds：PENDING 才能被重新纳入；证书必须绑定兑换时提交的两把公钥；serial=1 不复用。
    let mut adds = Vec::with_capacity(payload.device_adds.len());
    for add in &payload.device_adds {
        let cert = add.cert.clone();
        let cert_bytes = cert.encode().map_err(|_| invalid())?;
        if cert.instance_id != vault.instance_id || cert.vault_id != vault.vault_id {
            return Err(forbidden());
        }
        match device_status(conn, &cert.device_id)? {
            None => return Err(Error::api(ErrorCode::NotFound)),
            // 已 ACTIVE 不能通过恢复重新装证书（换钥/改权走轮换或设备管理记录）。
            Some(DeviceStatus::Active) => return Err(Error::api(ErrorCode::InvalidState)),
            // 已撤销永不复活。
            Some(DeviceStatus::Revoked) => return Err(forbidden()),
            Some(DeviceStatus::Pending) => {}
        }
        let (sign_pk, hpke_pk) = device_keys(conn, &cert.device_id)?;
        if sign_pk != cert.sign_ed25519_pub || hpke_pk != cert.hpke_x25519_pub {
            return Err(forbidden());
        }
        if cert.cert_serial != 1 || credential_count(conn, &cert.device_id)? != 0 {
            return Err(forbidden());
        }
        if !cert.data_rights.can_read() {
            return Err(forbidden());
        }
        if cert.issued_authz_rev != post_authz_rev {
            return Err(forbidden());
        }
        // grant 包：恰好一份、当前 epoch、本人 hpke、**新代** authority、提交后 authz rev。
        if add.grants.len() != 1 {
            return Err(forbidden());
        }
        let package = &add.grants[0];
        if package.aad.instance_id != vault.instance_id
            || package.aad.vault_id != vault.vault_id
            || package.aad.epoch != vault.active_epoch
            || package.aad.recipient_device_id != cert.device_id
            || package.aad.recipient_hpke_pub != cert.hpke_x25519_pub
            || package.aad.authority_rev != post_authority_rev
            || package.aad.issued_authz_rev != post_authz_rev
        {
            return Err(forbidden());
        }
        let reused: i64 = conn.query_row(
            "SELECT COUNT(*) FROM device_grants WHERE vault_id = ?1 AND grant_id = ?2",
            (conn.scope().as_slice(), package.aad.grant_id.as_slice()),
            |row| row.get(0),
        )?;
        if reused != 0 {
            return Err(Error::api(ErrorCode::IdConflict));
        }
        verify_add_pop(signed, &cert.device_id, &cert.sign_ed25519_pub, policy)?;
        adds.push(AddPlan {
            cert,
            cert_bytes,
            grant: package.clone(),
        });
    }

    // revokes：存在性 + 不能复活 PENDING 占位（与 kind 5 同口径）；已 revoked 幂等。
    for device_id in &payload.revoke_ids {
        match device_status(conn, device_id)? {
            None => return Err(Error::api(ErrorCode::NotFound)),
            Some(DeviceStatus::Pending) => return Err(forbidden()),
            Some(DeviceStatus::Active) | Some(DeviceStatus::Revoked) => {}
        }
    }

    let (after, keep_active) = active_after(conn, &adds, &payload.revoke_ids)?;
    let cap_plan = validate_caps(
        conn,
        now,
        vault,
        &payload.policy,
        &payload.bootstrap_caps,
        &after,
        post,
    )?;

    // 签名：旧恢复根（当前代）+ 新钥 PoP；role2 恰好两颗不同 pk（结构上也不允许相同）。
    ensure_role2_count(signed, 2)?;
    verify_recovery_signer(signed, &current_recovery_pk, policy)?;
    verify_recovery_signer(signed, &payload.fresh_recovery_pk, policy)?;
    // 不允许 role0：恢复记录不是普通管理写（离线根 + 新钥 + 设备 PoP 才是授权面）。
    if signed
        .operation
        .signers
        .iter()
        .any(|signer| signer.role == proto::SignerRole::Device)
    {
        return Err(forbidden());
    }

    Ok(RecoveryPlan::Reset(ResetPlan {
        policy_bytes,
        recovery_pk: payload.fresh_recovery_pk,
        caps: cap_plan.caps,
        adds,
        revoke_ids: payload.revoke_ids,
        keep_active,
        post_authz_rev,
        post_authority_rev,
    }))
}

// ---------------------------------------------------------------- kind 14：RecoveryKeyChange

pub(super) fn validate_key_change(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
    policy: SigPolicy<'_>,
) -> Result<RecoveryPlan> {
    let _ = require_v2(vault)?;
    if vault.authz_state != AuthzState::Active {
        // recovery_only 只能走离线 Reset（换钥不会补成员，不能摆脱 recovery_only）。
        return Err(Error::api(ErrorCode::InvalidState));
    }
    let payload = proto::RecoveryKeyChangePayloadV2::decode(&signed.operation.payload)
        .map_err(|_| invalid())?;
    ensure_fresh_recovery_key(vault, &payload.fresh_recovery_pk)?;
    let policy_v2 = super::load_policy(vault)?;
    let post = PostRevs::of(vault)?;
    let (after, keep_active) = active_after(conn, &[], &[])?;
    let cap_plan = validate_caps(
        conn,
        now,
        vault,
        &policy_v2,
        &payload.bootstrap_caps,
        &after,
        post,
    )?;
    // 两台 POLICY（不同设备/公钥）+ 恰一颗新钥 PoP。
    let digest_input =
        proto::control_signing_input(&signed.operation, proto_sha256).map_err(|_| invalid())?;
    two_scope_signers(
        conn,
        now,
        signed,
        &digest_input,
        &[proto::CapScope::POLICY],
        &[proto::SignerRole::OfflineRecovery],
        policy,
    )?;
    ensure_role2_count(signed, 1)?;
    verify_recovery_signer(signed, &payload.fresh_recovery_pk, policy)?;
    Ok(RecoveryPlan::KeyChange(KeyChangePlan {
        recovery_pk: payload.fresh_recovery_pk,
        keep_active,
        caps: cap_plan.caps,
        post_authz_rev: post.authz,
        post_authority_rev: post.authority,
    }))
}

// ---------------------------------------------------------------- 效果

pub(super) fn apply(
    conn: &ScopeConn,
    now: i64,
    vault: &Vault,
    rev: u64,
    plan: RecoveryPlan,
) -> Result<(u64, u64)> {
    match plan {
        RecoveryPlan::Reset(plan) => {
            // 1) 撤销（先撤后加：同一设备不会既 add 又 revoke，wire 层已拒绝）。
            for device_id in &plan.revoke_ids {
                conn.execute(
                    "UPDATE devices SET status = 'revoked', revoked_at = ?1
                     WHERE vault_id = ?2 AND device_id = ?3 AND status != 'revoked'",
                    (now, conn.scope().as_slice(), device_id.as_slice()),
                )?;
                conn.execute(
                    "UPDATE management_caps SET revoked_control_rev = ?1
                     WHERE vault_id = ?2 AND subject_device = ?3
                       AND revoked_control_rev IS NULL",
                    (
                        i64_of(rev, "management_caps.revoked_control_rev")?,
                        conn.scope().as_slice(),
                        device_id.as_slice(),
                    ),
                )?;
            }
            // 2) 设备 add：凭证历史 + devices 投影 + 当前 epoch 的 grant。
            for add in &plan.adds {
                crate::control_v2::devices::insert_credential(
                    conn,
                    now,
                    rev,
                    &add.cert,
                    &add.cert_bytes,
                )?;
                let changed = conn.execute(
                    "UPDATE devices SET status = 'active',
                                        certificate = ?1,
                                        certificate_signature = NULL,
                                        approval_control_rev = ?2,
                                        issued_authz_rev = ?3
                     WHERE vault_id = ?4 AND device_id = ?5 AND status = 'pending'",
                    (
                        add.cert_bytes.as_slice(),
                        i64_of(rev, "devices.approval_control_rev")?,
                        i64_of(plan.post_authz_rev, "devices.issued_authz_rev")?,
                        conn.scope().as_slice(),
                        add.cert.device_id.as_slice(),
                    ),
                )?;
                if changed != 1 {
                    return Err(Error::Invariant("recovery reset add projection"));
                }
                crate::control_v2::devices::upsert_grant(
                    conn,
                    rev,
                    vault,
                    &add.cert,
                    &add.grant,
                    plan.post_authz_rev,
                )?;
            }
            // 3) 新代 caps（issued rev 指向本次提交；issuer_authority_rev = 新代）。
            for cap in &plan.caps {
                insert_cap(
                    conn,
                    now,
                    rev,
                    plan.post_authz_rev,
                    plan.post_authority_rev,
                    cap,
                )?;
            }
            finish_authority_switch(
                conn,
                &plan.policy_bytes,
                &plan.recovery_pk,
                plan.keep_active,
                plan.post_authz_rev,
                plan.post_authority_rev,
            )?;
            Ok((plan.post_authz_rev, plan.post_authority_rev))
        }
        RecoveryPlan::KeyChange(plan) => {
            for cap in &plan.caps {
                insert_cap(
                    conn,
                    now,
                    rev,
                    plan.post_authz_rev,
                    plan.post_authority_rev,
                    cap,
                )?;
            }
            // 只换恢复钥 + 新代 caps：不改 policy、不动成员集合。
            let changed = conn.execute(
                "UPDATE vaults SET authority_rev = ?1,
                                   recovery_pub = ?2,
                                   authz_rev = ?3,
                                   auth_rev = auth_rev + 1
                 WHERE vault_id = ?4 AND authz_state = 'active'",
                (
                    i64_of(plan.post_authority_rev, "authority_rev")?,
                    plan.recovery_pk.as_slice(),
                    i64_of(plan.post_authz_rev, "authz_rev")?,
                    conn.scope().as_slice(),
                ),
            )?;
            if changed != 1 {
                return Err(Error::Invariant("recovery key change projection"));
            }
            Ok((plan.post_authz_rev, plan.post_authority_rev))
        }
    }
}

/// 原子切换 authority 代：`authority_rev+1`、写恢复钥/policy、按成员数决定 active/recovery_only。
fn finish_authority_switch(
    conn: &ScopeConn,
    policy_bytes: &[u8],
    recovery_pk: &[u8; 32],
    keep_active: bool,
    post_authz_rev: u64,
    post_authority_rev: u64,
) -> Result<()> {
    let state = if keep_active {
        "active"
    } else {
        "recovery_only"
    };
    let changed = conn.execute(
        "UPDATE vaults SET authority_rev = ?1,
                           recovery_pub = ?2,
                           policy_bytes = ?3,
                           authz_state = ?4,
                           authz_rev = ?5,
                           auth_rev = auth_rev + 1
         WHERE vault_id = ?6",
        (
            i64_of(post_authority_rev, "authority_rev")?,
            recovery_pk.as_slice(),
            policy_bytes,
            state,
            i64_of(post_authz_rev, "authz_rev")?,
            conn.scope().as_slice(),
        ),
    )?;
    if changed != 1 {
        return Err(Error::Invariant("recovery authority switch"));
    }
    Ok(())
}
