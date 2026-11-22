//! P3-J restart：候选根泄露/收件人被吊销时，生成更高 epoch 的全新秘密材料并走完整执行链。
//!
//! 严格顺序（ADR 0006 §33、§28.2 resume/restart、评审 0001 §二、评审 0002 P1-2/P1-D）：
//!
//! 1. **POST 之前**先把公开 intent durable 落盘（旧 rid/request_id/from/旧候选/CAS/
//!    冻结点/instance/冻结屏障=1），**不含**任何新 epoch 或秘密；
//! 2. 再调 relay `POST /rotations/{old}/restart`；从**实际响应**读取 `from_epoch`
//!    （active）、`previous_candidate_epoch`（旧 view 的 `to_epoch`）、`new_to_epoch`
//!    （relay 分配）、`rotation_id`、`control_rev`；客户端**绝不本地 +1**；
//! 3. POST 丢响应/进程被杀：重启后按 intent 的 `request_id` 查 relay 的 exact restart
//!    结果（`/rotations/current?request_id=` + `management/restart` scope）；查不到就
//!    fail-closed，绝不盲目重发；只有拿到 relay 分配后才生成 fresh root/password/R'/
//!    新管理 key，并把它们与固定槽密文、epoch、rotation_id、phase 写入 DPAPI journal；
//! 4. 重试只从 journal 重建同一份秘密与固定密文，不重新生成根；
//! 5. 冻结资产（含墓碑/无引用）在新根下重新下载→解密→真实 SHA256 校验→重加密，
//!    并按 relay 口径重算候选 epoch 的 `asset_set_digest`（不信任 relay 单值）；
//! 6. 两份**互不共享状态**的 full-history 基线（primary/fallback）各自从 relay 读回验证：
//!    对象身份独立、`S ≥ freeze_seq`、资产集合等于冻结集合；**primary 一完成就单独
//!    落 journal**（`RestartPrimaryBaselineReady`），fallback 失败/被杀后只重建 fallback，
//!    复用同一 primary cid/密文，不撞 relay 每 epoch role 唯一 CAS；
//! 7. activation T 的 `primary_commit_digest`/`fallback_commit_digest` **只取 journal 里
//!    durable 的本地 receipt**，绝不重新从 relay facts 读取后重签；
//! 8. 从控制链 active 成员证书取**未吊销**设备 HPKE 公钥，封新根并 stage
//!    （被吊销设备不生成 grant，绝对不封给吊销设备）；grant leaf 固定字节先落 journal；
//! 9. `ManagerTransitionV1` 旧/新管理 key **双签**，member/slot/grant/asset 四个 digest
//!    按 restart 新材料重算后 activate；activate 前必须 `recovery_backup_confirmed=true`；
//! 10. 只有 relay `activated` 回执（或超时后 `/rotations/current` 明确 activated）才把
//!     journal 推到 `RestartActivated`，**不**在本地推广身份（阶段 H 由调用方接）。
//!
//! **失败语义**：任何一步失败都保持 `sync.repack_required=1`、旧写冻结、不推进 cursor、
//! 不推广身份；journal 停在最后完成的 phase，重启后从 journal 续跑。
//!
//! **边界（不在本模块）**：
//!
//! - 不合并基线、不推广本地身份（阶段 H/I），由调用方用既有
//!   `rotation_activate::promote_identity_atomic` / `rotation_apply::apply_then_promote` 接；
//! - 不动 outbox：被吊销设备的离线 ops **不**自动重包（评审 0001 §二：只能显式导出后以
//!   新身份/新 ops 恢复）；未吊销设备的旧写也只在新 epoch 推广后按阶段 J 重打包。

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::checkpoint::{AssetSource, PreparedCheckpoint};
use crate::device::{DeviceSigningKey, ManagementKey};
use crate::error::SyncError;
use crate::http_relay::HttpTransport;
use crate::keys::{RootSecretV1, Secret32};
use crate::recovery::RecoverySeed;
use crate::relay::RelayClient;
use crate::rotation_activate::{activate_http, ActivatedOutcome, ActivationInput};
use crate::rotation_journal::{
    JournalBaseline, JournalRestartAsset, RestartAssignment, RestartIntent, RotationJournal,
    RotationPhase,
};
use crate::rotation_slots::{
    active_member_hpke_keys, build_manager_transition, candidate_asset_set_digest,
    checkpoint_commit_digest, frozen_asset_set_digest, generate_candidate_slots, grants_digest,
    slot_set_digest, upload_slot_with_reservation, ActivationTMaterial, CandidateSlots,
};
use crate::rotation_stage::{
    assert_independent_baselines, capture_baseline_evidence, fetch_frozen_assets, plan_grants,
    reencrypt_frozen_assets, stage_grants, BaselineEvidence, GrantLeaf, ReencryptedAsset,
};
use crate::slot::{KeySlotV1, WrappedRoot};
use crate::{DeviceId, VaultId};
use menghunote_storage::Vault;
use menghunote_sync_proto::{
    activation_t_signing_input, ActivationTV1, ControlEvent, ControlKind, HttpMethod,
    MemberCertDigestInputV1, NewManagerV1, SignedControlRecord, SlotDescriptorV1, SlotKind,
};

/// 旧写冻结屏障的 meta 键。
const REPACK_REQUIRED_META: &str = "sync.repack_required";
/// 未确认新恢复码时拒绝 activate 的稳定错误文本。
const BACKUP_UNCONFIRMED: &str = "新恢复码未确认备份：拒绝 activate（保持冻结）";
/// activate 可能已生效、但 relay 已拒绝旧管理 key 且本地没有新 key 时的稳定错误文本。
const ACTIVATE_NEEDS_NEW_MANAGER: &str = "activate 可能已生效但 relay 已拒绝旧管理 key：缺少 journal manager_seed 派生的新管理 key，无法确认 activated 回执（重启后按 journal 续跑）";
/// journal 与 relay restart 契约不一致的稳定错误文本。
const CONTRACT_MISMATCH: &str = "restart journal 与 relay 契约不一致（旧候选不可复用）";
/// 旧 journal 没有 POST 前 durable intent 时 fail-closed。
const INTENT_MISSING: &str =
    "restart journal 缺 POST 前 durable intent：拒绝按旧 journal 盲目恢复（保持冻结）";
/// 有 intent 但 relay 查不到该 exact request 的结果时 fail-closed（绝不盲目重发 POST）。
const INTENT_UNRESOLVED: &str =
    "restart intent 已持久化但 relay 无该 request_id 的 restart 结果：拒绝盲目 POST（保持冻结）";

/// relay restart 视图里本模块消费的字段。
///
/// 真实 relay 的 `POST /rotations/{r}/restart` 响应是 `{0: rotation view, 1: true}`；
/// `rotation view` 的键为 `0 rotation_id / 1 state / 2 from_epoch / 3 to_epoch /
/// 4 freeze_seq / ... / 10 control_rev / ... / 12 manager_rev`。旧候选 epoch 不在
/// restart 响应里，必须取 restart **前**旧 rotation view 的 `to_epoch`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartRotationView {
    /// rotation id。
    pub rotation_id: [u8; 16],
    /// relay 状态文本（`frozen` / `staging` / `aborted` / `activated`）。
    pub state: String,
    /// relay 保留的 active epoch（restart 的 `from_epoch`）。
    pub from_epoch: u64,
    /// 该 rotation 的候选 epoch（restart 后新 rotation 的 `new_to_epoch`）。
    pub to_epoch: u64,
    /// 冻结点 S（restart 保持原值）。
    pub freeze_seq: u64,
    /// restart 后的控制链 rev（activate CAS 的期望值）。
    pub control_rev: u64,
    /// 管理 key rev（restart 不改管理权）。
    pub manager_rev: u64,
}

impl RestartRotationView {
    /// 从完整 relay `RotationView` 提取本模块消费的字段（不猜缺省值）。
    #[must_use]
    pub fn from_relay_view(view: &crate::relay::RotationView) -> Self {
        Self {
            rotation_id: view.rotation_id,
            state: view.state.clone(),
            from_epoch: view.from_epoch,
            to_epoch: view.to_epoch,
            freeze_seq: view.freeze_seq,
            control_rev: view.control_rev,
            manager_rev: view.manager_rev,
        }
    }
}

/// relay restart 的完整事实：restart 前旧 view + restart 后新 view。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartRelayResponse {
    /// restart 前的旧 rotation（候选 epoch = `to_epoch`；response 丢失时重试可能是 aborted）。
    pub previous: RestartRotationView,
    /// restart 后 relay 返回的新 rotation（fresh 候选 epoch = `to_epoch`）。
    pub restarted: RestartRotationView,
}

/// restart 的确定性契约（全部来自 relay 响应/rotation view，禁止本地推导）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartContract {
    /// relay 保留的 active epoch（`from_epoch`）。
    pub from_epoch: u64,
    /// 被原子废弃的旧候选 epoch（旧 rotation view 的 `to_epoch`）。
    pub previous_candidate_epoch: u64,
    /// relay 新分配的候选 epoch（新 rotation view 的 `to_epoch`）。
    pub new_to_epoch: u64,
    /// relay 新分配的 rotation id。
    pub rotation_id: [u8; 16],
    /// restart 后的控制链 rev（activate CAS 的期望值；不是本地 `+1`）。
    pub control_rev: u64,
}

impl RestartContract {
    /// 从 relay **实际** restart 响应构造契约，并校验三项 epoch 严格递增。
    ///
    /// 错误示例（必须拒绝）：把 active 当 `to-1`（旧候选=2、新候选=2），或把旧候选当新候选。
    pub fn from_relay_response(response: &RestartRelayResponse) -> Result<Self, SyncError> {
        let previous = &response.previous;
        let restarted = &response.restarted;
        if previous.rotation_id == restarted.rotation_id {
            return Err(SyncError::RelayWire("restart 必须分配新的 rotation_id"));
        }
        if !matches!(previous.state.as_str(), "frozen" | "staging" | "aborted") {
            return Err(SyncError::RelayWire("restart 前旧 rotation 状态非法"));
        }
        if !matches!(restarted.state.as_str(), "frozen" | "staging") {
            return Err(SyncError::RelayWire(
                "restart 响应不是 frozen/staging 的新 rotation",
            ));
        }
        if previous.from_epoch != restarted.from_epoch {
            return Err(SyncError::RelayWire("restart 改变了 active from_epoch"));
        }
        if previous.to_epoch <= previous.from_epoch {
            return Err(SyncError::RelayWire("旧候选 epoch 不大于 active epoch"));
        }
        if restarted.to_epoch <= previous.to_epoch {
            return Err(SyncError::RelayWire(
                "relay 新分配的候选 epoch 必须高于被废弃的旧候选（禁止本地 +1）",
            ));
        }
        if previous.freeze_seq != restarted.freeze_seq {
            return Err(SyncError::RelayWire("restart 改变了冻结点 S"));
        }
        // 注意：旧 rotation view 的 `control_rev`/`manager_rev` 是 vault **当前**全局值，
        // 丢响应重试时它可能已经包含 restart（甚至 activate）后的推进；不能拿它做
        // 单调性判断。契约的 control_rev 只取 restart 响应（新 rotation view），
        // 再由 `validate_local` 与本地 CAS 假设逐值校验。
        Ok(Self {
            from_epoch: restarted.from_epoch,
            previous_candidate_epoch: previous.to_epoch,
            new_to_epoch: restarted.to_epoch,
            rotation_id: restarted.rotation_id,
            control_rev: restarted.control_rev,
        })
    }

    /// 契约自身必须满足 `active < 旧候选 < 新候选`（三段都来自 relay）。
    pub fn validate_progression(&self) -> Result<(), SyncError> {
        if !(self.from_epoch < self.previous_candidate_epoch
            && self.previous_candidate_epoch < self.new_to_epoch)
        {
            return Err(SyncError::RelayWire(
                "restart epoch 必须 active < 旧候选 < 新候选（禁止本地 +1）",
            ));
        }
        Ok(())
    }

