//! P3-I/J 的纯状态机：epoch 变化检测、激活 commit 校验、restart 判定。
//!
//! 这里**不写 storage**；baseline merge 由 storage task-108 提供事务 API 后接入。
//!
//! I 阶段的两件事是**分开**的（评审 0003 P1-C）：
//! - [`verify_activation_t_gate`]：T/latest grant 授权"把根推广到哪个 epoch"（双签、链、
//!   grant inclusion、pair 内 commit digest 强绑定）；
//! - [`verify_checkpoint_authorization`]：认证"当前这份 checkpoint 内容"（生产者资格/
//!   签名/epoch/manifest/refs/S 边界），从而支持激活 pair 被普通健康 checkpoint 替换后
//!   的旧设备消费。

use crate::device::verify_signature;
use crate::error::SyncError;
use crate::hpke::x25519_public_key;
use crate::identity::{IdentityRotationStateV1, LocalIdentityV1};
use crate::keys::{RootSecretV1, Secret32};
pub use crate::relay::GrantLeafEvidence;
use crate::relay::{
    BootstrapBundle, BootstrapCheckpoint, CheckpointStatus, EpochActivationLink, RelayClient,
    RelayState, RotationView, SelfGrantEvidence, VerifiedActivationT,
};
use crate::rotation_activate::AuthenticatedPinV1;
use crate::rotation_journal::{RotationApplyPhase, RotationApplyReceipt};
use crate::rotation_slots::signed_commit_digest;
use crate::rotation_stage::verify_snapshot_integrity;
use crate::upload::{decrypt_envelope, fetch_object};
use crate::{DeviceId, ObjectId, VaultId};
use menghunote_storage::{
    abort_baseline_apply, apply_baseline_assets, apply_baseline_batch, baseline_apply_state,
    begin_baseline_apply, finish_baseline_apply, repack_required, BaselineApplyFinish,
    BaselineApplyState, BaselineApplyStatus, BaselineAsset,
};
use menghunote_sync_proto::{
    activation_t_signing_input, checkpoint_signing_input, grant_package_digest, ActivationCommitV1,
    ActivationTV1, CheckpointCommit, Envelope, GrantAadV1, ManifestV1, ObjectType, SnapshotV1,
};
use sha2::{Digest, Sha256};

/// 已验证、尚未写库的基线内容（下载阶段只读）。
#[derive(Debug, Clone)]
pub struct BaselinePayload {
    pub checkpoint_id: [u8; 16],
    pub baseline_s: u64,
    pub docs: Vec<(String, Vec<u8>)>,
    pub assets: Vec<BaselineAsset>,
}

/// 完整 gate 产出的 apply 输入（评审 0003 P1-C）。
///
/// [`BaselinePayload`] 是**公开可变**的下载/合并数据，本身不构成授权；本类型把
/// 它与 T/`ActivationCommitV1`/选中 checkpoint/推广材料逐字段绑定，字段**私有**、
/// 只有只读访问器，且构造只发生在 [`crate::rotation_apply::apply_epoch_change_with_client`]
/// 通过 [`verify_activation_t_gate`] + manifest/候选根校验之后。
///
/// 因此纯入口 [`apply_epoch_change`] 只消费本类型，外部 crate 无法用一份改过的
/// `BaselinePayload` 拼出它（编译期 API 层阻断），也就不存在"绕过 T 门禁的公开入口"。
///
/// 外部 crate 无法构造本类型（字段私有 + 构造函数 `pub(crate)`）：
///
/// ```compile_fail
/// use menghunote_sync::rotation_apply::{BaselinePayload, VerifiedApplyPayload};
/// let _ = VerifiedApplyPayload {
///     rotation_id: [0u8; 16],
///     checkpoint_id: [0u8; 16],
///     baseline_s: 0,
///     baseline_cursor: String::new(),
///     from_epoch: 0,
///     to_epoch: 1,
///     control_rev: 0,
///     activation_hash: [0u8; 32],
///     instance_id: [0u8; 16],
///     new_manager_public: [0u8; 32],
///     new_manager_rev: 0,
///     baseline: BaselinePayload {
///         checkpoint_id: [0u8; 16],
///         baseline_s: 0,
///         docs: Vec::new(),
///         assets: Vec::new(),
///     },
/// };
/// ```
///
/// 通过只读访问器也改不了 gate 已经绑定的基线字段：
///
/// ```compile_fail
/// fn mutate(value: &menghunote_sync::rotation_apply::VerifiedApplyPayload) {
///     value.baseline().checkpoint_id = [0u8; 16];
/// }
/// ```
pub struct VerifiedApplyPayload {
    rotation_id: [u8; 16],
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    baseline_cursor: String,
    from_epoch: u64,
    to_epoch: u64,
    control_rev: u64,
    activation_hash: [u8; 32],
    instance_id: [u8; 16],
    new_manager_public: [u8; 32],
    new_manager_rev: u64,
    baseline: BaselinePayload,
}

impl VerifiedApplyPayload {
    /// 只能由完整 gate 之后的本模块调用（`pub(crate)`，外部不可见）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_gate(
        rotation_id: [u8; 16],
        checkpoint_id: [u8; 16],
        baseline_s: u64,
        baseline_cursor: &str,
        from_epoch: u64,
        to_epoch: u64,
        control_rev: u64,
        activation_hash: [u8; 32],
        instance_id: [u8; 16],
        new_manager_public: [u8; 32],
        new_manager_rev: u64,
        baseline: BaselinePayload,
    ) -> Self {
        Self {
            rotation_id,
            checkpoint_id,
            baseline_s,
            baseline_cursor: baseline_cursor.to_owned(),
            from_epoch,
            to_epoch,
            control_rev,
            activation_hash,
            instance_id,
            new_manager_public,
            new_manager_rev,
            baseline,
        }
    }

    /// 本次 activation 的 rotation id。
    #[must_use]
    pub fn rotation_id(&self) -> [u8; 16] {
        self.rotation_id
    }

    /// 选中的基线 checkpoint id。
    #[must_use]
    pub fn checkpoint_id(&self) -> [u8; 16] {
        self.checkpoint_id
    }

    /// 选中基线覆盖的 relay S。
    #[must_use]
    pub fn baseline_s(&self) -> u64 {
        self.baseline_s
    }

    /// 选中基线的 relay cursor。
    #[must_use]
    pub fn baseline_cursor(&self) -> &str {
        &self.baseline_cursor
    }

    /// 本地可信起点 epoch。
    #[must_use]
    pub fn from_epoch(&self) -> u64 {
        self.from_epoch
    }

    /// 推广目标 epoch。
    #[must_use]
    pub fn to_epoch(&self) -> u64 {
        self.to_epoch
    }

    /// 激活控制链 rev。
    #[must_use]
    pub fn control_rev(&self) -> u64 {
        self.control_rev
    }

    /// `SHA256(CDE(ActivationCommitV1))`。
    #[must_use]
    pub fn activation_hash(&self) -> [u8; 32] {
        self.activation_hash
    }

    /// 已验证的 relay instance。
    #[must_use]
    pub fn instance_id(&self) -> [u8; 16] {
        self.instance_id
    }

    /// 推广后的管理公钥。
    #[must_use]
    pub fn new_manager_public(&self) -> [u8; 32] {
        self.new_manager_public
    }

    /// 推广后的管理 key rev。
    #[must_use]
    pub fn new_manager_rev(&self) -> u64 {
        self.new_manager_rev
    }

    /// 已验证的基线内容（只读；不可再被调用方改写）。
    #[must_use]
    pub fn baseline(&self) -> &BaselinePayload {
        &self.baseline
    }

    fn receipt(&self, identity: &LocalIdentityV1) -> RotationApplyReceipt {
        // receipt 的 `from_epoch` 记**本地 apply 起点**（promotion 前 identity epoch），
        // 不是链尾 T 的 from_epoch：离线跨多 epoch 时两者不同，收尾时要用前者对
        // `IdentityRotationStateV1.previous_roots`。
        RotationApplyReceipt::new(
            identity.vault_id,
            identity.device_id,
            self.rotation_id,
            self.checkpoint_id,
            self.baseline_s,
            &self.baseline_cursor,
            identity.current_epoch,
            self.to_epoch,
            self.control_rev,
            self.instance_id,
            self.new_manager_public,
            self.new_manager_rev,
            self.activation_hash,
        )
    }
}

/// 下载并验证一份候选基线；不写 Vault。
pub fn collect_baseline_payload<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    report: &crate::checkpoint::CheckpointReport,
) -> Result<BaselinePayload, SyncError> {
    let mut docs = Vec::with_capacity(report.docs.len());
    for doc in &report.docs {
        let (_, bytes) = fetch_object(
            client,
            root,
            ObjectType::Snapshot,
            &doc.object.object_id,
            doc.object.part_count,
            doc.object.total_plain_len,
            &doc.object.object_digest,
            &[],
        )?;
        let snapshot = SnapshotV1::decode(&bytes)?;
        // 对象摘要/分片数/明文长度已由 `fetch_object` 按 MNE1 规则校验；这里再验
        // manifest 字段与**内层** Loro 数据（full-history + 实际 VV == 声明 VV）。
        verify_snapshot_integrity(&snapshot)?;
        if snapshot.doc_id != doc.doc_id
            || snapshot.kind != doc.kind
            || snapshot.generation != doc.generation
            || snapshot.vv != doc.vv
            || snapshot.frontier != doc.frontier
            || !snapshot.full_history
        {
            return Err(SyncError::Object("基线快照不是 full-history/字段不符"));
        }
        // storage 的 `apply_baseline_batch` 最终走 `BodyDoc::restore(..., Some(snapshot))`，
        // 需要的是**内层 Loro snapshot 字节**；`SnapshotV1` 的 CDE 外层不能直接导入
        // （评审 0002 P1-1.3：这里曾把整块 CDE 推进 docs）。
        if snapshot.loro_snapshot_bytes.is_empty() {
            return Err(SyncError::Object("基线快照内层 Loro 数据为空"));
        }
        docs.push((snapshot.doc_id, snapshot.loro_snapshot_bytes));
    }
    let mut assets = Vec::with_capacity(report.assets.len());
    for asset in &report.assets {
        let (_, payload) = fetch_object(
            client,
            root,
            ObjectType::Asset,
            &asset.object.object_id,
            asset.object.part_count,
            asset.object.total_plain_len,
            &asset.object.object_digest,
            &[],
        )?;
        let (real, byte_len, media_type) = crate::rotation_stage::decode_asset_plaintext(&payload)?;
        if real != asset.real_sha256 || byte_len != asset.byte_len || media_type != asset.media_type
        {
            return Err(SyncError::Object("基线资产明文与 manifest 不符"));
        }
        assets.push(BaselineAsset {
            object_id: asset.object.object_id,
            asset_hash: real,
            byte_len,
            media_type: Some(media_type),
            data: crate::rotation_stage::payload_asset_data(&payload)?.to_vec(),
        });
    }
    Ok(BaselinePayload {
        checkpoint_id: report.checkpoint_id,
        baseline_s: report.covered_seq,
        docs,
        assets,
    })
}

/// 分批 merge 已验证基线；全部成功才 `finish` 推进 cursor，失败/冲突 abort 且不推 cursor。
pub fn apply_verified_baseline(
    vault: &menghunote_storage::Vault,
    payload: &BaselinePayload,
    baseline_cursor: &str,
) -> Result<BaselineApplyState, SyncError> {
    begin_baseline_apply(vault, &payload.checkpoint_id, payload.baseline_s, "")?;
    let result = (|| {
        for chunk in payload.docs.chunks(64) {
            apply_baseline_batch(vault, chunk, &[])?;
        }
        if !payload.assets.is_empty() {
            apply_baseline_assets(vault, &payload.assets)?;
        }
        match finish_baseline_apply(vault, payload.baseline_s, baseline_cursor)? {
            BaselineApplyFinish::Applied(state) => Ok(state),
            BaselineApplyFinish::NotFound => Err(SyncError::Object("baseline apply journal 丢失")),
            BaselineApplyFinish::Conflict => {
                Err(SyncError::Object("baseline cursor CAS 冲突，未推进"))
            }
        }
    })();
    if result.is_err() {
        let _ = abort_baseline_apply(vault);
    }
    result
}

