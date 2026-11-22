//! ADR 0015 **S4c**：`RotationBegin`（6）/`RotationAbort`（7）/`RotationRestart`（8）/
//! `RotationAddRevokes`（9）/`EpochActivate`（10）。
//!
//! 五个 kind 都走 [`super::submit`] 的同一套链/CAS/时间窗/幂等/rev 绑定，本模块只负责
//! **kind 专属校验与投影**（全部在 handler 的 `BEGIN IMMEDIATE` 事务内）。
//!
//! | kind | payload | 门限 | 效果 |
//! | --- | --- | --- | --- |
//! | 6 Begin | `{0:rotation_id,1:from,2:to,3:revoke_ids[]}` | 2× ROTATE（+每位 REVOKE） | 冻结旧 epoch、吊销、候选 epoch、`frozen`；auth+1 |
//! | 7 Abort | `{0:rotation_id}` | 2× ROTATE | 作废候选（不复活已撤销 epoch） |
//! | 8 Restart | `{0:rotation_id,1:new_to,2:extra_ids[]}` | 2× ROTATE（+每位 REVOKE） | 作废旧候选 + 新候选 + 追加吊销 |
//! | 9 AddRevokes | `{0:rotation_id,1:ids[]}` | 2× ROTATE + 每人 REVOKE | 冻结期追加吊销；已有候选材料 → 409 |
//! | 10 EpochActivate | `{0:bytes(ActivationV2)}` | 2× ROTATE | 重算全部集合、切数据 epoch；auth（成员变更再 authz）+1 |
//!
//! # 复用 v1 的业务事实，不复用 v1 的授权
//!
//! 对象布局/双基线/资产冻结/槽与 grant 投影全部复用 `rotations`/`checkpoints`/`slots`/
//! `objects`/`vault` 的既有实现（v1 rotation 的同一批 DB 事实），但**授权只走 V2 控制
//! 记录**：本模块从不调用 `auth::require_management`/`verify_management`，因此 V2 vault
//! 上不存在"管理钥单签旁路"（v1 管理写在此之前已被 `verify_management` 的 legacy 门禁
//! 关闭）。
//!
//! # 候选不复用 / abort 不复活
//!
//! - 候选 epoch 必须**从未出现在 `epochs` 表**（aborted/retired 也是"已出现"，永不复用）；
//! - `RotationAbort` 只作废候选：删除该候选 epoch 的 staged grants、候选 checkpoint 标
//!   aborted。旧 epoch 只有在**没有**发生过吊销时才解冻（`epochs.revoked_once`），
//!   否则保持 `rotating`（ADR：「abort 不复活」）；
//! - `RotationAddRevokes` 在候选 epoch 已经有材料（staged grant / 候选 checkpoint /
//!   staging 槽）时返回 409：新候选 epoch 必须由**签名**的 `RotationRestart` 指定，
//!   relay 不能自己发明一个未签名的 epoch。
//!
//! # EpochActivate 的集合重算
//!
//! `ActivationV2` 的五个摘要全部由 relay **按 DB 事实重算**后逐字段比对（403/409），
//! 客户端必须取包重算，不能只信 relay 摘要（ADR）：
//!
//! | 字段 | 口径 |
//! | --- | --- |
//! | `slot_set_hash` | `rotations::slot_set_digest`（`SHA256(concat(slot_id \|\| SHA256(descriptor)))`） |
//! | `member_cert_hash` | `SetDigestV2`：`(device_id, devices.certificate)` 全体 ACTIVE |
//! | `grants_hash` | `SetDigestV2`：`(grant_id, 完整 GrantPackageV2 canonical)` |
//! | `old_asset_hash` | `objects::asset_set_digest(from_epoch, freeze_seq)` |
//! | `new_asset_hash` | `objects::candidate_asset_set_digest(to_epoch, rotation_id)` |
//!
//! **只切数据 epoch**：不换恢复 authority、不普遍撤 cap、不改 policy；`authz_rev` 只在
//! 成员集合真的变化时 +1（begin 时把成员摘要写进 `rotations.member_set_hash`，激活时与
//! 当前值比较）。

use rusqlite::OptionalExtension;

use super::{bump_authz_and_auth, forbidden, i64_of, invalid, proto_sha256, two_scope_signers};
use crate::checkpoints;
use crate::error::{Error, ErrorCode, Result};
use crate::ids::{sha256, Hash32, Id16};
use crate::objects;
use crate::rotations::{self, RotationState};
use crate::scope::ScopeConn;
use crate::time::secs;
use crate::vault::{self, Vault};
use menghunote_sync_proto as proto;