    /// 与本地身份/请求假设交叉校验：relay 返回的 epoch 与本地假设不一致 → 拒绝。
    pub fn validate_local(
        &self,
        old_root: &RootSecretV1,
        expected_control_rev: u64,
    ) -> Result<(), SyncError> {
        self.validate_progression()?;
        if old_root.key_epoch != self.from_epoch {
            return Err(SyncError::RelayWire(
                "relay restart from_epoch 与本地 active epoch 不一致",
            ));
        }
        if old_root.key_epoch >= self.new_to_epoch {
            return Err(SyncError::RelayWire(
                "relay 分配的候选 epoch 不高于本地 active epoch",
            ));
        }
        if self.new_to_epoch == old_root.key_epoch.saturating_add(1) {
            return Err(SyncError::RelayWire(
                "relay 候选 epoch 仍等于 active+1（旧候选未废弃，禁止本地 +1）",
            ));
        }
        let Some(next_control_rev) = expected_control_rev.checked_add(1) else {
            return Err(SyncError::RelayWire("control_rev 溢出"));
        };
        if self.control_rev != next_control_rev {
            return Err(SyncError::RelayWire(
                "relay restart control_rev 与本地 CAS 假设不一致",
            ));
        }
        Ok(())
    }
}

/// restart 的管理 realm 请求（幂等键与 activate 分开 scope，relay 按 request_id 去重）。
#[derive(Debug, Clone, Copy)]
pub struct RestartRelayRequest {
    /// 要重启的旧 rotation id。
    pub previous_rotation_id: [u8; 16],
    /// 幂等 request_id。
    pub request_id: [u8; 16],
    /// restart 管理签名的 CAS（本地已知 control_rev；relay 校验后必须 +1）。
    pub expected_control_rev: u64,
}

/// relay `epoch.activated` 事件里持久化的完整 activation T 与旧/新管理双签。
///
/// 崩溃在 "activate 已发出、journal 尚未落 activated" 时，客户端不能重新造一份 T
/// （重新造会换 prev_control_hash/commit digest，与 relay 已写入的事不一致）；必须从
/// `/control` 取回同一份 T，并用本地旧/新管理公钥复算双签后继续。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredActivationT {
    /// `ActivationTV1` 的 canonical CDE 原字节。
    pub activation_t: Vec<u8>,
    pub old_t_signature: Vec<u8>,
    pub new_t_signature: Vec<u8>,
}

/// 构造/恢复 activation T 所需、只有 relay 知道的事实（ADR 0006 §34）。
///
/// 注意：两条基线的 `commit_digest` **不在这里**——签 T 只允许用 journal 里 durable
/// 的本地 receipt，避免恢复时重新从 relay `signed_commit` 取摘要后重签（P1-2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartActivationFacts {
    /// `/control` 当前链尾 rev（正常路径必须等于 restart 视图的 `control_rev`）。
    pub control_rev: u64,
    /// 激活前控制链链尾 `record_hash`；若本次 rotation 已激活，取该激活记录的
    /// `prev_hash`（即激活前链尾）。
    pub prev_control_hash: [u8; 32],
    /// 本次 rotation 已激活时，事件里持久化的 T 与双签。
    pub stored: Option<StoredActivationT>,
}

/// 管理 realm 用哪一把管理 key：activate 是分界线。
///
/// relay 在 activate 成功后把 `manager_rev` 推到新管理 key，之后旧 key 连
/// challenge 都会被 403 `MANAGEMENT_REQUIRED` 拒绝；因此这个选择不是风格问题，
/// 而是 wire 契约（ADR 0006 §27/§34）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerKeyStage {
    /// activate 之前：relay 的 `manager_rev` 仍是旧值，只能用旧管理 key。
    PreActivation,
    /// activate 之后/已激活 resume：relay 只认新管理 key（旧 key 换 token 必然失败）。
    PostActivation,
}

/// restart 的管理 realm 网络步骤（真实现走 HTTP；测试可注入记录器）。
pub trait RestartManagement {
    /// `GET /rotations/{r}`：restart **前**的旧 rotation view。
    ///
    /// intent 的 `previous_candidate_epoch`（被废弃的旧候选）只能从这里取；必须在
    /// durable intent 落盘之前或同一阶段调用，绝不在 POST 后重新猜。
    fn previous_rotation(
        &mut self,
        _rotation_id: [u8; 16],
    ) -> Result<RestartRotationView, SyncError> {
        Err(SyncError::Unsupported(
            "该 restart 管理端不支持 restart 前旧 rotation 查询",
        ))
    }

    /// `POST /rotations/{old}/restart`，返回 restart 前后两份 relay view。
    fn restart(&mut self, request: &RestartRelayRequest)
        -> Result<RestartRelayResponse, SyncError>;

    /// 丢响应/崩溃后按 **exact request_id** 找回已提交的 restart 结果。
    ///
    /// `Ok(None)` 表示 relay 明确没有该 request 的 restart 结果；调用方必须 fail-closed
    /// （绝不盲目重发 POST）。`scope` 不是 `management/restart` 时必须报错，不能把其它
    /// 管理端点的幂等结果当成本次 restart。
    fn restart_outcome(
        &mut self,
        _request_id: [u8; 16],
        _previous_rotation_id: [u8; 16],
    ) -> Result<Option<RestartRelayResponse>, SyncError> {
        Ok(None)
    }

    /// 阶段 F：把 grant leaves stage 到候选 rotation。
    fn stage_grants(
        &mut self,
        rotation_id: [u8; 16],
        leaves: &[GrantLeaf],
    ) -> Result<(), SyncError>;

    /// 阶段 G：双签 activate（四个 digest 必填）。
    fn activate(&mut self, input: &ActivationInput) -> Result<ActivatedOutcome, SyncError>;

    /// activate 超时/进程崩溃后查 relay 是否已 activated；只信 relay 回执。
    fn activated_outcome(
        &mut self,
        _rotation_id: [u8; 16],
    ) -> Result<Option<ActivatedOutcome>, SyncError> {
        Ok(None)
    }

    /// ADR §34：取构造/恢复 activation T 所需、只有 relay 知道的控制链事实（链尾、
    /// 激活前 hash、已持久化的 T）。
    ///
    /// **不再返回 commit digest**：T 的 `primary_commit_digest`/`fallback_commit_digest`
    /// 只能来自 journal 的 durable 本地 receipt（评审 0002 P1-2）。
    ///
    /// **阶段决定管理 key**：activate 之前用旧 key（relay 的 `manager_rev` 还是旧值）；
    /// activate 之后（含已激活 resume）必须用新 key——旧 key 换 token 会被 relay 403。
    fn activation_facts(
        &mut self,
        _rotation_id: [u8; 16],
        _stage: ManagerKeyStage,
    ) -> Result<RestartActivationFacts, SyncError> {
        Err(SyncError::Unsupported(
            "该 restart 管理端不支持 activation T 事实查询",
        ))
    }

    /// 注入由 journal `manager_seed` 重建、并与 `new_manager_public` 校验过的新管理 key。
    ///
    /// `run_restart` 在读取 journal 后调用；`HttpRestartManagement` 只把它用于
    /// [`ManagerKeyStage::PostActivation`] 操作（activate 之前的操作仍用旧 key）。
    fn adopt_new_manager(&mut self, _manager: &ManagementKey) -> Result<(), SyncError> {
        Ok(())
    }
}

/// 一次 restart 的 fresh 材料（全部是新秘密，旧材料不可复用）。
pub struct RestartPlan {
    /// relay 保留的 active epoch。
    pub from_epoch: u64,
    /// 被废弃的旧候选 epoch（只用于审计/断言，绝不作为新候选）。
    pub previous_candidate_epoch: u64,
    /// relay 新分配的候选 epoch。
    pub to_epoch: u64,
    /// relay 新分配的 rotation id。
    pub rotation_id: [u8; 16],
    /// restart 后的 control_rev（activate CAS）。
    pub control_rev: u64,
    /// fresh 新口令。
    pub password: Zeroizing<String>,
    /// fixed password/recovery 槽的 MNE message_id（重试必须复用同一 envelope）。
    pub password_message_id: [u8; 16],
    pub recovery_message_id: [u8; 16],
    /// fresh password/recovery 槽（含固定密文）。
    pub slots: CandidateSlots,
    /// fresh 候选根（epoch = relay 新分配值）。
    pub candidate_root: RootSecretV1,
    /// 新管理 key rev（旧 rev + 1；管理私钥由 R' 派生）。
    pub new_manager_rev: u64,
}

/// 候选基线的固定角色（relay §28.1 条目 3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineRole {
    /// 主基线。
    Primary,
    /// 备基线。
    Fallback,
}

impl BaselineRole {
    /// relay 口径的 role 文本。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Fallback => "fallback",
        }
    }
}

/// restart 中被明确跳过的设备（审计用：跳过必须有原因）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedDevice {
    /// 设备 id。
    pub device_id: DeviceId,
    /// 跳过原因。
    pub reason: SkipReason,
}

/// 设备被跳过的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 已被吊销：不生成 grant，也不自动重包其离线 ops。
    Revoked,
    /// relay 给了 HPKE 公钥，但控制链没有该设备的 active 证书（不信任裸列表）。
    NotInControlChain,
}

impl SkipReason {
    /// 稳定文本（日志/报告用）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Revoked => "revoked",
            Self::NotInControlChain => "not-in-control-chain",
        }
    }
}

/// 已验证的一份 restart 基线（prepared + 读回证据）。
///
/// 首次执行时 `prepared/evidence` 为 Some；从 journal 续跑时不再重传随机 nonce 的
/// snapshot（避免 relay ID_CONFLICT），只保留已持久化的 cid/manifest 身份与 Option 句柄。
pub struct RestartBaseline {
    /// 固定角色。
    pub role: BaselineRole,
    /// checkpoint id（activate 指名）。
    pub checkpoint_id: [u8; 16],
    /// 覆盖的 S。
    pub covered_seq: u64,
    /// 本地 durable 的 `CheckpointCommit` 摘要（activation T 只认它）。
    pub commit_digest: [u8; 32],
    /// 首次执行时的读回证据；续跑为 None（journal 已记录 manifest 身份）。
    pub evidence: Option<BaselineEvidence>,
    /// 首次执行时的 prepared 句柄；续跑为 None（不得重新上传 snapshot）。
    pub prepared: Option<PreparedCheckpoint>,
}

/// 一次 restart 的输入。
///
/// fresh 秘密由 journal/`plan_restart` 生成，不从这里进；epoch/rotation_id/control_rev
/// 全部来自 relay restart 响应，调用方只提供本地旧材料和 CAS 假设。
pub struct RestartRequest<'a> {
    /// vault 身份。
    pub vault_id: VaultId,
    /// 发起设备 id。
    pub device_id: DeviceId,
    /// 本机 Loro peer id（基线导出用，不改变）。
    pub peer_id: u64,
    /// 发起设备签名私钥。
    pub signing_key: DeviceSigningKey,
    /// 旧 epoch 根（只用于解密旧资产/旧槽，不上 wire）。
    pub old_root: RootSecretV1,
    /// 当前（旧）管理 key：stage grants 与 activate 仍由它授权。
    pub old_manager: &'a ManagementKey,
    /// 当前 manager_rev（restart 响应必须与它一致）。
    pub manager_rev: u64,
    /// 要原子重启的旧 rotation id。
    pub previous_rotation_id: [u8; 16],
    /// relay instance id（管理签名绑定）。
    pub instance_id: [u8; 16],
    /// restart 管理签名的本地 CAS（restart 响应 control_rev 必须 = 它 + 1）。
    pub control_rev: u64,
    /// 旧 rotation 的冻结点 S：两份基线必须 `S' ≥ S`。
    pub freeze_seq: u64,
    /// 本次 restart/activate 的幂等 request_id。
    pub request_id: [u8; 16],
    /// 维护 reservation id（候选 epoch 上传必带）。
    pub reservation: [u8; 16],
    /// **已验证**控制链里 active 成员证书（`/control` 缓存口径）。
    ///
    /// 用它复算 relay 口径的 `member_cert_digest`（restart 不改成员集合，但绝不复用旧
    /// 候选的 journal 值）；revoked 设备必须**不在**这里。
    pub member_certs: &'a [MemberCertDigestInputV1],
    /// 本次 restart 前已吊销的设备（不生成 grant、不重包其离线 ops）。
    pub revoked: &'a [DeviceId],
    /// 旧 epoch 冻结资产的真实字节来源（两基线导出；必须覆盖墓碑/无引用资产）。
    pub assets: &'a dyn AssetSource,
    /// 本机 HPKE 公钥（与控制链交叉校验，防止封给错公钥）。
    pub local_hpke_public: [u8; 32],
    /// 冻结资产分页 limit（必须 >0）。
    pub page_limit: u32,
    /// 候选槽 created_at。
    pub created_at: u64,
    /// 四个 digest 的 canonical 绑定版本（v1 = 1）。
    pub pv: u64,
    /// 用户是否已离线备份新恢复码（activate 硬门槛；journal 也会持久化）。
    pub recovery_backup_confirmed: bool,
}