/// merge 已提交后：**先写 receipt，再推广身份**。
///
/// receipt 必须早于 rename：这样 rename 成功后、`applied` 写前崩溃时，重启能凭
/// receipt + 推广后的 identity/pin 做认证收尾；反过来（先 rename 后写 receipt）则会
/// 留下"新 identity + 无 receipt"的永久冻结窗口。
///
/// 失败语义：merge 失败不写 receipt；receipt 写失败不推广身份；推广失败保留 receipt
/// 但本地身份仍是旧 epoch（重试走正常 epoch change，不会被误判为已完成）。
fn apply_verified_and_promote(
    vault: &menghunote_storage::Vault,
    identity_path: &std::path::Path,
    identity: &LocalIdentityV1,
    verified: &VerifiedApplyPayload,
    candidate_root: &RootSecretV1,
) -> Result<BaselineApplyState, SyncError> {
    let state = apply_verified_baseline(vault, verified.baseline(), verified.baseline_cursor())?;
    verified.receipt(identity).store(vault)?;
    let authenticated = crate::rotation_activate::AuthenticatedActivation {
        instance_id: verified.instance_id(),
        control_rev: verified.control_rev(),
        activation_hash: verified.activation_hash(),
    };
    let material = crate::rotation_activate::PromotionMaterial {
        candidate_root: candidate_root.clone(),
        new_manager_seed: None,
        new_manager_public: verified.new_manager_public(),
        new_manager_rev: verified.new_manager_rev(),
        control_rev: verified.control_rev(),
        authentication: Some(authenticated),
    };
    crate::rotation_activate::promote_identity_with_material(
        vault,
        identity_path,
        identity,
        &material,
    )?;
    Ok(state)
}

/// B 侧应用 epoch 变化的业务结果；非 `Applied` 一律不得推 cursor/推广/恢复 push。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// 合并成功后才可能推广；`promoted=false` 表示只完成了 merge（无身份可推广）。
    Applied { baseline_s: u64, promoted: bool },
    /// 保持暂停，等待重试（网络/验证/merge 失败）。
    Paused { reason: String },
    /// 必须走更高 epoch 的 restart。
    NeedsRestart {
        to_epoch: u64,
        reason: crate::rotation_apply::RestartReason,
    },
    /// baseline cursor CAS 冲突；不推 cursor。
    Conflict,
}

/// 纯入口：消费完整 gate 产出的 [`VerifiedApplyPayload`]，完成 I 阶段收尾。
///
/// **不再接受公开可变的 [`BaselinePayload`] 作为授权输入**（评审 0003 P1-C）：
/// - T 双签/T-view-实例绑定、选中 checkpoint 的 `signed_commit` digest/生产者签名、
///   S/cursor、self grant inclusion 全部由网络入口在构造 [`VerifiedApplyPayload`]
///   之前完成；
/// - 本类型字段私有、构造 `pub(crate)`，外部 crate 无法伪造（见 `compile_fail` doctest），
///   因此本函数等价于 crate 内部/测试 helper，不存在"跳过 gate 的公开入口"。
///
/// 本函数负责阶段 I 的持久收尾：`begin_apply`（同一事务清旧 receipt + 落 `applying`）
/// → merge → 写 typed receipt → rename 推广 → `applied`。不做 epoch 检测与 restart 判定
/// （那是网络入口的职责）。
pub fn apply_epoch_change(
    vault: &menghunote_storage::Vault,
    identity_path: &std::path::Path,
    identity: &LocalIdentityV1,
    verified: &VerifiedApplyPayload,
    candidate_root: &RootSecretV1,
) -> Result<ApplyOutcome, SyncError> {
    // 候选根必须正是本次 activation 的目标 epoch：否则 merge 会写进错误 epoch 的根，
    // 身份推广后也无法用该根解开新 outbox。
    if candidate_root.key_epoch != verified.to_epoch() {
        return Ok(ApplyOutcome::Paused {
            reason: format!(
                "候选根 epoch={} 与 activation to_epoch={} 不符",
                candidate_root.key_epoch,
                verified.to_epoch()
            ),
        });
    }
    // 阶段 I 的 durable 冻结屏障：同一事务清旧 receipt + 落 applying，必须在任何
    // merge/写库之前完成。
    RotationApplyPhase::begin_apply(vault)?;
    match apply_verified_and_promote(vault, identity_path, identity, verified, candidate_root) {
        Ok(state) => {
            // 只有 merge + promotion 都成功才允许转 applied（engine 据此解冻）。
            RotationApplyPhase::Applied.store(vault)?;
            Ok(ApplyOutcome::Applied {
                baseline_s: state.baseline_s,
                promoted: true,
            })
        }
        Err(error) => {
            // 失败保持冻结：merge 可能已提交、身份可能尚未推广，绝不能让旧写继续。
            let message = error.to_string();
            if message.contains("CAS") || message.contains("冲突") {
                RotationApplyPhase::Conflict.store(vault)?;
                Ok(ApplyOutcome::Conflict)
            } else {
                RotationApplyPhase::Paused.store(vault)?;
                Ok(ApplyOutcome::Paused { reason: message })
            }
        }
    }
}

fn reconcile_paused(reason: impl Into<String>) -> Option<ApplyOutcome> {
    Some(ApplyOutcome::Paused {
        reason: reason.into(),
    })
}

/// same-epoch 重入：凭 durable receipt 做**认证后的完成 reconcile**（评审 0003 P1-D）。
///
/// 触发条件：`sync.rotation_apply_phase == applying`。此时本地身份可能已经完成
/// rename（`identity.current_epoch == relay.epoch`），网络入口看不到 epoch change；
/// RAM 里的 freeze 也会随进程消失。收尾必须由 durable 事实认证，不能"看到 applying
/// 就无条件清 phase"：
///
/// 1. receipt 必须存在且可解析（损坏/缺失 → `Paused`，保持冻结）；
/// 2. receipt 必须与内存身份、**磁盘身份**（重新解封比对 epoch/control_rev/pin/root）
///    和 `AuthenticatedPinV1` 逐字段一致；
/// 3. 身份旁路状态、baseline apply journal（completed + cid/S/cursor）也必须一致；
/// 4. 必须仍处于 `sync.repack_required=1` 的重包屏障（推广在 rename 前写入）。
///
/// 全部通过才写 `applied`（**不清重包屏障**；重包完成后由 repack 调用方清）。
/// 返回 `None` 表示当前不是 applying（无需收尾）。
pub(crate) fn reconcile_same_epoch_apply(
    vault: &menghunote_storage::Vault,
    identity_path: &std::path::Path,
    identity: &LocalIdentityV1,
) -> Result<Option<ApplyOutcome>, SyncError> {
    let phase = RotationApplyPhase::load(vault)?;
    if phase != Some(RotationApplyPhase::Applying) {
        return Ok(None);
    }
    let receipt = match RotationApplyReceipt::load(vault) {
        Ok(Some(receipt)) => receipt,
        Ok(None) => {
            return Ok(reconcile_paused(
                "phase=applying 但缺少 apply receipt（不能无条件解冻）",
            ))
        }
        Err(error) => {
            return Ok(reconcile_paused(format!(
                "phase=applying 但 apply receipt 损坏：{error}"
            )))
        }
    };
    // ① 内存身份必须已是 receipt 的推广目标。
    if receipt.vault_id() != identity.vault_id
        || receipt.device_id() != identity.device_id
        || receipt.epoch() != identity.current_epoch
        || receipt.control_rev() != identity.control_rev
        || receipt.instance_id() != identity.pinned_instance
        || receipt.new_manager_public() != identity.pinned_manager
        || identity.pending_rotation.is_some()
    {
        return Ok(reconcile_paused(
            "apply receipt 与本地身份不匹配（epoch/control_rev/pin），保持冻结",
        ));
    }
    // ② 磁盘身份才是 durable 真源：内存身份陈旧时不能被用来解冻。
    let durable = match std::fs::read(identity_path) {
        Ok(bytes) => {
            match LocalIdentityV1::unprotect(&bytes, &identity.vault_id, &identity.device_id) {
                Ok(durable) => durable,
                Err(error) => {
                    return Ok(reconcile_paused(format!(
                        "apply receipt 收尾无法解封磁盘身份：{error}"
                    )))
                }
            }
        }
        Err(error) => {
            return Ok(reconcile_paused(format!(
                "apply receipt 收尾无法读取磁盘身份：{error}"
            )))
        }
    };
    if durable.current_epoch != receipt.epoch()
        || durable.control_rev != receipt.control_rev()
        || durable.pinned_instance != receipt.instance_id()
        || durable.pinned_manager != receipt.new_manager_public()
        || durable.pending_rotation.is_some()
        || !durable.current_root.ct_eq(&identity.current_root)
    {
        return Ok(reconcile_paused("apply receipt 与磁盘身份不匹配，保持冻结"));
    }
    // ③ authenticated pin 是完整 gate + 推广的 durable 证据。
    let pin = match AuthenticatedPinV1::load(vault) {
        Ok(Some(pin)) => pin,
        Ok(None) => return Ok(reconcile_paused("缺少 authenticated pin，保持冻结")),
        Err(error) => return Ok(reconcile_paused(format!("authenticated pin 损坏：{error}"))),
    };
    if pin.epoch != receipt.epoch()
        || pin.control_rev != receipt.control_rev()
        || pin.instance_id != receipt.instance_id()
        || pin.activation_hash != receipt.activation_hash()
    {
        return Ok(reconcile_paused(
            "apply receipt 与 authenticated pin 不匹配，保持冻结",
        ));
    }
    // ④ baseline journal 必须证明 merge 已完成且正是 receipt 的 cid/S/cursor。
    let journal_ok = match baseline_apply_state(vault) {
        Ok(Some(state)) => {
            state.status == BaselineApplyStatus::Completed
                && state.checkpoint_id == receipt.checkpoint_id()
                && state.baseline_s == receipt.baseline_s()
                && state.chosen_cursor.as_deref() == Some(receipt.baseline_cursor())
        }
        Ok(None) => false,
        Err(error) => {
            return Ok(reconcile_paused(format!(
                "baseline apply journal 读取失败：{error}"
            )))
        }
    };
    if !journal_ok {
        return Ok(reconcile_paused(
            "baseline apply journal 与 receipt 不匹配（merge 未完成），保持冻结",
        ));
    }
    // ⑤ 身份旁路状态（epoch/control_rev/manager_rev/旧根）必须一致。
    match IdentityRotationStateV1::load_current(vault, identity) {
        Ok(Some(state))
            if state.control_rev == receipt.control_rev()
                && state.manager_rev == receipt.new_manager_rev()
                && state.root_for_epoch(receipt.from_epoch()).is_some() => {}
        Ok(_) => return Ok(reconcile_paused("身份旁路状态与 receipt 不匹配，保持冻结")),
        Err(error) => return Ok(reconcile_paused(format!("身份旁路状态读取失败：{error}"))),
    }
    // ⑥ 重包屏障必须仍在；收尾只写 applied，不提前解除 repack 冻结。
    match repack_required(vault) {
        Ok(true) => {}
        Ok(false) => {
            return Ok(reconcile_paused(
                "缺少 sync.repack_required 重包屏障，保持冻结",
            ))
        }
        Err(error) => return Ok(reconcile_paused(format!("读取重包屏障失败：{error}"))),
    }
    RotationApplyPhase::Applied.store(vault)?;
    Ok(Some(ApplyOutcome::Applied {
        baseline_s: receipt.baseline_s(),
        promoted: true,
    }))
}

/// 从 manifest 下载已验证基线（不写库）。
pub fn collect_manifest_payload<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    manifest_bytes: &[u8],
) -> Result<BaselinePayload, SyncError> {
    let manifest = ManifestV1::decode(manifest_bytes)?;
    if manifest.key_epoch != root.key_epoch {
        return Err(SyncError::Object("基线 manifest epoch 与解出根不符"));
    }
    let mut docs = Vec::with_capacity(manifest.docs.len());
    for doc in &manifest.docs {
        let (header, bytes) = fetch_object(
            client,
            root,
            ObjectType::Snapshot,
            &doc.snapshot_object_id,
            doc.part_count,
            doc.total_plain_len,
            &doc.object_digest,
            &[],
        )?;
        if header.part_count != doc.part_count {
            return Err(SyncError::Object("基线快照分片数与 manifest 不符"));
        }
        if bytes.len() as u64 != doc.total_plain_len {
            return Err(SyncError::Object("基线快照明文长度与 manifest 不符"));
        }
        let snapshot = SnapshotV1::decode(&bytes)?;
        // 快照载荷自报 VV 必须与实际 Loro 历史一致（T18）。
        verify_snapshot_integrity(&snapshot)?;
        if snapshot.doc_id != doc.doc_id
            || snapshot.kind != doc.kind
            || snapshot.generation != doc.generation
            || snapshot.vv != doc.vv
            || snapshot.frontier != doc.frontier
        {
            return Err(SyncError::Object("基线快照与 manifest 字段不符"));
        }
        // storage 的 `apply_baseline_batch` 最终走 `BodyDoc::restore(..., Some(snapshot))`，
        // 需要的是**内层 Loro snapshot 字节**；SnapshotV1 的 CDE 外层不能直接导入。
        if snapshot.loro_snapshot_bytes.is_empty() {
            return Err(SyncError::Object("基线快照内层 Loro 数据为空"));
        }
        docs.push((
            snapshot.doc_id.clone(),
            snapshot.loro_snapshot_bytes.clone(),
        ));
    }
    let mut assets = Vec::with_capacity(manifest.assets.len());
    for asset in &manifest.assets {
        let (_, payload) = fetch_object(
            client,
            root,
            ObjectType::Asset,
            &asset.object_id,
            asset.part_count,
            asset.total_plain_len,
            &asset.object_digest,
            &[],
        )?;
        let (real, byte_len, media_type) = crate::rotation_stage::decode_asset_plaintext(&payload)?;
        if real != asset.real_sha256 || byte_len != asset.byte_len || media_type != asset.media_type
        {
            return Err(SyncError::Object("基线资产明文与 manifest 不符"));
        }
        assets.push(BaselineAsset {
            object_id: asset.object_id,
            asset_hash: real,
            byte_len,
            media_type: Some(media_type),
            data: crate::rotation_stage::payload_asset_data(&payload)?.to_vec(),
        });
    }
    Ok(BaselinePayload {
        checkpoint_id,
        baseline_s,
        docs,
        assets,
    })
}