/// 已校验、待执行的一条轮换/激活记录。
pub(super) enum RotationPlan {
    Begin(BeginPlan),
    Abort(AbortPlan),
    Restart(RestartPlan),
    AddRevokes(AddRevokesPlan),
    Activate(ActivatePlan),
}

pub(super) struct BeginPlan {
    rotation_id: Id16,
    from_epoch: u64,
    to_epoch: u64,
    revoke_ids: Vec<Id16>,
    freeze_seq: u64,
    asset_set_digest: Hash32,
    /// 冻结时刻的成员集合摘要（激活时据此判断 authz 是否要 +1）。
    member_set_hash: Hash32,
}

pub(super) struct AbortPlan {
    rotation_id: Id16,
    from_epoch: u64,
    to_epoch: u64,
}

pub(super) struct RestartPlan {
    old_rotation_id: Id16,
    new_rotation_id: Id16,
    from_epoch: u64,
    old_to_epoch: u64,
    new_to_epoch: u64,
    freeze_seq: u64,
    asset_set_digest: Option<Hash32>,
    member_set_hash: Hash32,
    extra_ids: Vec<Id16>,
}

pub(super) struct AddRevokesPlan {
    rotation_id: Id16,
    from_epoch: u64,
    device_ids: Vec<Id16>,
}

pub(super) struct ActivatePlan {
    rotation_id: Id16,
    from_epoch: u64,
    to_epoch: u64,
    freeze_seq: u64,
    primary: Id16,
    fallback: Id16,
    activation_commit: Hash32,
    member_set_hash: Hash32,
    grants_digest: Hash32,
    asset_set_digest: Option<Hash32>,
    member_changed: bool,
}

// ---------------------------------------------------------------- 通用校验

/// 当前活动 epoch 的轮换前置条件（v1 `rotations::begin` 的同一语义）。
fn ensure_can_begin(conn: &ScopeConn, vault: &Vault) -> Result<()> {
    if vault.state == vault::VaultState::Initializing {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    if vault.state == vault::VaultState::Rotating {
        // 只允许"上一个候选已经废弃"后重新开始；否则请 resume/activate 现有 rotation。
        match rotations::latest(conn)? {
            Some(rotation) if rotation.state == RotationState::Aborted => {}
            _ => return Err(Error::api(ErrorCode::InvalidState)),
        }
    }
    Ok(())
}

/// 候选 epoch：必须是更高、且**从未分配过**的 epoch（aborted/retired 都算"分配过"）。
fn ensure_fresh_epoch(conn: &ScopeConn, vault: &Vault, epoch: u64) -> Result<()> {
    if epoch <= vault.active_epoch {
        return Err(Error::api(ErrorCode::CasConflict));
    }
    if vault::epoch_state(conn, epoch)?.is_some() {
        // 候选 epoch 永不复用（含 abort 废弃的候选）。
        return Err(Error::api(ErrorCode::CasConflict));
    }
    Ok(())
}

/// 落一个**指定编号**的候选 epoch（`vault::allocate_epoch` 只按 `next_epoch` 递增，
/// V2 的 epoch 由签名 payload 指定）。
fn allocate_specific_epoch(conn: &ScopeConn, epoch: u64, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO epochs(vault_id, epoch, state, created_at) VALUES (?1, ?2, 'staged', ?3)",
        (conn.scope().as_slice(), i64_of(epoch, "epoch")?, now),
    )?;
    // `next_epoch` 只前进不后退（跳号合法：候选 epoch 由客户端指定）。
    conn.execute(
        "UPDATE vaults SET next_epoch = ?1
         WHERE vault_id = ?2 AND next_epoch <= ?3",
        (
            i64_of(epoch.checked_add(1).ok_or_else(invalid)?, "next_epoch")?,
            conn.scope().as_slice(),
            i64_of(epoch, "epoch")?,
        ),
    )?;
    Ok(())
}

fn rotation_row(conn: &ScopeConn, rotation_id: &Id16) -> Result<rotations::RotationRow> {
    rotations::get(conn, rotation_id)
}

fn payload_id(conn: &ScopeConn, rotation_id: &Id16) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM rotations WHERE vault_id = ?1 AND rotation_id = ?2",
        (conn.scope().as_slice(), rotation_id.as_slice()),
        |row| row.get(0),
    )?;
    Ok(count != 0)
}

