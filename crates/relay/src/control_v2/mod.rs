//! ADR 0015 **S4b1/S4b2/S4c**：V2 控制提交链、设备管理、轮换/激活与恢复记录。
//!
//! 范围：
//! - S4b1：`UpgradePolicy`（kind 0）、`CapGrant`/`CapRevoke`（kind 2/3）、`InviteCreate`
//!   （kind 11），以及所有 V2 控制记录共用的链/CAS/时间窗/幂等校验；
//! - S4b2（子模块 `devices`）：`DeviceApprove`（kind 4）、`DeviceRevoke`（kind 5）、
//!   `DeviceRightsChange`（kind 16）——设备凭证历史、grant 投影、撤销级联与 quorum 保护；
//! - S4c：`InviteCancel`（kind 12，本模块）、轮换/激活 `RotationBegin/Abort/Restart/
//!   AddRevokes/EpochActivate`（kind 6–10，子模块 `rotation`）、`RecoveryReset`/
//!   `RecoveryKeyChange`（kind 13/14，子模块 `recovery`）与 `recovery_only` 门禁。
//!
//! **`PolicyReplace`（kind 1）仍 501 fail-stop**（后续切片）；未知 kind（15/≥17）在
//! `OperationV2` 严格解码阶段就拒绝（400）。
//!
//! # 提交入口
//!
//! 唯一路径 `/v2/vaults/{lowerhex32}/control/submit`，body 固定 `{0: bytes(SignedControlV2)}`
//! （禁止别名字段/query/转义路径）。提交是**自证**的：操作里的全部事实都被各签名者
//! 私钥签过，relay 只按本 vault 的 DB 事实验证门限；不需要 bearer token（管理 bearer
//! 只用于 `/authz/*` 选定 cap，不代替控制签名）。
//!
//! # 事务顺序（ADR 0015 §鉴权、revision 与事务）
//!
//! 交给本模块的闭包已经在 `BEGIN IMMEDIATE` 里，顺序固定为：
//!
//! 1. 结构/租户/路径校验（`vault_id`/`instance_id`/唯一路径/canonical 字节）；
//! 2. `dedup_v2` 幂等查询（同 request_id 同摘要回放原回执，异摘要 409）；
//! 3. 时间窗（`signed_at` 过去 ≤300s、未来 ≤30s；`not_after ≤ signed_at+300`）；
//! 4. 链 CAS（`expected_control_rev`/`prev_control_hash` 必须等于当前 head）；
//! 5. 按 kind 校验签名者角色/门限/时间/scope/authority 与 payload 语义；
//! 6. 追加控制提交（`SHA256(prev || SignedControlV2)`）、按 kind 更新投影与 rev；
//! 7. 同事务写 `dedup_v2` 回执。
//!
//! 幂等查询**先于**签名/时效校验：成功提交后 cap 可能被撤销/过期，重放必须仍能拿回原
//! 回执（ADR：不以幂等重放重新授权）。回执只含公开链事实（rev/hash/kind/request_id/revs）。
//!
//! # rev 推进
//!
//! | kind | control | authz | auth | authority |
//! | --- | --- | --- | --- | --- |
//! | 0 UpgradePolicy | +1 | =1 | +1 | =1 |
//! | 2 CapGrant | +1 | +1 | +1 | 不变 |
//! | 3 CapRevoke | +1 | +1 | +1 | 不变 |
//! | 4 DeviceApprove | +1 | +1 | +1 | 不变（已 ACTIVE 的幂等重放不推进 rev） |
//! | 5 DeviceRevoke | +1 | +1 | +1 | 不变（全部已 revoked 的重复不推进 rev） |
//! | 6 RotationBegin | +1 | 不变 | +1 | 不变（freeze 切断全部旧 token） |
//! | 7 RotationAbort | +1 | 不变 | 不变 | 不变（不复活已吊销的旧 epoch） |
//! | 8 RotationRestart | +1 | 不变 | 不变 | 不变 |
//! | 9 RotationAddRevokes | +1 | 不变 | +1/每台被吊销设备 | 不变 |
//! | 10 EpochActivate | +1 | 成员变更才 +1 | +1 | 不变（只切数据 epoch） |
//! | 11 InviteCreate | +1 | 不变 | 不变 | 不变 |
//! | 12 InviteCancel | +1 | 不变 | 不变 | 不变 |
//! | 13 RecoveryReset | +1 | +1 | +1 | +1（换离线恢复根代） |
//! | 14 RecoveryKeyChange | +1 | +1 | +1 | +1（只换恢复钥） |
//! | 16 DeviceRightsChange | +1 | +1 | +1 | 不变 |
//!
//! 授权变化（Upgrade/Grant/Revoke/Approve/Rights/设备撤销/恢复）同事务推进 authz/auth；
//! 邀请与纯 rotation 只推进 control（ADR）。cap 自然到期**不造记录**：每次使用按
//! `expires_at` 即时失效。
//!
//! # 失败语义（fail-stop）
//!
//! - 结构/CDE/时间格式 → 400 `INVALID_REQUEST`；签名不过 → 400 `SIGNATURE_INVALID`；
//! - 门限/角色/scope/authority/ACTIVE/manifest/quorum/PoP/证书-grant 不符 → 403 `FORBIDDEN`；
//! - CAS/head/rev 过期 → 409 `CAS_CONFLICT`；同 request_id 异摘要/重复 cap/invite/grant、
//!   目标状态与记录不符（例如设备已 ACTIVE 但提交的证书不同）→ 409；
//! - `recovery_only` 下除 kind 13 外的管理写 → 403 `FORBIDDEN`（绝不回退 creator 单签）；
//! - 已知但未实现的 kind（1 `PolicyReplace`）→ 501 `NOT_IMPLEMENTED`（绝不静默接受）；
//! - 未知 kind（15/≥17）在 `OperationV2` 严格解码阶段就拒绝（400）。
//!
//! 错误体**不泄露**授权明细：门限/状态/PoP 校验失败统一 403，只有"目标不存在"是 404。

use std::collections::BTreeSet;

use rusqlite::OptionalExtension;