/// restart 的结果：新材料 + 已验证基线 + 已 stage grants + activate 回执。
pub struct RestartExecution {
    /// fresh 材料（新恢复码/新口令/新管理 key 只在内存）。
    pub plan: RestartPlan,
    /// 新槽集合 digest（relay 口径）。
    pub slot_set_digest: [u8; 32],
    /// 候选 epoch 资产集合 digest（relay 口径，来自冻结分页）。
    pub asset_set_digest: [u8; 32],
    /// 控制链 active 成员证书集合 digest。
    pub member_cert_digest: [u8; 32],
    /// 已 stage grants 的集合 digest。
    pub grants_digest: [u8; 32],
    /// 主基线。
    pub primary: RestartBaseline,
    /// 备基线。
    pub fallback: RestartBaseline,
    /// 新根下重加密的冻结资产。
    pub reencrypted_assets: Vec<ReencryptedAsset>,
    /// 冻结资产数量（relay 分页 total）。
    pub frozen_asset_count: u64,
    /// 已 stage 的 grant leaves。
    pub grants: Vec<GrantLeaf>,
    /// 明确跳过的设备与原因。
    pub skipped: Vec<SkippedDevice>,
    /// activate 材料（四 digest + 双签 new_manager）。
    pub activation: ActivationInput,
    /// relay activate 回执。
    pub activated: ActivatedOutcome,
}

impl core::fmt::Debug for RestartExecution {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // 只打印定位信息：口令/根/管理私钥/恢复种子一律不进 Debug。
        f.debug_struct("RestartExecution")
            .field("from_epoch", &self.plan.from_epoch)
            .field(
                "previous_candidate_epoch",
                &self.plan.previous_candidate_epoch,
            )
            .field("to_epoch", &self.plan.to_epoch)
            .field("rotation_id", &crate::ids::hex_of(&self.plan.rotation_id))
            .field("new_manager_rev", &self.plan.new_manager_rev)
            .field(
                "slot_set_digest",
                &crate::ids::hex_of(&self.slot_set_digest),
            )
            .field(
                "asset_set_digest",
                &crate::ids::hex_of(&self.asset_set_digest),
            )
            .field(
                "member_cert_digest",
                &crate::ids::hex_of(&self.member_cert_digest),
            )
            .field("grants_digest", &crate::ids::hex_of(&self.grants_digest))
            .field("primary", &self.primary.checkpoint_id)
            .field("fallback", &self.fallback.checkpoint_id)
            .field("grants", &self.grants.len())
            .field("skipped", &self.skipped)
            .field("activated_epoch", &self.activated.epoch)
            .finish_non_exhaustive()
    }
}

/// 真实实现：restart + `stage_grants` + `activate_http`，并同时持有旧/新两把管理 key。
///
/// **key 选择由阶段决定**（见 [`ManagerKeyStage`]）：
/// - `restart` / `stage_grants` / `activate` 签名：旧管理 key；
/// - activate 丢响应后的读回、`/rotations/current`、`/control`、checkpoint
///   `signed_commit` 以及已激活 resume：新管理 key。
pub struct HttpRestartManagement<'a, T: HttpTransport> {
    transport: &'a mut T,
    base: &'a str,
    vault_id: VaultId,
    instance_id: [u8; 16],
    old_manager: &'a ManagementKey,
    /// 由 journal `manager_seed` 重建或调用方注入；activate 之前不使用。
    new_manager: Option<ManagementKey>,
    now: u64,
}

impl<'a, T: HttpTransport> HttpRestartManagement<'a, T> {
    /// 构造（只带旧管理 key；新 key 由 [`Self::with_new_manager`] 或
    /// [`RestartManagement::adopt_new_manager`] 注入）。
    pub fn new(
        transport: &'a mut T,
        base: &'a str,
        vault_id: VaultId,
        instance_id: [u8; 16],
        manager: &'a ManagementKey,
        now: u64,
    ) -> Self {
        Self {
            transport,
            base,
            vault_id,
            instance_id,
            old_manager: manager,
            new_manager: None,
            now,
        }
    }

    /// 调用方已从 journal `manager_seed` 重建并校验新管理 key 时直接注入。
    #[must_use]
    pub fn with_new_manager(mut self, manager: &ManagementKey) -> Self {
        self.new_manager = Some(manager.clone());
        self
    }

    /// 按阶段取管理 key：activate 前只给旧 key，activate 后只给新 key。
    fn manager_for(&self, stage: ManagerKeyStage) -> Result<&ManagementKey, SyncError> {
        match stage {
            ManagerKeyStage::PreActivation => Ok(self.old_manager),
            ManagerKeyStage::PostActivation => {
                self.new_manager.as_ref().ok_or(SyncError::Unsupported(
                    "activate 之后的管理操作需要新管理 key（由 journal manager_seed 派生）",
                ))
            }
        }
    }

    /// 按阶段取 management token（challenge→token 两步都绑定该阶段的 key）。
    fn token_for(&mut self, stage: ManagerKeyStage) -> Result<String, SyncError> {
        let manager = self.manager_for(stage)?.clone();
        let (token, _) = crate::recovery_enroll::management_token(
            self.transport,
            self.base,
            self.vault_id,
            &manager,
            self.now,
        )?;
        Ok(token)
    }

    /// `GET /rotations/{r}` 并解析成 restart 视图（restart 前/后都可能用到）。
    fn previous_rotation_view(
        &mut self,
        token: &str,
        rotation_id: &[u8; 16],
    ) -> Result<RestartRotationView, SyncError> {
        let response = crate::recovery_enroll::send(
            self.transport,
            "GET",
            format!(
                "{}/v1/vaults/{}/rotations/{}",
                self.base,
                crate::base64url::encode(&self.vault_id),
                crate::base64url::encode(rotation_id)
            ),
            Some(token),
            Vec::new(),
        )?;
        let fields = crate::recovery_enroll::decode(&response)?;
        parse_restart_rotation_view(&fields)
    }

    /// `GET /rotations/{r}`（管理 realm，使用 activate 前的旧管理 key）。
    pub fn fetch_previous_rotation(
        &mut self,
        rotation_id: [u8; 16],
    ) -> Result<RestartRotationView, SyncError> {
        let token = self.token_for(ManagerKeyStage::PreActivation)?;
        self.previous_rotation_view(&token, &rotation_id)
    }
}

impl<T: HttpTransport> RestartManagement for HttpRestartManagement<'_, T> {
    fn previous_rotation(
        &mut self,
        rotation_id: [u8; 16],
    ) -> Result<RestartRotationView, SyncError> {
        self.fetch_previous_rotation(rotation_id)
    }

    fn restart(
        &mut self,
        request: &RestartRelayRequest,
    ) -> Result<RestartRelayResponse, SyncError> {
        let token = self.token_for(ManagerKeyStage::PreActivation)?;
        // 旧候选 epoch 只能从 restart 前旧 rotation view 读取（restart 响应不含它）。
        let previous = self.previous_rotation_view(&token, &request.previous_rotation_id)?;

        let path = format!(
            "/v1/vaults/{}/rotations/{}/restart",
            crate::base64url::encode(&self.vault_id),
            crate::base64url::encode(&request.previous_rotation_id)
        );
        let body = crate::cbor::encode(Vec::new())?;
        let body_digest: [u8; 32] = Sha256::digest(menghunote_sync_proto::management_request_cde(
            HttpMethod::Post,
            &path,
            &body,
        )?)
        .into();
        let signature = self
            .old_manager
            .sign(&menghunote_sync_proto::management_signing_input(
                &self.instance_id,
                &self.vault_id,
                &request.request_id,
                request.expected_control_rev,
                &body_digest,
            )?);
        let signed = crate::cbor::CdeValue::Map(vec![
            (0, crate::cbor::bytes(body)),
            (1, crate::cbor::bytes(signature.to_vec())),
        ]);
        let fields = crate::cbor::encode(vec![
            (0, signed),
            (1, crate::cbor::bytes(request.request_id.to_vec())),
            (2, crate::cbor::uint(request.expected_control_rev)),
        ])?;
        let response = crate::recovery_enroll::send(
            self.transport,
            "POST",
            format!("{}{}", self.base, path),
            Some(&token),
            fields,
        )?;
        let response_fields = crate::recovery_enroll::decode(&response)?;
        if !crate::cbor::bool_field(&response_fields, 1)? {
            return Err(SyncError::RelayWire("restart 响应缺少 restarted=true"));
        }
        let restarted = parse_restart_rotation_view(&response_fields)?;
        Ok(RestartRelayResponse {
            previous,
            restarted,
        })
    }

    fn restart_outcome(
        &mut self,
        request_id: [u8; 16],
        previous_rotation_id: [u8; 16],
    ) -> Result<Option<RestartRelayResponse>, SyncError> {
        // 管理 realm（含管理写入）都可读；丢响应恢复时旧的 POST 已经提交，旧管理 key
        // 仍然有效（activate 之前不会被 relay 拒）。
        let token = self.token_for(ManagerKeyStage::PreActivation)?;
        let path = format!(
            "{}/v1/vaults/{}/rotations/current?request_id={}",
            self.base,
            crate::base64url::encode(&self.vault_id),
            crate::base64url::encode(&request_id)
        );
        let response =
            crate::recovery_enroll::send(self.transport, "GET", path, Some(&token), Vec::new())?;
        let fields = crate::recovery_enroll::decode(&response)?;
        let Some(replay) = crate::cbor::opt_bytes_field(&fields, 1)? else {
            return Ok(None);
        };
        // 幂等表跨全部管理 scope：必须确认这条结果来自 restart 端点，绝不能把
        // begin/activate/abort 的同 request_id 结果当成本次 restart。
        let scope = crate::cbor::text_field(&fields, 2)?;
        if scope != "management/restart" {
            return Err(SyncError::RelayWire(
                "request_id 命中的幂等结果不是 restart 端点（拒绝复用其它管理写结果）",
            ));
        }
        let replay_fields = crate::cbor::decode(&replay)?;
        if !crate::cbor::bool_field(&replay_fields, 1)? {
            return Err(SyncError::RelayWire("restart 重放响应缺少 restarted=true"));
        }
        let restarted = parse_restart_rotation_view(&replay_fields)?;
        let previous = self.previous_rotation_view(&token, &previous_rotation_id)?;
        Ok(Some(RestartRelayResponse {
            previous,
            restarted,
        }))
    }

    fn stage_grants(
        &mut self,
        rotation_id: [u8; 16],
        leaves: &[GrantLeaf],
    ) -> Result<(), SyncError> {
        // grants 在 activate 之前 stage，relay 此时仍只认旧管理 key。
        stage_grants(
            self.transport,
            self.base,
            self.vault_id,
            rotation_id,
            self.old_manager,
            leaves,
            self.now,
        )
    }

    fn activate(&mut self, input: &ActivationInput) -> Result<ActivatedOutcome, SyncError> {
        match activate_http(
            self.transport,
            self.base,
            self.vault_id,
            &self.instance_id,
            self.old_manager,
            input,
            self.now,
        ) {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                // activate 的成功回执可能丢失；relay 若已激活，`manager_rev` 已推进到
                // 新 key，旧 key 连 challenge 都会被 403。此时必须用新 key 读回
                // `/rotations/current`，不能把传输错误当成功，也不能继续用旧 key 重试。
                if self.new_manager.is_none() {
                    return Err(if is_manager_key_rejected(&error) {
                        SyncError::Unsupported(ACTIVATE_NEEDS_NEW_MANAGER)
                    } else {
                        error
                    });
                }
                match self.activated_outcome(input.rotation_id) {
                    Ok(Some(found)) => Ok(found),
                    // relay 明确还是未激活状态：保留 activate 的原错误，交给调用方重试。
                    Ok(None) => Err(error),
                    // 新 key 也换不到 token：说明 activate 尚未生效（旧 key 仍是当前管理 key）。
                    Err(query_error) if is_manager_key_rejected(&query_error) => Err(error),
                    Err(query_error) => Err(query_error),
                }
            }
        }
    }

    fn activated_outcome(
        &mut self,
        rotation_id: [u8; 16],
    ) -> Result<Option<ActivatedOutcome>, SyncError> {
        // 读回 activated 结果发生在 activate 之后（或已激活 resume），只认新 key。
        let token = self.token_for(ManagerKeyStage::PostActivation)?;
        let response = match crate::recovery_enroll::send(
            self.transport,
            "GET",
            format!(
                "{}/v1/vaults/{}/rotations/current",
                self.base,
                crate::base64url::encode(&self.vault_id)
            ),
            Some(&token),
            Vec::new(),
        ) {
            Ok(response) => response,
            Err(SyncError::RelayApi { code, .. }) if code == "NOT_FOUND" => return Ok(None),
            Err(error) => return Err(error),
        };
        let fields = crate::recovery_enroll::decode(&response)?;
        let Some(value) = fields
            .iter()
            .find(|(key, _)| *key == 0)
            .map(|(_, value)| value)
        else {
            return Ok(None);
        };
        let view = match value {
            crate::cbor::CdeValue::Map(_) => crate::cbor::value_map(value)?,
            _ => return Ok(None),
        };
        let state = crate::cbor::text_field(&view, 1)?;
        if state != "activated" {
            return Ok(None);
        }
        let current_rotation: [u8; 16] = crate::cbor::bytes_fixed(&view, 0)?;
        if current_rotation != rotation_id {
            return Ok(None);
        }
        Ok(Some(ActivatedOutcome {
            epoch: crate::cbor::uint_field(&view, 3)?,
            control_rev: crate::cbor::uint_field(&view, 10)?,
            auth_rev: crate::cbor::uint_field(&view, 11)?,
            member_set_hash: crate::cbor::opt_bytes_field(&view, 6)?
                .map(|bytes| fixed32(&bytes, "member_set_hash"))
                .transpose()?
                .unwrap_or_default(),
            primary_checkpoint: crate::cbor::bytes_fixed(&view, 18)?,
            fallback_checkpoint: crate::cbor::bytes_fixed(&view, 19)?,
            activation_commit: crate::cbor::bytes_field(&view, 20)?,
        }))
    }

    fn activation_facts(
        &mut self,
        rotation_id: [u8; 16],
        stage: ManagerKeyStage,
    ) -> Result<RestartActivationFacts, SyncError> {
        let token = self.token_for(stage)?;
        let response = crate::recovery_enroll::send(
            self.transport,
            "GET",
            format!(
                "{}/v1/vaults/{}/control?after_rev=0",
                self.base,
                crate::base64url::encode(&self.vault_id)
            ),
            Some(&token),
            Vec::new(),
        )?;
        let fields = crate::recovery_enroll::decode(&response)?;
        let head_rev = crate::cbor::uint_field(&fields, 1)?;
        let mut prev_hash = [0u8; 32];
        let mut last_hash = [0u8; 32];
        let mut last_rev = 0u64;
        let mut pre_activation_hash = None;
        let mut stored = None;
        for commit in crate::cbor::array_field(&fields, 0)? {
            let commit = crate::cbor::value_map(&commit)?;
            let rev = crate::cbor::uint_field(&commit, 0)?;
            let stored_prev = crate::cbor::bytes_fixed(&commit, 1)?;
            let stored_hash: [u8; 32] = crate::cbor::bytes_fixed(&commit, 2)?;
            let signed_record = crate::cbor::bytes_field(&commit, 3)?;
            if rev != last_rev.saturating_add(1) || stored_prev != prev_hash {
                return Err(SyncError::RelayWire(
                    "activation T: 控制链 rev/prev_hash 不连续",
                ));
            }
            let mut hasher = Sha256::new();
            hasher.update(stored_prev);
            hasher.update(&signed_record);
            let computed: [u8; 32] = hasher.finalize().into();
            if computed != stored_hash {
                return Err(SyncError::RelayWire(
                    "activation T: 控制链 record_hash 不符",
                ));
            }
            // 只解析 typed 记录；旧裸记录（rev 1 等）与本次 T 无关。
            if let Ok(record) = SignedControlRecord::decode(&signed_record) {
                if record.kind == ControlKind::EpochActivated {
                    if let Ok(Some(ControlEvent::EpochActivated(event))) = record.decoded_event() {
                        if event.rotation_id == rotation_id {
                            // 该激活记录的 prev_hash 就是"激活前控制链链尾"。
                            pre_activation_hash = Some(stored_prev);
                            stored = match (
                                event.activation_t,
                                event.old_t_signature,
                                event.new_t_signature,
                            ) {
                                (Some(t), Some(old), Some(new)) => Some(StoredActivationT {
                                    activation_t: t,
                                    old_t_signature: old,
                                    new_t_signature: new,
                                }),
                                _ => None,
                            };
                        }
                    }
                }
            }
            last_rev = rev;
            last_hash = stored_hash;
            prev_hash = stored_hash;
        }
        if last_rev != head_rev {
            return Err(SyncError::RelayWire("activation T: 控制链链尾 rev 不一致"));
        }
        Ok(RestartActivationFacts {
            control_rev: last_rev,
            prev_control_hash: pre_activation_hash.unwrap_or(last_hash),
            stored,
        })
    }

    fn adopt_new_manager(&mut self, manager: &ManagementKey) -> Result<(), SyncError> {
        if manager.public_key() == self.old_manager.public_key() {
            return Err(SyncError::LocalCorrupt(
                "restart journal 的新管理 key 与旧 key 相同（manager_seed 未轮换）",
            ));
        }
        self.new_manager = Some(manager.clone());
        Ok(())
    }
}