/// 撤销列表的持久事实：目标必须存在；PENDING/已吊销语义与 kind 5 对齐（PENDING 403、
/// 已 revoked 幂等）；真正的 quorum 守卫交给 [`devices::ensure_revoke_quorum`]。
fn ensure_revocable(conn: &ScopeConn, device_ids: &[Id16]) -> Result<()> {
    for device_id in device_ids {
        let status: Option<String> = conn
            .query_row(
                "SELECT status FROM devices WHERE vault_id = ?1 AND device_id = ?2",
                (conn.scope().as_slice(), device_id.as_slice()),
                |row| row.get(0),
            )
            .optional()?;
        match status.as_deref() {
            None => return Err(Error::api(ErrorCode::NotFound)),
            Some("pending") => return Err(forbidden()),
            Some("active") | Some("revoked") => {}
            Some(_) => return Err(Error::Invariant("devices.status")),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- kind 6：RotationBegin

pub(super) fn validate_begin(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
) -> Result<RotationPlan> {
    ensure_can_begin(conn, vault)?;
    let payload =
        proto::RotationBeginPayloadV2::decode(&signed.operation.payload).map_err(|_| invalid())?;
    if payload.from_epoch != vault.active_epoch {
        return Err(Error::api(ErrorCode::CasConflict));
    }
    ensure_fresh_epoch(conn, vault, payload.to_epoch)?;
    if payload_id(conn, &payload.rotation_id)? {
        return Err(Error::api(ErrorCode::IdConflict));
    }
    let digest_input =
        proto::control_signing_input(&signed.operation, proto_sha256).map_err(|_| invalid())?;
    // 含撤销时**每一位**签名者还必须具备 REVOKE（不能靠两签拼权限）。
    let mut scopes = vec![proto::CapScope::ROTATE];
    if !payload.revoke_ids.is_empty() {
        scopes.push(proto::CapScope::REVOKE);
    }
    two_scope_signers(conn, now, signed, &digest_input, &scopes, &[])?;
    ensure_revocable(conn, &payload.revoke_ids)?;
    if let Some(first) = payload.revoke_ids.first() {
        super::devices::ensure_revoke_quorum(conn, now, vault, &payload.revoke_ids)?;
        let _ = first;
    }
    // §25.1：冻结时刻的资产集合摘要（候选复制以此为准）；冻结水位 = 当前 head_seq。
    let freeze_seq = vault.head_seq;
    let asset_set_digest = objects::asset_set_digest(conn, vault.active_epoch, freeze_seq)?;
    Ok(RotationPlan::Begin(BeginPlan {
        rotation_id: payload.rotation_id,
        from_epoch: payload.from_epoch,
        to_epoch: payload.to_epoch,
        revoke_ids: payload.revoke_ids,
        freeze_seq,
        asset_set_digest,
        member_set_hash: rotations::member_set_hash(conn)?,
    }))
}

// ---------------------------------------------------------------- kind 7：RotationAbort

pub(super) fn validate_abort(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
) -> Result<RotationPlan> {
    let payload =
        proto::RotationAbortPayloadV2::decode(&signed.operation.payload).map_err(|_| invalid())?;
    let rotation = rotation_row(conn, &payload.rotation_id)?;
    if !matches!(
        rotation.state,
        RotationState::Frozen | RotationState::Staging
    ) {
        // 已 activated/aborted 的候选不能再次 abort（候选 EOF 只能前进）。
        return Err(Error::api(ErrorCode::InvalidState));
    }
    let digest_input =
        proto::control_signing_input(&signed.operation, proto_sha256).map_err(|_| invalid())?;
    two_scope_signers(
        conn,
        now,
        signed,
        &digest_input,
        &[proto::CapScope::ROTATE],
        &[],
    )?;
    let _ = vault;
    Ok(RotationPlan::Abort(AbortPlan {
        rotation_id: payload.rotation_id,
        from_epoch: rotation.from_epoch,
        to_epoch: rotation.to_epoch,
    }))
}

// ---------------------------------------------------------------- kind 8：RotationRestart

pub(super) fn validate_restart(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
) -> Result<RotationPlan> {
    if vault.state != vault::VaultState::Rotating {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    let payload = proto::RotationRestartPayloadV2::decode(&signed.operation.payload)
        .map_err(|_| invalid())?;
    let rotation = rotation_row(conn, &payload.rotation_id)?;
    if !matches!(
        rotation.state,
        RotationState::Frozen | RotationState::Staging
    ) {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    if payload.new_to_epoch <= vault.active_epoch || payload.new_to_epoch <= rotation.to_epoch {
        // 新候选必须严格高于旧候选（不复用、不回退）。
        return Err(Error::api(ErrorCode::CasConflict));
    }
    ensure_fresh_epoch(conn, vault, payload.new_to_epoch)?;
    let digest_input =
        proto::control_signing_input(&signed.operation, proto_sha256).map_err(|_| invalid())?;
    let mut scopes = vec![proto::CapScope::ROTATE];
    if !payload.extra_ids.is_empty() {
        scopes.push(proto::CapScope::REVOKE);
    }
    two_scope_signers(conn, now, signed, &digest_input, &scopes, &[])?;
    ensure_revocable(conn, &payload.extra_ids)?;
    if !payload.extra_ids.is_empty() {
        super::devices::ensure_revoke_quorum(conn, now, vault, &payload.extra_ids)?;
    }
    let new_rotation_id = crate::ids::random_id16()?;
    Ok(RotationPlan::Restart(RestartPlan {
        old_rotation_id: rotation.rotation_id,
        new_rotation_id,
        from_epoch: rotation.from_epoch,
        old_to_epoch: rotation.to_epoch,
        new_to_epoch: payload.new_to_epoch,
        freeze_seq: rotation.freeze_seq,
        asset_set_digest: rotation.asset_set_digest,
        member_set_hash: rotations::member_set_hash(conn)?,
        extra_ids: payload.extra_ids,
    }))
}

// ---------------------------------------------------------------- kind 9：RotationAddRevokes

pub(super) fn validate_add_revokes(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
) -> Result<RotationPlan> {
    if vault.state != vault::VaultState::Rotating {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    let payload = proto::RotationAddRevokesPayloadV2::decode(&signed.operation.payload)
        .map_err(|_| invalid())?;
    let rotation = rotation_row(conn, &payload.rotation_id)?;
    if !matches!(
        rotation.state,
        RotationState::Frozen | RotationState::Staging
    ) {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    let digest_input =
        proto::control_signing_input(&signed.operation, proto_sha256).map_err(|_| invalid())?;
    two_scope_signers(
        conn,
        now,
        signed,
        &digest_input,
        &[proto::CapScope::ROTATE, proto::CapScope::REVOKE],
        &[],
    )?;
    ensure_revocable(conn, &payload.device_ids)?;
    super::devices::ensure_revoke_quorum(conn, now, vault, &payload.device_ids)?;
    // 候选 epoch 已有材料时不能继续用同一候选根：新 epoch 必须由**签名的** restart 指定。
    if candidate_material_exists(conn, rotation.to_epoch)? {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    Ok(RotationPlan::AddRevokes(AddRevokesPlan {
        rotation_id: rotation.rotation_id,
        from_epoch: rotation.from_epoch,
        device_ids: payload.device_ids,
    }))
}

/// 候选 epoch 是否已有 staged grant / 候选 checkpoint / staging 槽。
fn candidate_material_exists(conn: &ScopeConn, epoch: u64) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT
           (SELECT COUNT(*) FROM device_grants
             WHERE epoch = ?1 AND vault_id = ?2)
         + (SELECT COUNT(*) FROM checkpoints
             WHERE epoch = ?1 AND state IN ('staging','candidate') AND vault_id = ?2)
         + (SELECT COUNT(*) FROM key_slots
             WHERE epoch = ?1 AND state = 'staging' AND vault_id = ?2)",
        (i64_of(epoch, "epoch")?, conn.scope().as_slice()),
        |row| row.get(0),
    )?;
    Ok(count != 0)
}

// ---------------------------------------------------------------- kind 10：EpochActivate

pub(super) fn validate_activate(
    conn: &ScopeConn,
    now: u64,
    vault: &Vault,
    signed: &proto::SignedControlV2,
) -> Result<RotationPlan> {
    let payload =
        proto::EpochActivatePayloadV2::decode(&signed.operation.payload).map_err(|_| invalid())?;
    let activation = payload.activation;
    let rotation = rotation_row(conn, &activation.rotation_id)?;
    if !matches!(
        rotation.state,
        RotationState::Frozen | RotationState::Staging
    ) {
        // 已 aborted 的候选 epoch 绝不能激活（不得复用已泄露的候选根）。
        return Err(Error::api(ErrorCode::InvalidState));
    }
    if vault::epoch_state(conn, rotation.to_epoch)? != Some(vault::EpochState::Staged) {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    // 上下文绑定：ActivationV2 的 rotation/from/to/S 必须逐字段等于本 vault 的持久事实。
    if activation.from_epoch != rotation.from_epoch
        || activation.from_epoch != vault.active_epoch
        || activation.to_epoch != rotation.to_epoch
        || activation.freeze_seq != rotation.freeze_seq
    {
        return Err(Error::api(ErrorCode::CasConflict));
    }
    let digest_input =
        proto::control_signing_input(&signed.operation, proto_sha256).map_err(|_| invalid())?;
    two_scope_signers(
        conn,
        now,
        signed,
        &digest_input,
        &[proto::CapScope::ROTATE],
        &[],
    )?;
    // §28.1 条目 2–3：只 promote 被指名的两个 cid（独立性/覆盖 S/完整性都在这里校验）。
    let pair = checkpoints::promote_named_candidates(
        conn,
        rotation.to_epoch,
        &activation.primary_cid,
        &activation.fallback_cid,
        rotation.freeze_seq,
        i64_of(now, "now")?,
    )?;
    checkpoints::schedule_plan_for_pair(
        conn,
        i64_of(now, "now")?,
        rotation.to_epoch,
        pair.cut_seq,
        &pair.proof,
    )?;
    // 双基线 commit hash 必须等于 relay 存的 signed_commit 摘要。
    if activation.primary_commit_hash != commit_digest(&pair.primary)?
        || activation.fallback_commit_hash != commit_digest(&pair.fallback)?
    {
        return Err(Error::api(ErrorCode::CasConflict));
    }
    // 全成员 grant：每个 ACTIVE 设备都必须有候选 epoch 的包。
    let missing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM devices d
         WHERE d.status = 'active' AND d.vault_id = ?2
           AND NOT EXISTS (SELECT 1 FROM device_grants g
                           WHERE g.epoch = ?1 AND g.device_id = d.device_id
                             AND g.vault_id = d.vault_id)",
        (i64_of(rotation.to_epoch, "epoch")?, conn.scope().as_slice()),
        |row| row.get(0),
    )?;
    if missing != 0 {
        return Err(Error::api(ErrorCode::Incomplete));
    }
    // §8 步 5：候选 epoch 必须已发布新槽（password + recovery 各至少一个 staging）。
    if !crate::slots::rotation_slots_ready(conn, rotation.to_epoch)?.is_empty() {
        return Err(Error::api(ErrorCode::Incomplete));
    }
    // 五个集合摘要：relay 按 DB 事实重算，逐字段比对（客户端必须取包重算）。
    let actual_slots = rotations::slot_set_digest(conn, rotation.to_epoch)?;
    let actual_members = member_cert_set_digest(conn)?;
    let (_, actual_grants) = grant_set_digest(conn, rotation.to_epoch)?;
    let actual_old_assets = rotation
        .asset_set_digest
        .ok_or_else(|| Error::api(ErrorCode::CasConflict))?;
    let actual_new_assets =
        objects::candidate_asset_set_digest(conn, rotation.to_epoch, &rotation.rotation_id)?;
    let mismatched = activation.slot_set_hash != actual_slots
        || activation.member_cert_hash != actual_members
        || activation.grants_hash != actual_grants
        || activation.old_asset_hash != actual_old_assets
        || activation.new_asset_hash != actual_new_assets;
    if mismatched {
        // 集合被替换/掉包 → 409（客户端必须重新准备并重签）。
        return Err(Error::api(ErrorCode::CasConflict));
    }
    let current_members = rotations::member_set_hash(conn)?;
    // begin 时记录的成员摘要为空（v1 遗留行）时保守地按"成员已变化"处理。
    let member_changed = rotation.member_set_hash != Some(current_members);
    let activation_commit = sha256(&activation.encode().map_err(|_| invalid())?);
    Ok(RotationPlan::Activate(ActivatePlan {
        rotation_id: rotation.rotation_id,
        from_epoch: rotation.from_epoch,
        to_epoch: rotation.to_epoch,
        freeze_seq: rotation.freeze_seq,
        primary: pair.primary.checkpoint_id,
        fallback: pair.fallback.checkpoint_id,
        activation_commit,
        member_set_hash: current_members,
        grants_digest: actual_grants,
        asset_set_digest: rotation.asset_set_digest,
        member_changed,
    }))
}

/// `SHA256(signed_commit)`：双基线 commit hash 的公共口径（v1 `ActivationTFacts` 同口径）。
fn commit_digest(checkpoint: &checkpoints::CheckpointRow) -> Result<Hash32> {
    let signed = checkpoint
        .signed_commit
        .as_deref()
        .ok_or_else(|| Error::api(ErrorCode::CasConflict))?;
    Ok(sha256(signed))
}

// ---------------------------------------------------------------- 集合摘要

/// 成员证书集合摘要（`SetDigestV2`：id = device_id，bytes = `devices.certificate` 全字节）。
///
/// ADR 要求「成员含完整 `DeviceCertV2`」：这里取每台 ACTIVE 设备的**当前证书字节**
/// （V2 证书或升级前的 `DeviceCertV1` 都是 canonical CDE，客户端可用 `/control` 的
/// members 字段逐字节复算）。
pub fn member_cert_set_digest(conn: &ScopeConn) -> Result<Hash32> {
    let mut stmt = conn.prepare(
        "SELECT device_id, certificate FROM devices
         WHERE vault_id = ?1 AND status = 'active' ORDER BY device_id ASC",
    )?;
    let rows = stmt.query_map([conn.scope().as_slice()], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut entries: Vec<proto::SetDigestEntryV2> = Vec::new();
    for row in rows {
        let (device_id, certificate) = row?;
        entries.push((
            device_id
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("devices.device_id"))?,
            certificate,
        ));
    }
    proto::set_digest_v2(&entries, proto_sha256).map_err(|_| Error::Invariant("member cert set"))
}

/// 某 epoch 的 grant 集合摘要（`SetDigestV2`：id = grant_id，bytes = 完整 `GrantPackageV2`）。
///
/// 返回 `(数量, 摘要)`；`grant_id` 缺失/与 AAD 不一致 → 不变量错误（投影必须可复算）。
pub fn grant_set_digest(conn: &ScopeConn, epoch: u64) -> Result<(u64, Hash32)> {
    let mut stmt = conn.prepare(
        "SELECT hpke_enc, hpke_ciphertext, signed_header, grant_id FROM device_grants
         WHERE vault_id = ?1 AND epoch = ?2 ORDER BY epoch ASC, device_id ASC",
    )?;
    let rows = stmt.query_map((conn.scope().as_slice(), i64_of(epoch, "epoch")?), |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, Option<Vec<u8>>>(3)?,
        ))
    })?;
    let mut entries: Vec<proto::SetDigestEntryV2> = Vec::new();
    for row in rows {
        let (enc, ciphertext, signed_header, grant_id) = row?;
        // v1 时代的 grant 行（`signed_header` 是 `AAD||enc||hash` 裸拼接、`grant_id` 可能为
        // NULL）不是 V2 grant 包，不能进 `SetDigestV2`（id 必须是 grant_id）。跳过它们：
        // 候选 epoch 的包全部由 V2 路径写入，客户端也只会拿到 V2 包，因此两边口径一致。
        let Ok(aad) = proto::GrantAadV2::decode(&signed_header) else {
            continue;
        };
        if aad.aad_bytes().map_err(|_| Error::Invariant("grant aad"))? != signed_header {
            continue;
        }
        if grant_id.as_deref() != Some(aad.grant_id.as_slice()) {
            return Err(Error::Invariant("device_grants.grant_id"));
        }
        let package = proto::GrantPackageV2 {
            aad,
            enc: enc
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("device_grants.hpke_enc"))?,
            ciphertext,
        };
        let bytes = package
            .encode()
            .map_err(|_| Error::Invariant("grant package encode"))?;
        entries.push((package.aad.grant_id, bytes));
    }
    let count = u64::try_from(entries.len()).map_err(|_| Error::Invariant("grant count"))?;
    let digest =
        proto::set_digest_v2(&entries, proto_sha256).map_err(|_| Error::Invariant("grant set"))?;
    Ok((count, digest))
}

// ---------------------------------------------------------------- 效果

pub(super) fn apply(
    conn: &ScopeConn,
    now: i64,
    vault: &Vault,
    rev: u64,
    plan: RotationPlan,
) -> Result<(u64, u64)> {
    match plan {
        RotationPlan::Begin(plan) => {
            // §24.1①：freeze 本身必须让全部旧 token 失效（即使 revoke 为空）。
            vault::bump_auth_rev(conn)?;
            let mut revoked_count = 0u64;
            for device_id in &plan.revoke_ids {
                if crate::auth::revoke_device(conn, now, device_id)? {
                    revoked_count += 1;
                }
            }
            // §23.1：该旧 epoch 冻结期间发生过吊销 → 永久事实，abort 绝不能解冻。
            if revoked_count > 0 {
                vault::mark_epoch_revoked_once(conn, plan.from_epoch)?;
            }
            allocate_specific_epoch(conn, plan.to_epoch, now)?;
            conn.execute(
                "INSERT INTO rotations(vault_id, rotation_id, from_epoch, to_epoch, freeze_seq,
                                       member_set_hash, asset_set_digest, revoked_count, state,
                                       created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'frozen', ?9, ?9)",
                (
                    conn.scope().as_slice(),
                    plan.rotation_id.as_slice(),
                    i64_of(plan.from_epoch, "from_epoch")?,
                    i64_of(plan.to_epoch, "to_epoch")?,
                    i64_of(plan.freeze_seq, "freeze_seq")?,
                    plan.member_set_hash.as_slice(),
                    plan.asset_set_digest.as_slice(),
                    i64_of(revoked_count, "revoked_count")?,
                    now,
                ),
            )?;
            vault::set_state(conn, vault::VaultState::Rotating)?;
            let _ = rev;
            // 纯 rotation：只推进 control（ADR：邀请/纯 rotation 只推进 control）。
            Ok((vault.authz_rev, vault.authority_rev))
        }
        RotationPlan::Abort(plan) => {
            // 废弃候选 epoch 与它的全部 staged grants（根已可能泄露 → 永不复用）。
            conn.execute(
                "DELETE FROM device_grants WHERE epoch = ?1 AND vault_id = ?",
                (i64_of(plan.to_epoch, "epoch")?, conn.scope().as_slice()),
            )?;
            checkpoints::abort_candidates(conn, plan.to_epoch)?;
            vault::set_epoch_state(conn, plan.to_epoch, vault::EpochState::Aborted)?;
            let changed = conn.execute(
                "UPDATE rotations SET state = 'aborted', updated_at = ?1
                 WHERE rotation_id = ?2 AND vault_id = ?
                   AND state IN ('frozen','staging')",
                (now, plan.rotation_id.as_slice(), conn.scope().as_slice()),
            )?;
            if changed != 1 {
                return Err(Error::Invariant("rotation abort projection"));
            }
            conn.execute(
                "UPDATE maintenance_reservations SET state = 'expired'
                 WHERE purpose = 'rotation' AND resource_id = ?1 AND state = 'active'
                   AND vault_id = ?",
                (plan.rotation_id.as_slice(), conn.scope().as_slice()),
            )?;
            // 关键：吊销过设备的轮换失败后**不能**回旧 epoch 继续写；只有未吊销才解冻。
            if !vault::epoch_revoked_once(conn, plan.from_epoch)? {
                vault::set_state(conn, vault::VaultState::Active)?;
            }
            Ok((vault.authz_rev, vault.authority_rev))
        }
        RotationPlan::Restart(plan) => {
            for device_id in &plan.extra_ids {
                if crate::auth::revoke_device(conn, now, device_id)? {
                    // 追加吊销同样让旧 epoch 永久不能解冻。
                    vault::mark_epoch_revoked_once(conn, plan.from_epoch)?;
                }
            }
            discard_candidate(conn, now, &plan)?;
            Ok((vault.authz_rev, vault.authority_rev))
        }
        RotationPlan::AddRevokes(plan) => {
            let mut revoked = 0u64;
            for device_id in &plan.device_ids {
                if crate::auth::revoke_device(conn, now, device_id)? {
                    revoked += 1;
                }
            }
            if revoked > 0 {
                vault::mark_epoch_revoked_once(conn, plan.from_epoch)?;
            }
            conn.execute(
                "UPDATE rotations SET revoked_count = revoked_count + ?1, updated_at = ?2
                 WHERE rotation_id = ?3 AND vault_id = ?",
                (
                    i64_of(revoked, "revoked_count")?,
                    now,
                    plan.rotation_id.as_slice(),
                    conn.scope().as_slice(),
                ),
            )?;
            Ok((vault.authz_rev, vault.authority_rev))
        }
        RotationPlan::Activate(plan) => activate(conn, now, vault, rev, plan),
    }
}

/// §8 步 8：原子废弃当前候选 epoch 并分配更高 epoch（保持冻结）。
fn discard_candidate(conn: &ScopeConn, now: i64, plan: &RestartPlan) -> Result<()> {
    conn.execute(
        "DELETE FROM device_grants WHERE epoch = ?1 AND vault_id = ?",
        (i64_of(plan.old_to_epoch, "epoch")?, conn.scope().as_slice()),
    )?;
    checkpoints::abort_candidates(conn, plan.old_to_epoch)?;
    vault::set_epoch_state(conn, plan.old_to_epoch, vault::EpochState::Aborted)?;
    conn.execute(
        "UPDATE rotations SET state = 'aborted', updated_at = ?1
         WHERE rotation_id = ?2 AND state IN ('frozen','staging') AND vault_id = ?",
        (
            now,
            plan.old_rotation_id.as_slice(),
            conn.scope().as_slice(),
        ),
    )?;
    allocate_specific_epoch(conn, plan.new_to_epoch, now)?;
    conn.execute(
        "INSERT INTO rotations(vault_id, rotation_id, from_epoch, to_epoch, freeze_seq,
                               member_set_hash, asset_set_digest, revoked_count, state,
                               created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 'frozen', ?8, ?8)",
        (
            conn.scope().as_slice(),
            plan.new_rotation_id.as_slice(),
            i64_of(plan.from_epoch, "from_epoch")?,
            i64_of(plan.new_to_epoch, "to_epoch")?,
            i64_of(plan.freeze_seq, "freeze_seq")?,
            plan.member_set_hash.as_slice(),
            plan.asset_set_digest
                .as_ref()
                .map(|digest| digest.as_slice()),
            now,
        ),
    )?;
    conn.execute(
        "UPDATE maintenance_reservations SET state = 'expired'
         WHERE purpose = 'rotation' AND resource_id = ?1 AND state = 'active'
           AND vault_id = ?",
        (plan.old_rotation_id.as_slice(), conn.scope().as_slice()),
    )?;
    Ok(())
}