use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{sha256, Hash32, Id16};
use crate::scope::ScopeConn;
use crate::time::Clock;
use crate::vault::{self, AuthzState, Vault};
use menghunote_sync_proto as proto;

mod devices;
mod recovery;
mod rotation;

// 恢复证据视图（`api::get_recovery_evidence`）与测试用的集合摘要只读入口。
pub use rotation::{grant_set_digest, member_cert_set_digest};

/// 提交窗口：签名时间最多早于服务器 300s。
pub const SIGNED_AT_PAST_SKEW: u64 = 300;
/// 提交窗口：签名时间最多晚于服务器 30s。
pub const SIGNED_AT_FUTURE_SKEW: u64 = 30;
/// `not_after - signed_at` 上限（必须是短窗，防"永久有效的待提交操作"）。
pub const DEADLINE_MAX_SECS: u64 = 300;

fn proto_sha256(bytes: &[u8]) -> [u8; 32] {
    sha256(bytes)
}

fn invalid() -> Error {
    Error::api(ErrorCode::InvalidRequest)
}

fn forbidden() -> Error {
    Error::api(ErrorCode::Forbidden)
}

fn cas_conflict(head_rev: u64) -> Error {
    Error::Api(ApiError::new(ErrorCode::CasConflict).control_rev(head_rev))
}

fn signature_invalid() -> Error {
    Error::api(ErrorCode::SignatureInvalid)
}

fn i64_of(value: u64, field: &'static str) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Invariant(field))
}

// ---------------------------------------------------------------- 回执

/// 控制提交回执（首次提交与幂等重放都是同一份字节）。
///
/// wire（CDE map，键升序）：
/// `{0: control_rev, 1: head_hash(32B), 2: kind, 3: request_id(16B),
///   4: authz_rev, 5: authority_rev}`。
/// **不含** cap/私钥/root/明文 payload。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlReceipt {
    pub control_rev: u64,
    pub head_hash: Hash32,
    pub kind: u8,
    pub request_id: [u8; 16],
    pub authz_rev: u64,
    pub authority_rev: u64,
}

impl ControlReceipt {
    pub fn fields(&self) -> crate::wire::Fields {
        vec![
            (0, crate::wire::uint(self.control_rev)),
            (1, crate::wire::bytes(self.head_hash)),
            (2, crate::wire::uint(u64::from(self.kind))),
            (3, crate::wire::bytes(self.request_id)),
            (4, crate::wire::uint(self.authz_rev)),
            (5, crate::wire::uint(self.authority_rev)),
        ]
    }
}

/// 提交结果：`response` 是回执的 canonical CDE 字节（重放时字节一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitOutcome {
    pub response: Vec<u8>,
    pub replayed: bool,
}

// ---------------------------------------------------------------- 提交主流程