/// 按 `manifest_oid` 下载并解密 MANIFEST 对象（只读），返回 `(manifest CDE, object_digest)`。
///
/// relay 的 `GET /checkpoints/{cid}` 键 4 是 manifest 的 **object id**（`api.rs:1859–1860`），
/// 不是 manifest 明文；对象本体是 MNE1 密文，必须用当前 epoch 根解密。这里复用
/// `RelayClient::part` + `decrypt_envelope`，并校验分片 header、对象摘要与明文长度。
pub fn fetch_manifest_object<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    manifest_oid: &ObjectId,
) -> Result<(Vec<u8>, [u8; 32]), SyncError> {
    let first = client.part(manifest_oid, 0)?;
    let envelope = Envelope::decode(&first)?;
    if envelope.header.object_type != ObjectType::Manifest
        || envelope.header.object_id != *manifest_oid
        || envelope.header.part_count == 0
    {
        return Err(SyncError::Object("MANIFEST 对象 header 不符"));
    }
    let part_count = envelope.header.part_count;
    let total_plain_len = envelope.header.total_plaintext_len;
    let mut parts = Vec::with_capacity(part_count as usize);
    let mut digests = Vec::with_capacity(part_count as usize);
    for index in 0..part_count {
        let bytes = if index == 0 {
            first.clone()
        } else {
            client.part(manifest_oid, index)?
        };
        let envelope = Envelope::decode(&bytes)?;
        if envelope.header.object_type != ObjectType::Manifest
            || envelope.header.object_id != *manifest_oid
            || envelope.header.part_index != index
            || envelope.header.part_count != part_count
        {
            return Err(SyncError::Object("MANIFEST 分片 header 不符"));
        }
        digests.push(Sha256::digest(&bytes).into());
        parts.push(bytes);
    }
    let digest_input = menghunote_sync_proto::object_digest_input(part_count, &digests)?;
    let object_digest: [u8; 32] = Sha256::digest(&digest_input).into();
    let mut plaintext = Vec::with_capacity(total_plain_len as usize);
    for bytes in &parts {
        let (_, chunk) = decrypt_envelope(root, ObjectType::Manifest, manifest_oid, bytes, &[])?;
        plaintext.extend_from_slice(&chunk);
    }
    if plaintext.len() as u64 != total_plain_len {
        return Err(SyncError::Object("MANIFEST 明文长度不符"));
    }
    Ok((plaintext, object_digest))
}

// ---------------------------------------------------------------- activation T 授权（I 阶段）
//
// `/control` 里**已复算**的 activation T 与双签通过 `RelayClient::verified_activation_t`
// 暴露（带默认 `Ok(None)` 实现）；本模块只消费它，不再自建扩展 trait。
// **缺 T 绝不等于验证通过**：`Ok(None)` 时调用方必须拒绝 apply（fail-closed），
// 不得回退到"只信 relay 自报的 `ActivationCommitV1` 32B hash"。

/// relay `grants_digest` 口径：`SHA256(concat(sorted(device_id || SHA256(signed_header))))`。
///
/// 与 `crates/relay/src/rotations.rs::grants_digest`、`sync::rotation_slots::grants_digest`
/// 同口径（ADR 0006 §34）；按 `device_id` 升序拼接后才哈希。
#[must_use]
pub fn grant_set_digest(leaves: &[GrantLeafEvidence]) -> [u8; 32] {
    let mut ordered = leaves.to_vec();
    ordered.sort_by_key(|leaf| leaf.device_id);
    let mut input = Vec::with_capacity(ordered.len() * 48);
    for leaf in ordered {
        input.extend_from_slice(&leaf.device_id);
        input.extend_from_slice(&leaf.signed_header_sha256);
    }
    Sha256::digest(&input).into()
}

/// 选中 baseline 的 `CheckpointCommit` 证据（T 门禁的 commit digest/签名输入）。
///
/// 真实 HTTP 客户端只能从 `/bootstrap` 拿到主/备基线的 `signed_commit + 生产者签名`
/// （键 6/7）；`GET /checkpoints/{cid}` 只在 `http_relay` 内部解码同名字段，不透出原字节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointCommitEvidence {
    /// checkpoint id（必须等于 T 的 primary/fallback cid）。
    pub checkpoint_id: [u8; 16],
    /// 该 checkpoint 覆盖的 S（必须等于 `T.freeze_seq`）。
    pub covered_seq: u64,
    /// relay 为该 S 给出的游标（`/bootstrap` 键 5；与调用方输入逐字节比对）。
    pub cursor: String,
    /// relay 持久化的 `signed_commit` 字节（`CheckpointCommit` CDE）。
    pub signed_commit: Vec<u8>,
    /// 生产者设备对该 commit 的 64B Ed25519 签名。
    pub signature: [u8; 64],
    /// 生产者设备证书里的 Ed25519 公钥（`RelayClient::device_cert(producer)`）。
    pub producer_sign_public: [u8; 32],
    /// manifest 对象 id（relay 为该 checkpoint 指名的 OID）。
    pub manifest_object_id: ObjectId,
}

impl CheckpointCommitEvidence {
    /// 从 relay `/bootstrap` 的一条基线取 commit 证据；缺 `signed_commit`/签名 → `Ok(None)`。
    #[must_use]
    pub fn from_bootstrap(
        entry: &BootstrapCheckpoint,
        producer_sign_public: [u8; 32],
    ) -> Option<Self> {
        let (Some(signed_commit), Some(signature)) = (entry.signed_commit.clone(), entry.signature)
        else {
            return None;
        };
        Some(Self {
            checkpoint_id: entry.checkpoint_id,
            covered_seq: entry.covered_seq,
            cursor: entry.cursor.clone(),
            signed_commit,
            signature,
            producer_sign_public,
            manifest_object_id: entry.manifest_oid,
        })
    }
}

/// 一份"当前合法 checkpoint"的认证结果（评审 0003 P1-C 的第一半）。
///
/// 它回答的是"**这份 checkpoint 内容是谁签的、覆盖到哪、对应哪份 manifest**"，与
/// "T/latest grant 是否授权把根推广到某 epoch"是**两件事**。把两者解耦后，激活 pair
/// 被普通健康 checkpoint 替换（relay `prune_retired` 会 prune 旧 pair/清 refs）时，
/// 旧设备仍能消费当前 pair，而不是永久 `Paused`。
///
/// 字段**私有**、构造只发生在 [`verify_checkpoint_authorization`]（`pub(crate)`）：
/// 外部 crate 无法伪造本类型，也就无法让 [`verify_activation_t_gate`] 对
/// `cid ∉ T.pair` 的选择走"跳过认证"的弱路径。
///
/// ```compile_fail
/// use menghunote_sync::rotation_apply::AuthenticatedCheckpoint;
/// let _ = AuthenticatedCheckpoint {
///     checkpoint_id: [0u8; 16],
///     covered_seq: 0,
///     cursor: String::new(),
///     epoch: 0,
///     signed_commit_sha256: [0u8; 32],
///     manifest_object_id: [0u8; 32],
///     manifest_object_digest: [0u8; 32],
/// };
/// ```
pub struct AuthenticatedCheckpoint {
    checkpoint_id: [u8; 16],
    covered_seq: u64,
    cursor: String,
    epoch: u64,
    signed_commit_sha256: [u8; 32],
    manifest_object_id: ObjectId,
    manifest_object_digest: [u8; 32],
}

impl AuthenticatedCheckpoint {
    /// 已认证的 checkpoint id。
    #[must_use]
    pub fn checkpoint_id(&self) -> [u8; 16] {
        self.checkpoint_id
    }

    /// 已认证的覆盖水位 S。
    #[must_use]
    pub fn covered_seq(&self) -> u64 {
        self.covered_seq
    }

    /// relay 为该 S 给出的游标。
    #[must_use]
    pub fn cursor(&self) -> &str {
        &self.cursor
    }

    /// checkpoint 所属 epoch。
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 生产者签名的 `signed_commit` 字节摘要。
    #[must_use]
    pub fn signed_commit_sha256(&self) -> [u8; 32] {
        self.signed_commit_sha256
    }

    /// MANIFEST 对象 id。
    #[must_use]
    pub fn manifest_object_id(&self) -> ObjectId {
        self.manifest_object_id
    }

    /// MANIFEST 密文对象摘要（`fetch_manifest_object` 复算，不是 relay 自报）。
    #[must_use]
    pub fn manifest_object_digest(&self) -> [u8; 32] {
        self.manifest_object_digest
    }
}