fn parse_restart_rotation_view(
    fields: &crate::cbor::Fields,
) -> Result<RestartRotationView, SyncError> {
    let view = crate::cbor::value_map(crate::cbor::field(fields, 0)?)?;
    Ok(RestartRotationView {
        rotation_id: crate::cbor::bytes_fixed(&view, 0)?,
        state: crate::cbor::text_field(&view, 1)?,
        from_epoch: crate::cbor::uint_field(&view, 2)?,
        to_epoch: crate::cbor::uint_field(&view, 3)?,
        freeze_seq: crate::cbor::uint_field(&view, 4)?,
        control_rev: crate::cbor::uint_field(&view, 10)?,
        manager_rev: crate::cbor::uint_field(&view, 12)?,
    })
}

fn fixed32(bytes: &[u8], what: &'static str) -> Result<[u8; 32], SyncError> {
    bytes.try_into().map_err(|_| SyncError::RelayWire(what))
}

fn fixed64(bytes: &[u8], what: &'static str) -> Result<[u8; 64], SyncError> {
    bytes.try_into().map_err(|_| SyncError::RelayWire(what))
}

/// restart 的第 1 个执行步骤：把 fresh 槽上传到候选 epoch（带维护 reservation）。
#[allow(clippy::too_many_arguments)]
pub fn upload_restart_slots<C: RelayClient>(
    client: &mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    plan: &RestartPlan,
    reservation: &[u8; 16],
) -> Result<[u8; 32], SyncError> {
    // 固定 message_id + 固定槽体 nonce/ciphertext：重试产生**逐字节相同**的 MNE1 envelope。
    upload_slot_with_reservation(
        client,
        vault_id,
        device_id,
        signing_key,
        &plan.candidate_root,
        &plan.slots.password,
        Some(plan.password.as_str()),
        None,
        plan.password_message_id,
        reservation,
    )?;
    upload_slot_with_reservation(
        client,
        vault_id,
        device_id,
        signing_key,
        &plan.candidate_root,
        &plan.slots.recovery,
        None,
        Some(&plan.slots.recovery_seed),
        plan.recovery_message_id,
        reservation,
    )?;
    slot_set_digest(&[
        plan.slots.password.descriptor.clone(),
        plan.slots.recovery.descriptor.clone(),
    ])
}

/// 生成 relay 分配 epoch 的 fresh root/password/R'/管理 key，并置 repack 屏障。
///
/// 旧写保持冻结：调用方必须在整个 restart 期间保持 engine `rotation_frozen`。
#[allow(clippy::too_many_arguments)]
pub fn plan_restart(
    vault: &Vault,
    vault_id: VaultId,
    contract: &RestartContract,
    manager_rev: u64,
    device_id: [u8; 16],
    created_at: u64,
    rng: &mut impl crate::random::RandomSource,
) -> Result<RestartPlan, SyncError> {
    contract.validate_progression()?;
    let to_epoch = contract.new_to_epoch;
    let candidate_root = RootSecretV1::generate(vault_id, to_epoch, rng)?;
    // 新口令也是 fresh 秘密，不能用旧口令/旧 salt。
    let password = Zeroizing::new(crate::ids::hex_of(&crate::random::array::<32>(rng)?));
    let password_message_id = crate::random::array::<16>(rng)?;
    let recovery_message_id = crate::random::array::<16>(rng)?;
    let new_manager_rev = manager_rev
        .checked_add(1)
        .ok_or(SyncError::LocalCorrupt("manager_rev 溢出"))?;
    let slots = generate_candidate_slots(
        vault_id,
        to_epoch,
        password.as_str(),
        new_manager_rev,
        created_at,
        device_id,
        &candidate_root,
        rng,
    )?;
    vault.set_meta(REPACK_REQUIRED_META, b"1")?;
    Ok(RestartPlan {
        from_epoch: contract.from_epoch,
        previous_candidate_epoch: contract.previous_candidate_epoch,
        to_epoch,
        rotation_id: contract.rotation_id,
        control_rev: contract.control_rev,
        password,
        password_message_id,
        recovery_message_id,
        slots,
        candidate_root,
        new_manager_rev,
    })
}

/// restart 前的公开 intent：全部来自调用方请求 + relay 旧 rotation view。
///
/// 绝不生成/保存 fresh 秘密；`freeze_barrier` 记录调用时屏障已置 1。
fn restart_intent(
    request: &RestartRequest<'_>,
    previous: &RestartRotationView,
) -> Result<RestartIntent, SyncError> {
    if previous.rotation_id != request.previous_rotation_id {
        return Err(SyncError::RelayWire("restart 前旧 rotation id 不符"));
    }
    if previous.from_epoch != request.old_root.key_epoch {
        return Err(SyncError::RelayWire(
            "restart 前旧 rotation active epoch 与本地根不一致",
        ));
    }
    if previous.to_epoch <= previous.from_epoch {
        return Err(SyncError::RelayWire("旧 rotation 候选 epoch 不大于 active"));
    }
    if !matches!(previous.state.as_str(), "frozen" | "staging") {
        return Err(SyncError::RelayWire("restart 前旧 rotation 状态非法"));
    }
    if previous.freeze_seq != request.freeze_seq {
        return Err(SyncError::RelayWire("旧 rotation 冻结点与本地假设不一致"));
    }
    if previous.manager_rev != request.manager_rev {
        return Err(SyncError::RelayWire(
            "旧 rotation manager_rev 与本地假设不一致",
        ));
    }
    // CAS 假设必须在写 intent 之前成立；否则 POST 必然 409，且 intent 会永久卡住
    // （有 intent + relay 无结果 = fail-closed）。
    if previous.control_rev != request.control_rev {
        return Err(SyncError::RelayWire(
            "旧 rotation control_rev 与本地 CAS 假设不一致",
        ));
    }
    Ok(RestartIntent {
        previous_rotation_id: request.previous_rotation_id,
        request_id: request.request_id,
        from_epoch: request.old_root.key_epoch,
        previous_candidate_epoch: previous.to_epoch,
        expected_control_rev: request.control_rev,
        manager_rev: request.manager_rev,
        freeze_seq: request.freeze_seq,
        instance_id: request.instance_id,
        freeze_barrier: true,
    })
}