/// V2 控制提交（事务内调用；外层已 `BEGIN IMMEDIATE`）。
pub fn submit(
    conn: &ScopeConn,
    clock: &dyn Clock,
    signed: &proto::SignedControlV2,
    signed_bytes: &[u8],
) -> Result<SubmitOutcome> {
    let now_i64 = clock.now_secs();
    let now = u64::try_from(now_i64).map_err(|_| invalid())?;
    let vault = vault::load(conn)?;
    let operation = &signed.operation;

    // ① 结构/租户/路径：签名覆盖了这些事实，不一致即 fail-stop。
    if operation.vault_id != vault.vault_id || operation.instance_id != vault.instance_id {
        return Err(forbidden());
    }
    if operation.canonical_path != proto::canonical_submit_path(&vault.vault_id) {
        return Err(Error::api(ErrorCode::NotFound));
    }
    // 链 hash 的输入必须唯一：只接受重新编码后逐字节相同的 SignedControlV2。
    if signed.encode().map_err(|_| invalid())? != signed_bytes {
        return Err(invalid());
    }

    // ② 幂等：先于时效/签名/CAS（成功提交后的重放必须仍返回原回执）。
    let request_hash = sha256(signed_bytes);
    if let Some((stored_hash, response)) = lookup(conn, &operation.request_id)? {
        if stored_hash != request_hash {
            return Err(Error::api(ErrorCode::IdConflict));
        }
        return Ok(SubmitOutcome {
            response,
            replayed: true,
        });
    }

    // ③ 时间窗（只看首次提交；历史回放走上面的幂等分支）。
    check_time_window(operation, now)?;

    // ④ 链 CAS + 时间不倒退。
    let (head_rev, head_hash) = vault::control_head(conn)?;
    if operation.expected_control_rev != head_rev || operation.prev_control_hash != head_hash {
        return Err(cas_conflict(head_rev));
    }
    check_signed_at_monotonic(conn, operation.signed_at)?;

    // ⑤ 已知但本切片未实现的 kind（S4c）先 fail-stop，不让客户端从错误码
    //    推断授权状态；未知数值 kind 在 `OperationV2` 严格解码阶段已拒绝。
    match operation.kind {
        proto::ControlKindV2::UpgradePolicy
        | proto::ControlKindV2::CapGrant
        | proto::ControlKindV2::CapRevoke
        | proto::ControlKindV2::DeviceApprove
        | proto::ControlKindV2::DeviceRevoke
        | proto::ControlKindV2::DeviceRightsChange
        | proto::ControlKindV2::InviteCreate
        | proto::ControlKindV2::InviteCancel
        | proto::ControlKindV2::RotationBegin
        | proto::ControlKindV2::RotationAbort
        | proto::ControlKindV2::RotationRestart
        | proto::ControlKindV2::RotationAddRevokes
        | proto::ControlKindV2::EpochActivate
        | proto::ControlKindV2::RecoveryReset
        | proto::ControlKindV2::RecoveryKeyChange => {}
        // 已知但未实现（15 未分配，其余数值在严格解码处已拒绝）。
        proto::ControlKindV2::PolicyReplace => return Err(Error::api(ErrorCode::NotImplemented)),
    }

    // ⑥ 授权状态与 authz/authority rev：除 Upgrade（首条 V2 记录）外，
    //    操作必须绑定**当前**授权状态（陈旧 rev 一律 CAS 409）。
    //
    //    `recovery_only`（可用设备不足两台）：只许离线 `RecoveryReset`（kind 13），
    //    其余管理写一律 403 —— 绝不回退 creator 单签。
    if operation.kind != proto::ControlKindV2::UpgradePolicy {
        if vault.authz_state == AuthzState::RecoveryOnly
            && operation.kind != proto::ControlKindV2::RecoveryReset
        {
            return Err(forbidden());
        }
        require_v2_authz(&vault)?;
        if operation.expected_authz_rev != vault.authz_rev
            || operation.authority_rev != vault.authority_rev
        {
            return Err(cas_conflict(head_rev));
        }
    }

    // ⑦ kind 专属校验（含全部签名者验签）。
    let plan = match operation.kind {
        proto::ControlKindV2::UpgradePolicy => {
            Plan::Upgrade(validate_upgrade(conn, now, &vault, signed, head_rev)?)
        }
        proto::ControlKindV2::CapGrant => {
            Plan::CapGrant(validate_cap_grant(conn, now, &vault, signed)?)
        }
        proto::ControlKindV2::CapRevoke => {
            Plan::CapRevoke(validate_cap_revoke(conn, now, &vault, signed)?)
        }
        proto::ControlKindV2::InviteCreate => {
            Plan::Invite(validate_invite_create(conn, now, &vault, signed)?)
        }
        // ADR 0015 S4b2：设备凭证/撤销/改权（见 `devices` 子模块）。
        proto::ControlKindV2::DeviceApprove => {
            Plan::Device(devices::validate_approve(conn, now, &vault, signed)?)
        }
        proto::ControlKindV2::DeviceRevoke => {
            Plan::Device(devices::validate_revoke(conn, now, &vault, signed)?)
        }
        proto::ControlKindV2::DeviceRightsChange => {
            Plan::Device(devices::validate_rights_change(conn, now, &vault, signed)?)
        }
        // ADR 0015 S4c：轮换/激活（授权只走 V2 控制记录，无 v1 管理签名旁路）。
        proto::ControlKindV2::RotationBegin => {
            Plan::Rotation(rotation::validate_begin(conn, now, &vault, signed)?)
        }
        proto::ControlKindV2::RotationAbort => {
            Plan::Rotation(rotation::validate_abort(conn, now, &vault, signed)?)
        }
        proto::ControlKindV2::RotationRestart => {
            Plan::Rotation(rotation::validate_restart(conn, now, &vault, signed)?)
        }
        proto::ControlKindV2::RotationAddRevokes => {
            Plan::Rotation(rotation::validate_add_revokes(conn, now, &vault, signed)?)
        }
        proto::ControlKindV2::EpochActivate => {
            Plan::Rotation(rotation::validate_activate(conn, now, &vault, signed)?)
        }
        proto::ControlKindV2::InviteCancel => {
            Plan::InviteCancel(validate_invite_cancel(conn, now, &vault, signed)?)
        }
        proto::ControlKindV2::RecoveryReset => {
            Plan::Recovery(recovery::validate_reset(conn, now, &vault, signed)?)
        }
        proto::ControlKindV2::RecoveryKeyChange => {
            Plan::Recovery(recovery::validate_key_change(conn, now, &vault, signed)?)
        }
        // 已知但未实现（PolicyReplace 属后续切片）：fail-stop，绝不静默接受。
        proto::ControlKindV2::PolicyReplace => return Err(Error::api(ErrorCode::NotImplemented)),
    };

    // ⑧ 执行（控制提交 + 投影/rev）与回执。
    let rev = vault::append_control(conn, now_i64, signed_bytes)?;
    if rev
        != operation
            .expected_control_rev
            .checked_add(1)
            .ok_or_else(invalid)?
    {
        return Err(Error::Invariant("control rev"));
    }
    let record_hash = signed
        .chain_hash(&head_hash, proto_sha256)
        .map_err(|_| invalid())?;
    let (authz_rev, authority_rev) = apply(conn, now_i64, &vault, rev, plan)?;
    let receipt = ControlReceipt {
        control_rev: rev,
        head_hash: record_hash,
        kind: operation.kind.as_u8(),
        request_id: operation.request_id,
        authz_rev,
        authority_rev,
    };
    let response = crate::wire::encode(receipt.fields())?;
    // ⑨ 幂等回执与业务写入同事务（dedup_v2 的 control_rev 复合 FK 指向刚写出的提交）。
    conn.execute(
        "INSERT INTO dedup_v2(vault_id, request_id, request_hash, control_rev, accepted_seq,
                               response, created_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
        (
            conn.scope().as_slice(),
            operation.request_id.as_slice(),
            request_hash.as_slice(),
            i64_of(rev, "dedup_v2.control_rev")?,
            response.as_slice(),
            now_i64,
        ),
    )?;
    Ok(SubmitOutcome {
        response,
        replayed: false,
    })
}

/// 读 `dedup_v2` 回执：`(request_hash, response_bytes)`。
fn lookup(conn: &ScopeConn, request_id: &[u8; 16]) -> Result<Option<(Hash32, Vec<u8>)>> {
    let mut stmt = conn.prepare(
        "SELECT request_hash, response FROM dedup_v2
         WHERE vault_id = ?1 AND request_id = ?2",
    )?;
    let row = stmt
        .query_row((conn.scope().as_slice(), request_id.as_slice()), |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .optional()?;
    match row {
        None => Ok(None),
        Some((hash, response)) => Ok(Some((
            hash.as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("dedup_v2.request_hash"))?,
            response,
        ))),
    }
}