/// 认证"当前合法 checkpoint"本身：**只读**，不写库、不推游标。
///
/// 这是评审 0003 P1-C 的第二半：`cid ∈ T.pair` 时 T 的双签已经授权了该 commit，
/// 但"激活 pair 被普通健康 checkpoint 替换"之后（relay `checkpoints::prune_retired`
/// 会 prune 旧 pair、删 `checkpoint_refs`），新 pair 的 cid 不可能出现在 T 里。
/// 此时改用本函数认证：生产者证书/活动成员资格、epoch、`CheckpointCommit` 签名、
/// manifest、refs（复算 `refs_digest`）、S 的 freeze/GC/head 边界。
///
/// 入参全部是**读回/复算**事实：`entry` 来自 `/bootstrap`（cursor 的唯一来源），
/// `status` 来自 `GET /checkpoints/{cid}`，`commit` 是 `entry.signed_commit` 的解码，
/// `manifest` 是按 `entry.manifest_oid` 解密出的明文，`manifest_object_digest` 是密文
/// 分片的复算摘要。任何一项不符都是 `RelayWire`（调用方转 `Paused`，可重试）。
///
/// `require_active_producer`：`cid ∉ T.pair` 时**必须**为 true——这份 checkpoint 没有
/// T 的 digest 授权，只能靠"已验证控制链里的 active 成员"资格；`cid ∈ T.pair` 时传
/// false（T 的双签已授权该 commit，不因生产者后来被吊销而拒绝冻结期的合法基线）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn verify_checkpoint_authorization<C: RelayClient>(
    client: &mut C,
    vault_id: VaultId,
    entry: &BootstrapCheckpoint,
    status: &CheckpointStatus,
    commit: &CheckpointCommit,
    manifest: &ManifestV1,
    manifest_object_digest: [u8; 32],
    expected_epoch: u64,
    freeze_seq: u64,
    bundle: &BootstrapBundle,
    state: &RelayState,
    require_active_producer: bool,
) -> Result<AuthenticatedCheckpoint, SyncError> {
    // ① relay 必须明确把这份 checkpoint 标成 `published`：staging/candidate/aborted/
    //    pruned 都不是"当前合法基线"（pruned 的 refs 已删、对象可能已被 retire）。
    if status.state != "published" {
        return Err(SyncError::RelayWire(
            "checkpoint 状态不是 published（拒绝未发布/已 prune 的基线）",
        ));
    }
    // ② cid 三方一致：bootstrap 条目、`GET /checkpoints/{cid}` 与签名 commit。
    if entry.checkpoint_id != status.checkpoint_id || commit.checkpoint_id != status.checkpoint_id {
        return Err(SyncError::RelayWire(
            "checkpoint cid 在 bootstrap/status/commit 三处不一致",
        ));
    }
    // ③ epoch 三方一致，且正是本次 activation 的目标 epoch。
    if entry.epoch != expected_epoch
        || status.epoch != expected_epoch
        || commit.epoch != expected_epoch
    {
        return Err(SyncError::RelayWire(
            "checkpoint epoch 与 bootstrap/status/commit/激活目标 epoch 不一致",
        ));
    }
    // ④ S 四方一致（bootstrap/status/commit/manifest）：cursor 与 apply 都必须落在同一 S。
    if entry.covered_seq != status.covered_seq
        || commit.covered_relay_seq != status.covered_seq
        || manifest.covered_relay_seq != status.covered_seq
    {
        return Err(SyncError::RelayWire(
            "checkpoint 覆盖水位 S 在 bootstrap/status/commit/manifest 四处不一致",
        ));
    }
    // ⑤ 下界/上界：
    //    - `S >= freeze_seq`：`(S, freeze]` 的更新写在旧根下，基线不覆盖就会永久丢；
    //    - `S >= gc_floor`：低于 floor 的日志前缀已删，从该 cursor 续读必缺洞；
    //    - `S <= head`：不得消费 relay 尚未产生的更新。
    //    floor/head 取 `/bootstrap` 与 `/state` 两处中更严格的一侧（两次请求之间可能前进）。
    if status.covered_seq < freeze_seq {
        return Err(SyncError::RelayWire(
            "checkpoint S 早于 activation T 的 freeze_seq",
        ));
    }
    let gc_floor = bundle.gc_floor.max(state.gc_floor);
    if status.covered_seq < gc_floor {
        return Err(SyncError::RelayWire(
            "checkpoint S 低于 relay GC floor（该游标已不可继续）",
        ));
    }
    let head_seq = bundle.head_seq.min(state.head_seq);
    if status.covered_seq > head_seq {
        return Err(SyncError::RelayWire("checkpoint S 超过 relay head_seq"));
    }
    // ⑥ manifest 对象 id 三方一致；`/bootstrap` 必须带游标（Opaque 客户端不可解）。
    let status_manifest = status
        .manifest_object_id
        .ok_or(SyncError::RelayWire("checkpoint 缺少 manifest_oid"))?;
    if entry.manifest_oid != status_manifest || commit.manifest_object_id != status_manifest {
        return Err(SyncError::RelayWire(
            "checkpoint manifest_oid 在 bootstrap/status/commit 三处不一致",
        ));
    }
    if entry.cursor.is_empty() {
        return Err(SyncError::RelayWire("bootstrap 条目缺 relay 游标"));
    }
    // ⑦ 密文对象摘要必须等于签名 commit 里承诺的 manifest_digest（已在
    //    `fetch_manifest_object` 处按分片复算，不是 relay 自报）。
    if commit.manifest_digest != manifest_object_digest {
        return Err(SyncError::RelayWire(
            "manifest 密文摘要与 checkpoint commit 不符",
        ));
    }
    // ⑧ manifest 明文必须绑到本 vault/本 cid/本 epoch。
    if manifest.vault_id != vault_id
        || manifest.checkpoint_id != commit.checkpoint_id
        || manifest.key_epoch != expected_epoch
    {
        return Err(SyncError::RelayWire(
            "manifest 明文与 vault/checkpoint/epoch 不绑定",
        ));
    }
    // ⑨ refs 摘要**复算**：只凭 manifest 条目重算 refs 集合的 `refs_digest`，与签名
    //    commit 以及 relay 自报（status/boot 条目有值时必须相等）对撞，防止"签名覆盖的
    //    对象集合"与实际 manifest 不一致。
    let refs = crate::bootstrap::manifest_refs(manifest);
    let refs_digest: [u8; 32] =
        Sha256::digest(menghunote_sync_proto::refs_digest_input(&refs)?).into();
    if commit.refs_digest != refs_digest {
        return Err(SyncError::RelayWire(
            "checkpoint refs_digest 与 manifest 复算结果不符",
        ));
    }
    if status
        .refs_digest
        .is_some_and(|advertised| advertised != refs_digest)
    {
        return Err(SyncError::RelayWire(
            "checkpoint status 自报 refs_digest 与复算不符",
        ));
    }
    if entry
        .refs_digest
        .is_some_and(|advertised| advertised != refs_digest)
    {
        return Err(SyncError::RelayWire(
            "bootstrap 条目自报 refs_digest 与复算不符",
        ));
    }
    // ⑩ 生产者证书必须存在且归属本 vault/该设备。
    let certificate = client
        .device_cert(&commit.producer_device_id)?
        .ok_or(SyncError::RelayWire("找不到 checkpoint 生产者设备证书"))?;
    if certificate.vault_id != vault_id || certificate.device_id != commit.producer_device_id {
        return Err(SyncError::RelayWire(
            "checkpoint 生产者证书与 vault/commit 不符",
        ));
    }
    // ⑪ 生产者 Ed25519 签名覆盖 canonical `checkpoint_signing_input`。
    let signature = entry
        .signature
        .ok_or(SyncError::RelayWire("checkpoint 缺 commit 生产者签名"))?;
    verify_signature(
        &certificate.sign_ed25519_pub,
        &checkpoint_signing_input(commit)?,
        &signature,
    )?;
    // ⑫ 生产者资格：必须是**已验证控制链**里的 active 成员。用 HPKE 公钥做身份对撞
    //    （`active_member_hpke_keys` 由 `active_certs` 派生，relay `member.status` 不参与），
    //    避免再造一个"证书存在即通过"的洞。客户端给不出该集合时 fail-closed。
    if require_active_producer {
        let active_keys = match client.active_member_hpke_keys() {
            Ok(keys) => keys,
            // 传输类错误保持 `Err`（可重试，不写库）。
            Err(error @ SyncError::Transport(_)) => return Err(error),
            Err(_) => {
                return Err(SyncError::RelayWire(
                    "客户端无法提供已验证控制链的 active 成员集合，禁止消费 T pair 之外的基线",
                ))
            }
        };
        let active = active_keys.iter().any(|(device_id, hpke)| {
            *device_id == commit.producer_device_id && *hpke == certificate.hpke_x25519_pub
        });
        if !active {
            return Err(SyncError::RelayWire(
                "checkpoint 生产者不是已验证控制链的 active 成员",
            ));
        }
    }
    let signed_commit = entry
        .signed_commit
        .as_deref()
        .ok_or(SyncError::RelayWire("checkpoint 缺 signed_commit"))?;
    Ok(AuthenticatedCheckpoint {
        checkpoint_id: commit.checkpoint_id,
        covered_seq: commit.covered_relay_seq,
        cursor: entry.cursor.clone(),
        epoch: commit.epoch,
        signed_commit_sha256: signed_commit_digest(signed_commit),
        manifest_object_id: commit.manifest_object_id,
        manifest_object_digest,
    })
}

/// T 授权门禁的全部输入：T 证据 + 已下载/已读回的公开事实（不含秘密）。
pub struct ActivationTGate<'a> {
    /// 已复算的 T 与双签（缺失时调用方不得构造门禁）。
    pub evidence: &'a VerifiedActivationT,
    /// current rotation 视图（relay 公开事实，仅用于与 T 交叉绑定）。
    pub view: &'a RotationView,
    /// 本地 vault。
    pub vault_id: VaultId,
    /// relay 实例（`/state` 键 2；必须与本地 pin 一致）。
    pub instance_id: [u8; 16],
    /// 期望的 `T.from_epoch`：单轮 = 本地 `current_epoch`；离线跨多 epoch =
    /// 已验证 activation 链的前一环 `to_epoch`（见
    /// [`verify_epoch_activation_chain`]）。
    pub expected_from_epoch: u64,
    /// 本设备 id（grant recipient）。
    pub device_id: DeviceId,
    /// 本设备 X25519 私钥（复算 grant AAD 的 recipient 公钥）。
    pub device_x25519_secret: &'a Secret32,
    /// 签发 grant / T 旧签名的管理公钥（阶段 A 本地锚点）。
    pub old_manager_public: [u8; 32],
    /// T 新签名的新管理公钥（推广后 pin）。
    pub new_manager_public: [u8; 32],
    /// 新管理 key rev（推广输入）。
    pub new_manager_rev: u64,
    /// 调用方选中的 baseline cid。
    pub selected_checkpoint: [u8; 16],
    /// 调用方选中的 baseline S。
    pub baseline_s: u64,
    /// 调用方选中的 baseline cursor。
    pub baseline_cursor: &'a str,
    /// 选中 checkpoint 的 `signed_commit` 证据（`/bootstrap` 读回）。
    pub commit: &'a CheckpointCommitEvidence,
    /// 选中 MANIFEST 对象的密文摘要（`fetch_manifest_object` 复算，不是 relay 自报）。
    pub manifest_object_digest: [u8; 32],
    /// 本设备 latest grant 原始证据。
    pub grant: &'a SelfGrantEvidence,
    /// 完整 grant leaf（`None` 时只有 `grants_count == 1` 能证明 inclusion）。
    pub grant_leaves: Option<&'a [GrantLeafEvidence]>,
    /// relay view 的 grants 数量（`T.grants_digest` 覆盖的 leaf 数）。
    pub grants_count: u64,
}

/// T 证据的**双签复算 + 逐字段绑定**（门禁、epoch 链与纯入口共用）。
///
/// ① 旧/新管理公钥分别对 `activation_t_signing_input(T)` 复算 64B 签名（不信 relay
/// "已验证"自报）；② T 与 rotation view（rid/epoch/freeze S/候选 pair/激活 control_rev）、
/// relay 实例、链上位置 `expected_from_epoch`、推广用新管理 key/rev 逐字段绑定。
///
/// `expected_from_epoch` 只允许两种来源：单轮 apply 的本地可信 `current_epoch`，或
/// [`verify_epoch_activation_chain`] 已验证链的**前一项 `to_epoch`**。绝不接受"任意小"
/// 或 `<=` 语义——跳环/回滚必须在链验证阶段 fail-closed。
///
/// 这里**不**检查 baseline 的 S/cursor/commit digest/grant inclusion——那些需要 relay
/// 读回事实，由 [`verify_activation_t_gate`] 继续完成。
#[allow(clippy::too_many_arguments)]
pub fn verify_activation_t_bindings(
    evidence: &VerifiedActivationT,
    view: &RotationView,
    vault_id: VaultId,
    instance_id: [u8; 16],
    expected_from_epoch: u64,
    old_manager_public: [u8; 32],
    new_manager_public: [u8; 32],
    new_manager_rev: u64,
) -> Result<(), SyncError> {
    let activation_t = &evidence.activation_t;

    // ① T 的旧/新管理双签必须由本地已知公钥复算（不信 relay "已验证"自报）。
    let signing_input = activation_t_signing_input(activation_t)?;
    let old_signature: [u8; 64] = evidence
        .old_t_signature
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::RelayWire("activation T 旧管理签名不是 64B"))?;
    let new_signature: [u8; 64] = evidence
        .new_t_signature
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::RelayWire("activation T 新管理签名不是 64B"))?;
    verify_signature(&old_manager_public, &signing_input, &old_signature)?;
    verify_signature(&new_manager_public, &signing_input, &new_signature)?;

    // ② T 与 rotation view / relay 实例 / 本地身份逐字段绑定。
    if activation_t.vault_id != vault_id
        || activation_t.instance_id != instance_id
        || activation_t.rotation_id != view.rotation_id
    {
        return Err(SyncError::RelayWire(
            "activation T 与 vault/instance/rotation 不绑定",
        ));
    }
    if view.state != "activated"
        || activation_t.from_epoch != view.from_epoch
        || activation_t.to_epoch != view.to_epoch
        || activation_t.freeze_seq != view.freeze_seq
    {
        return Err(SyncError::RelayWire(
            "activation T 与 rotation view 的 epoch/S 不符",
        ));
    }
    // `from_epoch` 必须精确等于"链上前一环的 to_epoch"：
    // - 单轮 apply = 本地可信 `current_epoch`；
    // - 离线跨多 epoch = 已验证链的前一环（由
    //   [`verify_epoch_activation_chain`] 逐环推进后传入）。
    // **不是**放宽为 `<=`：任何跳环/回滚都必须在链验证阶段被拒绝。
    if activation_t.from_epoch != expected_from_epoch
        || activation_t.to_epoch <= activation_t.from_epoch
    {
        return Err(SyncError::RelayWire(
            "activation T 的 from/to epoch 与链上位置/本地状态不符",
        ));
    }
    if view.primary_checkpoint != Some(activation_t.primary_cid)
        || view.fallback_checkpoint != Some(activation_t.fallback_cid)
        || activation_t.primary_cid == activation_t.fallback_cid
    {
        return Err(SyncError::RelayWire(
            "activation T 的 primary/fallback cid 与 view 不符",
        ));
    }
    if view.activated_control_rev != Some(activation_t.expected_control_rev) {
        return Err(SyncError::RelayWire(
            "activation T 的 expected_control_rev 与激活控制记录不符",
        ));
    }
    if activation_t.new_manager_pubkey != new_manager_public
        || u64::from(activation_t.new_manager_rev) != new_manager_rev
    {
        return Err(SyncError::RelayWire(
            "activation T 的新管理 key/rev 与推广输入不符",
        ));
    }
    Ok(())
}