/// 已有 journal 与本次 request/intent 交叉校验；不匹配一律 fail-closed。
fn validate_restart_journal(
    existing: &RotationJournal,
    request: &RestartRequest<'_>,
) -> Result<(), SyncError> {
    let intent = existing
        .restart_intent
        .as_ref()
        .ok_or(SyncError::Unsupported(INTENT_MISSING))?;
    if intent.previous_rotation_id != request.previous_rotation_id
        || intent.request_id != request.request_id
        || intent.from_epoch != request.old_root.key_epoch
        || intent.expected_control_rev != request.control_rev
        || intent.manager_rev != request.manager_rev
        || intent.freeze_seq != request.freeze_seq
        || intent.instance_id != request.instance_id
        || !intent.freeze_barrier
    {
        return Err(SyncError::Unsupported(CONTRACT_MISMATCH));
    }
    if existing.request_id != intent.request_id
        || existing.restart_previous_rotation_id != Some(intent.previous_rotation_id)
        || existing.from_epoch != intent.from_epoch
        || existing.freeze_seq != Some(intent.freeze_seq)
        || existing.previous_candidate_epoch != Some(intent.previous_candidate_epoch)
    {
        return Err(SyncError::LocalCorrupt(
            "restart journal 与 durable intent 不一致",
        ));
    }
    if existing.phase == RotationPhase::RestartIntentRecorded {
        if existing.rotation_id.is_some()
            || existing.to_epoch != 0
            || existing.control_rev != intent.expected_control_rev
            || existing.candidate_root.is_some()
        {
            return Err(SyncError::LocalCorrupt(
                "restart intent 阶段不应已有 relay 分配或 fresh 秘密",
            ));
        }
        return Ok(());
    }
    let Some(expected_control_rev) = intent.expected_control_rev.checked_add(1) else {
        return Err(SyncError::LocalCorrupt("restart journal control_rev 溢出"));
    };
    if existing.rotation_id.is_none()
        || existing.rotation_id == Some(intent.previous_rotation_id)
        || existing.to_epoch <= intent.previous_candidate_epoch
        || existing.control_rev != expected_control_rev
    {
        return Err(SyncError::LocalCorrupt(
            "restart journal 分配字段与 durable intent 不一致",
        ));
    }
    // 新管理 key 只能由 journal 的 manager_seed 重建，并与 new_manager_public
    // 校验；缺失/不匹配必须在任何管理网络调用（尤其 restart）之前拒绝。
    let _ = manager_from_journal(existing)?;
    Ok(())
}

/// relay 的 restart 结果必须与 durable intent 逐字段一致（禁止本地 +1）。
fn validate_restart_response(
    response: &RestartRelayResponse,
    intent: &RestartIntent,
    request: &RestartRequest<'_>,
) -> Result<RestartContract, SyncError> {
    if response.previous.rotation_id != intent.previous_rotation_id {
        return Err(SyncError::RelayWire(
            "relay restart 响应的旧 rotation id 与 intent 不符",
        ));
    }
    let contract = RestartContract::from_relay_response(response)?;
    contract.validate_local(&request.old_root, intent.expected_control_rev)?;
    if contract.from_epoch != intent.from_epoch
        || contract.previous_candidate_epoch != intent.previous_candidate_epoch
        || response.restarted.manager_rev != intent.manager_rev
        || response.previous.manager_rev != intent.manager_rev
        || response.restarted.freeze_seq != intent.freeze_seq
        || response.previous.freeze_seq != intent.freeze_seq
    {
        return Err(SyncError::RelayWire(
            "relay restart 分配与 durable intent 不一致（禁止本地 +1）",
        ));
    }
    if request.request_id != intent.request_id
        || request.previous_rotation_id != intent.previous_rotation_id
        || request.freeze_seq != intent.freeze_seq
    {
        return Err(SyncError::Unsupported(CONTRACT_MISMATCH));
    }
    Ok(contract)
}

/// 拿到 relay 分配后才生成 fresh 秘密，并把 assignment + 秘密一次性落 journal。
fn adopt_restart_assignment(
    vault: &Vault,
    intent: &RestartIntent,
    contract: &RestartContract,
    request: &RestartRequest<'_>,
) -> Result<RotationJournal, SyncError> {
    let plan = plan_restart(
        vault,
        request.vault_id,
        contract,
        request.manager_rev,
        request.device_id,
        request.created_at,
        &mut crate::random::OsRandom,
    )?;
    let manager_seed =
        *crate::recovery::management_seed(&plan.slots.recovery_seed, &request.vault_id)?.expose();
    let assignment = RestartAssignment {
        rotation_id: plan.rotation_id,
        from_epoch: plan.from_epoch,
        previous_candidate_epoch: plan.previous_candidate_epoch,
        to_epoch: plan.to_epoch,
        control_rev: plan.control_rev,
        freeze_seq: request.freeze_seq,
    };
    RotationJournal::begin_restart(
        vault,
        intent,
        assignment,
        plan.candidate_root.clone(),
        plan.password.as_bytes().to_vec(),
        *plan.slots.recovery_seed.expose(),
        manager_seed,
        plan.password_message_id,
        plan.recovery_message_id,
        plan.slots.new_manager_public,
        plan.new_manager_rev,
        &[plan.slots.password.clone(), plan.slots.recovery.clone()],
        request.recovery_backup_confirmed,
    )
}

/// 首次执行的基线 → journal durable receipt（只落公开验证字段 + 两个本地摘要）。
fn journal_baseline_from_prepared(
    baseline: &RestartBaseline,
) -> Result<JournalBaseline, SyncError> {
    let evidence = baseline
        .evidence
        .as_ref()
        .ok_or(SyncError::LocalCorrupt("restart 基线缺读回验证证据"))?;
    let prepared = baseline.prepared.as_ref().ok_or(SyncError::LocalCorrupt(
        "restart 基线缺本地 prepared report",
    ))?;
    Ok(JournalBaseline {
        checkpoint_id: baseline.checkpoint_id,
        covered_seq: baseline.covered_seq,
        cursor: String::new(),
        manifest_object_id: evidence.manifest_object_id,
        manifest_digest: prepared.report.manifest.object_digest,
        refs_digest: prepared.report.refs_digest,
        verified: true,
        commit_digest: baseline.commit_digest,
        asset_set_digest: evidence.asset_set_digest,
    })
}

/// activation T 的两条 commit digest 只允许来自 journal durable receipt。
fn durable_commit_digests(journal: &RotationJournal) -> Result<([u8; 32], [u8; 32]), SyncError> {
    if journal.baselines.len() != 2 {
        return Err(SyncError::LocalCorrupt(
            "restart journal 缺两份已验证候选基线",
        ));
    }
    let primary = journal.baselines[0].commit_digest;
    let fallback = journal.baselines[1].commit_digest;
    if primary == [0u8; 32] || fallback == [0u8; 32] {
        return Err(SyncError::LocalCorrupt(
            "restart journal 缺本地 commit receipt：拒绝从 relay facts 重签 T",
        ));
    }
    Ok((primary, fallback))
}

/// 执行一次完整 restart（顺序见模块文档）。
///
/// **失败语义**：任何一步失败都保持 `sync.repack_required=1`，不推进 relay 游标、
/// 不推广本地身份、不解冻旧写；调用方只需按错误决定重试或保留冻结。
pub fn execute_restart<C: RelayClient, M: RestartManagement>(
    vault: &Vault,
    client: &mut C,
    management: &mut M,
    request: &RestartRequest<'_>,
) -> Result<RestartExecution, SyncError> {
    let result = run_restart(vault, client, management, request);
    // 屏障先于任何上传落盘；失败也必须保持。
    let fence = vault
        .set_meta(REPACK_REQUIRED_META, b"1")
        .map_err(SyncError::from);
    match (result, fence) {
        (Ok(execution), Ok(())) => Ok(execution),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), _) => Err(error),
    }
}