/// `signed_at`/`not_after` 的实时窗口（ADR 0015 §能力、门限与恢复）。
fn check_time_window(operation: &proto::OperationV2, now: u64) -> Result<()> {
    let signed_at = operation.signed_at;
    if signed_at > now.saturating_add(SIGNED_AT_FUTURE_SKEW) {
        // 未来时间戳不得当"预授权"用。
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    if signed_at.saturating_add(SIGNED_AT_PAST_SKEW) < now {
        return Err(Error::api(ErrorCode::Expired));
    }
    if operation.not_after > signed_at.saturating_add(DEADLINE_MAX_SECS) {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    if operation.not_after < now {
        return Err(Error::api(ErrorCode::Expired));
    }
    Ok(())
}

/// V2 链签名时间不得倒退（v1 裸记录不参与比较）。
fn check_signed_at_monotonic(conn: &ScopeConn, signed_at: u64) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT signed_record FROM control_commits
         WHERE vault_id = ?1 ORDER BY rev DESC LIMIT 1",
    )?;
    let last: Option<Vec<u8>> = stmt
        .query_row([conn.scope().as_slice()], |row| row.get(0))
        .optional()?;
    if let Some(bytes) = last {
        if let Ok(previous) = proto::SignedControlV2::decode(&bytes) {
            if signed_at < previous.operation.signed_at {
                return Err(Error::api(ErrorCode::InvalidRequest));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- 校验与执行

enum Plan {
    Upgrade(UpgradePlan),
    CapGrant(proto::ManagementCapV2),
    CapRevoke(Id16),
    Invite(InvitePlan),
    /// ADR 0015 S4b2：kind 4/5/16（见 `devices::DevicePlan`）。
    Device(devices::DevicePlan),
    /// ADR 0015 S4c：kind 6–10（见 `rotation::RotationPlan`）。
    Rotation(rotation::RotationPlan),
    /// ADR 0015 S4c：kind 12（`InviteCancel`）。
    InviteCancel(InviteCancelPlan),
    /// ADR 0015 S4c：kind 13/14（见 `recovery::RecoveryPlan`）。
    Recovery(recovery::RecoveryPlan),
}

struct InviteCancelPlan {
    invite_id: Id16,
}

struct UpgradePlan {
    policy_bytes: Vec<u8>,
    recovery_pk: [u8; 32],
    caps: Vec<proto::ManagementCapV2>,
}

struct InvitePlan {
    payload: proto::InviteCreatePayloadV2,
    issuer_cap_id: Id16,
}

fn validate_upgrade(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
    head_rev: u64,
) -> Result<UpgradePlan> {
    // Upgrade 只在 legacy（未启用 V2）的 vault 上成立，且必须是全新 authority 代。
    if vault.authz_state != AuthzState::Legacy
        || vault.authz_version != 1
        || vault.authz_rev != 0
        || vault.authority_rev != 0
    {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    let operation = &signed.operation;
    if operation.expected_authz_rev != 0 || operation.authority_rev != 1 {
        return Err(cas_conflict(head_rev));
    }
    let payload =
        proto::UpgradePolicyPayloadV2::decode(&operation.payload).map_err(|_| invalid())?;
    // 旧管理 rev 必须就是 relay 当前值（防止旧创世材料夹带）。
    if payload.old_manager_rev != vault.manager_rev {
        return Err(forbidden());
    }
    let policy_bytes = payload.policy.encode().map_err(|_| invalid())?;

    // manifest 必须逐字段等于 DB 的全部 ACTIVE 设备（顺序 = device_id 升序）。
    let active = active_devices(conn)?;
    if active.len() < 2 || payload.active_manifest.len() != active.len() {
        return Err(forbidden());
    }
    for (cert, row) in payload.active_manifest.iter().zip(active.iter()) {
        let matches = cert.device_id == row.device_id
            && cert.instance_id == vault.instance_id
            && cert.vault_id == vault.vault_id
            && cert.sign_ed25519_pub == row.sign_pk
            && cert.hpke_x25519_pub == row.hpke_pk
            // 新 authority 代的第一批证书：issued_authz_rev 固定 = 1（S4b 冻结 §3/§7）。
            && cert.issued_authz_rev == 1;
        if !matches {
            return Err(forbidden());
        }
    }

    // 签名者角色：1 个旧 creator（role1）+ ≥2 台 ACTIVE（role0）+ 1 个恢复钥 PoP（role2）。
    let digest_input =
        proto::control_signing_input(operation, proto_sha256).map_err(|_| invalid())?;
    let upgrade_input =
        proto::upgrade_signing_input(operation, proto_sha256).map_err(|_| invalid())?;
    let recover_input =
        proto::recover_signing_input(operation, proto_sha256).map_err(|_| invalid())?;
    let mut creator: Option<(usize, &proto::SignerV2)> = None;
    let mut recovery: Option<(usize, &proto::SignerV2)> = None;
    let mut confirmations: Vec<(usize, &proto::SignerV2)> = Vec::new();
    for (index, signer) in operation.signers.iter().enumerate() {
        match signer.role {
            proto::SignerRole::CreatorUpgrade => {
                if creator.replace((index, signer)).is_some() {
                    return Err(forbidden());
                }
            }
            proto::SignerRole::OfflineRecovery => {
                if recovery.replace((index, signer)).is_some() {
                    return Err(forbidden());
                }
            }
            proto::SignerRole::Device => {
                // 升级时还没有 cap；role0 只确认同一摘要。
                if signer.cap_id.is_some() {
                    return Err(forbidden());
                }
                confirmations.push((index, signer));
            }
            // 首次准入的 PoP 属于 DeviceApprove（kind 4），升级记录不得夹带。
            proto::SignerRole::TargetPop => return Err(forbidden()),
        }
    }
    let (creator_index, creator) = creator.ok_or_else(forbidden)?;
    let manager_pub = vault::manager_pubkey(conn)?.ok_or_else(forbidden)?;
    if creator.pk != manager_pub
        || verify_signature(
            &upgrade_input,
            &creator.pk,
            &signed.signatures[creator_index],
        )
        .is_err()
    {
        return Err(signature_invalid());
    }
    let (recovery_index, recovery) = recovery.ok_or_else(forbidden)?;
    if recovery.pk != payload.fresh_recovery_pk
        || verify_signature(
            &recover_input,
            &recovery.pk,
            &signed.signatures[recovery_index],
        )
        .is_err()
    {
        return Err(signature_invalid());
    }

    // ≥2 台**当前 ACTIVE**、device id 与公钥都不同的设备确认同一摘要。
    if confirmations.len() < 2 {
        return Err(forbidden());
    }
    let mut confirmed_devices = BTreeSet::new();
    let mut confirmed_keys = BTreeSet::new();
    for (index, signer) in &confirmations {
        let Some(device_id) = signer.device_id else {
            return Err(forbidden());
        };
        let Some(row) = active.iter().find(|row| row.device_id == device_id) else {
            return Err(forbidden());
        };
        if signer.pk != row.sign_pk
            || !confirmed_devices.insert(device_id)
            || !confirmed_keys.insert(signer.pk)
        {
            return Err(forbidden());
        }
        if verify_signature(&digest_input, &signer.pk, &signed.signatures[*index]).is_err() {
            return Err(signature_invalid());
        }
    }

    // bootstrap caps：rev 绑定到提交后的值、subject 是 ACTIVE 设备、TTL 在 policy 内；
    // 且不得把任一高危 scope 的 quorum 降到 1（这里至少要求两台不同设备持 CAP_ADMIN）。
    let mut cap_admin_devices = BTreeSet::new();
    for cap in &payload.bootstrap_caps {
        if cap.instance_id != vault.instance_id
            || cap.vault_id != vault.vault_id
            || cap.issued_control_rev != head_rev + 1
            || cap.issued_authz_rev != 1
            || cap.issuer_authority_rev != 1
        {
            return Err(forbidden());
        }
        let Some(row) = active
            .iter()
            .find(|row| row.device_id == cap.subject_device_id)
        else {
            return Err(forbidden());
        };
        if row.sign_pk != cap.subject_ed_pk {
            return Err(forbidden());
        }
        if cap.not_before > now || cap.expires_at <= now {
            return Err(forbidden());
        }
        if cap.expires_at.saturating_sub(cap.not_before) > payload.policy.max_cap_ttl {
            return Err(forbidden());
        }
        if cap.scopes.bits() & proto::CapScope::CAP_ADMIN != 0 {
            cap_admin_devices.insert(cap.subject_device_id);
        }
    }
    if cap_admin_devices.len() < 2 {
        return Err(forbidden());
    }

    Ok(UpgradePlan {
        policy_bytes,
        recovery_pk: payload.fresh_recovery_pk,
        caps: payload.bootstrap_caps,
    })
}

fn validate_cap_grant(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
) -> Result<proto::ManagementCapV2> {
    require_active(vault)?;
    let operation = &signed.operation;
    let payload = proto::CapGrantPayloadV2::decode(&operation.payload).map_err(|_| invalid())?;
    let cap = payload.cap;
    let policy = load_policy(vault)?;
    let digest_input =
        proto::control_signing_input(operation, proto_sha256).map_err(|_| invalid())?;
    let signers = two_scope_signers(
        conn,
        now,
        signed,
        &digest_input,
        &[proto::CapScope::CAP_ADMIN],
        &[],
    )?;

    for signer in &signers {
        // 签发 scope 不得超过**每一位**签发者的有效 scope（不能靠两签拼权限）。
        if !cap.scopes.is_subset_of(signer.cap.scopes) {
            return Err(forbidden());
        }
    }

    if cap.instance_id != vault.instance_id || cap.vault_id != vault.vault_id {
        return Err(forbidden());
    }
    if cap.issued_control_rev != operation.expected_control_rev.saturating_add(1)
        || cap.issued_authz_rev != vault.authz_rev.saturating_add(1)
        || cap.issuer_authority_rev != vault.authority_rev
    {
        return Err(forbidden());
    }
    if cap.not_before > now || cap.expires_at <= now {
        return Err(forbidden());
    }
    if cap.expires_at.saturating_sub(cap.not_before) > policy.max_cap_ttl {
        return Err(forbidden());
    }
    // subject 必须是本 vault 的 ACTIVE 设备，且 key 与登记公钥一致。
    let subject_pk = vault::device_sign_pk(conn, &cap.subject_device_id)?.ok_or_else(forbidden)?;
    if subject_pk != cap.subject_ed_pk {
        return Err(forbidden());
    }
    if cap_exists(conn, &cap.cap_id)? {
        return Err(Error::api(ErrorCode::IdConflict));
    }
    Ok(cap)
}

fn validate_cap_revoke(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
) -> Result<Id16> {
    require_active(vault)?;
    let operation = &signed.operation;
    let payload = proto::CapRevokePayloadV2::decode(&operation.payload).map_err(|_| invalid())?;
    let digest_input =
        proto::control_signing_input(operation, proto_sha256).map_err(|_| invalid())?;
    two_scope_signers(
        conn,
        now,
        signed,
        &digest_input,
        &[proto::CapScope::CAP_ADMIN],
        &[],
    )?;

    // 目标必须存在且未撤销（重复撤销不是幂等成功：必须显式换 request_id 重签）。
    let revoked: Option<Option<i64>> = conn
        .query_row(
            "SELECT revoked_control_rev FROM management_caps
             WHERE vault_id = ?1 AND cap_id = ?2",
            (conn.scope().as_slice(), payload.cap_id.as_slice()),
            |row| row.get(0),
        )
        .optional()?;
    let Some(revoked) = revoked else {
        return Err(Error::api(ErrorCode::NotFound));
    };
    if revoked.is_some() {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    Ok(payload.cap_id)
}

fn validate_invite_create(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
) -> Result<InvitePlan> {
    require_active(vault)?;
    let operation = &signed.operation;
    let payload =
        proto::InviteCreatePayloadV2::decode(&operation.payload).map_err(|_| invalid())?;
    let policy = load_policy(vault)?;
    let digest_input =
        proto::control_signing_input(operation, proto_sha256).map_err(|_| invalid())?;

    // 一台 INVITE cap 即可（ADR 0015 kind 11）。
    if operation.signers.len() != 1 {
        return Err(forbidden());
    }
    let signer = &operation.signers[0];
    if signer.role != proto::SignerRole::Device {
        return Err(forbidden());
    }
    let Some(cap_id) = signer.cap_id else {
        return Err(forbidden());
    };
    let cap = effective_cap(conn, i64_of(now, "now")?, &cap_id)?;
    if cap.subject_device_id != signer.device_id.unwrap_or_default()
        || cap.subject_ed_pk != signer.pk
        || cap.scopes.bits() & proto::CapScope::INVITE == 0
    {
        return Err(forbidden());
    }
    if verify_signature(&digest_input, &signer.pk, &signed.signatures[0]).is_err() {
        return Err(signature_invalid());
    }
    // TTL 以签名时刻为基准（signed_at 已在实时窗口内），且邀请必须仍在未来。
    if payload.expires_at <= operation.signed_at || payload.expires_at <= now {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    if payload.expires_at.saturating_sub(operation.signed_at) > policy.max_invite_ttl {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    // 邀请/令牌事实永不复用：主键或 token_hash 冲突一律 409。
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM invites
         WHERE vault_id = ?1 AND (invite_id = ?2 OR token_hash = ?3)",
        (
            conn.scope().as_slice(),
            payload.invite_id.as_slice(),
            payload.token_hash.as_slice(),
        ),
        |row| row.get(0),
    )?;
    if exists != 0 {
        return Err(Error::api(ErrorCode::IdConflict));
    }
    Ok(InvitePlan {
        payload,
        issuer_cap_id: cap_id,
    })
}

/// 校验 `InviteCancel`（kind 12）：一台 `INVITE` cap（或签发该邀请的 cap 本人）取消。
///
/// - 目标必须存在（否则 404）；**已兑换**（`used_at IS NOT NULL` 且 `pending_device`
///   非空）是永久事实 → 409 `INVALID_STATE`（不伪造 `used_at`，也不删行）；
/// - 已取消（`used_at` 非空、`pending_device` 为空）→ 幂等成功（只追加控制记录，
///   不再改投影）；
/// - 只推进 control（ADR：邀请只推进 control），不推进 authz/auth。
fn validate_invite_cancel(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
) -> Result<InviteCancelPlan> {
    require_active(vault)?;
    let operation = &signed.operation;
    let payload =
        proto::InviteCancelPayloadV2::decode(&operation.payload).map_err(|_| invalid())?;
    let digest_input =
        proto::control_signing_input(operation, proto_sha256).map_err(|_| invalid())?;
    // 一台 INVITE cap 即可（与 kind 11 同口径）。
    if operation.signers.len() != 1 {
        return Err(forbidden());
    }
    let signer = &operation.signers[0];
    if signer.role != proto::SignerRole::Device {
        return Err(forbidden());
    }
    let Some(cap_id) = signer.cap_id else {
        return Err(forbidden());
    };
    let cap = effective_cap(conn, i64_of(now, "now")?, &cap_id)?;
    if cap.subject_device_id != signer.device_id.unwrap_or_default()
        || cap.subject_ed_pk != signer.pk
        || cap.scopes.bits() & proto::CapScope::INVITE == 0
    {
        return Err(forbidden());
    }
    if verify_signature(&digest_input, &signer.pk, &signed.signatures[0]).is_err() {
        return Err(signature_invalid());
    }
    let row: Option<(Option<i64>, Option<Vec<u8>>)> = conn
        .query_row(
            "SELECT used_at, pending_device FROM invites
             WHERE vault_id = ?1 AND invite_id = ?2",
            (conn.scope().as_slice(), payload.invite_id.as_slice()),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((used_at, pending_device)) = row else {
        return Err(Error::api(ErrorCode::NotFound));
    };
    if used_at.is_some() && pending_device.is_some() {
        // 已兑换：永久事实，不能取消（必须显式撤销设备）。
        return Err(Error::api(ErrorCode::InvalidState));
    }
    Ok(InviteCancelPlan {
        invite_id: payload.invite_id,
    })
}

/// 两签门限的公共部分：role0、两个不同 device id/公钥、各自 cap 有效且**同时**具备
/// `required_scopes` 的每一位（不能靠两签拼权限、也不能用同设备两把 cap 凑数）。
///
/// 返回每个签名者绑定的 cap（供调用方再做主体/scope 校验）。签名验证也在本函数内完成：
/// 签名覆盖的正是 `OperationV2` 摘要，换 payload/路径/rev 都会失败。
///
/// `allowed_extra` 是除 role0 之外**允许**出现的角色（按升序逐项比较，多/少/重复都拒绝）。
fn two_scope_signers(
    conn: &ScopeConn,
    now: u64,
    signed: &proto::SignedControlV2,
    digest_input: &[u8],
    required_scopes: &[u32],
    allowed_extra: &[proto::SignerRole],
) -> Result<Vec<CapSigner>> {
    let operation = &signed.operation;
    let mut approvers: Vec<CapSigner> = Vec::new();
    let mut extras: Vec<proto::SignerRole> = Vec::new();
    for (index, signer) in operation.signers.iter().enumerate() {
        if signer.role != proto::SignerRole::Device {
            extras.push(signer.role);
            continue;
        }
        let (Some(device_id), Some(cap_id)) = (signer.device_id, signer.cap_id) else {
            return Err(forbidden());
        };
        let cap = effective_cap(conn, i64_of(now, "now")?, &cap_id)?;
        if cap.subject_device_id != device_id || cap.subject_ed_pk != signer.pk {
            return Err(forbidden());
        }
        for scope in required_scopes {
            if cap.scopes.bits() & scope == 0 {
                return Err(forbidden());
            }
        }
        if verify_signature(digest_input, &signer.pk, &signed.signatures[index]).is_err() {
            return Err(signature_invalid());
        }
        approvers.push(CapSigner { device_id, cap });
    }
    if approvers.len() != 2 {
        return Err(forbidden());
    }
    // 两个不同 ACTIVE device + 两个不同公钥（同设备两 key / 双 cap 都不能凑门限）。
    if approvers[0].device_id == approvers[1].device_id
        || approvers[0].cap.subject_ed_pk == approvers[1].cap.subject_ed_pk
    {
        return Err(forbidden());
    }
    let mut expected = allowed_extra.to_vec();
    expected.sort_by_key(|role| role.as_u8());
    extras.sort_by_key(|role| role.as_u8());
    if extras != expected {
        return Err(forbidden());
    }
    Ok(approvers)
}

struct CapSigner {
    device_id: Id16,
    cap: proto::ManagementCapV2,
}

fn verify_signature(input: &[u8], public_key: &[u8; 32], signature: &[u8]) -> Result<()> {
    crate::sig::verify_ed25519(public_key, input, signature)
}

fn require_active(vault: &Vault) -> Result<()> {
    if vault.authz_state != AuthzState::Active || vault.authz_version != 2 {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    Ok(())
}

/// V2 授权状态（`active` 或 `recovery_only`）：`recovery_only` 只放行离线 Reset。
fn require_v2_authz(vault: &Vault) -> Result<()> {
    if vault.authz_version != 2 || vault.authz_state == AuthzState::Legacy {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    Ok(())
}

// ---------------------------------------------------------------- cap / policy 读取

/// 一台 ACTIVE 设备（控制记录校验用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveDevice {
    device_id: Id16,
    sign_pk: [u8; 32],
    hpke_pk: [u8; 32],
}

fn active_devices(conn: &ScopeConn) -> Result<Vec<ActiveDevice>> {
    let mut stmt = conn.prepare(
        "SELECT device_id, sign_pk, hpke_pk FROM devices
         WHERE vault_id = ?1 AND status = 'active' ORDER BY device_id ASC",
    )?;
    let rows = stmt.query_map([conn.scope().as_slice()], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (device_id, sign_pk, hpke_pk) = row?;
        out.push(ActiveDevice {
            device_id: device_id
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("devices.device_id"))?,
            sign_pk: sign_pk
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("devices.sign_pk"))?,
            hpke_pk: hpke_pk
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("devices.hpke_pk"))?,
        });
    }
    Ok(out)
}

/// cap 是否**此刻有效**：存在、未撤销、`authority_rev` 当前、时间窗内、主体设备 ACTIVE
/// 且登记公钥与 cap 一致。
///
/// 任何一项不满足都统一返回 403（不给 cap 状态的细分 oracle）。`issued_authz_rev` 只是
/// 历史证据，不要求等于当前 rev（ADR 0015 §能力、门限与恢复）。
pub fn effective_cap(conn: &ScopeConn, now: i64, cap_id: &Id16) -> Result<proto::ManagementCapV2> {
    let vault = vault::load(conn)?;
    let mut stmt = conn.prepare(
        "SELECT canonical_bytes, subject_device, subject_pk, scopes, not_before, expires_at,
                issued_control_rev, issued_authz_rev, issuer_authority_rev, revoked_control_rev
         FROM management_caps WHERE vault_id = ?1 AND cap_id = ?2",
    )?;
    let row = stmt
        .query_row((conn.scope().as_slice(), cap_id.as_slice()), |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, Option<i64>>(9)?,
            ))
        })
        .optional()?;
    let Some((
        canonical_bytes,
        subject_device,
        subject_pk,
        scopes,
        not_before,
        expires_at,
        issued_control_rev,
        issued_authz_rev,
        issuer_authority_rev,
        revoked_control_rev,
    )) = row
    else {
        return Err(forbidden());
    };
    let cap = proto::ManagementCapV2::decode(&canonical_bytes).map_err(|_| forbidden())?;
    if cap.encode().map_err(|_| forbidden())? != canonical_bytes {
        return Err(Error::Invariant("management_caps.canonical_bytes"));
    }
    // 投影列必须与 canonical 事实一致（DB 行的防漂移断言）。
    let matches = subject_device.as_slice() == cap.subject_device_id
        && subject_pk.as_slice() == cap.subject_ed_pk
        && scopes == i64::from(cap.scopes.bits())
        && not_before == i64_of(cap.not_before, "cap.not_before")?
        && expires_at == i64_of(cap.expires_at, "cap.expires_at")?
        && issued_control_rev == i64_of(cap.issued_control_rev, "cap.issued_control_rev")?
        && issued_authz_rev == i64_of(cap.issued_authz_rev, "cap.issued_authz_rev")?
        && issuer_authority_rev == i64_of(cap.issuer_authority_rev, "cap.issuer_authority_rev")?;
    if !matches {
        return Err(Error::Invariant("management_caps projection"));
    }
    if revoked_control_rev.is_some() {
        return Err(forbidden());
    }
    if cap.issuer_authority_rev != vault.authority_rev {
        return Err(forbidden());
    }
    if now < i64_of(cap.not_before, "cap.not_before")?
        || now >= i64_of(cap.expires_at, "cap.expires_at")?
    {
        return Err(forbidden());
    }
    if !crate::auth::device_active(conn, &cap.subject_device_id)? {
        return Err(forbidden());
    }
    if vault::device_sign_pk(conn, &cap.subject_device_id)? != Some(cap.subject_ed_pk) {
        return Err(forbidden());
    }
    Ok(cap)
}

fn load_policy(vault: &Vault) -> Result<proto::PolicyV2> {
    let bytes = vault
        .policy_bytes
        .as_ref()
        .ok_or(Error::Invariant("vaults.policy_bytes"))?;
    let policy =
        proto::PolicyV2::decode(bytes).map_err(|_| Error::Invariant("vaults.policy_bytes"))?;
    if policy
        .encode()
        .map_err(|_| Error::Invariant("vaults.policy_bytes"))?
        != *bytes
    {
        return Err(Error::Invariant("vaults.policy_bytes"));
    }
    Ok(policy)
}

pub(super) fn cap_exists(conn: &ScopeConn, cap_id: &Id16) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM management_caps WHERE vault_id = ?1 AND cap_id = ?2",
        (conn.scope().as_slice(), cap_id.as_slice()),
        |row| row.get(0),
    )?;
    Ok(count != 0)
}

// ---------------------------------------------------------------- 效果

/// 按 kind 写投影与 rev，返回提交后的 `(authz_rev, authority_rev)`。
fn apply(conn: &ScopeConn, now: i64, vault: &Vault, rev: u64, plan: Plan) -> Result<(u64, u64)> {
    match plan {
        Plan::Upgrade(plan) => {
            // 先写 cap（复合 FK 指向已追加的 control_commits 行），再原子切 vault 状态。
            for cap in &plan.caps {
                insert_cap(conn, now, rev, 1, 1, cap)?;
            }
            let changed = conn.execute(
                "UPDATE vaults SET authz_version = 2,
                                   authz_rev = 1,
                                   authority_rev = 1,
                                   recovery_pub = ?1,
                                   policy_bytes = ?2,
                                   upgrade_control_rev = ?3,
                                   authz_state = 'active',
                                   auth_rev = auth_rev + 1
                 WHERE vault_id = ?4",
                (
                    plan.recovery_pk.as_slice(),
                    plan.policy_bytes.as_slice(),
                    i64_of(rev, "vaults.upgrade_control_rev")?,
                    conn.scope().as_slice(),
                ),
            )?;
            if changed != 1 {
                return Err(Error::Invariant("vault row missing"));
            }
            Ok((1, 1))
        }
        Plan::CapGrant(cap) => {
            let issued_authz = vault.authz_rev.checked_add(1).ok_or_else(invalid)?;
            insert_cap(conn, now, rev, issued_authz, vault.authority_rev, &cap)?;
            bump_authz_and_auth(conn)?;
            Ok((issued_authz, vault.authority_rev))
        }
        Plan::CapRevoke(cap_id) => {
            let changed = conn.execute(
                "UPDATE management_caps SET revoked_control_rev = ?1
                 WHERE vault_id = ?2 AND cap_id = ?3 AND revoked_control_rev IS NULL",
                (
                    i64_of(rev, "management_caps.revoked_control_rev")?,
                    conn.scope().as_slice(),
                    cap_id.as_slice(),
                ),
            )?;
            if changed != 1 {
                return Err(Error::Invariant("cap revoke projection"));
            }
            let authz = vault.authz_rev.checked_add(1).ok_or_else(invalid)?;
            bump_authz_and_auth(conn)?;
            Ok((authz, vault.authority_rev))
        }
        Plan::Invite(plan) => {
            conn.execute(
                "INSERT INTO invites(vault_id, invite_id, token_hash, expires_at, pending_device,
                                     used_at, issuer_cap_id)
                 VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5)",
                (
                    conn.scope().as_slice(),
                    plan.payload.invite_id.as_slice(),
                    plan.payload.token_hash.as_slice(),
                    i64_of(plan.payload.expires_at, "invites.expires_at")?,
                    plan.issuer_cap_id.as_slice(),
                ),
            )?;
            Ok((vault.authz_rev, vault.authority_rev))
        }
        // ADR 0015 S4b2：设备凭证/撤销/改权的投影与 rev 都在同一事务内完成。
        Plan::Device(plan) => devices::apply(conn, now, vault, rev, plan),
        // ADR 0015 S4c：轮换/激活（复用 v1 对象布局，不换 authority、不普遍撤 cap）。
        Plan::Rotation(plan) => rotation::apply(conn, now, vault, rev, plan),
        Plan::InviteCancel(plan) => {
            // 取消 = 把未兑换邀请投影成"已使用"（`used_at` = 取消时刻、`pending_device` 保持
            // NULL）。兑换投影必须同时写 `pending_device`，因此"已取消"与"已兑换"可区分；
            // `expires_at` 保留原值供审计，控制记录才是永久事实。只推进 control。
            let changed = conn.execute(
                "UPDATE invites SET used_at = ?1
                 WHERE vault_id = ?2 AND invite_id = ?3 AND used_at IS NULL",
                (now, conn.scope().as_slice(), plan.invite_id.as_slice()),
            )?;
            let _ = changed;
            Ok((vault.authz_rev, vault.authority_rev))
        }
        // ADR 0015 S4c：恢复（换 authority 代 + 原子装新代 cap/policy/成员）。
        Plan::Recovery(plan) => recovery::apply(conn, now, vault, rev, plan),
    }
}