/// 已验证的 activation 链尾：离线跨多 epoch 后 B 侧 apply 的唯一授权结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedActivationChain {
    /// 链尾 T（必须与 network 入口拿到的 latest T 逐字节一致）。
    pub tail: VerifiedActivationT,
    /// 链尾 T 的 `from_epoch`（= 链上前一环 `to_epoch`；单轮 = 本地 epoch）。
    pub tail_from_epoch: u64,
    /// 链首 T 的旧管理 rev（= 本地 epoch 位置上的管理 rev）。
    ///
    /// 链内相邻环已保证 `old == 上一环 new`；链首必须再与本地可信管理 rev 锚定，
    /// 否则 relay 可以给出"链内自洽但整体错位"的 rev 序列。
    pub head_old_manager_rev: u64,
    /// 链尾 T 的旧管理公钥（签发 latest grant 的那把；单轮 = 本地 pin）。
    pub tail_old_manager_public: [u8; 32],
    /// 链尾 T 的新管理公钥（推广目标；必须等于调用方传入的推广材料）。
    pub tail_new_manager_public: [u8; 32],
    /// 链尾 T 的新管理 key rev（推广目标）。
    pub tail_new_manager_rev: u64,
}

/// 从本地可信 `current_epoch` 出发，逐环验证 `/control` 的 activation 链。
///
/// 背景（评审 0003 追加）：B 离线期间 A 连续完成 1→2、2→3 后，latest T 的
/// `from_epoch` 是 2 而本地 epoch 仍是 1。旧门禁强制 `T.from_epoch == 本地 epoch`，
/// 于是"latest grant 存在但必定 Paused"。修复**不是**把等号放宽为 `<=`，而是补上
/// 授权链本身：
///
/// 1. 必须能在链上定位 `from_epoch == 本地可信 current_epoch` 的那一环，并从它开始
///    逐环验证（找不到 = 链不完整/回滚/缺中间激活 → 拒绝）；
/// 2. 相邻环 `to_epoch == 下一环 from_epoch`，且每环 `to > from`（缺中间激活 → 拒绝）；
/// 3. 每环 T 的双签/T-view/实例绑定复用 [`verify_activation_t_bindings`]；旧管理 key
///    由上一环 T 的 `new_manager_pubkey` 推出，管理 rev 必须首尾相接；链首的旧管理
///    rev 由调用方锚定到本地可信状态（本函数把它放在 [`VerifiedActivationChain::head_old_manager_rev`] 返回）；
/// 4. 每环的 `ActivationCommitV1` 必须能重建、摘要等于 view 的 32B 摘要，且与 T
///    逐字段一致（复用 [`verify_activation_commit`] /
///    [`verify_activation_commit_matches_t`]）；
/// 5. 每环 `control_rev == T.expected_control_rev`，且 `prev_control_hash` 等于该 rev
///    前一条记录的链上 hash（由 [`RelayClient::epoch_activations`] 从已验证的
///    `/control` 给出）；
/// 6. 链尾必须正是 latest T（否则 latest 来自另一条链/更新的激活 → 拒绝）。
///
/// 中间的 baseline/grant 不逐个下载（latest baseline 覆盖历史），但**授权链逐环验**。
/// 通过后调用方只能拿链尾的 T 授权 latest grant/baseline 的 apply。
pub fn verify_epoch_activation_chain(
    links: &[EpochActivationLink],
    latest: &VerifiedActivationT,
    vault_id: VaultId,
    instance_id: [u8; 16],
    current_epoch: u64,
    anchor_manager_public: [u8; 32],
) -> Result<VerifiedActivationChain, SyncError> {
    if links.is_empty() {
        return Err(SyncError::RelayWire(
            "缺少 activation 链（客户端未提供 /control 逐环证据），拒绝 apply",
        ));
    }
    // ① 全链结构：epoch 严格前进、相邻环首尾相接、控制链 rev 严格递增。
    for (index, link) in links.iter().enumerate() {
        let activation_t = &link.evidence.activation_t;
        if activation_t.to_epoch <= activation_t.from_epoch {
            return Err(SyncError::RelayWire(
                "activation 链的 to_epoch 必须大于 from_epoch",
            ));
        }
        if index > 0 {
            let prev = &links[index - 1];
            if activation_t.from_epoch != prev.evidence.activation_t.to_epoch {
                return Err(SyncError::RelayWire(
                    "activation 链缺中间激活（to_epoch 与下一环 from_epoch 不相接）",
                ));
            }
            if link.control_rev <= prev.control_rev {
                return Err(SyncError::RelayWire(
                    "activation 链的 control_rev 未严格递增（重排/回放）",
                ));
            }
        }
    }
    // ② 从本地可信 epoch 对应的那一环开始；找不到 = 链不完整/回滚/缺中间激活。
    //    客户端（[`RelayClient::epoch_activations`]）按 `to_epoch > from_epoch` 过滤后
    //    通常 start = 0；这里仍显式定位，避免不同实现返回全链时把信任锚放错位置。
    let start = links
        .iter()
        .position(|link| link.evidence.activation_t.from_epoch == current_epoch)
        .ok_or(SyncError::RelayWire(
            "本地 epoch 不在 activation 链首（链不完整/回滚/缺中间激活），拒绝 apply",
        ))?;
    let mut tail_old_manager_public = anchor_manager_public;
    for (index, link) in links.iter().enumerate().skip(start) {
        let activation_t = &link.evidence.activation_t;
        let expected_from_epoch = if index == start {
            current_epoch
        } else {
            links[index - 1].evidence.activation_t.to_epoch
        };
        // ③ 旧管理 key 由链上前一环推出；链首 = 本地锚点。管理 rev 必须相接。
        if index > start {
            let prev = &links[index - 1].evidence.activation_t;
            if activation_t.old_manager_rev != prev.new_manager_rev {
                return Err(SyncError::RelayWire("activation 链的管理权 rev 不连续"));
            }
            tail_old_manager_public = prev.new_manager_pubkey;
        }
        // ④ 双签 + T/view/实例/epoch 绑定（复用单步门禁的同一条校验）。
        verify_activation_t_bindings(
            &link.evidence,
            &link.view,
            vault_id,
            instance_id,
            expected_from_epoch,
            tail_old_manager_public,
            activation_t.new_manager_pubkey,
            u64::from(activation_t.new_manager_rev),
        )?;
        // ⑤ 控制链位置：rev 与 prev_control_hash 必须正是链上这一条。
        if link.control_rev != activation_t.expected_control_rev {
            return Err(SyncError::RelayWire(
                "activation 的 control_rev 与 T.expected_control_rev 不符",
            ));
        }
        if link.prev_control_hash != activation_t.prev_control_hash {
            return Err(SyncError::RelayWire(
                "activation T 的 prev_control_hash 与链上位置不符",
            ));
        }
        // ⑥ activation commit：重建摘要必须等于 view 的 32B 摘要，且与 T 逐字段一致。
        let Some(expected_commit) = link.view.activation_commit.as_deref() else {
            return Err(SyncError::RelayWire(
                "activation 链的 view 缺少 activation commit",
            ));
        };
        let commit_bytes = link.activation_commit.encode()?;
        let control_rev = link.view.activated_control_rev.ok_or(SyncError::RelayWire(
            "activation 链的 view 缺少 activated_control_rev",
        ))?;
        verify_activation_commit(
            &commit_bytes,
            expected_commit,
            vault_id,
            instance_id,
            link.view.rotation_id,
            link.view.from_epoch,
            link.view.to_epoch,
            control_rev,
        )?;
        verify_activation_commit_matches_t(&link.activation_commit, activation_t)?;
    }
    // ⑦ 链尾 = latest：否则 latest 可能来自另一条链/更新的激活，不能拿它授权。
    let tail = links
        .last()
        .ok_or(SyncError::RelayWire("activation 链为空（不能授权 apply）"))?;
    if tail.evidence != *latest {
        return Err(SyncError::RelayWire(
            "activation 链尾不是 latest T（本地看到的激活不在链尾），拒绝 apply",
        ));
    }
    let activation_t = &tail.evidence.activation_t;
    Ok(VerifiedActivationChain {
        tail: tail.evidence.clone(),
        tail_from_epoch: activation_t.from_epoch,
        head_old_manager_rev: u64::from(links[start].evidence.activation_t.old_manager_rev),
        tail_old_manager_public,
        tail_new_manager_public: activation_t.new_manager_pubkey,
        tail_new_manager_rev: u64::from(activation_t.new_manager_rev),
    })
}