fn run_restart<C: RelayClient, M: RestartManagement>(
    vault: &Vault,
    client: &mut C,
    management: &mut M,
    request: &RestartRequest<'_>,
) -> Result<RestartExecution, SyncError> {
    if request.old_root.vault_id != request.vault_id {
        return Err(SyncError::Object("旧根归属 vault 不符"));
    }
    // 冻结屏障必须**先于** durable intent 落盘；intent 自身也记录 freeze_barrier=1。
    vault.set_meta(REPACK_REQUIRED_META, b"1")?;

    // 先看 journal：已激活的 restart 绝不能再发一次 relay restart（旧候选不可复用）。
    let loaded = RotationJournal::load(vault)?;
    let fresh_intent = loaded.is_none();
    let mut member_ids = None;
    let mut journal = match loaded {
        None => {
            // 成员集合前置校验（纯本地）：被吊销设备绝不能进入 intent/grant 路径。
            member_ids = Some(check_members(request)?);
            // 旧候选 epoch 只能从 restart 前的旧 rotation view 读取；它是公开契约事实，
            // 必须在写 intent 之前拿到，POST 之后不得重新猜。
            let previous = management.previous_rotation(request.previous_rotation_id)?;
            let intent = restart_intent(request, &previous)?;
            let journal = RotationJournal::record_restart_intent(vault, intent)?;
            // 评审 0003 P1-D：任何网络调用之前写下本次 restart 的 per-role 基线 journal
            // 标记。它让后续 resume 能区分"新协议 journal"与可能已发生 relay create 的旧 journal。
            crate::checkpoint::RestartBaselineJournal::reset(vault, request.vault_id)?;
            journal
        }
        Some(existing) => {
            if !existing.phase.is_restart() {
                return Err(SyncError::Unsupported(
                    "已有未完成 rotation journal（不是 restart）",
                ));
            }
            validate_restart_journal(&existing, request)?;
            // 已激活/激活中的 journal 只做查询/收尾，不受当前吊销缓存变化影响，
            // 也绝不重发 restart 或 activate。
            if existing.phase == RotationPhase::RestartActivated {
                return resume_already_activated(vault, management, request, existing);
            }
            if existing.phase == RotationPhase::RestartActivating {
                if let Some(execution) =
                    resume_if_activation_landed(vault, management, request, existing.clone())?
                {
                    return Ok(execution);
                }
            }
            // 评审 0003 P1-D：仍要创建候选基线的 phase 必须有 per-role create intent。
            // 旧版本 journal 没写这份记录，无法证明 relay create 是否已落（角色 CAS 可能
            // 已被占用）；fresh create 会撞 409，因此一律 fail-closed，保持冻结。
            if matches!(
                existing.phase,
                RotationPhase::RestartPrepared
                    | RotationPhase::RestartSlotsUploaded
                    | RotationPhase::RestartPrimaryBaselineReady
            ) {
                crate::checkpoint::RestartBaselineJournal::require(vault)?;
            }
            existing
        }
    };
    // 续跑路径（非 activated/activating）仍需成员集合做后续 grant 校验。
    let member_ids = match member_ids {
        Some(ids) => ids,
        None => check_members(request)?,
    };

    // POST 之前 durable intent 已就绪（评审 0002 P1-2）：
    // - 本次刚写 intent：允许首发 POST；
    // - 进程重启后仍在 intent：只能按 exact request 查回 relay 结果，查不到 fail-closed。
    if journal.phase == RotationPhase::RestartIntentRecorded {
        let intent = journal
            .restart_intent
            .clone()
            .ok_or(SyncError::LocalCorrupt(INTENT_MISSING))?;
        let response = if fresh_intent {
            management.restart(&RestartRelayRequest {
                previous_rotation_id: intent.previous_rotation_id,
                request_id: intent.request_id,
                expected_control_rev: intent.expected_control_rev,
            })?
        } else {
            management
                .restart_outcome(intent.request_id, intent.previous_rotation_id)?
                .ok_or(SyncError::Unsupported(INTENT_UNRESOLVED))?
        };
        let contract = validate_restart_response(&response, &intent, request)?;
        journal = adopt_restart_assignment(vault, &intent, &contract, request)?;
    }

    // 从 journal（而不是内存临时值）重建同一份 fresh 材料；重试不换根/不换密文。
    let plan = plan_from_journal(&journal, request.vault_id)?;
    // activate 之后 relay 只认新管理 key；由 journal manager_seed 重建并交给管理器。
    let new_manager = manager_from_journal(&journal)?;
    management.adopt_new_manager(&new_manager)?;
    if journal.rotation_id != Some(plan.rotation_id)
        || journal.to_epoch != plan.to_epoch
        || journal.previous_candidate_epoch != Some(plan.previous_candidate_epoch)
        || journal.control_rev != plan.control_rev
        || journal.from_epoch != plan.from_epoch
    {
        return Err(SyncError::LocalCorrupt(
            "restart journal 分配字段与重建计划不一致",
        ));
    }
    crate::rotation_slots::verify_recovery_code(&plan.slots)?;
    crate::rotation_slots::verify_new_manager(&plan.slots, request.vault_id)?;

    // 1. fresh 槽：固定密文重传（同一 journal 材料 → 逐字节相同 envelope）。
    let slot_set_digest = upload_restart_slots(
        client,
        request.vault_id,
        request.device_id,
        &request.signing_key,
        &plan,
        &request.reservation,
    )?;
    if journal.phase == RotationPhase::RestartPrepared {
        journal.advance(RotationPhase::RestartSlotsUploaded)?;
        journal.save(vault)?;
    }

    // 2. 冻结资产：先取 relay 分页复算 relay 口径 asset_set_digest，再下载/校验/重加密。
    let (frozen_refs, relay_asset_digest, frozen_total) =
        fetch_frozen_assets(client, plan.rotation_id, request.page_limit)?;
    let asset_set_digest = frozen_asset_set_digest(&frozen_refs)?;
    if let Some(relay_digest) = relay_asset_digest {
        if relay_digest != asset_set_digest {
            return Err(SyncError::Object("relay 冻结资产 digest 与本地复算不一致"));
        }
    }
    let (reencrypted_assets, reencrypted_total) = if journal.restart_assets.is_empty() {
        let (assets, _, total) = reencrypt_frozen_assets(
            vault,
            client,
            request.vault_id,
            request.device_id,
            &request.signing_key,
            &request.old_root,
            &plan.candidate_root,
            plan.rotation_id,
            request.page_limit,
            Some(&request.reservation),
        )?;
        let stored = assets
            .iter()
            .map(|asset| JournalRestartAsset {
                real_sha256: asset.real_sha256,
                byte_len: asset.byte_len,
                media_type: asset.media_type.clone(),
                object_id: asset.object_id,
                object_digest: asset.object_digest,
            })
            .collect::<Vec<_>>();
        journal.set_restart_assets(vault, stored)?;
        (assets, total)
    } else {
        (
            journal
                .restart_assets
                .iter()
                .map(|asset| ReencryptedAsset {
                    real_sha256: asset.real_sha256,
                    byte_len: asset.byte_len,
                    media_type: asset.media_type.clone(),
                    object_id: asset.object_id,
                    object_digest: asset.object_digest,
                })
                .collect::<Vec<_>>(),
            frozen_total,
        )
    };
    if reencrypted_total != frozen_total || reencrypted_assets.len() as u64 != frozen_total {
        return Err(SyncError::RelayWire("冻结资产分页在重加密前后发生变化"));
    }
    let frozen_hashes = reencrypted_assets
        .iter()
        .map(|asset| asset.real_sha256)
        .collect::<Vec<_>>();

    // 3. 两份互不共享状态的基线（评审 0002 P1-D）：
    //    - primary 一完成就单独 durable（RestartPrimaryBaselineReady）；
    //    - fallback 失败/被杀后，重跑只重建 fallback，复用同一 primary cid/密文/commit，
    //      避免重建 random primary 撞 relay 每 epoch role 唯一 CAS。
    let (primary, fallback) = match journal.phase {
        RotationPhase::RestartSlotsUploaded => {
            let primary = prepare_baseline(
                vault,
                client,
                request,
                &plan,
                BaselineRole::Primary,
                frozen_hashes.clone(),
            )?;
            journal
                .set_restart_primary_baseline(vault, journal_baseline_from_prepared(&primary)?)?;
            let fallback = prepare_baseline(
                vault,
                client,
                request,
                &plan,
                BaselineRole::Fallback,
                frozen_hashes,
            )?;
            assert_independent_baselines(
                primary
                    .evidence
                    .as_ref()
                    .ok_or(SyncError::LocalCorrupt("primary 缺首次基线证据"))?,
                fallback
                    .evidence
                    .as_ref()
                    .ok_or(SyncError::LocalCorrupt("fallback 缺首次基线证据"))?,
                request.freeze_seq,
            )?;
            journal
                .set_restart_fallback_baseline(vault, journal_baseline_from_prepared(&fallback)?)?;
            (primary, fallback)
        }
        RotationPhase::RestartPrimaryBaselineReady => {
            // primary 已 durable：绝不能重新生成 random primary（role CAS 会拒绝）。
            let primary = baseline_from_journal(vault, &journal, BaselineRole::Primary)?;
            let fallback = prepare_baseline(
                vault,
                client,
                request,
                &plan,
                BaselineRole::Fallback,
                frozen_hashes,
            )?;
            let fallback_evidence = fallback
                .evidence
                .as_ref()
                .ok_or(SyncError::LocalCorrupt("fallback 缺首次基线证据"))?;
            assert_fallback_independent_of_primary(
                journal
                    .baselines
                    .first()
                    .ok_or(SyncError::LocalCorrupt("restart journal 缺主基线"))?,
                fallback_evidence,
                request.freeze_seq,
            )?;
            journal
                .set_restart_fallback_baseline(vault, journal_baseline_from_prepared(&fallback)?)?;
            (primary, fallback)
        }
        _ => baselines_from_journal(vault, &journal)?,
    };

    // 4. 全设备 grants：只封给控制链 active 且未吊销的设备；固定 leaf 字节先落 journal。
    let (recipients, skipped) = grant_recipients(client, request, &member_ids)?;
    let leaves = if journal.restart_grant_manifests.is_empty() {
        let leaves = plan_grants(
            &plan.candidate_root,
            request.old_manager,
            request.manager_rev,
            &recipients,
        )?;
        let manifests = leaves
            .iter()
            .map(GrantLeaf::manifest_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        let digest = grants_digest(&leaves)?;
        journal.set_restart_grant_manifests(vault, manifests, digest)?;
        leaves
    } else {
        let leaves = decode_grant_leaves(&journal.restart_grant_manifests)?;
        assert_grants_cover_recipients(&leaves, &recipients)?;
        leaves
    };
    let staged_grants_digest = grants_digest(&leaves)?;
    if journal
        .grants_digest
        .is_some_and(|digest| digest != staged_grants_digest)
    {
        return Err(SyncError::Object("journal grant digest 与固定 leaf 不一致"));
    }
    management.stage_grants(plan.rotation_id, &leaves)?;
    if journal.phase == RotationPhase::RestartBaselinesReady {
        journal.advance(RotationPhase::RestartGrantsStaged)?;
        journal.save(vault)?;
    }

    // 5. activate 前必须确认新恢复码已备份；否则拒绝并保持冻结。
    if request.recovery_backup_confirmed {
        journal.confirm_recovery_backup(vault)?;
    }
    if !journal.recovery_backup_confirmed {
        return Err(SyncError::Unsupported(BACKUP_UNCONFIRMED));
    }

    // 6. 完整 activation T + manager transition 双签：四个 digest 全部按 restart 新材料
    //    重算；T 的控制链字段来自 relay 已验证事实，但两条 commit digest **只取 journal
    //    里 durable 的本地 receipt**（评审 0002 P1-2：禁止恢复时重新从 relay 取摘要重签）。
    //    先 durable 再发网络。
    let member_cert_digest: [u8; 32] = Sha256::digest(menghunote_sync_proto::member_cert_digest(
        request.member_certs,
    ))
    .into();
    let new_asset_set_digest = candidate_asset_set_digest(plan.to_epoch, &reencrypted_assets)?;
    let facts = management.activation_facts(plan.rotation_id, ManagerKeyStage::PreActivation)?;
    if facts.stored.is_some() {
        return Err(SyncError::RelayWire(
            "relay 已存在本次 rotation 的 activation T，必须按已激活路径恢复",
        ));
    }
    if facts.control_rev != plan.control_rev {
        return Err(SyncError::RelayWire(
            "activation 前控制链 rev 与 restart 契约不一致",
        ));
    }
    let (primary_commit_digest, fallback_commit_digest) = durable_commit_digests(&journal)?;
    if primary.commit_digest != primary_commit_digest
        || fallback.commit_digest != fallback_commit_digest
    {
        return Err(SyncError::LocalCorrupt(
            "restart 内存基线与 journal durable receipt 不一致",
        ));
    }
    let material = ActivationTMaterial {
        vault_id: request.vault_id,
        instance_id: request.instance_id,
        rotation_id: plan.rotation_id,
        from_epoch: plan.from_epoch,
        to_epoch: plan.to_epoch,
        freeze_seq: request.freeze_seq,
        old_manager_rev: request.manager_rev,
        new_manager_rev: plan.new_manager_rev,
        new_manager_public: plan.slots.new_manager_public,
        prev_control_hash: facts.prev_control_hash,
        expected_control_rev: plan.control_rev.saturating_add(1),
        primary_checkpoint: primary.checkpoint_id,
        primary_commit_digest,
        fallback_checkpoint: fallback.checkpoint_id,
        fallback_commit_digest,
        slot_set_digest,
        member_cert_digest,
        grants_digest: staged_grants_digest,
        old_asset_set_digest: asset_set_digest,
        new_asset_set_digest,
    };
    let new_manager =
        build_manager_transition(&material, request.old_manager, &plan.slots.new_manager)?;
    let activation = ActivationInput {
        rotation_id: plan.rotation_id,
        request_id: request.request_id,
        expected_control_rev: plan.control_rev,
        primary_checkpoint: primary.checkpoint_id,
        fallback_checkpoint: fallback.checkpoint_id,
        member_cert_digest,
        slot_set_digest,
        grants_digest: staged_grants_digest,
        asset_set_digest,
        new_manager: Some(new_manager),
        candidate_root: plan.candidate_root.clone(),
        new_manager_public: plan.slots.new_manager_public,
        new_manager_rev: plan.new_manager_rev,
        pv: request.pv,
    };
    if let Some(new_manager) = &activation.new_manager {
        journal.set_restart_activation(
            vault,
            activation.primary_checkpoint,
            activation.fallback_checkpoint,
            activation.member_cert_digest,
            activation.slot_set_digest,
            activation.grants_digest,
            activation.asset_set_digest,
            new_manager.transition.clone(),
            new_manager
                .old_signature
                .as_deref()
                .and_then(|signature| signature.try_into().ok())
                .ok_or(SyncError::Object("旧管理签名长度非法"))?,
            new_manager
                .new_signature
                .as_deref()
                .and_then(|signature| signature.try_into().ok())
                .ok_or(SyncError::Object("新管理签名长度非法"))?,
            0,
        )?;
    } else {
        return Err(SyncError::Object("activate 必须带 new_manager（§27.2）"));
    }

    let activated = match management.activate(&activation) {
        Ok(activated) => activated,
        Err(error) => {
            // 超时/丢响应：只信 relay 明确 activated，绝不把传输错误当成功。
            match management.activated_outcome(plan.rotation_id)? {
                Some(found) if activated_matches(&found, &activation) => found,
                _ => return Err(error),
            }
        }
    };
    if !activated_matches(&activated, &activation) {
        return Err(SyncError::RelayWire(
            "activate 回执 epoch/cid 与本次候选不一致",
        ));
    }
    journal.mark_restart_activated(
        vault,
        activated.epoch,
        activated.control_rev,
        activated.activation_commit.clone(),
    )?;

    Ok(RestartExecution {
        plan,
        slot_set_digest,
        asset_set_digest,
        member_cert_digest,
        grants_digest: staged_grants_digest,
        primary,
        fallback,
        reencrypted_assets,
        frozen_asset_count: frozen_total,
        grants: leaves,
        skipped,
        activation,
        activated,
    })
}

/// activate 回执必须是本次候选 epoch、且指名的两个 cid 与 activate 输入一致。
fn activated_matches(activated: &ActivatedOutcome, input: &ActivationInput) -> bool {
    activated.epoch == input.candidate_root.key_epoch
        && activated.primary_checkpoint == input.primary_checkpoint
        && activated.fallback_checkpoint == input.fallback_checkpoint
        && activated.control_rev == input.expected_control_rev.saturating_add(1)
}

/// relay 是否因为管理 key 不再有效而拒绝了请求（activate 后旧 key 的典型响应）。
///
/// 这个判定只用于区分"activate 还没生效（应继续用旧 key 重试）"与"其他真实错误"，
/// **绝不**把拒绝当成成功。
fn is_manager_key_rejected(error: &SyncError) -> bool {
    match error {
        SyncError::RelayApi { code, .. } => matches!(
            code.as_str(),
            "MANAGEMENT_REQUIRED" | "UNAUTHORIZED" | "FORBIDDEN" | "REVOKED"
        ),
        SyncError::HttpStatus(status) => matches!(status, 401 | 403),
        _ => false,
    }
}

/// 从 journal 的 `manager_seed` 重建新管理 key，并校验公钥等于 `new_manager_public`。
///
/// 缺失或不匹配都是 journal 损坏：activate 之后 relay 只认这把 key，绝不能猜。
fn manager_from_journal(journal: &RotationJournal) -> Result<ManagementKey, SyncError> {
    let seed = journal.manager_seed.ok_or(SyncError::LocalCorrupt(
        "restart journal 缺 manager_seed（无法派生 activate 之后的新管理 key）",
    ))?;
    let expected = journal.new_manager_public.ok_or(SyncError::LocalCorrupt(
        "restart journal 缺 new_manager_public（无法校验新管理 key）",
    ))?;
    let manager = ManagementKey::from_seed(Secret32::from_array(seed));
    if manager.public_key() != expected {
        return Err(SyncError::LocalCorrupt(
            "restart journal 管理 key 与 new_manager_public 不匹配",
        ));
    }
    Ok(manager)
}

/// 已落 `RestartActivated` 的 journal：只查询 relay 并重建结果，绝不重发 restart/activate。
fn resume_already_activated<M: RestartManagement>(
    vault: &Vault,
    management: &mut M,
    request: &RestartRequest<'_>,
    mut journal: RotationJournal,
) -> Result<RestartExecution, SyncError> {
    let plan = plan_from_journal(&journal, request.vault_id)?;
    let new_manager = manager_from_journal(&journal)?;
    management.adopt_new_manager(&new_manager)?;
    let found = management
        .activated_outcome(plan.rotation_id)?
        .ok_or(SyncError::RelayWire(
            "restart journal 已激活，但 relay /rotations/current 未返回该 rotation",
        ))?;
    resume_activated_restart(management, vault, &mut journal, request, plan, found, true)
}

/// activate 可能已生效但回执丢失：先用新管理 key 查一次；
/// `Ok(None)` 表示 relay 尚未激活（或新 key 还未生效），调用方应继续普通重试。
fn resume_if_activation_landed<M: RestartManagement>(
    vault: &Vault,
    management: &mut M,
    request: &RestartRequest<'_>,
    mut journal: RotationJournal,
) -> Result<Option<RestartExecution>, SyncError> {
    let plan = plan_from_journal(&journal, request.vault_id)?;
    let new_manager = manager_from_journal(&journal)?;
    management.adopt_new_manager(&new_manager)?;
    match management.activated_outcome(plan.rotation_id) {
        Ok(Some(found)) => {
            resume_activated_restart(management, vault, &mut journal, request, plan, found, false)
                .map(Some)
        }
        Ok(None) => Ok(None),
        // 新 key 换 token 被拒 = activate 还没生效，旧 key 仍是当前管理 key：继续重试。
        Err(error) if is_manager_key_rejected(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// crash 在 relay activated 之后、journal 落 phase 之前：只信 relay 回执并重建执行结果。
///
/// 此时不能重新造一份 T（`prev_control_hash`/commit 摘要已随激活固定），必须从
/// `/control` 取回 relay 已持久化的同一份 T，用本地旧/新管理公钥复算双签，并与
/// journal 里的候选材料逐字段比对。
fn resume_activated_restart<M: RestartManagement>(
    management: &mut M,
    vault: &Vault,
    journal: &mut RotationJournal,
    request: &RestartRequest<'_>,
    plan: RestartPlan,
    found: ActivatedOutcome,
    already_activated: bool,
) -> Result<RestartExecution, SyncError> {
    if found.epoch != plan.to_epoch {
        return Err(SyncError::RelayWire("relay activated epoch 不是候选 epoch"));
    }
    if already_activated {
        // journal 已落 `RestartActivated`：relay 回执必须与 journal 持久化的一致
        // （否则可能是另一个 rotation/回执被错误复用）。
        if journal.phase != RotationPhase::RestartActivated
            || journal.activated_epoch != Some(found.epoch)
            || journal.activated_control_rev != Some(found.control_rev)
        {
            return Err(SyncError::RelayWire(
                "relay activated 回执与 journal 已记录的激活回执不一致",
            ));
        }
    }
    let (primary, fallback) = baselines_from_journal(vault, journal)?;
    let reencrypted_assets = journal
        .restart_assets
        .iter()
        .map(|asset| ReencryptedAsset {
            real_sha256: asset.real_sha256,
            byte_len: asset.byte_len,
            media_type: asset.media_type.clone(),
            object_id: asset.object_id,
            object_digest: asset.object_digest,
        })
        .collect::<Vec<_>>();
    let facts = management.activation_facts(plan.rotation_id, ManagerKeyStage::PostActivation)?;
    let stored = facts.stored.clone().ok_or(SyncError::LocalCorrupt(
        "relay 已激活但控制事件缺 activation T，无法复算",
    ))?;
    let activation_t = ActivationTV1::decode(&stored.activation_t)?;
    let t_signing_input = activation_t_signing_input(&activation_t)?;
    crate::device::verify_signature(
        &request.old_manager.public_key(),
        &t_signing_input,
        &fixed64(&stored.old_t_signature, "旧管理 T 签名")?,
    )?;
    crate::device::verify_signature(
        &plan.slots.new_manager.public_key(),
        &t_signing_input,
        &fixed64(&stored.new_t_signature, "新管理 T 签名")?,
    )?;
    let slot_set_digest = journal
        .slot_set_digest
        .ok_or(SyncError::LocalCorrupt("restart journal 缺 slot digest"))?;
    let member_cert_digest = journal
        .member_set_digest
        .ok_or(SyncError::LocalCorrupt("restart journal 缺 member digest"))?;
    let grants_digest = journal
        .grants_digest
        .ok_or(SyncError::LocalCorrupt("restart journal 缺 grants digest"))?;
    let old_asset_set_digest = journal
        .asset_set_digest
        .ok_or(SyncError::LocalCorrupt("restart journal 缺 asset digest"))?;
    let new_asset_set_digest = candidate_asset_set_digest(plan.to_epoch, &reencrypted_assets)?;
    // 只信 journal 里 durable 的本地 receipt，绝不重新从 relay facts 取 commit 摘要。
    let (primary_commit_digest, fallback_commit_digest) = durable_commit_digests(journal)?;
    let expected_control_rev = journal
        .control_rev
        .checked_add(1)
        .ok_or(SyncError::LocalCorrupt("restart journal control_rev 溢出"))?;
    let old_manager_rev = u32::try_from(request.manager_rev)
        .map_err(|_| SyncError::LocalCorrupt("restart manager_rev 超出 u32"))?;
    let new_manager_rev = u32::try_from(plan.new_manager_rev)
        .map_err(|_| SyncError::LocalCorrupt("restart new_manager_rev 超出 u32"))?;
    let matches = activation_t.vault_id == request.vault_id
        && activation_t.instance_id == request.instance_id
        && activation_t.rotation_id == plan.rotation_id
        && activation_t.from_epoch == plan.from_epoch
        && activation_t.to_epoch == plan.to_epoch
        && activation_t.freeze_seq == request.freeze_seq
        && activation_t.old_manager_rev == old_manager_rev
        && activation_t.new_manager_rev == new_manager_rev
        && activation_t.new_manager_pubkey == plan.slots.new_manager_public
        && activation_t.prev_control_hash == facts.prev_control_hash
        && activation_t.expected_control_rev == expected_control_rev
        && activation_t.primary_cid == primary.checkpoint_id
        && activation_t.fallback_cid == fallback.checkpoint_id
        && activation_t.primary_commit_digest == primary_commit_digest
        && activation_t.fallback_commit_digest == fallback_commit_digest
        && activation_t.slot_set_digest == slot_set_digest
        && activation_t.member_cert_digest == member_cert_digest
        && activation_t.grants_digest == grants_digest
        && activation_t.old_asset_set_digest == old_asset_set_digest
        && activation_t.new_asset_set_digest == new_asset_set_digest;
    if !matches {
        return Err(SyncError::RelayWire(
            "relay 已持久化的 activation T 与 journal 候选材料不一致",
        ));
    }
    let new_manager = match (
        &journal.new_manager_transition,
        journal.old_manager_signature,
        journal.new_manager_signature,
    ) {
        (Some(transition), Some(old), Some(new)) => NewManagerV1 {
            transition: transition.clone(),
            old_signature: Some(old.to_vec()),
            new_signature: Some(new.to_vec()),
            activation_t: Some(stored.activation_t.clone()),
            old_t_signature: Some(stored.old_t_signature.clone()),
            new_t_signature: Some(stored.new_t_signature.clone()),
        },
        _ => {
            return Err(SyncError::LocalCorrupt(
                "restart journal 缺 activate 管理转换材料",
            ))
        }
    };
    let activation = ActivationInput {
        rotation_id: plan.rotation_id,
        request_id: request.request_id,
        expected_control_rev: journal.control_rev,
        primary_checkpoint: primary.checkpoint_id,
        fallback_checkpoint: fallback.checkpoint_id,
        member_cert_digest,
        slot_set_digest,
        grants_digest,
        asset_set_digest: old_asset_set_digest,
        new_manager: Some(new_manager),
        candidate_root: plan.candidate_root.clone(),
        new_manager_public: journal
            .new_manager_public
            .ok_or(SyncError::LocalCorrupt("restart journal 缺新管理公钥"))?,
        new_manager_rev: journal.new_manager_rev,
        pv: request.pv,
    };
    if !activated_matches(&found, &activation) {
        return Err(SyncError::RelayWire(
            "relay activated 回执与 journal 里的候选材料不一致",
        ));
    }
    if !already_activated {
        journal.mark_restart_activated(
            vault,
            found.epoch,
            found.control_rev,
            found.activation_commit.clone(),
        )?;
    }
    let grants = decode_grant_leaves(&journal.restart_grant_manifests)?;
    let skipped = request
        .revoked
        .iter()
        .map(|device_id| SkippedDevice {
            device_id: *device_id,
            reason: SkipReason::Revoked,
        })
        .collect::<Vec<_>>();
    Ok(RestartExecution {
        slot_set_digest: activation.slot_set_digest,
        asset_set_digest: activation.asset_set_digest,
        member_cert_digest: activation.member_cert_digest,
        grants_digest: activation.grants_digest,
        frozen_asset_count: reencrypted_assets.len() as u64,
        primary,
        fallback,
        reencrypted_assets,
        grants,
        skipped,
        activation,
        activated: found,
        plan,
    })
}

/// 成员集合前置校验：本机必须在 active 控制链里，且 active 证书不得含已吊销设备。
fn check_members(request: &RestartRequest<'_>) -> Result<BTreeSet<DeviceId>, SyncError> {
    if request.revoked.contains(&request.device_id) {
        return Err(SyncError::Unsupported("本机已被吊销，不能发起 restart"));
    }
    let member_ids = request
        .member_certs
        .iter()
        .map(|input| input.device_id)
        .collect::<BTreeSet<_>>();
    if member_ids.is_empty() {
        return Err(SyncError::Unsupported("控制链没有 active 成员证书"));
    }
    if !member_ids.contains(&request.device_id) {
        return Err(SyncError::Object("本机不在控制链 active 成员集合"));
    }
    if request
        .member_certs
        .iter()
        .any(|input| request.revoked.contains(&input.device_id))
    {
        return Err(SyncError::Object(
            "active 成员证书集合仍含已吊销设备（控制链缓存过期）",
        ));
    }
    Ok(member_ids)
}

/// 准备一份候选基线并读回验证。
fn prepare_baseline<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    request: &RestartRequest<'_>,
    plan: &RestartPlan,
    role: BaselineRole,
    frozen_hashes: Vec<[u8; 32]>,
) -> Result<RestartBaseline, SyncError> {
    // 评审 0003 P1-D：writer 在 create 之前 durable per-role intent，并在
    // create/upload/validate/promote 每个阶段落 receipt；重启按同一 request_id/cid 恢复。
    let prepared = crate::rotation_stage::prepare_restart_baseline_with_role(
        vault,
        client,
        request.vault_id,
        request.device_id,
        request.signing_key.clone(),
        plan.candidate_root.clone(),
        request.peer_id,
        request.assets,
        role.as_str(),
        plan.rotation_id,
        plan.control_rev,
        // 候选基线 coverage 复核必须用旧 active 根（relay 仍在 from_epoch）；缺根时
        // checkpoint writer 会暂停/报错，绝不拿候选根猜。
        &request.old_root,
    )?;
    if prepared.report.covered_seq < request.freeze_seq {
        return Err(SyncError::Object("候选基线 S 未覆盖 restart 冻结点"));
    }
    // 本地 durable receipt：T 的 commit digest 只能来自这里（绝不回 relay 取）。
    let commit_digest =
        checkpoint_commit_digest(request.vault_id, request.device_id, &prepared.report)?;
    let evidence =
        capture_baseline_evidence(client, &plan.candidate_root, &prepared, &frozen_hashes)?;
    Ok(RestartBaseline {
        role,
        checkpoint_id: prepared.report.checkpoint_id,
        covered_seq: prepared.report.covered_seq,
        commit_digest,
        evidence: Some(evidence),
        prepared: Some(prepared),
    })
}

/// 从 journal 复用已读回验证的候选基线（不重传 snapshot，cid 必须仍是激活目标）。
///
/// 安全门：journal 里的 `verified=true` 只说明"旧进程写过这个标记"，不足以证明
/// 覆盖证据被实证过。只有本机存在**新修订**的 durable verified receipt，且其
/// commit digest 与 journal 完全一致，才允许快捷恢复；否则 fail-closed（旧开发
/// vault 的 journal 一律拒绝，必须重建候选基线）。
fn baseline_from_journal(
    vault: &Vault,
    journal: &RotationJournal,
    role: BaselineRole,
) -> Result<RestartBaseline, SyncError> {
    let index = match role {
        BaselineRole::Primary => 0,
        BaselineRole::Fallback => 1,
    };
    let baseline = journal
        .baselines
        .get(index)
        .ok_or(SyncError::LocalCorrupt("restart journal 缺候选基线"))?;
    if !baseline.verified {
        return Err(SyncError::LocalCorrupt("restart journal 基线未验证"));
    }
    if baseline.commit_digest == [0u8; 32] {
        return Err(SyncError::LocalCorrupt(
            "restart journal 缺本地 commit receipt：拒绝从 relay facts 重签 T",
        ));
    }
    let receipt = crate::checkpoint::load_verified_receipt(vault, &baseline.checkpoint_id)?.ok_or(
        SyncError::LocalCorrupt(
            "restart journal 缺新修订 verified receipt：拒绝按旧 verified=true 快捷恢复",
        ),
    )?;
    if receipt.checkpoint_id != baseline.checkpoint_id
        || receipt.commit_digest != baseline.commit_digest
        || receipt.epoch != journal.to_epoch
        || receipt.covered_seq < baseline.covered_seq
    {
        return Err(SyncError::LocalCorrupt(
            "verified receipt 与 restart journal 候选基线不一致",
        ));
    }
    let covered_seq = journal.freeze_seq.unwrap_or(0);
    Ok(RestartBaseline {
        role,
        checkpoint_id: baseline.checkpoint_id,
        covered_seq: baseline.covered_seq.max(covered_seq),
        commit_digest: baseline.commit_digest,
        evidence: None,
        prepared: None,
    })
}

/// 从 journal 复用已读回验证的两份候选基线（不重传 snapshot，cid 必须仍是激活目标）。
fn baselines_from_journal(
    vault: &Vault,
    journal: &RotationJournal,
) -> Result<(RestartBaseline, RestartBaseline), SyncError> {
    if journal.baselines.len() != 2 {
        return Err(SyncError::LocalCorrupt(
            "restart journal 缺两份已验证候选基线",
        ));
    }
    let primary = &journal.baselines[0];
    let fallback = &journal.baselines[1];
    if !primary.verified || !fallback.verified {
        return Err(SyncError::LocalCorrupt("restart journal 基线未验证"));
    }
    if primary.checkpoint_id == fallback.checkpoint_id
        || primary.manifest_object_id == fallback.manifest_object_id
    {
        return Err(SyncError::LocalCorrupt("restart journal 两份基线不独立"));
    }
    if primary.commit_digest == [0u8; 32] || fallback.commit_digest == [0u8; 32] {
        return Err(SyncError::LocalCorrupt(
            "restart journal 缺本地 commit receipt：拒绝从 relay facts 重签 T",
        ));
    }
    Ok((
        baseline_from_journal(vault, journal, BaselineRole::Primary)?,
        baseline_from_journal(vault, journal, BaselineRole::Fallback)?,
    ))
}

/// fallback 重试时与已持久 primary 的独立性校验（不再重建 primary evidence）。
fn assert_fallback_independent_of_primary(
    primary: &JournalBaseline,
    fallback: &BaselineEvidence,
    freeze_seq: u64,
) -> Result<(), SyncError> {
    if primary.checkpoint_id == fallback.checkpoint_id
        || primary.manifest_object_id == fallback.manifest_object_id
    {
        return Err(SyncError::Object("两份基线复用了同一 cid/manifest"));
    }
    if primary.covered_seq < freeze_seq || fallback.covered_seq < freeze_seq {
        return Err(SyncError::Object("基线 S 未覆盖冻结点"));
    }
    if primary.asset_set_digest != fallback.asset_set_digest {
        return Err(SyncError::Object("两份基线的资产集合不一致"));
    }
    Ok(())
}

/// grant 收件人：`(device_id, HPKE 公钥)`。
type GrantRecipients = Vec<(DeviceId, [u8; 32])>;

/// 从控制链 active 成员证书取未吊销收件人；被跳过的设备必须带原因。
fn grant_recipients<C: RelayClient>(
    client: &mut C,
    request: &RestartRequest<'_>,
    member_ids: &BTreeSet<DeviceId>,
) -> Result<(GrantRecipients, Vec<SkippedDevice>), SyncError> {
    let advertised = active_member_hpke_keys(client)?;
    let mut skipped = request
        .revoked
        .iter()
        .map(|device_id| SkippedDevice {
            device_id: *device_id,
            reason: SkipReason::Revoked,
        })
        .collect::<Vec<_>>();
    let mut usable = Vec::with_capacity(member_ids.len());
    for (device_id, hpke_public) in advertised {
        // 双保险：即使控制链缓存没剔除，也绝不封给已吊销设备。
        if request.revoked.contains(&device_id) {
            continue;
        }
        if !member_ids.contains(&device_id) {
            skipped.push(SkippedDevice {
                device_id,
                reason: SkipReason::NotInControlChain,
            });
            continue;
        }
        usable.push((device_id, hpke_public));
    }
    for member in member_ids {
        if !usable.iter().any(|(device_id, _)| device_id == member) {
            return Err(SyncError::Object(
                "active 成员缺少 HPKE 公钥，grant 集合不完整",
            ));
        }
    }
    match usable
        .iter()
        .find(|(device_id, _)| *device_id == request.device_id)
    {
        Some((_, hpke_public)) if *hpke_public == request.local_hpke_public => {}
        Some(_) => {
            return Err(SyncError::Object(
                "控制链本机 HPKE 公钥与本地密钥不符（拒绝封给错公钥）",
            ))
        }
        None => return Err(SyncError::Object("控制链没有本机 active HPKE 公钥")),
    }
    Ok((usable, skipped))
}

/// 从 journal 的固定秘密重建 plan（绝不重新生成根/口令/R'）。
fn plan_from_journal(
    journal: &RotationJournal,
    vault_id: VaultId,
) -> Result<RestartPlan, SyncError> {
    if !journal.phase.is_restart() {
        return Err(SyncError::LocalCorrupt("journal 不是 restart phase"));
    }
    if journal.phase == RotationPhase::RestartIntentRecorded {
        return Err(SyncError::LocalCorrupt(
            "restart intent 尚未拿到 relay 分配（缺 rotation_id/候选 epoch）",
        ));
    }
    let rotation_id = journal
        .rotation_id
        .ok_or(SyncError::LocalCorrupt("restart journal 缺 rotation_id"))?;
    let candidate_root = journal
        .candidate_root
        .clone()
        .ok_or(SyncError::LocalCorrupt("restart journal 缺候选根"))?;
    if candidate_root.vault_id != vault_id || candidate_root.key_epoch != journal.to_epoch {
        return Err(SyncError::LocalCorrupt("restart journal 候选根 epoch 不符"));
    }
    let previous_candidate_epoch = journal
        .previous_candidate_epoch
        .ok_or(SyncError::LocalCorrupt("restart journal 缺旧候选 epoch"))?;
    let password_bytes = journal
        .restart_password
        .as_ref()
        .ok_or(SyncError::LocalCorrupt("restart journal 缺新口令"))?;
    let password = Zeroizing::new(
        String::from_utf8(password_bytes.clone())
            .map_err(|_| SyncError::LocalCorrupt("restart journal 新口令不是 UTF-8"))?,
    );
    let recovery_bytes = journal
        .recovery_seed
        .ok_or(SyncError::LocalCorrupt("restart journal 缺 R'"))?;
    let recovery_seed = RecoverySeed::from_array(recovery_bytes);
    let new_manager = manager_from_journal(journal)?;
    let expected_manager_public = journal
        .new_manager_public
        .ok_or(SyncError::LocalCorrupt("restart journal 缺新管理公钥"))?;
    let password_slot = slot_from_journal(journal, SlotKind::Password)?;
    let recovery_slot = slot_from_journal(journal, SlotKind::Recovery)?;
    let recovery_code = recovery_seed.to_code()?;
    let password_message_id =
        journal
            .restart_password_message_id
            .ok_or(SyncError::LocalCorrupt(
                "restart journal 缺 password message_id",
            ))?;
    let recovery_message_id =
        journal
            .restart_recovery_message_id
            .ok_or(SyncError::LocalCorrupt(
                "restart journal 缺 recovery message_id",
            ))?;
    Ok(RestartPlan {
        from_epoch: journal.from_epoch,
        previous_candidate_epoch,
        to_epoch: journal.to_epoch,
        rotation_id,
        control_rev: journal.control_rev,
        password,
        password_message_id,
        recovery_message_id,
        slots: CandidateSlots {
            password: password_slot,
            recovery: recovery_slot,
            recovery_seed,
            recovery_code,
            new_manager,
            new_manager_public: expected_manager_public,
        },
        candidate_root,
        new_manager_rev: journal.new_manager_rev,
    })
}

fn slot_from_journal(journal: &RotationJournal, kind: SlotKind) -> Result<KeySlotV1, SyncError> {
    let mut found = None;
    for (index, descriptor_bytes) in journal.new_slot_descriptors.iter().enumerate() {
        let descriptor = SlotDescriptorV1::decode(descriptor_bytes)?;
        if descriptor.kind != kind {
            continue;
        }
        let body = journal
            .new_slot_bodies
            .get(index)
            .ok_or(SyncError::LocalCorrupt("restart journal 槽体数量不符"))?;
        if journal.new_slot_ids.get(index).copied() != Some(descriptor.slot_id) {
            return Err(SyncError::LocalCorrupt(
                "restart journal 槽 id 与描述符不符",
            ));
        }
        if found.is_some() {
            return Err(SyncError::LocalCorrupt("restart journal 槽 kind 重复"));
        }
        found = Some(KeySlotV1 {
            descriptor,
            wrapped: WrappedRoot::from_bytes(body)?,
        });
    }
    found.ok_or(SyncError::LocalCorrupt("restart journal 缺候选槽"))
}

fn decode_grant_leaves(manifests: &[Vec<u8>]) -> Result<Vec<GrantLeaf>, SyncError> {
    let mut leaves = Vec::with_capacity(manifests.len());
    for manifest in manifests {
        let map = crate::cbor::decode(manifest)?;
        let mut leaf = GrantLeaf {
            device_id: crate::cbor::bytes_fixed(&map, 0)?,
            epoch: crate::cbor::uint_field(&map, 1)?,
            grant_id: crate::cbor::bytes_fixed(&map, 2)?,
            hpke_enc: crate::cbor::bytes_fixed(&map, 3)?,
            ciphertext: crate::cbor::bytes_field(&map, 4)?,
            signed_header: crate::cbor::bytes_field(&map, 5)?,
            signature: crate::cbor::bytes_fixed(&map, 6)?,
            manager_rev: crate::cbor::uint_field(&map, 7)?,
            leaf_digest: [0u8; 32],
        };
        leaf.leaf_digest = Sha256::digest(manifest).into();
        leaves.push(leaf);
    }
    Ok(leaves)
}

fn assert_grants_cover_recipients(
    leaves: &[GrantLeaf],
    recipients: &GrantRecipients,
) -> Result<(), SyncError> {
    let leaf_devices = leaves
        .iter()
        .map(|leaf| leaf.device_id)
        .collect::<BTreeSet<_>>();
    let recipient_devices = recipients
        .iter()
        .map(|(device_id, _)| *device_id)
        .collect::<BTreeSet<_>>();
    if leaf_devices != recipient_devices {
        return Err(SyncError::Object(
            "journal 固定 grant 集合与当前 active 收件人不一致",
        ));
    }
    Ok(())
}