pub(super) fn insert_cap(
    conn: &ScopeConn,
    now: i64,
    issued_control_rev: u64,
    issued_authz_rev: u64,
    issuer_authority_rev: u64,
    cap: &proto::ManagementCapV2,
) -> Result<()> {
    let canonical = cap.encode().map_err(|_| invalid())?;
    conn.execute(
        "INSERT INTO management_caps(vault_id, cap_id, canonical_bytes, subject_device,
                                     subject_pk, scopes, not_before, expires_at,
                                     issued_control_rev, issued_authz_rev, issuer_authority_rev,
                                     revoked_control_rev, delegable, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, 0, ?12)",
        (
            conn.scope().as_slice(),
            cap.cap_id.as_slice(),
            canonical.as_slice(),
            cap.subject_device_id.as_slice(),
            cap.subject_ed_pk.as_slice(),
            i64::from(cap.scopes.bits()),
            i64_of(cap.not_before, "management_caps.not_before")?,
            i64_of(cap.expires_at, "management_caps.expires_at")?,
            i64_of(issued_control_rev, "management_caps.issued_control_rev")?,
            i64_of(issued_authz_rev, "management_caps.issued_authz_rev")?,
            i64_of(issuer_authority_rev, "management_caps.issuer_authority_rev")?,
            now,
        ),
    )?;
    Ok(())
}

/// 授权变化：同事务推进 `authz_rev` 与 `auth_rev` 各一次（ADR 0015 §鉴权）。
fn bump_authz_and_auth(conn: &ScopeConn) -> Result<()> {
    let changed = conn.execute(
        "UPDATE vaults SET authz_rev = authz_rev + 1, auth_rev = auth_rev + 1
         WHERE vault_id = ?",
        [conn.scope().as_slice()],
    )?;
    if changed != 1 {
        return Err(Error::Invariant("vault row missing"));
    }
    Ok(())
}

// ---------------------------------------------------------------- 跨模块辅助

/// 供 `pairing::redeem_invite` 使用：V2 邀请绑定的签发 cap 必须仍然有效
/// （cap 撤销/到期/authority 变更 → 未兑换邀请失效）。
pub fn require_effective_cap(
    conn: &ScopeConn,
    clock: &dyn Clock,
    cap_id: &Id16,
) -> Result<proto::ManagementCapV2> {
    effective_cap(conn, clock.now_secs(), cap_id)
}