/// 用 activation T 对一次 B 侧 apply 授权（评审 0002 P1-1.4 / ADR 0006 §34）。
///
/// 覆盖：T 双签复算、T 与 view/身份/管理权逐字段绑定、选中 baseline 的 cid/commit
/// 授权、`CheckpointCommit` 字段与生产者签名、本设备 grant 的
/// AAD/enc/ciphertext/外签与 `T.grants_digest` inclusion。
///
/// **两条路径（评审 0003 P1-C）**：
/// - `selected_checkpoint ∈ {T.primary_cid, T.fallback_cid}`：T 的双签点名了该
///   `signed_commit` 的 digest，必须逐字节相等。这是**强路径**，绝不因为弱路径可用而降级
///   （否则恶意 relay 可在激活 pair 仍健康时改喂"同 cid、不同 commit"的基线，把轮换冻结点
///   唯一的 have-frozen 保证静默弱化；也给"生产者事后被吊销"的历史基线保留合法消费口径）。
/// - `selected_checkpoint ∉ T.pair`：激活 pair 已被普通健康 checkpoint 替换（relay
///   `prune_retired`），必须由调用方先经 [`verify_checkpoint_authorization`] 完成
///   checkpoint 级认证并传入 `authenticated`；门禁再把它与本次选择的
///   cid/S/cursor/epoch/commit 逐字段对撞。
///
/// **管理 rev 语义**：staging grant 的 AAD `manager_rev` 是 `T.old_manager_rev`
/// （activate 前 relay 的管理权仍是旧 rev）；`T.new_manager_rev` 只用于 promote/最终
/// 身份，**不**参与 grant 校验。调用方必须先经 [`verify_epoch_activation_chain`] 并
/// 把链首 `T.old_manager_rev` 锚定到本地可信管理 rev（见
/// `apply_epoch_change_with_client` 的链首锚点检查）。
///
/// **S 语义**：T 绑定的是**冻结** S（`T.freeze_seq`），候选基线可以覆盖 `S >= freeze_seq`
/// （冻结之后创建的候选 checkpoint 也是合法基线）；因此这里只要求
/// `baseline_s >= T.freeze_seq` 且 `CheckpointCommit.covered_relay_seq >= T.freeze_seq`，
/// `baseline_cursor` 仍必须逐字节等于 relay 为该 S 给出的 `/bootstrap` 游标。
///
/// 通过后返回选中的、已验签的 [`CheckpointCommit`]。任一步不符都是 `RelayWire` 错误：
/// 调用方必须转成 `Paused`，**不得**写库/推游标。
#[allow(clippy::too_many_arguments)]
pub fn verify_activation_t_gate(
    input: &ActivationTGate<'_>,
    authenticated: Option<&AuthenticatedCheckpoint>,
) -> Result<CheckpointCommit, SyncError> {
    let activation_t = &input.evidence.activation_t;
    verify_activation_t_bindings(
        input.evidence,
        input.view,
        input.vault_id,
        input.instance_id,
        input.expected_from_epoch,
        input.old_manager_public,
        input.new_manager_public,
        input.new_manager_rev,
    )?;

    // ③ 选中 baseline 的 cid/digest 授权（两条路径，见函数文档）。
    //    S 只要求 `>= freeze_seq`：T 绑定的是**冻结** S，冻结之后创建的候选 checkpoint
    //    覆盖更晚的 S 仍是合法基线。
    let pair_digest = if input.selected_checkpoint == activation_t.primary_cid {
        Some(activation_t.primary_commit_digest)
    } else if input.selected_checkpoint == activation_t.fallback_cid {
        Some(activation_t.fallback_commit_digest)
    } else {
        None
    };
    if input.commit.checkpoint_id != input.selected_checkpoint {
        return Err(SyncError::RelayWire(
            "checkpoint commit 与选中基线 cid 不符",
        ));
    }
    if input.baseline_s < activation_t.freeze_seq || input.commit.covered_seq != input.baseline_s {
        return Err(SyncError::RelayWire(
            "baseline S 早于 activation T 的 freeze_seq 或与 checkpoint 覆盖的 S 不符",
        ));
    }
    // cursor 必须正是 relay 为该 S 给出的游标（`/bootstrap` 键 5，Opaque 客户端不可解）：
    // 换了 S 却复用旧游标 = 从错误位置续读，必须拒绝。
    if input.baseline_cursor != input.commit.cursor {
        return Err(SyncError::RelayWire(
            "baseline cursor 与 relay 该 baseline_s 的 /bootstrap 游标不符",
        ));
    }
    match pair_digest {
        // 强路径：pair 内必须逐字节等于 T 的双签 digest，**不得**降级到 checkpoint 级认证。
        Some(expected_commit_digest) => {
            if signed_commit_digest(&input.commit.signed_commit) != expected_commit_digest {
                return Err(SyncError::RelayWire(
                    "checkpoint commit digest 与 activation T 不符",
                ));
            }
        }
        // 弱路径：非 T pair 的选择必须已完成 checkpoint 级认证，并与本次选择逐字段一致。
        None => {
            let authenticated = authenticated.ok_or(SyncError::RelayWire(
                "选中基线不是 T 的 activation pair，且未通过 checkpoint 级认证",
            ))?;
            if authenticated.checkpoint_id != input.selected_checkpoint
                || authenticated.covered_seq != input.baseline_s
                || authenticated.cursor.as_str() != input.baseline_cursor
                || authenticated.epoch != activation_t.to_epoch
                || authenticated.signed_commit_sha256
                    != signed_commit_digest(&input.commit.signed_commit)
                || authenticated.manifest_object_id != input.commit.manifest_object_id
                || authenticated.manifest_object_digest != input.manifest_object_digest
            {
                return Err(SyncError::RelayWire(
                    "checkpoint 级认证材料与选中的 cid/S/commit 不一致",
                ));
            }
        }
    }

    // ④ checkpoint commit 自身：字段绑定 + 生产者签名。digest 只证明"同一份字节"，
    //    签名才证明生产者真的为这个 S/manifest/refs 签过。
    let commit = CheckpointCommit::decode(&input.commit.signed_commit)?;
    if commit.vault_id != input.vault_id
        || commit.checkpoint_id != input.selected_checkpoint
        || commit.epoch != activation_t.to_epoch
        // `CheckpointCommit.covered_relay_seq` 只受 freeze S 下界约束（候选可覆盖更晚的 S）；
        // 它必须与调用方选中的 S 一致，manifest/apply 才会把 cursor 推到同一个 S。
        || commit.covered_relay_seq < activation_t.freeze_seq
        || commit.covered_relay_seq != input.baseline_s
        || commit.manifest_object_id != input.commit.manifest_object_id
        || commit.manifest_digest != input.manifest_object_digest
    {
        return Err(SyncError::RelayWire(
            "checkpoint commit 字段/对象摘要与 T 或读回事实不符",
        ));
    }
    verify_signature(
        &input.commit.producer_sign_public,
        &checkpoint_signing_input(&commit)?,
        &input.commit.signature,
    )?;

    // ⑤ 本设备 latest grant：AAD/enc/ciphertext/外签 + 包 digest + T.grants_digest inclusion。
    //
    // **AAD.manager_rev 是旧管理 rev，不是推广目标 rev**：staging grant 由 relay 在
    // activate 之前写入，那时 `vault.manager_rev` 仍是旧值（relay `stage_grant` 对
    // 新 rev 返回 CAS_CONFLICT），grant 的 AAD 因此固定是 `T.old_manager_rev`。
    // `T.new_manager_rev` 只用于 promote/最终身份，绝不参与 staging grant 的校验。
    let grant = input.grant;
    let (aad, enc, ciphertext_sha256) = decode_self_grant_header(&grant.signed_header)?;
    if aad.vault_id != input.vault_id
        || aad.epoch != activation_t.to_epoch
        || aad.manager_rev != u64::from(activation_t.old_manager_rev)
        || aad.recipient_device_id != input.device_id
    {
        return Err(SyncError::RelayWire("self grant AAD 与 T/本地设备不绑定"));
    }
    if aad.recipient_x25519_pub != x25519_public_key(input.device_x25519_secret) {
        return Err(SyncError::RelayWire(
            "self grant recipient 公钥不是本设备 HPKE key",
        ));
    }
    let ciphertext_digest: [u8; 32] = Sha256::digest(&grant.ciphertext).into();
    if grant.device_id != input.device_id
        || grant.epoch != activation_t.to_epoch
        || grant.manager_rev != u64::from(activation_t.old_manager_rev)
        || grant.hpke_enc != enc
        || ciphertext_digest != ciphertext_sha256
    {
        return Err(SyncError::RelayWire(
            "self grant 字段与 signed_header/AAD 不符",
        ));
    }
    let grant_signature: [u8; 64] = grant
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::RelayWire("self grant 管理签名不是 64B"))?;
    verify_signature(
        &input.old_manager_public,
        &grant.signed_header,
        &grant_signature,
    )?;
    // 包 digest（relay `grants/latest` 键 10 口径）：若 relay 给了自报值，必须逐字节一致。
    let header_sha256: [u8; 32] = Sha256::digest(&grant.signed_header).into();
    let package_input = grant_package_digest(
        &grant.hpke_enc,
        &ciphertext_sha256,
        &header_sha256,
        &grant.signature,
    );
    let package_digest: [u8; 32] = Sha256::digest(&package_input).into();
    if grant
        .package_digest
        .is_some_and(|expected| expected != package_digest)
    {
        return Err(SyncError::RelayWire(
            "self grant 包 digest 与 relay 自报不符",
        ));
    }
    // inclusion：`T.grants_digest` 只覆盖 `(device_id, SHA256(signed_header))`。
    let owned_leaf = GrantLeafEvidence {
        device_id: input.device_id,
        signed_header_sha256: header_sha256,
    };
    match input.grant_leaves {
        Some(leaves) => {
            if !leaves.contains(&owned_leaf) {
                return Err(SyncError::RelayWire("本设备 grant 不在 staged leaf 集合内"));
            }
            if grant_set_digest(leaves) != activation_t.grants_digest {
                return Err(SyncError::RelayWire(
                    "完整 grant leaf 集合与 T.grants_digest 不符",
                ));
            }
        }
        None if input.grants_count == 1 => {
            if grant_set_digest(&[owned_leaf]) != activation_t.grants_digest {
                return Err(SyncError::RelayWire(
                    "本设备 grant 未被 T.grants_digest 覆盖",
                ));
            }
        }
        None => {
            return Err(SyncError::RelayWire(
                "缺完整 grant leaf manifest，无法证明 self grant 被 T.grants_digest 覆盖",
            ))
        }
    }
    Ok(commit)
}

/// 把 self grant 的 `signed_header` 拆成 `(AAD, enc32, SHA256(ciphertext))`。
///
/// AAD 是 `CDE(GrantAadV1)`；后 64B 固定是 `enc32 || ciphertext_sha256`
/// （`hpke::grant_signature_input` 口径）。三者必须与 relay 下发的
/// `hpke_enc`/`ciphertext` 逐字节一致，否则 grant 不在 T 授权的包内。
fn decode_self_grant_header(
    signed_header: &[u8],
) -> Result<(GrantAadV1, [u8; 32], [u8; 32]), SyncError> {
    if signed_header.len() < 64 {
        return Err(SyncError::RelayWire("self grant signed_header 过短"));
    }
    let aad_len = signed_header.len() - 64;
    let aad = GrantAadV1::decode(&signed_header[..aad_len])?;
    let enc: [u8; 32] = signed_header[aad_len..aad_len + 32]
        .try_into()
        .map_err(|_| SyncError::RelayWire("self grant enc 长度非法"))?;
    let ciphertext_sha256: [u8; 32] = signed_header[aad_len + 32..]
        .try_into()
        .map_err(|_| SyncError::RelayWire("self grant ciphertext 摘要长度非法"))?;
    Ok((aad, enc, ciphertext_sha256))
}

/// 重建出的 `ActivationCommitV1`（relay 键 20 的公开事实）必须与 T 逐字段一致。
///
/// relay 的 32B hash **不是**授权来源：它只说明"重建材料 == relay 自报材料"，
/// 真正的授权是双签 T。
///
/// **S 语义**：T 的 `freeze_seq` 是候选 checkpoint 的**下界**（冻结之后创建的候选可以
/// 覆盖更晚的 S），因此 `primary_covered_seq`/`fallback_covered_seq` 只要求
/// `>= T.freeze_seq`；`ActivationCommitV1.covered_seq` 仍是冻结 S，必须精确相等。
pub fn verify_activation_commit_matches_t(
    commit: &ActivationCommitV1,
    activation_t: &ActivationTV1,
) -> Result<(), SyncError> {
    if commit.vault_id != activation_t.vault_id
        || commit.instance_id != activation_t.instance_id
        || commit.rotation_id != activation_t.rotation_id
        || commit.from_epoch != activation_t.from_epoch
        || commit.to_epoch != activation_t.to_epoch
        || commit.covered_seq != activation_t.freeze_seq
        || commit.primary_checkpoint != activation_t.primary_cid
        || commit.fallback_checkpoint != activation_t.fallback_cid
        // 候选 checkpoint 的 S 只受 freeze S 下界约束（冻结后创建的候选可以更晚）。
        || commit.primary_covered_seq < activation_t.freeze_seq
        || commit.fallback_covered_seq < activation_t.freeze_seq
        || commit.grants_digest != activation_t.grants_digest
        || commit.asset_set_digest != Some(activation_t.old_asset_set_digest)
        || commit.control_rev != activation_t.expected_control_rev
    {
        return Err(SyncError::RelayWire(
            "重建的 activation commit 与 T 授权材料不一致",
        ));
    }
    Ok(())
}

/// MANIFEST 明文必须与 T 授权的 checkpoint commit / S / 根绑定。
pub fn verify_manifest_matches_commit(
    commit: &CheckpointCommit,
    manifest: &ManifestV1,
    vault_id: VaultId,
    key_epoch: u64,
    baseline_s: u64,
) -> Result<(), SyncError> {
    if manifest.vault_id != vault_id
        || manifest.checkpoint_id != commit.checkpoint_id
        || manifest.covered_relay_seq != baseline_s
        || manifest.key_epoch != key_epoch
        || commit.covered_relay_seq != baseline_s
    {
        return Err(SyncError::RelayWire("manifest 与 checkpoint/S/根不绑定"));
    }
    Ok(())
}