/// 原子激活：publish 候选 → 切槽/epoch → 控制事实与 rev（**不换 authority、不撤 cap**）。
fn activate(
    conn: &ScopeConn,
    now: i64,
    vault: &Vault,
    rev: u64,
    plan: ActivatePlan,
) -> Result<(u64, u64)> {
    // key slot：旧 active 进入 retiring（14 天），候选 epoch 的 staging 槽上线。
    conn.execute(
        "UPDATE key_slots SET state = 'retiring', retire_at = ?1
         WHERE state = 'active' AND vault_id = ?",
        (now + secs::OLD_SLOT_RETENTION, conn.scope().as_slice()),
    )?;
    conn.execute(
        "UPDATE key_slots SET state = 'active', slot_set_rev = slot_set_rev + 1
         WHERE state = 'staging' AND epoch = ?1 AND vault_id = ?",
        (i64_of(plan.to_epoch, "epoch")?, conn.scope().as_slice()),
    )?;
    // epoch 切换（同一事务：旧 retired、新 active）。
    vault::set_epoch_state(conn, plan.from_epoch, vault::EpochState::Retired)?;
    vault::set_epoch_retire_at(conn, plan.from_epoch, now + secs::OLD_SLOT_RETENTION)?;
    vault::set_epoch_state(conn, plan.to_epoch, vault::EpochState::Active)?;
    conn.execute(
        "UPDATE rotations SET state = 'activated', member_set_hash = ?1,
                             primary_checkpoint = ?2, fallback_checkpoint = ?3,
                             activation_commit = ?4, activated_at = ?5,
                             activated_control_rev = ?6, updated_at = ?5
         WHERE rotation_id = ?7 AND vault_id = ?8",
        (
            plan.member_set_hash.as_slice(),
            plan.primary.as_slice(),
            plan.fallback.as_slice(),
            plan.activation_commit.as_slice(),
            now,
            i64_of(rev, "activated_control_rev")?,
            plan.rotation_id.as_slice(),
            conn.scope().as_slice(),
        ),
    )?;
    conn.execute(
        "UPDATE vaults SET active_epoch = ?1, state = 'active' WHERE vault_id = ?2",
        (i64_of(plan.to_epoch, "epoch")?, conn.scope().as_slice()),
    )?;
    // 激活后立即让全部旧 token 失效：客户端必须重新认证并重新取 grant。
    let authz_rev = if plan.member_changed {
        bump_authz_and_auth(conn)?;
        vault.authz_rev.checked_add(1).ok_or_else(invalid)?
    } else {
        vault::bump_auth_rev(conn)?;
        vault.authz_rev
    };
    // 轮换 reservation 完成（剩余维护额度归还；已写数据保留）。
    conn.execute(
        "UPDATE maintenance_reservations SET state = 'complete'
         WHERE purpose = 'rotation' AND resource_id = ?1 AND state = 'active'
           AND vault_id = ?",
        (plan.rotation_id.as_slice(), conn.scope().as_slice()),
    )?;
    // 激活后按真实 parts 对账（新 pair 升为 normal，旧 epoch 宽限/退役槽降 maintenance）。
    crate::quota::recompute(conn)?;
    let _ = (plan.freeze_seq, plan.grants_digest, plan.asset_set_digest);
    Ok((authz_rev, vault.authority_rev))
}