/// 网络入口（I 阶段生产路径）：**先取 latest activation T 与整条 activation 链**，
/// 逐环验证后才用链尾的 T 授权 apply，最后按 manifest_oid 下载 → merge → 推广。
///
/// T 的来源是 [`RelayClient::verified_activation_t`]（带默认 `Ok(None)`），链的来源是
/// [`RelayClient::epoch_activations`]（默认空链）：
/// - 真实客户端 `HttpRelayClient` 从已验证 `/control` 缓存产出 T 与每一环的位置/commit；
/// - `FakeRelay`/测试 wrapper 直接注入已复算证据；
/// - 不覆盖的客户端拿默认 `None`/空链 → 本函数一律 `Paused`（fail-closed），**绝不**
///   回退到"只信 relay 自报的 `ActivationCommitV1` 32B hash"。
///
/// **离线跨多 epoch**：本地 `current_epoch` 只可能等于链首/链中间某一环的 `from_epoch`；
/// latest T 的 `from_epoch` 不再要求等于本地 epoch，而是由
/// [`verify_epoch_activation_chain`] 从本地 epoch 逐环推进到链尾（要求链首正是本地
/// epoch、环与环首尾相接、每环双签/绑定/commit/链位置有效、链尾 == latest T）。
/// 中间 baseline/grant 不逐个下载，只有 latest 的基线被 merge。
///
/// 顺序（评审 0002 §三 I / 0003 P1-C）：重新鉴权/分页验控制链 → 取 latest T → 逐环验
/// activation 链 → 用链尾 T 交叉绑定 view/身份/管理权 → 取选中 checkpoint 的
/// `status`/`signed_commit` 证据与 `/bootstrap` pair → 下载/解密 MANIFEST →
/// **checkpoint 级认证**（[`verify_checkpoint_authorization`]）→ T 门禁（pair 路径的
/// commit digest 强绑定 / 非 pair 路径的认证对撞、S/cursor 下界、self grant inclusion）
/// → merge → 推广。链不完整/T 缺失/双签复算失败/认证失败：`Paused`，不写库、不推 cursor。
///
/// **两件事分开**（评审 0003 P1-C）：T/latest grant 授权的是"把根推广到哪个 epoch"；
/// "当前这份 checkpoint 内容是否合法"由 [`verify_checkpoint_authorization`] 独立认证。
/// 激活 pair 被普通健康 checkpoint 替换（relay `prune_retired` 会 prune 旧 pair）后，
/// 旧设备仍能消费当前 pair，而不是永久 `Paused`。
#[allow(clippy::too_many_arguments)]
pub fn apply_epoch_change_with_client<C: RelayClient>(
    client: &mut C,
    vault: &menghunote_storage::Vault,
    identity_path: &std::path::Path,
    identity: &crate::identity::LocalIdentityV1,
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    baseline_cursor: &str,
    candidate_root: &RootSecretV1,
    manager_public: [u8; 32],
    new_manager_public: [u8; 32],
    new_manager_rev: u64,
    restart_reason: Option<RestartReason>,
) -> Result<ApplyOutcome, SyncError> {
    let state = client.state()?;
    // same-epoch 重入（评审 0003 P1-C/P1-D）：rename 已提交、`applied` 未写时，relay 与
    // 本地身份已是同一 epoch，下面必然会看到 "no epoch change"。必须先凭 durable receipt
    // 做认证收尾，否则 engine 会被 `applying` 永久冻结。
    if state.epoch == identity.current_epoch {
        if let Some(outcome) = reconcile_same_epoch_apply(vault, identity_path, identity)? {
            return Ok(outcome);
        }
    }
    // §30.3 的 `/state` 键 15（current_rotation_id）只对 frozen/staging 返回；activate 之后
    // 它不再出现。此时必须回落到 `GET /rotations/current`（relay 返回最新 rotation 视图），
    // 再由 `state.epoch > local` + 控制链 `epoch.activated` 判定，不能因为没有
    // current_rotation_id 就 Paused。
    let view = match state.current_rotation_id {
        Some(rotation_id) => client.rotation(rotation_id)?,
        None if state.epoch > identity.current_epoch => match client.current_rotation()? {
            Some(view) => view,
            None => {
                return Ok(ApplyOutcome::Paused {
                    reason: "relay epoch 已前进但找不到 rotation 视图".to_owned(),
                })
            }
        },
        None => {
            return Ok(ApplyOutcome::Paused {
                reason: "no epoch change".to_owned(),
            })
        }
    };
    let control = client.control_kinds()?;
    match detect_epoch_change(&state, &view, &control, identity.current_epoch) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Ok(ApplyOutcome::Paused {
                reason: "no epoch change".to_owned(),
            })
        }
        Err(error) => {
            return Ok(ApplyOutcome::Paused {
                reason: error.to_string(),
            })
        }
    }
    if let Some(reason) = restart_reason {
        // restart 不消费 baseline，不需要 T；它只要求 relay 分到更高 epoch。
        return Ok(ApplyOutcome::NeedsRestart {
            to_epoch: view.to_epoch + 1,
            reason,
        });
    }
    if state.instance_id != identity.pinned_instance {
        return Ok(ApplyOutcome::Paused {
            reason: "relay instance 与本地 pin 不符（必须重新 pin/bootstrap）".to_owned(),
        });
    }
    // ① 先取"已复算"的 activation T（旧/新管理双签已过）。缺失/未验证 → 拒绝：
    //    绝不再用 relay 自报的 32B activation hash 当授权（评审 0002 P1-1.4）。
    let evidence = client.verified_activation_t(view.rotation_id)?;
    let Some(evidence) = evidence else {
        return Ok(ApplyOutcome::Paused {
            reason: "缺少可复算的 activation T（/control 无 T 或双签未通过），拒绝 apply"
                .to_owned(),
        });
    };
    // ② 离线跨多 epoch：latest T 的 `from_epoch` 只可能等于本地 epoch 之后的某一环，
    //    必须先从本地可信 `current_epoch` 沿 `/control` 的 `epoch.activated` 逐环验证，
    //    再由链尾 T 授权 apply。链缺失/断裂/位置不符/链尾不是 latest → fail-closed。
    let chain = {
        let links = match client.epoch_activations(identity.current_epoch) {
            Ok(links) => links,
            // 传输类错误保持 `Err`（可重试）；认证/链材料错误一律 Paused 且不写库。
            Err(error @ SyncError::Transport(_)) => return Err(error),
            Err(error) => {
                return Ok(ApplyOutcome::Paused {
                    reason: error.to_string(),
                })
            }
        };
        match verify_epoch_activation_chain(
            &links,
            &evidence,
            identity.vault_id,
            state.instance_id,
            identity.current_epoch,
            manager_public,
        ) {
            Ok(chain) => chain,
            Err(error) => {
                return Ok(ApplyOutcome::Paused {
                    reason: error.to_string(),
                })
            }
        }
    };
    // 链尾 T 的新管理 key/rev 必须正是本次推广材料：否则 apply 会把身份推到错误的
    // 管理权（例如 relay 在链尾之后又换了 key，而调用方还拿着旧材料）。
    if chain.tail_new_manager_public != new_manager_public
        || chain.tail_new_manager_rev != new_manager_rev
    {
        return Ok(ApplyOutcome::Paused {
            reason: "activation 链尾的新管理 key/rev 与推广材料不符".to_owned(),
        });
    }
    // 链首的旧管理 rev 必须锚定在**本地可信身份旁路状态**上：链内相邻环的
    // `old == 上一环 new` 只保证相对自洽，relay 仍可给出整体错位的 rev 序列
    // （例如把 T.old 抬成尚未生效的 rev）。旁路状态缺失/损坏时 fail-closed，
    // 绝不拿 T 自报的 rev 当锚点。
    let local_manager_rev = match IdentityRotationStateV1::load_current(vault, identity) {
        Ok(Some(rotation)) => rotation.manager_rev,
        // 从未轮换过的 vault：身份仍在 genesis epoch，管理 rev 固定为 1（无旁路记录）。
        Ok(None) if identity.current_epoch == crate::genesis::GENESIS_EPOCH => {
            crate::genesis::GENESIS_MANAGER_REV
        }
        Ok(None) => {
            return Ok(ApplyOutcome::Paused {
                reason: "缺少身份旁路状态，无法锚定 activation 链首的管理 rev".to_owned(),
            })
        }
        Err(error) => {
            return Ok(ApplyOutcome::Paused {
                reason: format!("身份旁路状态读取失败：{error}"),
            })
        }
    };
    if chain.head_old_manager_rev != local_manager_rev {
        return Ok(ApplyOutcome::Paused {
            reason: format!(
                "activation 链首的旧管理 rev={} 与本地管理链位置 rev={local_manager_rev} 不符",
                chain.head_old_manager_rev
            ),
        });
    }
    // 后续所有授权都只看**链尾**的 T（链验证已要求链尾与 latest T 逐字节相等）。
    let VerifiedActivationChain {
        tail: evidence,
        tail_from_epoch,
        tail_old_manager_public,
        ..
    } = chain;
    let leaves = client.grant_leaf_evidence(view.rotation_id)?;
    let grant = client.self_grant_evidence()?;
    let Some(activation_digest) = view.activation_commit.as_deref() else {
        return Ok(ApplyOutcome::Paused {
            reason: "view 缺少 activation commit".to_owned(),
        });
    };
    // 键 20 是 `ActivationCommitV1` 的 SHA256，不是 CDE：先按 view/checkpoint 公开事实
    // 重建 commit 材料。它**只做**与 T 的一致性交叉验证，不再是授权来源。
    let (primary_checkpoint, fallback_checkpoint) =
        match (view.primary_checkpoint, view.fallback_checkpoint) {
            (Some(primary), Some(fallback)) => (primary, fallback),
            _ => {
                return Ok(ApplyOutcome::Paused {
                    reason: "view 缺少 primary/fallback checkpoint".to_owned(),
                })
            }
        };
    let primary = client.checkpoint_status(primary_checkpoint)?;
    let fallback = client.checkpoint_status(fallback_checkpoint)?;
    let commit = match activation_commit_materials(
        identity.vault_id,
        state.instance_id,
        &view,
        &primary,
        &fallback,
    ) {
        Ok(commit) => commit,
        Err(error) => {
            return Ok(ApplyOutcome::Paused {
                reason: error.to_string(),
            })
        }
    };
    let commit_bytes = commit.encode()?;
    if let Err(error) = verify_activation_commit(
        &commit_bytes,
        activation_digest,
        identity.vault_id,
        state.instance_id,
        view.rotation_id,
        view.from_epoch,
        view.to_epoch,
        commit.control_rev,
    ) {
        return Ok(ApplyOutcome::Paused {
            reason: error.to_string(),
        });
    }
    // T 才是授权来源：重建材料必须与 T 逐字段一致（epoch/S/pair/grants/asset/control_rev）。
    if let Err(error) = verify_activation_commit_matches_t(&commit, &evidence.activation_t) {
        return Ok(ApplyOutcome::Paused {
            reason: error.to_string(),
        });
    }
    client.reauthenticate()?;
    // relay `GET /checkpoints/{cid}` 键 4 只是 MANIFEST 的 **object id**（api.rs:1859–1860），
    // 不是 manifest 明文；按 oid 下载、解密并校验后才能交给 `collect_manifest_payload`。
    let status = client.checkpoint_status(checkpoint_id)?;
    if status.checkpoint_id != checkpoint_id
        || status.covered_seq != baseline_s
        || status.epoch != view.to_epoch
    {
        return Ok(ApplyOutcome::Paused {
            reason: "checkpoint 状态与选中 cid/S/epoch 不符".to_owned(),
        });
    }
    let Some(manifest_oid) = status.manifest_object_id else {
        return Ok(ApplyOutcome::Paused {
            reason: "checkpoint 缺少 manifest_oid".to_owned(),
        });
    };
    // ② 选中基线的 `signed_commit` 与生产者签名只能从 relay 公开的 `/bootstrap`
    //    主/备条目取（`GET /checkpoints/{cid}` 的 commit 原字节不经 `RelayClient` 透出），
    //    relay 游标也只在这里给。
    //
    //    **降级 bootstrap 不得用于根推广**：degraded 是"只允许显式请求"的诊断模式，
    //    其条目可能已被 prune/回收，不能作为一次身份推广的授权材料。
    //    < 2 份健康基线时 relay 返回业务错误 → 转 `Paused`（保持冻结、下次从新 pair 重试，
    //    绝不回落到旧全量基线或静默接受）。传输类错误仍 `Err`（可重试）。
    let bundle = match client.bootstrap() {
        Ok(bundle) => bundle,
        Err(error @ SyncError::Transport(_)) => return Err(error),
        Err(error) => {
            return Ok(ApplyOutcome::Paused {
                reason: format!("无法取得 relay /bootstrap 主/备基线：{error}"),
            })
        }
    };
    if bundle.degraded {
        return Ok(ApplyOutcome::Paused {
            reason: "relay bootstrap 处于 degraded 模式，禁止用于根推广".to_owned(),
        });
    }
    if bundle.epoch != view.to_epoch {
        return Ok(ApplyOutcome::Paused {
            reason: "bootstrap epoch 与 T 的 to_epoch 不符".to_owned(),
        });
    }
    // 本地水位不得回退：`finish_baseline_apply` 只做 max+CAS，会"导入旧快照但保留高水位"，
    // 于是 receipt 记的 S 与实际消费位置不一致（随后 pull 重复/跳段）。这是防状态不一致，
    // 不是防丢数据（Loro merge 是并集）。
    let local = menghunote_storage::sync_cursor_state(vault)?;
    let local_highwater = local.confirmed_highwater.unwrap_or(0);
    if baseline_s < local.integrated_seq || baseline_s < local_highwater {
        return Ok(ApplyOutcome::Paused {
            reason: format!(
                "选中基线 S={baseline_s} 低于本地水位（integrated_seq={}，confirmed_highwater={local_highwater}），禁止回退",
                local.integrated_seq
            ),
        });
    }
    // 目标 cid 必须是**当前** `/bootstrap` pair 之一：relay 游标只在这里给，且
    // `checkpoint_refs` 被 prune 的旧 cid 已无法作为续读起点。
    let entry = if bundle.primary.checkpoint_id == checkpoint_id {
        &bundle.primary
    } else if bundle.fallback.checkpoint_id == checkpoint_id {
        &bundle.fallback
    } else {
        return Ok(ApplyOutcome::Paused {
            reason:
                "选中基线不在 relay bootstrap 的主/备 pair（无法取得该 S 的游标/signed_commit）"
                    .to_owned(),
        });
    };
    if entry.covered_seq != baseline_s
        || entry.cursor != baseline_cursor
        || entry.manifest_oid != manifest_oid
    {
        return Ok(ApplyOutcome::Paused {
            reason: "bootstrap 条目与选中 cid/S/cursor/manifest 不符".to_owned(),
        });
    }
    let Some(signed_commit) = entry.signed_commit.clone() else {
        return Ok(ApplyOutcome::Paused {
            reason: "checkpoint 缺 signed_commit（无法复算 commit digest/签名）".to_owned(),
        });
    };
    let Some(signature) = entry.signature else {
        return Ok(ApplyOutcome::Paused {
            reason: "checkpoint 缺 commit 生产者签名".to_owned(),
        });
    };
    let baseline_commit = match CheckpointCommit::decode(&signed_commit) {
        Ok(commit) => commit,
        Err(error) => {
            return Ok(ApplyOutcome::Paused {
                reason: error.to_string(),
            })
        }
    };
    let Some(producer_cert) = client.device_cert(&baseline_commit.producer_device_id)? else {
        return Ok(ApplyOutcome::Paused {
            reason: "找不到 checkpoint 生产者设备证书".to_owned(),
        });
    };
    if producer_cert.vault_id != identity.vault_id
        || producer_cert.device_id != baseline_commit.producer_device_id
    {
        return Ok(ApplyOutcome::Paused {
            reason: "checkpoint 生产者证书与 vault/commit 不符".to_owned(),
        });
    }
    let commit_evidence = CheckpointCommitEvidence {
        checkpoint_id: entry.checkpoint_id,
        covered_seq: entry.covered_seq,
        cursor: entry.cursor.clone(),
        signed_commit,
        signature,
        producer_sign_public: producer_cert.sign_ed25519_pub,
        manifest_object_id: manifest_oid,
    };
    let (manifest_bytes, manifest_object_digest) =
        fetch_manifest_object(client, candidate_root, &manifest_oid)?;
    let manifest = ManifestV1::decode(&manifest_bytes)?;
    // ③ checkpoint 级认证（评审 0003 P1-C 的第二半）：认证"这份 checkpoint 内容"。
    //    `cid ∈ T.pair` 时 T 的双签已授权该 commit，但**不**因此跳过认证（published/
    //    refs/S 边界仍是必须的自洽检查），只是不额外要求 active 生产者资格——否则生产者
    //    在冻结后被吊销，冻结期唯一合法基线会永久不可消费。
    //    `cid ∉ T.pair`（激活 pair 已被普通健康 checkpoint 替换）时，本认证是该内容
    //    唯一的授权来源，必须**同时**满足 active 成员资格。
    let in_t_pair = checkpoint_id == evidence.activation_t.primary_cid
        || checkpoint_id == evidence.activation_t.fallback_cid;
    let authenticated = match verify_checkpoint_authorization(
        client,
        identity.vault_id,
        entry,
        &status,
        &baseline_commit,
        &manifest,
        manifest_object_digest,
        view.to_epoch,
        evidence.activation_t.freeze_seq,
        &bundle,
        &state,
        !in_t_pair,
    ) {
        Ok(auth) => auth,
        Err(error) => {
            return Ok(ApplyOutcome::Paused {
                reason: error.to_string(),
            })
        }
    };
    // ④ T/latest grant 授权门禁：commit digest（pair 路径强绑定，不许降级）、S/cursor、
    //    生产者签名、self grant inclusion 一次验完。
    let parsed_commit = match verify_activation_t_gate(
        &ActivationTGate {
            evidence: &evidence,
            view: &view,
            vault_id: identity.vault_id,
            instance_id: state.instance_id,
            expected_from_epoch: tail_from_epoch,
            device_id: identity.device_id,
            device_x25519_secret: &identity.x25519_secret,
            // latest grant 由**链尾 T 的旧管理 key**签发：多轮时它不是本地旧 pin，
            // 而是上一环 T 的新管理公钥（由 [`verify_epoch_activation_chain`] 推出）。
            old_manager_public: tail_old_manager_public,
            new_manager_public,
            new_manager_rev,
            selected_checkpoint: checkpoint_id,
            baseline_s,
            baseline_cursor,
            commit: &commit_evidence,
            manifest_object_digest,
            grant: &grant,
            grant_leaves: leaves.as_deref(),
            grants_count: view.grants_count,
        },
        Some(&authenticated),
    ) {
        Ok(commit) => commit,
        Err(error) => {
            return Ok(ApplyOutcome::Paused {
                reason: error.to_string(),
            })
        }
    };
    if let Err(error) = verify_manifest_matches_commit(
        &parsed_commit,
        &manifest,
        identity.vault_id,
        candidate_root.key_epoch,
        baseline_s,
    ) {
        return Ok(ApplyOutcome::Paused {
            reason: error.to_string(),
        });
    }
    let payload = collect_manifest_payload(
        client,
        candidate_root,
        checkpoint_id,
        baseline_s,
        &manifest_bytes,
    )?;
    if candidate_root.key_epoch != commit.to_epoch {
        return Ok(ApplyOutcome::Paused {
            reason: format!(
                "候选根 epoch={} 与 activation to_epoch={} 不符",
                candidate_root.key_epoch, commit.to_epoch
            ),
        });
    }
    let activation_hash: [u8; 32] = Sha256::digest(commit.encode()?).into();
    // 完整 gate 已通过：把授权事实与已下载基线绑定成**不可外部构造**的输入，再交给
    // 纯入口完成 applying/receipt/promote/applied。这样公开入口不再接受可变的
    // `BaselinePayload` 作为授权输入（评审 0003 P1-C）。
    let verified = VerifiedApplyPayload::from_gate(
        commit.rotation_id,
        checkpoint_id,
        baseline_s,
        baseline_cursor,
        commit.from_epoch,
        commit.to_epoch,
        commit.control_rev,
        activation_hash,
        state.instance_id,
        new_manager_public,
        new_manager_rev,
        payload,
    );
    apply_epoch_change(vault, identity_path, identity, &verified, candidate_root)
}

/// B 看到 epoch 变化后的下一步动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EpochAction {
    /// 正常应用：验证并解根，等 baseline merge 完成后再推广。
    Apply {
        rotation_id: [u8; 16],
        from_epoch: u64,
        to_epoch: u64,
    },
    /// 原子 restart：更高 epoch、fresh root/slots/R'/管理 key；旧写保持冻结。
    Restart {
        next_epoch: u64,
        reason: RestartReason,
    },
}

/// restart 原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartReason {
    /// 候选根泄露。
    CandidateRootLeaked,
    /// 收件人被吊销。
    RecipientRevoked,
}

/// 纯检测：relay state 的 epoch 高于本地，且控制链里有 `epoch.activated` 记录。
///
/// relay 的 typed kind 是 `epoch.activated`（`crates/relay/src/control.rs` 的
/// `ControlKind::as_str`）；若 view 带 `activated_control_rev`，还要求该 rev 上确实是
/// `epoch.activated`，避免"任取一条历史激活记录"。
pub fn detect_epoch_change(
    state: &RelayState,
    view: &RotationView,
    control_kinds: &[(u64, String)],
    current_epoch: u64,
) -> Result<Option<RotationView>, SyncError> {
    if state.epoch <= current_epoch {
        return Ok(None);
    }
    if view.state != "activated" || view.to_epoch != state.epoch {
        return Err(SyncError::Object(
            "relay epoch 已变但 rotation 未 activated",
        ));
    }
    let has_activated = match view.activated_control_rev {
        Some(rev) => control_kinds
            .iter()
            .any(|(record_rev, kind)| *record_rev == rev && kind == "epoch.activated"),
        None => control_kinds
            .iter()
            .any(|(_, kind)| kind == "epoch.activated"),
    };
    if !has_activated {
        return Err(SyncError::Object("缺少 epoch.activated 控制记录"));
    }
    Ok(Some(view.clone()))
}

/// 用 view + 两个候选 checkpoint 的公开元数据重建 `ActivationCommitV1`。
///
/// relay 的 `GET rotations/{r}` 只给 commit 的 SHA256（键 20），不给 CDE 明文；客户端必须
/// 用公开事实重建后再比 hash（见 `verify_activation_commit`）。
pub fn activation_commit_materials(
    vault_id: VaultId,
    instance_id: [u8; 16],
    view: &RotationView,
    primary: &CheckpointStatus,
    fallback: &CheckpointStatus,
) -> Result<ActivationCommitV1, SyncError> {
    let primary_checkpoint = view
        .primary_checkpoint
        .ok_or(SyncError::RelayWire("view 缺 primary checkpoint"))?;
    let fallback_checkpoint = view
        .fallback_checkpoint
        .ok_or(SyncError::RelayWire("view 缺 fallback checkpoint"))?;
    if primary.checkpoint_id != primary_checkpoint || fallback.checkpoint_id != fallback_checkpoint
    {
        return Err(SyncError::RelayWire(
            "checkpoint 状态与 view 指名的 cid 不符",
        ));
    }
    let primary_manifest = primary
        .manifest_object_id
        .ok_or(SyncError::RelayWire("primary checkpoint 缺 manifest_oid"))?;
    let fallback_manifest = fallback
        .manifest_object_id
        .ok_or(SyncError::RelayWire("fallback checkpoint 缺 manifest_oid"))?;
    Ok(ActivationCommitV1 {
        vault_id,
        instance_id,
        rotation_id: view.rotation_id,
        from_epoch: view.from_epoch,
        to_epoch: view.to_epoch,
        covered_seq: view.freeze_seq,
        primary_checkpoint,
        fallback_checkpoint,
        primary_covered_seq: primary.covered_seq,
        fallback_covered_seq: fallback.covered_seq,
        primary_manifest,
        fallback_manifest,
        member_set_hash: view
            .member_set_hash
            .ok_or(SyncError::RelayWire("view 缺 member_set_hash"))?,
        grants_digest: view
            .grants_digest
            .ok_or(SyncError::RelayWire("view 缺 grants_digest"))?,
        asset_set_digest: view.freeze_asset_digest,
        // relay 的 `Rotations::activate` 用 `activated_control_rev` 记录 commit 的 control_rev；
        // 后续控制记录会推进 view.control_rev，因此必须优先用激活时的值。
        control_rev: view
            .activated_control_rev
            .ok_or(SyncError::RelayWire("view 缺 activated_control_rev"))?,
    })
}

/// 校验激活 commit：绑定字段 + `SHA256(CDE(ActivationCommitV1)) == relay 字段`。
///
/// `expected_digest` 必须是 relay 返回的 32B SHA256；**不得**把 hash 当 CDE 解码。
#[allow(clippy::too_many_arguments)]
pub fn verify_activation_commit(
    commit_bytes: &[u8],
    expected_digest: &[u8],
    vault_id: VaultId,
    instance_id: [u8; 16],
    rotation_id: [u8; 16],
    from_epoch: u64,
    to_epoch: u64,
    control_rev: u64,
) -> Result<ActivationCommitV1, SyncError> {
    let commit = ActivationCommitV1::decode(commit_bytes)?;
    if commit.vault_id != vault_id
        || commit.instance_id != instance_id
        || commit.rotation_id != rotation_id
        || commit.from_epoch != from_epoch
        || commit.to_epoch != to_epoch
        || commit.control_rev != control_rev
    {
        return Err(SyncError::RelayWire("激活 commit 与 rotation/身份不绑定"));
    }
    if commit.primary_checkpoint == commit.fallback_checkpoint {
        return Err(SyncError::RelayWire("激活 commit 的两 cid 相同"));
    }
    if expected_digest.len() != 32 {
        return Err(SyncError::RelayWire(
            "relay activation_commit 不是 32B SHA256",
        ));
    }
    // 摘要按 canonical CDE 重算（等价于 relay `ActivationCommit::digest()`）。
    let canonical = commit.encode()?;
    let actual: [u8; 32] = Sha256::digest(&canonical).into();
    if actual.as_slice() != expected_digest {
        return Err(SyncError::RelayWire(
            "activation_commit 与重建的 ActivationCommitV1 不符",
        ));
    }
    Ok(commit)
}

/// 校验 B 自己的 grant 头：recipient/epoch 必须匹配，manager 外签必须由新管理公钥验过。
pub fn verify_self_grant_header(
    signed_header: &[u8],
    signature: &[u8],
    manager_public: &[u8; 32],
    device_id: DeviceId,
    epoch: u64,
) -> Result<(), SyncError> {
    if signed_header.len() < 64 || signature.len() != 64 {
        return Err(SyncError::RelayWire("self grant 签名长度非法"));
    }
    let aad_len = signed_header.len() - 64;
    let aad = menghunote_sync_proto::GrantAadV1::decode(&signed_header[..aad_len])?;
    if aad.epoch != epoch || aad.recipient_device_id != device_id {
        return Err(SyncError::RelayWire("self grant recipient/epoch 不符"));
    }
    let signature: [u8; 64] = signature
        .try_into()
        .map_err(|_| SyncError::RelayWire("self grant 签名长度非法"))?;
    crate::device::verify_signature(manager_public, signed_header, &signature)
}

/// restart 判定：候选根泄露或收件人被吊销 → 更高 epoch；否则正常应用。
pub fn decide_epoch_action(
    view: &RotationView,
    current_epoch: u64,
    restart_reason: Option<RestartReason>,
) -> Result<EpochAction, SyncError> {
    if view.state != "activated" {
        return Err(SyncError::Object("epoch 变化但 rotation 未 activated"));
    }
    if view.to_epoch <= current_epoch {
        return Err(SyncError::Object("目标 epoch 不高于当前 epoch"));
    }
    if let Some(reason) = restart_reason {
        return Ok(EpochAction::Restart {
            next_epoch: view.to_epoch + 1,
            reason,
        });
    }
    Ok(EpochAction::Apply {
        rotation_id: view.rotation_id,
        from_epoch: view.from_epoch,
        to_epoch: view.to_epoch,
    })
}
