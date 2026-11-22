//! 轮换 journal（S7-28，ADR §28.2；阶段 A–C 的持久状态）。
//!
//! 这里只落**本地持久状态机**，不调用 relay：阶段 A freeze 固定 intent/secret，阶段 B
//! 记录 begin 响应（request_id 幂等查询在 relay task-101 落地后接），阶段 C 要求旧根
//! 已 integrate 到 S 且没有 ≤S 的 pending outbox，才允许继续。
//!
//! journal 经 [`crate::identity_wrap`] 封装后存 `vault.meta`（Windows DPAPI / Linux
//! 用户主密钥，见 ADR 0008），不落明文密钥；根/新管理 key 等敏感字段只在内存与
//! 本地封装密文中出现。未完成字段用 `TODO(task-101)` 标注的占位保持可往返。

use sha2::{Digest, Sha256};

use crate::cbor;
use crate::error::SyncError;
use crate::identity_wrap;
use crate::keys::{RootSecretV1, Secret32};
use crate::slot::KeySlotV1;
use crate::{DeviceId, VaultId};
use menghunote_storage::{AffectedGuard, Statement, Value, Vault};

/// journal 在 vault meta 中的键。
pub const ROTATION_JOURNAL_META: &str = "sync.rotation_journal";
const JOURNAL_DOMAIN: &[u8] = b"MN/v1/rotation-journal\0";

/// 轮换阶段（只允许按顺序推进；Aborted 可从任意阶段进入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationPhase {
    /// 阶段 A：已冻结旧 epoch，网络 push/encrypt 停止。
    Frozen,
    /// 阶段 B：relay `/rotations` begin 已成功，S 固定。
    Begun,
    /// 阶段 C：旧根已 integrate 至 S、本地 cut L 已固定。
    Integrated,
    /// 阶段 D：旧资产已全量重加密（task-103）。
    Reencrypted,
    /// 阶段 E：两份独立基线已各自导入验证（task-103）。
    BaselinesReady,
    /// 阶段 F：grants 已 staging（task-103）。
    GrantsReady,
    /// 阶段 G：双签 activate（task-104）。
    Activated,
    /// 阶段 H：本地原子推广（task-104）。
    Promoted,
    /// 阶段 I/J：其它设备/outbox 重打包完成（task-104）。
    Completed,
    /// 原子 restart / abort。
    Aborted,
    /// restart：POST 前的公开 intent 已 DPAPI 落盘（无新鲜秘密、无 relay 分配）。
    RestartIntentRecorded,
    /// restart：fresh 秘密与固定槽密文已 DPAPI 落盘（上传前）。
    RestartPrepared,
    /// restart：fresh 槽已用同一份固定密文上传。
    RestartSlotsUploaded,
    /// restart：主基线已读回验证并落盘（fallback 尚未生成）。
    RestartPrimaryBaselineReady,
    /// restart：两份独立基线已读回验证。
    RestartBaselinesReady,
    /// restart：未吊销设备的 grant 已按固定 leaf 字节 stage。
    RestartGrantsStaged,
    /// restart：activate 请求已持久化、正在等 relay activated 回执。
    RestartActivating,
    /// restart：已收到 relay activated 回执（本地身份推广由阶段 H 接）。
    RestartActivated,
}

impl RotationPhase {
    fn code(self) -> u64 {
        match self {
            Self::Frozen => 1,
            Self::Begun => 2,
            Self::Integrated => 3,
            Self::Reencrypted => 4,
            Self::BaselinesReady => 5,
            Self::GrantsReady => 6,
            Self::Activated => 7,
            Self::Promoted => 8,
            Self::Completed => 9,
            Self::Aborted => 10,
            Self::RestartPrepared => 11,
            Self::RestartSlotsUploaded => 12,
            Self::RestartBaselinesReady => 13,
            Self::RestartGrantsStaged => 14,
            Self::RestartActivating => 15,
            Self::RestartActivated => 16,
            Self::RestartIntentRecorded => 17,
            Self::RestartPrimaryBaselineReady => 18,
        }
    }

    fn from_code(code: u64) -> Result<Self, SyncError> {
        Ok(match code {
            1 => Self::Frozen,
            2 => Self::Begun,
            3 => Self::Integrated,
            4 => Self::Reencrypted,
            5 => Self::BaselinesReady,
            6 => Self::GrantsReady,
            7 => Self::Activated,
            8 => Self::Promoted,
            9 => Self::Completed,
            10 => Self::Aborted,
            11 => Self::RestartPrepared,
            12 => Self::RestartSlotsUploaded,
            13 => Self::RestartBaselinesReady,
            14 => Self::RestartGrantsStaged,
            15 => Self::RestartActivating,
            16 => Self::RestartActivated,
            17 => Self::RestartIntentRecorded,
            18 => Self::RestartPrimaryBaselineReady,
            _ => return Err(SyncError::LocalCorrupt("rotation journal phase 非法")),
        })
    }

    /// 中文短标签（app 的轮换失败报告用；只描述**公开阶段名**，不含任何秘密）。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Frozen => "冻结旧 epoch",
            Self::Begun => "已被 relay 接受（S 固定）",
            Self::Integrated => "旧根已整合到 S",
            Self::Reencrypted => "冻结资产已重加密",
            Self::BaselinesReady => "候选基线已就绪",
            Self::GrantsReady => "grants 已暂存",
            Self::Activated => "已激活",
            Self::Promoted => "身份已推广",
            Self::Completed => "已完成",
            Self::Aborted => "已中止",
            Self::RestartIntentRecorded => "restart：intent 已落盘",
            Self::RestartPrepared => "restart：新秘密已落盘",
            Self::RestartSlotsUploaded => "restart：候选槽已上传",
            Self::RestartPrimaryBaselineReady => "restart：主基线已读回",
            Self::RestartBaselinesReady => "restart：双基线已读回",
            Self::RestartGrantsStaged => "restart：grants 已暂存",
            Self::RestartActivating => "restart：已发 activate",
            Self::RestartActivated => "restart：relay 已激活",
        }
    }

    /// 是否为原子 restart 的 phase（区别于普通轮换 A–J）。
    #[must_use]
    pub fn is_restart(self) -> bool {
        matches!(
            self,
            Self::RestartPrepared
                | Self::RestartSlotsUploaded
                | Self::RestartPrimaryBaselineReady
                | Self::RestartBaselinesReady
                | Self::RestartGrantsStaged
                | Self::RestartActivating
                | Self::RestartActivated
                | Self::RestartIntentRecorded
        )
    }

    fn next(self) -> Option<Self> {
        Some(match self {
            Self::Frozen => Self::Begun,
            Self::Begun => Self::Integrated,
            Self::Integrated => Self::Reencrypted,
            Self::Reencrypted => Self::BaselinesReady,
            Self::BaselinesReady => Self::GrantsReady,
            Self::GrantsReady => Self::Activated,
            Self::Activated => Self::Promoted,
            Self::Promoted => Self::Completed,
            Self::Completed | Self::Aborted => return None,
            Self::RestartPrepared => Self::RestartSlotsUploaded,
            Self::RestartSlotsUploaded => Self::RestartPrimaryBaselineReady,
            Self::RestartPrimaryBaselineReady => Self::RestartBaselinesReady,
            Self::RestartBaselinesReady => Self::RestartGrantsStaged,
            Self::RestartGrantsStaged => Self::RestartActivating,
            Self::RestartActivating => Self::RestartActivated,
            Self::RestartActivated => Self::Promoted,
            Self::RestartIntentRecorded => Self::RestartPrepared,
        })
    }
}

/// 应用（阶段 I）的持久 phase 键（评审 0003 P1-3）。
///
/// B 首次 apply 在**任何本地写库/merge 之前**先把本键写成 [`RotationApplyPhase::Applying`]：
/// 这是 I 阶段的 durable 冻结屏障，不依赖 RAM。engine 构造期读它（与
/// `sync.repack_required`、rotation journal 并列）决定是否冻结旧 epoch 的 encrypt/push。
///
/// 状态机只允许单向收尾：
///
/// ```text
/// none ──apply 开始──> applying ──合并+推广成功──> applied（解冻）
///                        │
///                        ├─merge/推广失败──> paused  （保持冻结，可重试）
///                        ├─cursor CAS 冲突─> conflict（保持冻结，可重试）
///                        └─restart 判定────> needs_restart（保持冻结，走更高 epoch）
/// ```
///
/// 任何未知/损坏值都按 [`RotationApplyPhase::Failed`] 处理（fail-closed）：宁可继续冻结，
/// 也不能因为无法证明 apply 已完成而让旧写继续。
pub const ROTATION_APPLY_PHASE_META: &str = "sync.rotation_apply_phase";

/// 阶段 I 的持久 apply 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationApplyPhase {
    /// apply 已启动：全部验证通过、即将写 baseline/merge。**这是冻结屏障本身**。
    Applying,
    /// 合并 + 身份推广均成功；唯一可以解除 I 阶段冻结的终态。
    Applied,
    /// 可重试失败（网络/IO/merge/推广）；保持冻结，等下一次 apply。
    Paused,
    /// baseline cursor CAS 冲突（未推进 cursor）；保持冻结。
    Conflict,
    /// restart 判定：必须用更高 epoch 的原子 restart 收尾；保持冻结。
    NeedsRestart,
    /// 损坏/未知 phase：fail-closed，保持冻结。
    Failed,
}

impl RotationApplyPhase {
    /// 持久化文本（写入 `vault.meta`）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applying => "applying",
            Self::Applied => "applied",
            Self::Paused => "paused",
            Self::Conflict => "conflict",
            Self::NeedsRestart => "needs_restart",
            Self::Failed => "failed",
        }
    }

    /// 解析持久值。旧版本写过 `staging`，语义等同 `applying`；其余未知值 fail-closed。
    #[must_use]
    pub fn parse(raw: &[u8]) -> Self {
        match raw {
            b"applying" | b"staging" => Self::Applying,
            b"applied" => Self::Applied,
            b"paused" => Self::Paused,
            b"conflict" => Self::Conflict,
            b"needs_restart" => Self::NeedsRestart,
            _ => Self::Failed,
        }
    }

    /// 读持久 phase；不存在/空值返回 `None`（从未 apply 过）。
    pub fn load(vault: &Vault) -> Result<Option<Self>, SyncError> {
        let Some(raw) = vault.get_meta(ROTATION_APPLY_PHASE_META)? else {
            return Ok(None);
        };
        if raw.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self::parse(&raw)))
    }

    /// durable 写入 phase（`meta` 走 SQLite WAL，`synchronous=FULL`）。
    pub fn store(self, vault: &Vault) -> Result<(), SyncError> {
        vault.set_meta(ROTATION_APPLY_PHASE_META, self.as_str().as_bytes())?;
        Ok(())
    }

    /// 开始一次新的 apply：**同一事务**里清空旧 receipt 并落 `applying`。
    ///
    /// 两步必须原子（评审 0003 P1-C/P1-D）：
    /// - 先落 `applying` 再清 receipt：若两步之间崩溃，旧 receipt 可能与当前身份匹配，
    ///   same-epoch 重入会把"新 apply 还没开始"误判成"上一笔已收尾"；
    /// - 先清 receipt 再落 `applying`：若两步之间崩溃，phase 仍是旧的 `applied`，
    ///   持久冻结屏障会短暂消失。
    ///
    /// 因此这里用一条 `meta` 事务同时完成两件事。
    pub fn begin_apply(vault: &Vault) -> Result<(), SyncError> {
        let statements = [
            Statement::new(
                "INSERT INTO meta(key,value) VALUES(?1,?2) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                vec![
                    Value::Text(ROTATION_APPLY_RECEIPT_META.to_owned()),
                    Value::Blob(Vec::new()),
                ],
            ),
            Statement::new(
                "INSERT INTO meta(key,value) VALUES(?1,?2) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                vec![
                    Value::Text(ROTATION_APPLY_PHASE_META.to_owned()),
                    Value::Blob(Self::Applying.as_str().as_bytes().to_vec()),
                ],
            ),
        ];
        vault.exec_transaction(
            &statements,
            &[AffectedGuard::exactly(0, 1), AffectedGuard::exactly(1, 1)],
        )?;
        Ok(())
    }

    /// 清除 phase（只在调用方确认整条轮换链已收尾时使用；engine 解冻只认 `applied`）。
    pub fn clear(vault: &Vault) -> Result<(), SyncError> {
        vault.set_meta(ROTATION_APPLY_PHASE_META, b"")?;
        Ok(())
    }

    /// 是否需要冻结旧 epoch 的 encrypt/push。只有 `Applied` 不冻结。
    #[must_use]
    pub fn is_frozen(self) -> bool {
        !matches!(self, Self::Applied)
    }
}

/// 阶段 I 的 durable 屏障：返回 `true` 表示必须冻结。
///
/// 读取失败/未知值一律 fail-closed；`None`（从未 apply）不构成屏障。
pub(crate) fn apply_phase_barrier(vault: &Vault) -> bool {
    match RotationApplyPhase::load(vault) {
        Ok(None) => false,
        Ok(Some(phase)) => phase.is_frozen(),
        Err(_) => true,
    }
}

/// 阶段 I apply receipt 在 vault meta 中的键。
pub const ROTATION_APPLY_RECEIPT_META: &str = "sync.rotation_apply_receipt";

/// receipt 的 CDE 版本（字段布局变更必须升 version，旧版本 fail-closed）。
const APPLY_RECEIPT_VERSION: u64 = 1;
/// receipt 的 CDE 字段数（严格校验，缺失/多余都拒绝）。
const APPLY_RECEIPT_FIELDS: usize = 14;

/// 一次 I 阶段 apply 的 durable、类型化 receipt（评审 0003 P1-C/P1-D）。
///
/// 写入时机：baseline merge **已提交**、身份 rename 推广**之前**。因此：
/// - rename 成功后、`applied` 写前崩溃（本缺陷的主窗口）→ receipt 与推广后的
///   身份/pin 完全匹配，重启可做**认证后的完成 reconcile**，不需要 epoch change；
/// - rename 之前崩溃 → 本地身份仍是旧 epoch，receipt 与身份不匹配，只能走正常 epoch
///   change 重试（merge 幂等），绝不能被误当成"已完成"。
///
/// 字段全部来自完整 gate 的授权事实（T/`ActivationCommitV1`/选中 checkpoint/推广材料），
/// 字段私有 + 只读访问器：外部无法伪造 receipt 来绕过 T 门禁。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationApplyReceipt {
    vault_id: VaultId,
    device_id: DeviceId,
    rotation_id: [u8; 16],
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    baseline_cursor: String,
    from_epoch: u64,
    epoch: u64,
    control_rev: u64,
    instance_id: [u8; 16],
    new_manager_public: [u8; 32],
    new_manager_rev: u64,
    activation_hash: [u8; 32],
}

impl RotationApplyReceipt {
    /// 由完整 gate 的已验证事实构造（crate 内部；外部不可伪造）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        vault_id: VaultId,
        device_id: DeviceId,
        rotation_id: [u8; 16],
        checkpoint_id: [u8; 16],
        baseline_s: u64,
        baseline_cursor: &str,
        from_epoch: u64,
        epoch: u64,
        control_rev: u64,
        instance_id: [u8; 16],
        new_manager_public: [u8; 32],
        new_manager_rev: u64,
        activation_hash: [u8; 32],
    ) -> Self {
        Self {
            vault_id,
            device_id,
            rotation_id,
            checkpoint_id,
            baseline_s,
            baseline_cursor: baseline_cursor.to_owned(),
            from_epoch,
            epoch,
            control_rev,
            instance_id,
            new_manager_public,
            new_manager_rev,
            activation_hash,
        }
    }

    /// 归属 vault。
    #[must_use]
    pub fn vault_id(&self) -> VaultId {
        self.vault_id
    }

    /// 归属设备。
    #[must_use]
    pub fn device_id(&self) -> DeviceId {
        self.device_id
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

    /// 激活前 epoch（本地可信起点）。
    #[must_use]
    pub fn from_epoch(&self) -> u64 {
        self.from_epoch
    }

    /// 激活后 epoch（推广目标）。
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 激活控制链 rev。
    #[must_use]
    pub fn control_rev(&self) -> u64 {
        self.control_rev
    }

    /// 已验证的 relay instance（control pin）。
    #[must_use]
    pub fn instance_id(&self) -> [u8; 16] {
        self.instance_id
    }

    /// 推广后的管理公钥（pin）。
    #[must_use]
    pub fn new_manager_public(&self) -> [u8; 32] {
        self.new_manager_public
    }

    /// 推广后的管理 key rev。
    #[must_use]
    pub fn new_manager_rev(&self) -> u64 {
        self.new_manager_rev
    }

    /// `SHA256(CDE(ActivationCommitV1))`（与 authenticated pin 对账）。
    #[must_use]
    pub fn activation_hash(&self) -> [u8; 32] {
        self.activation_hash
    }

    fn encode(&self) -> Result<Vec<u8>, SyncError> {
        cbor::encode(vec![
            (0, cbor::uint(APPLY_RECEIPT_VERSION)),
            (1, cbor::bytes(self.vault_id.to_vec())),
            (2, cbor::bytes(self.device_id.to_vec())),
            (3, cbor::bytes(self.rotation_id.to_vec())),
            (4, cbor::bytes(self.checkpoint_id.to_vec())),
            (5, cbor::uint(self.baseline_s)),
            (6, cbor::text(self.baseline_cursor.clone())),
            (7, cbor::uint(self.from_epoch)),
            (8, cbor::uint(self.epoch)),
            (9, cbor::uint(self.control_rev)),
            (10, cbor::bytes(self.instance_id.to_vec())),
            (11, cbor::bytes(self.new_manager_public.to_vec())),
            (12, cbor::uint(self.new_manager_rev)),
            (13, cbor::bytes(self.activation_hash.to_vec())),
        ])
    }

    /// 写入 `vault.meta`（单条 SQLite WAL 事务，durable 后才返回）。
    pub fn store(&self, vault: &Vault) -> Result<(), SyncError> {
        let blob = self.encode()?;
        vault.set_meta(ROTATION_APPLY_RECEIPT_META, &blob)?;
        Ok(())
    }

    /// 读取 receipt；不存在/空值返回 `None`，损坏一律 `Err`（调用方必须 fail-closed）。
    pub fn load(vault: &Vault) -> Result<Option<Self>, SyncError> {
        let Some(blob) = vault.get_meta(ROTATION_APPLY_RECEIPT_META)? else {
            return Ok(None);
        };
        if blob.is_empty() {
            return Ok(None);
        }
        let fields = cbor::decode(&blob)?;
        if fields.len() != APPLY_RECEIPT_FIELDS {
            return Err(SyncError::LocalCorrupt("apply receipt 字段数不符"));
        }
        if cbor::uint_field(&fields, 0)? != APPLY_RECEIPT_VERSION {
            return Err(SyncError::LocalCorrupt("apply receipt 版本不支持"));
        }
        Ok(Some(Self {
            vault_id: cbor::bytes_fixed::<16>(&fields, 1)?,
            device_id: cbor::bytes_fixed::<16>(&fields, 2)?,
            rotation_id: cbor::bytes_fixed::<16>(&fields, 3)?,
            checkpoint_id: cbor::bytes_fixed::<16>(&fields, 4)?,
            baseline_s: cbor::uint_field(&fields, 5)?,
            baseline_cursor: cbor::text_field(&fields, 6)?,
            from_epoch: cbor::uint_field(&fields, 7)?,
            epoch: cbor::uint_field(&fields, 8)?,
            control_rev: cbor::uint_field(&fields, 9)?,
            instance_id: cbor::bytes_fixed::<16>(&fields, 10)?,
            new_manager_public: cbor::bytes_fixed::<32>(&fields, 11)?,
            new_manager_rev: cbor::uint_field(&fields, 12)?,
            activation_hash: cbor::bytes_fixed::<32>(&fields, 13)?,
        }))
    }

    /// 清空 receipt（`begin_apply` 会用同一事务完成，这里只给测试/显式收尾用）。
    pub fn clear(vault: &Vault) -> Result<(), SyncError> {
        vault.set_meta(ROTATION_APPLY_RECEIPT_META, b"")?;
        Ok(())
    }
}

/// restart **POST 之前**必须 durable 的公开请求事实（评审 0002 P1-2）。
///
/// 这里只允许放公开契约/请求事实：绝不包含候选根、口令、R'、管理私钥等 fresh 秘密。
/// 字段来源：
///
/// | 字段 | 来源 |
/// | --- | --- |
/// | `previous_rotation_id` | 调用方要原子重启的旧 rotation id |
/// | `request_id` | 本次 restart/activate 的幂等键（exact request 查询用） |
/// | `from_epoch` | 本地 active 根 epoch（relay 必须原样保留） |
/// | `previous_candidate_epoch` | restart **前**旧 rotation view 的 `to_epoch`（abandoned candidate） |
/// | `expected_control_rev` | 管理签名的 CAS 期望值（restart 成功后 relay = 它 + 1） |
/// | `manager_rev` | 旧管理 key rev（restart 不改管理权） |
/// | `freeze_seq` | 冻结点 S（两份基线必须 ≥S） |
/// | `instance_id` | 管理签名绑定的 relay instance |
/// | `freeze_barrier` | 写入 intent 时冻结屏障必须已为 1 |
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartIntent {
    /// 被原子重启的旧 rotation id。
    pub previous_rotation_id: [u8; 16],
    /// 本次 restart 的幂等 request_id（relay 按它找回已提交结果）。
    pub request_id: [u8; 16],
    /// relay 保留的 active epoch（restart 的 `from_epoch`）。
    pub from_epoch: u64,
    /// 旧 rotation view 的 `to_epoch`：被原子废弃的旧候选 epoch。
    pub previous_candidate_epoch: u64,
    /// 管理签名的 CAS 期望值；restart 成功后 relay 的 control_rev 必须 = 它 + 1。
    pub expected_control_rev: u64,
    /// 旧管理 key rev。
    pub manager_rev: u64,
    /// 冻结点 S。
    pub freeze_seq: u64,
    /// relay instance id（管理签名绑定）。
    pub instance_id: [u8; 16],
    /// 记录 intent 时冻结屏障已置 1（重启后必须继续保持）。
    pub freeze_barrier: bool,
}

/// relay 对 restart intent 的原子分配（只在拿到 relay 结果后落盘）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartAssignment {
    /// relay 新分配的 rotation id。
    pub rotation_id: [u8; 16],
    /// relay 保留的 active epoch。
    pub from_epoch: u64,
    /// 被废弃的旧候选 epoch。
    pub previous_candidate_epoch: u64,
    /// relay 新分配的候选 epoch（绝不本地 +1）。
    pub to_epoch: u64,
    /// restart 后的 control_rev（activate CAS 的期望值）。
    pub control_rev: u64,
    /// 冻结点 S。
    pub freeze_seq: u64,
}

/// 一份候选基线的验证回执。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalBaseline {
    pub checkpoint_id: [u8; 16],
    pub covered_seq: u64,
    pub cursor: String,
    pub manifest_object_id: [u8; 32],
    pub manifest_digest: [u8; 32],
    pub refs_digest: [u8; 32],
    pub verified: bool,
    /// 本地 durable 的 `CheckpointCommit` 摘要（签 activation T 只用它，不再问 relay）。
    pub commit_digest: [u8; 32],
    /// 读回验证得到的资产集合 digest（两份基线必须相等）。
    pub asset_set_digest: [u8; 32],
}

/// restart 在候选 epoch 重加密后的冻结资产（固定 object id/digest，重试不重加密）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRestartAsset {
    pub real_sha256: [u8; 32],
    pub byte_len: u64,
    pub media_type: String,
    pub object_id: [u8; 32],
    pub object_digest: [u8; 32],
}

/// 轮换 journal（DPAPI 持久）。
#[derive(Clone)]
pub struct RotationJournal {
    pub phase: RotationPhase,
    /// 幂等 request_id（begin 响应丢失时用它查询，绝不盲 begin）。
    pub request_id: [u8; 16],
    /// canonical begin body 摘要。
    pub body_digest: [u8; 32],
    pub from_epoch: u64,
    pub to_epoch: u64,
    /// relay begin 返回的 rotation id；阶段 B 前为 None。
    pub rotation_id: Option<[u8; 16]>,
    /// 阶段 A 固定的 freeze 信息。
    pub freeze_seq: Option<u64>,
    pub freeze_log_id: Option<[u8; 16]>,
    pub control_rev: u64,
    pub control_hash: [u8; 32],
    /// 新 epoch 的候选根（DPAPI 保护；阶段 E 前生成）。
    pub candidate_root: Option<RootSecretV1>,
    /// 新管理 key 公钥/rev；私钥由 R' 派生，不单独存。
    pub new_manager_public: Option<[u8; 32]>,
    pub new_manager_rev: u64,
    /// activate 的 new_manager 三键（relay `fd89cfa` 后布局）：
    /// `(CDE ManagerTransition 9 字段, old_signature64, new_signature64)`。
    pub new_manager_transition: Option<Vec<u8>>,
    pub old_manager_signature: Option<[u8; 64]>,
    pub new_manager_signature: Option<[u8; 64]>,
    /// `/control` 键 5 管理转换链的 rev。
    pub control_chain_rev: Option<u64>,
    /// 阶段 A pin 的 relay instance/log/control head。
    pub pinned_instance_id: Option<[u8; 16]>,
    pub pinned_log_id: Option<[u8; 16]>,
    pub pinned_control_hash: Option<[u8; 32]>,
    /// R' 已离线备份。
    pub r_prime_backed_up: bool,
    /// 新 password/recovery 槽描述符、固定槽体、槽 id（阶段 D 上传用）。
    pub new_slot_descriptors: Vec<Vec<u8>>,
    pub new_slot_bodies: Vec<Vec<u8>>,
    pub new_slot_ids: Vec<[u8; 32]>,
    /// 两份候选基线（phase E 后各一条，互不共享状态）。
    pub baselines: Vec<JournalBaseline>,
    /// 成员证书/grants/资产集合摘要（阶段 F 用）。
    pub member_set_digest: Option<[u8; 32]>,
    pub grants_digest: Option<[u8; 32]>,
    /// restart：候选槽集合 digest（activate 四 digest 之一）。
    pub slot_set_digest: Option<[u8; 32]>,
    /// 阶段 F：每个待 stage 设备的完整 grant leaf manifest（规范 CDE 字节）。
    pub grant_leaf_manifests: Vec<Vec<u8>>,
    /// 阶段 F：所有 leaf manifest 的排序集合 digest（供 activate 锁定）。
    pub grant_leaf_digest: Option<[u8; 32]>,
    pub asset_set_digest: Option<[u8; 32]>,
    /// 上传进度（对象数）。
    pub uploaded_objects: u32,
    pub total_objects: u32,
    /// outbox 重打包进度（task-104）。
    pub repacked_outbox: u32,
    pub total_outbox: u32,
    /// 本地 cut L（阶段 C 固定）。
    pub local_cut: Option<u64>,
    /// restart：被废弃的旧候选 epoch（relay restart 前 rotation view 的 `to_epoch`）。
    pub previous_candidate_epoch: Option<u64>,
    /// restart：POST 之前 durable 的公开 intent（无秘密；恢复时按 exact request 查回）。
    pub restart_intent: Option<RestartIntent>,
    /// restart：fresh 新口令（DPAPI 保护的 journal 内；上传/重试复用）。
    pub restart_password: Option<Vec<u8>>,
    /// restart：fresh R'（32B；重试时据此重建管理 key，不重新生成）。
    pub recovery_seed: Option<[u8; 32]>,
    /// restart：由 R' 派生的新管理私钥种子（32B；重试不重新生成）。
    pub manager_seed: Option<[u8; 32]>,
    /// restart：用户已确认新恢复码（R'）离线备份；activate 的硬门槛。
    pub recovery_backup_confirmed: bool,
    /// restart：固定在 journal 里的 grant leaf manifest（重试复用同一字节）。
    pub restart_grant_manifests: Vec<Vec<u8>>,
    /// restart：fixed slot 的 MNE message_id（重试必须复用同一 envelope 字节）。
    pub restart_password_message_id: Option<[u8; 16]>,
    pub restart_recovery_message_id: Option<[u8; 16]>,
    /// restart：被重启的旧 rotation id（重试必须命中同一旧候选）。
    pub restart_previous_rotation_id: Option<[u8; 16]>,
    /// restart：已上传的候选 epoch 重加密资产（重试不再重加密/重传）。
    pub restart_assets: Vec<JournalRestartAsset>,
    /// restart：relay activated 回执里的 epoch/control_rev/activation_commit。
    pub activated_epoch: Option<u64>,
    pub activated_control_rev: Option<u64>,
    pub activation_commit: Option<Vec<u8>>,
}

impl RotationJournal {
    /// 阶段 A：冻结旧 epoch，生成持久 intent。新根/新槽在后续阶段填充。
    pub fn freeze(
        vault: &Vault,
        from_epoch: u64,
        request_id: [u8; 16],
        control_rev: u64,
        control_hash: [u8; 32],
    ) -> Result<Self, SyncError> {
        if Self::load(vault)?.is_some() {
            return Err(SyncError::Unsupported("已有未完成 rotation journal"));
        }
        let journal = Self {
            phase: RotationPhase::Frozen,
            request_id,
            body_digest: begin_body_digest(from_epoch, &request_id),
            from_epoch,
            to_epoch: from_epoch.saturating_add(1),
            rotation_id: None,
            freeze_seq: None,
            freeze_log_id: None,
            control_rev,
            control_hash,
            candidate_root: None,
            new_manager_public: None,
            new_manager_rev: 0,
            new_manager_transition: None,
            old_manager_signature: None,
            new_manager_signature: None,
            control_chain_rev: None,
            pinned_instance_id: None,
            pinned_log_id: None,
            pinned_control_hash: None,
            r_prime_backed_up: false,
            new_slot_descriptors: Vec::new(),
            new_slot_bodies: Vec::new(),
            new_slot_ids: Vec::new(),
            baselines: Vec::new(),
            member_set_digest: None,
            grants_digest: None,
            slot_set_digest: None,
            grant_leaf_manifests: Vec::new(),
            grant_leaf_digest: None,
            asset_set_digest: None,
            uploaded_objects: 0,
            total_objects: 0,
            repacked_outbox: 0,
            total_outbox: 0,
            local_cut: None,
            previous_candidate_epoch: None,
            restart_intent: None,
            restart_password: None,
            recovery_seed: None,
            manager_seed: None,
            recovery_backup_confirmed: false,
            restart_grant_manifests: Vec::new(),
            restart_password_message_id: None,
            restart_recovery_message_id: None,
            restart_previous_rotation_id: None,
            restart_assets: Vec::new(),
            activated_epoch: None,
            activated_control_rev: None,
            activation_commit: None,
        };
        journal.save(vault)?;
        Ok(journal)
    }

    /// restart：POST **之前**只落公开 intent（评审 0002 P1-2）。
    ///
    /// 这里不生成任何新 epoch/秘密：epoch、rotation_id、control_rev 必须等 relay
    /// 分配后才能写（`begin_restart`）。`to_epoch = 0` 是"尚未分配"的显式哨兵，
    /// `rotation_id = None` 表示还没有 relay 结果。
    pub fn record_restart_intent(vault: &Vault, intent: RestartIntent) -> Result<Self, SyncError> {
        if Self::load(vault)?.is_some() {
            return Err(SyncError::Unsupported("已有未完成 rotation journal"));
        }
        if !intent.freeze_barrier {
            return Err(SyncError::Object(
                "restart intent 必须在冻结屏障=1 之后落盘",
            ));
        }
        if intent.from_epoch >= intent.previous_candidate_epoch {
            return Err(SyncError::RelayWire(
                "restart intent epoch 必须 active < 旧候选（禁止本地 +1）",
            ));
        }
        let previous_rotation_id = intent.previous_rotation_id;
        let journal = Self {
            phase: RotationPhase::RestartIntentRecorded,
            request_id: intent.request_id,
            body_digest: restart_body_digest(),
            from_epoch: intent.from_epoch,
            // 尚未拿到 relay 分配；`begin_restart` 才会填入真实候选 epoch。
            to_epoch: 0,
            rotation_id: None,
            freeze_seq: Some(intent.freeze_seq),
            freeze_log_id: None,
            control_rev: intent.expected_control_rev,
            control_hash: [0u8; 32],
            candidate_root: None,
            new_manager_public: None,
            new_manager_rev: 0,
            new_manager_transition: None,
            old_manager_signature: None,
            new_manager_signature: None,
            control_chain_rev: None,
            pinned_instance_id: None,
            pinned_log_id: None,
            pinned_control_hash: None,
            r_prime_backed_up: false,
            new_slot_descriptors: Vec::new(),
            new_slot_bodies: Vec::new(),
            new_slot_ids: Vec::new(),
            baselines: Vec::new(),
            member_set_digest: None,
            grants_digest: None,
            slot_set_digest: None,
            grant_leaf_manifests: Vec::new(),
            grant_leaf_digest: None,
            asset_set_digest: None,
            uploaded_objects: 0,
            total_objects: 0,
            repacked_outbox: 0,
            total_outbox: 0,
            local_cut: None,
            previous_candidate_epoch: Some(intent.previous_candidate_epoch),
            restart_intent: Some(intent),
            restart_password: None,
            recovery_seed: None,
            manager_seed: None,
            recovery_backup_confirmed: false,
            restart_grant_manifests: Vec::new(),
            restart_password_message_id: None,
            restart_recovery_message_id: None,
            restart_previous_rotation_id: Some(previous_rotation_id),
            restart_assets: Vec::new(),
            activated_epoch: None,
            activated_control_rev: None,
            activation_commit: None,
        };
        journal.save(vault)?;
        Ok(journal)
    }

    /// restart：拿到 relay 分配后，把 fresh 秘密/epoch/rotation_id/固定槽密文 DPAPI 落盘。
    ///
    /// 必须在任何新槽/新对象上传前调用；调用方随后只能从 journal 重建同一份材料。
    /// 前置条件：已有 `RestartIntentRecorded` journal，且 `intent` 与其中持久事实逐字段
    /// 相等（防止把另一次 restart 的分配套到旧 intent 上）。
    #[allow(clippy::too_many_arguments)]
    pub fn begin_restart(
        vault: &Vault,
        intent: &RestartIntent,
        assignment: RestartAssignment,
        candidate_root: RootSecretV1,
        restart_password: Vec<u8>,
        recovery_seed: [u8; 32],
        manager_seed: [u8; 32],
        password_message_id: [u8; 16],
        recovery_message_id: [u8; 16],
        new_manager_public: [u8; 32],
        new_manager_rev: u64,
        slots: &[KeySlotV1],
        recovery_backup_confirmed: bool,
    ) -> Result<Self, SyncError> {
        let mut journal = Self::load(vault)?.ok_or(SyncError::LocalCorrupt(
            "restart 缺 POST 前 durable intent：拒绝在无 intent 时落 fresh 材料",
        ))?;
        if journal.phase != RotationPhase::RestartIntentRecorded {
            return Err(SyncError::Unsupported(
                "restart journal 已越过 intent phase，不能重复写入分配",
            ));
        }
        let stored_intent = journal
            .restart_intent
            .as_ref()
            .ok_or(SyncError::LocalCorrupt("restart journal 缺 durable intent"))?;
        if stored_intent != intent {
            return Err(SyncError::Unsupported(
                "restart intent 与 relay 分配前的持久事实不一致",
            ));
        }
        if assignment.from_epoch != intent.from_epoch
            || assignment.previous_candidate_epoch != intent.previous_candidate_epoch
            || assignment.freeze_seq != intent.freeze_seq
            || assignment.rotation_id == intent.previous_rotation_id
        {
            return Err(SyncError::RelayWire(
                "restart 分配与 durable intent 不一致（禁止复用旧候选）",
            ));
        }
        if !(assignment.from_epoch < assignment.previous_candidate_epoch
            && assignment.previous_candidate_epoch < assignment.to_epoch)
        {
            return Err(SyncError::RelayWire(
                "restart journal epoch 必须严格递增（禁止本地 +1）",
            ));
        }
        let Some(expected_control_rev) = intent.expected_control_rev.checked_add(1) else {
            return Err(SyncError::RelayWire("restart control_rev 溢出"));
        };
        if assignment.control_rev != expected_control_rev {
            return Err(SyncError::RelayWire(
                "relay restart control_rev 与本地 CAS 假设不一致",
            ));
        }
        if candidate_root.key_epoch != assignment.to_epoch || restart_password.is_empty() {
            return Err(SyncError::Object(
                "restart journal fresh 材料与候选 epoch 不一致",
            ));
        }
        journal.to_epoch = assignment.to_epoch;
        journal.rotation_id = Some(assignment.rotation_id);
        journal.control_rev = assignment.control_rev;
        journal.candidate_root = Some(candidate_root);
        journal.new_manager_public = Some(new_manager_public);
        journal.new_manager_rev = new_manager_rev;
        journal.restart_password = Some(restart_password);
        journal.recovery_seed = Some(recovery_seed);
        journal.manager_seed = Some(manager_seed);
        journal.recovery_backup_confirmed = recovery_backup_confirmed;
        journal.r_prime_backed_up = recovery_backup_confirmed;
        journal.restart_password_message_id = Some(password_message_id);
        journal.restart_recovery_message_id = Some(recovery_message_id);
        journal.set_slots(slots)?;
        journal.advance(RotationPhase::RestartPrepared)?;
        journal.save(vault)?;
        Ok(journal)
    }

    /// restart：用户确认新恢复码已离线备份（activate 前必须为 true）。
    pub fn confirm_recovery_backup(&mut self, vault: &Vault) -> Result<(), SyncError> {
        if !self.phase.is_restart() {
            return Err(SyncError::Unsupported("不是 restart journal"));
        }
        self.recovery_backup_confirmed = true;
        self.r_prime_backed_up = true;
        self.save(vault)
    }

    /// restart：**主基线一完成就落盘**（评审 0002 P1-D）。
    ///
    /// 这是给 fallback 崩溃/失败兜底的持久点：重跑只能复用这里的 cid/manifest/commit
    /// receipt，绝不能再生成随机 primary（relay 每 epoch role 唯一 CAS）。
    pub fn set_restart_primary_baseline(
        &mut self,
        vault: &Vault,
        baseline: JournalBaseline,
    ) -> Result<(), SyncError> {
        if self.phase != RotationPhase::RestartSlotsUploaded {
            return Err(SyncError::LocalCorrupt(
                "restart 主基线落盘时机非法（必须先完成槽上传）",
            ));
        }
        if !baseline.verified
            || baseline.commit_digest == [0u8; 32]
            || baseline.asset_set_digest == [0u8; 32]
        {
            return Err(SyncError::Object("restart 主基线缺少本地验证 receipt"));
        }
        if baseline.covered_seq < self.freeze_seq.unwrap_or(u64::MAX) {
            return Err(SyncError::Object("主基线 S 未覆盖 restart 冻结点"));
        }
        self.baselines = vec![baseline];
        self.advance(RotationPhase::RestartPrimaryBaselineReady)?;
        self.save(vault)
    }

    /// restart：备基线完成后再落盘，并与已持久的主基线交叉校验独立性。
    pub fn set_restart_fallback_baseline(
        &mut self,
        vault: &Vault,
        baseline: JournalBaseline,
    ) -> Result<(), SyncError> {
        if self.phase != RotationPhase::RestartPrimaryBaselineReady {
            return Err(SyncError::LocalCorrupt(
                "restart 备基线落盘时机非法（必须先持久主基线）",
            ));
        }
        let primary = self
            .baselines
            .first()
            .ok_or(SyncError::LocalCorrupt("restart journal 缺主基线"))?;
        if !baseline.verified
            || baseline.commit_digest == [0u8; 32]
            || baseline.asset_set_digest == [0u8; 32]
        {
            return Err(SyncError::Object("restart 备基线缺少本地验证 receipt"));
        }
        if primary.checkpoint_id == baseline.checkpoint_id
            || primary.manifest_object_id == baseline.manifest_object_id
        {
            return Err(SyncError::Object("restart 两份候选基线不独立"));
        }
        if primary.asset_set_digest != baseline.asset_set_digest {
            return Err(SyncError::Object("restart 两份基线的资产集合不一致"));
        }
        let freeze_seq = self
            .freeze_seq
            .ok_or(SyncError::LocalCorrupt("restart journal 缺冻结点 S"))?;
        if primary.covered_seq < freeze_seq || baseline.covered_seq < freeze_seq {
            return Err(SyncError::Object("restart 基线 S 未覆盖冻结点"));
        }
        self.baselines.push(baseline);
        self.advance(RotationPhase::RestartBaselinesReady)?;
        self.save(vault)
    }

    /// restart：固定已上传的重加密资产（重试不重复上传随机 nonce 的对象）。
    pub fn set_restart_assets(
        &mut self,
        vault: &Vault,
        assets: Vec<JournalRestartAsset>,
    ) -> Result<(), SyncError> {
        self.restart_assets = assets;
        self.save(vault)
    }

    /// restart：固定 grant leaf manifest（stage 前落盘，重试复用同一字节）。
    pub fn set_restart_grant_manifests(
        &mut self,
        vault: &Vault,
        manifests: Vec<Vec<u8>>,
        grants_digest: [u8; 32],
    ) -> Result<(), SyncError> {
        if manifests.is_empty() {
            return Err(SyncError::Object("restart grants 不能为空"));
        }
        self.restart_grant_manifests = manifests;
        self.grants_digest = Some(grants_digest);
        self.save(vault)
    }

    /// restart：持久化 activate 请求材料（重试不再依赖内存里的 PreparedCheckpoint）。
    #[allow(clippy::too_many_arguments)]
    pub fn set_restart_activation(
        &mut self,
        vault: &Vault,
        _primary_checkpoint: [u8; 16],
        _fallback_checkpoint: [u8; 16],
        member_set_digest: [u8; 32],
        slot_set_digest: [u8; 32],
        grants_digest: [u8; 32],
        asset_set_digest: [u8; 32],
        transition: Vec<u8>,
        old_signature: [u8; 64],
        new_signature: [u8; 64],
        control_chain_rev: u64,
    ) -> Result<(), SyncError> {
        if self.baselines.len() != 2 {
            return Err(SyncError::LocalCorrupt("restart activate 前缺两份候选基线"));
        }
        self.member_set_digest = Some(member_set_digest);
        self.slot_set_digest = Some(slot_set_digest);
        self.grants_digest = Some(grants_digest);
        self.asset_set_digest = Some(asset_set_digest);
        self.new_manager_transition = Some(transition);
        self.old_manager_signature = Some(old_signature);
        self.new_manager_signature = Some(new_signature);
        self.control_chain_rev = Some(control_chain_rev);
        if self.phase != RotationPhase::RestartActivating {
            self.advance(RotationPhase::RestartActivating)?;
        }
        self.save(vault)
    }

    /// restart：relay activated 回执落盘（只信 activated 才进入此 phase）。
    pub fn mark_restart_activated(
        &mut self,
        vault: &Vault,
        epoch: u64,
        control_rev: u64,
        activation_commit: Vec<u8>,
    ) -> Result<(), SyncError> {
        if self.phase != RotationPhase::RestartActivating {
            return Err(SyncError::LocalCorrupt("restart 未在 activating phase"));
        }
        if epoch != self.to_epoch {
            return Err(SyncError::RelayWire(
                "activated epoch 不是 relay 分配的候选 epoch",
            ));
        }
        self.advance(RotationPhase::RestartActivated)?;
        self.activated_epoch = Some(epoch);
        self.activated_control_rev = Some(control_rev);
        self.activation_commit = Some(activation_commit);
        self.save(vault)
    }

    /// 清空 journal（本地推广完成后调用；绝不能在 activate 前清）。
    pub fn clear(vault: &Vault) -> Result<(), SyncError> {
        vault.set_meta(ROTATION_JOURNAL_META, b"")?;
        Ok(())
    }

    /// 阶段 F：持久化每个未吊销设备的 grant leaf manifest 与集合 digest。
    pub fn set_grants(&mut self, vault: &Vault, leaves: &[Vec<u8>]) -> Result<[u8; 32], SyncError> {
        let mut sorted = leaves.to_vec();
        sorted.sort();
        let mut input = Vec::with_capacity(4 + sorted.iter().map(Vec::len).sum::<usize>());
        input.extend_from_slice(&(sorted.len() as u32).to_be_bytes());
        for leaf in &sorted {
            input.extend_from_slice(leaf);
        }
        let digest: [u8; 32] = sha2::Sha256::digest(&input).into();
        self.grant_leaf_manifests = sorted;
        self.grant_leaf_digest = Some(digest);
        self.grants_digest = Some(digest);
        self.save(vault)?;
        Ok(digest)
    }

    /// 阶段 A：确认新恢复码（R'）已离线备份；未确认前禁止 begin。
    pub fn confirm_r_prime_backed_up(&mut self, vault: &Vault) -> Result<(), SyncError> {
        self.r_prime_backed_up = true;
        self.save(vault)
    }

    /// 读 journal；不存在返回 None。
    pub fn load(vault: &Vault) -> Result<Option<Self>, SyncError> {
        let Some(blob) = vault.get_meta(ROTATION_JOURNAL_META)? else {
            return Ok(None);
        };
        if blob.is_empty() {
            return Ok(None);
        }
        let plain = identity_wrap::unprotect(&blob, &journal_entropy(vault))?;
        Ok(Some(Self::decode(&plain)?))
    }

    /// 经本地封装（Windows DPAPI / Linux 用户主钥）保护后写回 journal。
    pub fn save(&self, vault: &Vault) -> Result<(), SyncError> {
        let bytes = zeroize::Zeroizing::new(self.encode()?);
        let blob = identity_wrap::protect(&bytes, &journal_entropy(vault))?;
        vault.set_meta(ROTATION_JOURNAL_META, &blob)?;
        Ok(())
    }

    /// phase 只允许前进一步（Aborted 例外）。
    pub fn advance(&mut self, next: RotationPhase) -> Result<(), SyncError> {
        if next == RotationPhase::Aborted {
            self.phase = next;
            return Ok(());
        }
        if self.phase.next() != Some(next) {
            return Err(SyncError::LocalCorrupt("rotation phase 不能跳步"));
        }
        self.phase = next;
        Ok(())
    }

    /// 阶段 B：记录 relay begin 响应；调用方必须先用 `request_id` 查询（task-101 接口）。
    pub fn mark_begun(
        &mut self,
        rotation_id: [u8; 16],
        freeze_seq: u64,
        freeze_log_id: [u8; 16],
    ) -> Result<(), SyncError> {
        self.advance(RotationPhase::Begun)?;
        self.rotation_id = Some(rotation_id);
        self.freeze_seq = Some(freeze_seq);
        self.freeze_log_id = Some(freeze_log_id);
        Ok(())
    }

    /// 阶段 C：旧根 integrate 至 S、固定本地 cut L；门禁与 `rotation::ensure_c_phase_gate`
    /// 完全一致（只拦 ≤S 的入站缺口与已发送未 ACK，**不再拦本地 pending/encrypted 编辑**）。
    pub fn mark_integrated(
        &mut self,
        vault: &Vault,
        target_s: u64,
        local_cut: u64,
    ) -> Result<(), SyncError> {
        crate::rotation::ensure_c_phase_gate(vault, target_s)?;
        self.advance(RotationPhase::Integrated)?;
        self.local_cut = Some(local_cut);
        Ok(())
    }

    /// 阶段 B 的 request_id 幂等句柄（响应丢失时查询用，绝不重新 begin）。
    pub fn request_id(&self) -> [u8; 16] {
        self.request_id
    }

    /// 记录新根/新管理公钥（阶段 D 用）。
    pub fn set_candidate(
        &mut self,
        root: RootSecretV1,
        manager_public: [u8; 32],
        manager_rev: u64,
        r_prime_backed_up: bool,
    ) {
        self.candidate_root = Some(root);
        self.new_manager_public = Some(manager_public);
        self.new_manager_rev = manager_rev;
        self.r_prime_backed_up = r_prime_backed_up;
    }

    /// 记录新槽（描述符 + 固定槽体字节 + slot_id）。
    pub fn set_slots(&mut self, slots: &[KeySlotV1]) -> Result<(), SyncError> {
        self.new_slot_ids = slots.iter().map(|slot| slot.descriptor.slot_id).collect();
        self.new_slot_descriptors = slots
            .iter()
            .map(|slot| slot.descriptor.encode())
            .collect::<Result<Vec<_>, _>>()?;
        self.new_slot_bodies = slots.iter().map(|slot| slot.wrapped_bytes()).collect();
        Ok(())
    }

    /// 记录 activate 的 new_manager 三键（relay `fd89cfa`：transition/old_sig/new_sig）。
    pub fn set_activate_new_manager(
        &mut self,
        transition: Vec<u8>,
        old_signature: [u8; 64],
        new_signature: [u8; 64],
        control_chain_rev: u64,
    ) {
        self.new_manager_transition = Some(transition);
        self.old_manager_signature = Some(old_signature);
        self.new_manager_signature = Some(new_signature);
        self.control_chain_rev = Some(control_chain_rev);
    }

    /// 记录一份候选基线验证回执。
    pub fn push_baseline(&mut self, baseline: JournalBaseline) {
        self.baselines.push(baseline);
    }

    // ---------------------------------------------------------------- CDE

    fn encode(&self) -> Result<Vec<u8>, SyncError> {
        let mut fields = vec![
            (0, cbor::uint(self.phase.code())),
            (1, cbor::bytes(self.request_id.to_vec())),
            (2, cbor::bytes(self.body_digest.to_vec())),
            (3, cbor::uint(self.from_epoch)),
            (4, cbor::uint(self.to_epoch)),
            (8, cbor::uint(self.control_rev)),
            (9, cbor::bytes(self.control_hash.to_vec())),
            (12, cbor::uint(self.new_manager_rev)),
            (13, cbor::boolean(self.r_prime_backed_up)),
            (
                14,
                cbor::array(
                    self.new_slot_descriptors
                        .iter()
                        .map(|value| cbor::bytes(value.clone()))
                        .collect(),
                ),
            ),
            (
                15,
                cbor::array(
                    self.new_slot_bodies
                        .iter()
                        .map(|value| cbor::bytes(value.clone()))
                        .collect(),
                ),
            ),
            (
                16,
                cbor::array(
                    self.new_slot_ids
                        .iter()
                        .map(|value| cbor::bytes(value.to_vec()))
                        .collect(),
                ),
            ),
            (
                17,
                cbor::array(self.baselines.iter().map(baseline_cde).collect()),
            ),
            (21, cbor::uint(u64::from(self.uploaded_objects))),
            (22, cbor::uint(u64::from(self.total_objects))),
            (23, cbor::uint(u64::from(self.repacked_outbox))),
            (24, cbor::uint(u64::from(self.total_outbox))),
        ];
        if let Some(rotation_id) = self.rotation_id {
            fields.push((5, cbor::bytes(rotation_id.to_vec())));
        }
        if let Some(freeze_seq) = self.freeze_seq {
            fields.push((6, cbor::uint(freeze_seq)));
        }
        if let Some(log_id) = self.freeze_log_id {
            fields.push((7, cbor::bytes(log_id.to_vec())));
        }
        if let Some(root) = &self.candidate_root {
            fields.push((10, cbor::bytes(root.encode()?)));
        }
        if let Some(public) = self.new_manager_public {
            fields.push((11, cbor::bytes(public.to_vec())));
        }
        if let Some(digest) = self.member_set_digest {
            fields.push((18, cbor::bytes(digest.to_vec())));
        }
        if let Some(digest) = self.grants_digest {
            fields.push((19, cbor::bytes(digest.to_vec())));
        }
        if !self.grant_leaf_manifests.is_empty() {
            fields.push((
                33,
                cbor::array(
                    self.grant_leaf_manifests
                        .iter()
                        .map(|leaf| cbor::bytes(leaf.clone()))
                        .collect(),
                ),
            ));
        }
        if let Some(digest) = self.grant_leaf_digest {
            fields.push((34, cbor::bytes(digest.to_vec())));
        }
        if let Some(digest) = self.asset_set_digest {
            fields.push((20, cbor::bytes(digest.to_vec())));
        }
        if let Some(cut) = self.local_cut {
            fields.push((25, cbor::uint(cut)));
        }
        if let Some(transition) = &self.new_manager_transition {
            fields.push((26, cbor::bytes(transition.clone())));
        }
        if let Some(signature) = self.old_manager_signature {
            fields.push((27, cbor::bytes(signature.to_vec())));
        }
        if let Some(signature) = self.new_manager_signature {
            fields.push((28, cbor::bytes(signature.to_vec())));
        }
        if let Some(rev) = self.control_chain_rev {
            fields.push((29, cbor::uint(rev)));
        }
        if let Some(instance) = self.pinned_instance_id {
            fields.push((30, cbor::bytes(instance.to_vec())));
        }
        if let Some(log_id) = self.pinned_log_id {
            fields.push((31, cbor::bytes(log_id.to_vec())));
        }
        if let Some(hash) = self.pinned_control_hash {
            fields.push((32, cbor::bytes(hash.to_vec())));
        }
        if let Some(epoch) = self.previous_candidate_epoch {
            fields.push((35, cbor::uint(epoch)));
        }
        if let Some(password) = &self.restart_password {
            fields.push((36, cbor::bytes(password.clone())));
        }
        if let Some(seed) = self.recovery_seed {
            fields.push((37, cbor::bytes(seed.to_vec())));
        }
        if let Some(seed) = self.manager_seed {
            fields.push((38, cbor::bytes(seed.to_vec())));
        }
        if self.recovery_backup_confirmed {
            fields.push((39, cbor::boolean(true)));
        }
        if !self.restart_grant_manifests.is_empty() {
            fields.push((
                40,
                cbor::array(
                    self.restart_grant_manifests
                        .iter()
                        .map(|leaf| cbor::bytes(leaf.clone()))
                        .collect(),
                ),
            ));
        }
        if let Some(epoch) = self.activated_epoch {
            fields.push((41, cbor::uint(epoch)));
        }
        if let Some(rev) = self.activated_control_rev {
            fields.push((42, cbor::uint(rev)));
        }
        if let Some(commit) = &self.activation_commit {
            fields.push((43, cbor::bytes(commit.clone())));
        }
        if let Some(digest) = self.slot_set_digest {
            fields.push((44, cbor::bytes(digest.to_vec())));
        }
        if !self.restart_assets.is_empty() {
            fields.push((
                45,
                cbor::array(self.restart_assets.iter().map(restart_asset_cde).collect()),
            ));
        }
        if let Some(message_id) = self.restart_password_message_id {
            fields.push((46, cbor::bytes(message_id.to_vec())));
        }
        if let Some(message_id) = self.restart_recovery_message_id {
            fields.push((47, cbor::bytes(message_id.to_vec())));
        }
        if let Some(rotation_id) = self.restart_previous_rotation_id {
            fields.push((48, cbor::bytes(rotation_id.to_vec())));
        }
        if let Some(intent) = &self.restart_intent {
            fields.push((49, restart_intent_cde(intent)));
        }
        fields.sort_by_key(|(key, _)| *key);
        cbor::encode(fields)
    }

    fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        let map = cbor::decode(bytes)?;
        let phase = RotationPhase::from_code(cbor::uint_field(&map, 0)?)?;
        let request_id = cbor::bytes_fixed(&map, 1)?;
        let body_digest = cbor::bytes_fixed(&map, 2)?;
        let from_epoch = cbor::uint_field(&map, 3)?;
        let to_epoch = cbor::uint_field(&map, 4)?;
        let rotation_id = cbor::opt_bytes_field(&map, 5)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let freeze_seq = cbor::opt_uint_field(&map, 6)?;
        let freeze_log_id = cbor::opt_bytes_field(&map, 7)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let control_rev = cbor::uint_field(&map, 8)?;
        let control_hash = cbor::bytes_fixed(&map, 9)?;
        let candidate_root = cbor::opt_bytes_field(&map, 10)?
            .map(|value| RootSecretV1::decode(&value))
            .transpose()?;
        let new_manager_public = cbor::opt_bytes_field(&map, 11)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let new_manager_rev = cbor::uint_field(&map, 12)?;
        let r_prime_backed_up = cbor::bool_field(&map, 13)?;
        let new_slot_descriptors = cbor::array_field(&map, 14)?
            .iter()
            .map(value_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        let new_slot_bodies = cbor::array_field(&map, 15)?
            .iter()
            .map(value_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        let new_slot_ids = cbor::array_field(&map, 16)?
            .iter()
            .map(|value| fixed32(&value_bytes(value)?))
            .collect::<Result<Vec<_>, _>>()?;
        let baselines = cbor::array_field(&map, 17)?
            .iter()
            .map(decode_baseline)
            .collect::<Result<Vec<_>, _>>()?;
        let member_set_digest = cbor::opt_bytes_field(&map, 18)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let grants_digest = cbor::opt_bytes_field(&map, 19)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let slot_set_digest = cbor::opt_bytes_field(&map, 44)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let grant_leaf_manifests = cbor::opt_array_field(&map, 33)?
            .unwrap_or_default()
            .iter()
            .map(cbor::value_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        let grant_leaf_digest = cbor::opt_bytes_field(&map, 34)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let asset_set_digest = cbor::opt_bytes_field(&map, 20)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let uploaded_objects = u32::try_from(cbor::uint_field(&map, 21)?)
            .map_err(|_| SyncError::LocalCorrupt("journal uploaded_objects 溢出"))?;
        let total_objects = u32::try_from(cbor::uint_field(&map, 22)?)
            .map_err(|_| SyncError::LocalCorrupt("journal total_objects 溢出"))?;
        let repacked_outbox = u32::try_from(cbor::uint_field(&map, 23)?)
            .map_err(|_| SyncError::LocalCorrupt("journal repacked 溢出"))?;
        let total_outbox = u32::try_from(cbor::uint_field(&map, 24)?)
            .map_err(|_| SyncError::LocalCorrupt("journal total_outbox 溢出"))?;
        let local_cut = cbor::opt_uint_field(&map, 25)?;
        let new_manager_transition = cbor::opt_bytes_field(&map, 26)?;
        let old_manager_signature = cbor::opt_bytes_field(&map, 27)?
            .map(|value| fixed64(&value))
            .transpose()?;
        let new_manager_signature = cbor::opt_bytes_field(&map, 28)?
            .map(|value| fixed64(&value))
            .transpose()?;
        let control_chain_rev = cbor::opt_uint_field(&map, 29)?;
        let pinned_instance_id = cbor::opt_bytes_field(&map, 30)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let pinned_log_id = cbor::opt_bytes_field(&map, 31)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let pinned_control_hash = cbor::opt_bytes_field(&map, 32)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let previous_candidate_epoch = cbor::opt_uint_field(&map, 35)?;
        let restart_intent = cbor::opt_field(&map, 49)
            .map(decode_restart_intent)
            .transpose()?;
        let restart_password = cbor::opt_bytes_field(&map, 36)?;
        let recovery_seed = cbor::opt_bytes_field(&map, 37)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let manager_seed = cbor::opt_bytes_field(&map, 38)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let recovery_backup_confirmed = cbor::bool_field(&map, 39).unwrap_or(false);
        let restart_grant_manifests = cbor::opt_array_field(&map, 40)?
            .unwrap_or_default()
            .iter()
            .map(cbor::value_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        let restart_assets = cbor::opt_array_field(&map, 45)?
            .unwrap_or_default()
            .iter()
            .map(decode_restart_asset)
            .collect::<Result<Vec<_>, _>>()?;
        let restart_password_message_id = cbor::opt_bytes_field(&map, 46)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let restart_recovery_message_id = cbor::opt_bytes_field(&map, 47)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let restart_previous_rotation_id = cbor::opt_bytes_field(&map, 48)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let activated_epoch = cbor::opt_uint_field(&map, 41)?;
        let activated_control_rev = cbor::opt_uint_field(&map, 42)?;
        let activation_commit = cbor::opt_bytes_field(&map, 43)?;
        Ok(Self {
            phase,
            request_id,
            body_digest,
            from_epoch,
            to_epoch,
            rotation_id,
            freeze_seq,
            freeze_log_id,
            control_rev,
            control_hash,
            candidate_root,
            new_manager_public,
            new_manager_rev,
            new_manager_transition,
            old_manager_signature,
            new_manager_signature,
            control_chain_rev,
            pinned_instance_id,
            pinned_log_id,
            pinned_control_hash,
            r_prime_backed_up,
            new_slot_descriptors,
            new_slot_bodies,
            new_slot_ids,
            baselines,
            member_set_digest,
            grants_digest,
            slot_set_digest,
            grant_leaf_manifests,
            grant_leaf_digest,
            asset_set_digest,
            uploaded_objects,
            total_objects,
            repacked_outbox,
            total_outbox,
            local_cut,
            previous_candidate_epoch,
            restart_intent,
            restart_password,
            recovery_seed,
            manager_seed,
            recovery_backup_confirmed,
            restart_grant_manifests,
            restart_password_message_id,
            restart_recovery_message_id,
            restart_previous_rotation_id,
            restart_assets,
            activated_epoch,
            activated_control_rev,
            activation_commit,
        })
    }
}

fn journal_entropy(_vault: &Vault) -> Vec<u8> {
    JOURNAL_DOMAIN.to_vec()
}

fn begin_body_digest(from_epoch: u64, request_id: &[u8; 16]) -> [u8; 32] {
    let mut input = Vec::with_capacity(8 + 16);
    input.extend_from_slice(&from_epoch.to_be_bytes());
    input.extend_from_slice(request_id);
    Sha256::digest(&input).into()
}

/// restart 的唯一 canonical body 是空 CDE map（relay `CanonicalBody::parse(..., 0)` 要求零字段）。
fn restart_body_digest() -> [u8; 32] {
    // 空 map 的 CDE 编码；构造失败在正常参数下不可能，退回全零摘要只用于 journal 辅助字段。
    let body = cbor::encode(Vec::new()).unwrap_or_default();
    Sha256::digest(&body).into()
}

fn baseline_cde(baseline: &JournalBaseline) -> cbor::CdeValue {
    cbor::CdeValue::Map(vec![
        (0, cbor::bytes(baseline.checkpoint_id.to_vec())),
        (1, cbor::uint(baseline.covered_seq)),
        (2, cbor::text(baseline.cursor.clone())),
        (3, cbor::bytes(baseline.manifest_object_id.to_vec())),
        (4, cbor::bytes(baseline.manifest_digest.to_vec())),
        (5, cbor::bytes(baseline.refs_digest.to_vec())),
        (6, cbor::boolean(baseline.verified)),
        (7, cbor::bytes(baseline.commit_digest.to_vec())),
        (8, cbor::bytes(baseline.asset_set_digest.to_vec())),
    ])
}

/// restart intent 的 journal CDE（公开事实，不含任何 fresh 秘密）。
fn restart_intent_cde(intent: &RestartIntent) -> cbor::CdeValue {
    cbor::CdeValue::Map(vec![
        (0, cbor::bytes(intent.previous_rotation_id.to_vec())),
        (1, cbor::bytes(intent.request_id.to_vec())),
        (2, cbor::uint(intent.from_epoch)),
        (3, cbor::uint(intent.previous_candidate_epoch)),
        (4, cbor::uint(intent.expected_control_rev)),
        (5, cbor::uint(intent.manager_rev)),
        (6, cbor::uint(intent.freeze_seq)),
        (7, cbor::bytes(intent.instance_id.to_vec())),
        (8, cbor::boolean(intent.freeze_barrier)),
    ])
}

fn restart_asset_cde(asset: &JournalRestartAsset) -> cbor::CdeValue {
    cbor::CdeValue::Map(vec![
        (0, cbor::bytes(asset.real_sha256.to_vec())),
        (1, cbor::uint(asset.byte_len)),
        (2, cbor::text(asset.media_type.clone())),
        (3, cbor::bytes(asset.object_id.to_vec())),
        (4, cbor::bytes(asset.object_digest.to_vec())),
    ])
}

fn decode_restart_asset(value: &cbor::CdeValue) -> Result<JournalRestartAsset, SyncError> {
    let map = cbor::value_map(value)?;
    Ok(JournalRestartAsset {
        real_sha256: cbor::bytes_fixed(&map, 0)?,
        byte_len: cbor::uint_field(&map, 1)?,
        media_type: cbor::text_field(&map, 2)?,
        object_id: cbor::bytes_fixed(&map, 3)?,
        object_digest: cbor::bytes_fixed(&map, 4)?,
    })
}

fn value_bytes(value: &cbor::CdeValue) -> Result<Vec<u8>, SyncError> {
    match value {
        cbor::CdeValue::Bytes(bytes) => Ok(bytes.clone()),
        _ => Err(SyncError::LocalCorrupt("journal 数组元素不是 bytes")),
    }
}

fn decode_baseline(value: &cbor::CdeValue) -> Result<JournalBaseline, SyncError> {
    let map = cbor::value_map(value)?;
    Ok(JournalBaseline {
        checkpoint_id: cbor::bytes_fixed(&map, 0)?,
        covered_seq: cbor::uint_field(&map, 1)?,
        cursor: cbor::text_field(&map, 2)?,
        manifest_object_id: cbor::bytes_fixed(&map, 3)?,
        manifest_digest: cbor::bytes_fixed(&map, 4)?,
        refs_digest: cbor::bytes_fixed(&map, 5)?,
        verified: cbor::bool_field(&map, 6)?,
        // 旧 journal 没有这两个字段：保留零值，签 T 前会 fail-closed 拒绝。
        commit_digest: cbor::opt_bytes_field(&map, 7)?
            .map(|value| fixed32(&value))
            .transpose()?
            .unwrap_or([0u8; 32]),
        asset_set_digest: cbor::opt_bytes_field(&map, 8)?
            .map(|value| fixed32(&value))
            .transpose()?
            .unwrap_or([0u8; 32]),
    })
}

fn decode_restart_intent(value: &cbor::CdeValue) -> Result<RestartIntent, SyncError> {
    let map = cbor::value_map(value)?;
    Ok(RestartIntent {
        previous_rotation_id: cbor::bytes_fixed(&map, 0)?,
        request_id: cbor::bytes_fixed(&map, 1)?,
        from_epoch: cbor::uint_field(&map, 2)?,
        previous_candidate_epoch: cbor::uint_field(&map, 3)?,
        expected_control_rev: cbor::uint_field(&map, 4)?,
        manager_rev: cbor::uint_field(&map, 5)?,
        freeze_seq: cbor::uint_field(&map, 6)?,
        instance_id: cbor::bytes_fixed(&map, 7)?,
        freeze_barrier: cbor::bool_field(&map, 8)?,
    })
}

fn fixed16(bytes: &[u8]) -> Result<[u8; 16], SyncError> {
    bytes
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("journal 字段长度不是 16B"))
}

fn fixed32(bytes: &[u8]) -> Result<[u8; 32], SyncError> {
    bytes
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("journal 字段长度不是 32B"))
}

fn fixed64(bytes: &[u8]) -> Result<[u8; 64], SyncError> {
    bytes
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("journal 签名长度不是 64B"))
}

/// 新秘密的固定长度（password 必须由用户新给，R'=random32）。
pub const NEW_RECOVERY_SEED_LEN: usize = 32;

/// 生成 R'（调用方负责离线备份并置 `r_prime_backed_up`）。
pub fn generate_r_prime() -> Result<Secret32, SyncError> {
    Ok(Secret32::from_array(crate::random::array(
        &mut crate::random::OsRandom,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use menghunote_storage::{create_body_doc, set_confirmed_highwater};

    fn fresh() -> (tempfile::TempDir, Vault) {
        crate::test_support::ensure_local_wrap();
        let base = std::env::var("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/tmp"));
        let base = base.as_path();
        std::fs::create_dir_all(base).expect("target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("rotation-journal-")
            .tempdir_in(base)
            .expect("临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
        (dir, vault)
    }

    fn frozen(vault: &Vault, request_id: [u8; 16]) -> RotationJournal {
        RotationJournal::freeze(vault, 1, request_id, 7, [0x11; 32]).expect("freeze")
    }

    #[test]
    fn apply_phase_is_durable_and_fail_closed() {
        let (_dir, vault) = fresh();
        assert_eq!(
            RotationApplyPhase::load(&vault).expect("phase"),
            None,
            "从未 apply 过时没有屏障"
        );
        assert!(!apply_phase_barrier(&vault), "absent 不冻结");

        // applying 是写库前的持久屏障：除 applied 外一律冻结。
        RotationApplyPhase::Applying
            .store(&vault)
            .expect("applying");
        assert_eq!(
            RotationApplyPhase::load(&vault).expect("phase"),
            Some(RotationApplyPhase::Applying)
        );
        assert!(apply_phase_barrier(&vault));

        RotationApplyPhase::Applied.store(&vault).expect("applied");
        assert!(!apply_phase_barrier(&vault), "只有 applied 解冻");

        // 旧版本写过的 staging 语义等同 applying（重启后仍冻结）。
        vault
            .set_meta(ROTATION_APPLY_PHASE_META, b"staging")
            .expect("legacy");
        assert_eq!(
            RotationApplyPhase::load(&vault).expect("phase"),
            Some(RotationApplyPhase::Applying)
        );
        assert!(apply_phase_barrier(&vault));

        // 未知/损坏值 fail-closed。
        vault
            .set_meta(ROTATION_APPLY_PHASE_META, b"garbage")
            .expect("garbage");
        assert_eq!(
            RotationApplyPhase::load(&vault).expect("phase"),
            Some(RotationApplyPhase::Failed)
        );
        assert!(apply_phase_barrier(&vault));

        // 失败态（paused/conflict/needs_restart）同样保持冻结。
        for phase in [
            RotationApplyPhase::Paused,
            RotationApplyPhase::Conflict,
            RotationApplyPhase::NeedsRestart,
        ] {
            phase.store(&vault).expect("store");
            assert!(apply_phase_barrier(&vault), "{phase:?} 必须冻结");
        }

        RotationApplyPhase::clear(&vault).expect("clear");
        assert_eq!(RotationApplyPhase::load(&vault).expect("phase"), None);
        assert!(!apply_phase_barrier(&vault));
    }

    #[test]
    fn apply_receipt_roundtrips_and_rejects_corruption() {
        let (_dir, vault) = fresh();
        let receipt = RotationApplyReceipt::new(
            [0x11; 16], [0x22; 16], [0x33; 16], [0x44; 16], 6, "c6", 1, 2, 4, [0x55; 16],
            [0x66; 32], 3, [0x77; 32],
        );
        assert_eq!(RotationApplyReceipt::load(&vault).expect("load"), None);
        receipt.store(&vault).expect("store");
        let loaded = RotationApplyReceipt::load(&vault)
            .expect("load")
            .expect("receipt");
        assert_eq!(loaded, receipt);
        assert_eq!(loaded.rotation_id(), [0x33; 16]);
        assert_eq!(loaded.checkpoint_id(), [0x44; 16]);
        assert_eq!(loaded.baseline_s(), 6);
        assert_eq!(loaded.baseline_cursor(), "c6");
        assert_eq!(loaded.from_epoch(), 1);
        assert_eq!(loaded.epoch(), 2);
        assert_eq!(loaded.control_rev(), 4);
        assert_eq!(loaded.instance_id(), [0x55; 16]);
        assert_eq!(loaded.new_manager_public(), [0x66; 32]);
        assert_eq!(loaded.new_manager_rev(), 3);
        assert_eq!(loaded.activation_hash(), [0x77; 32]);

        // 未知版本 / 字段数不符 / 非 canonical 一律 fail-closed。
        let mut value = crate::wire::decode_entries(
            &vault
                .get_meta(ROTATION_APPLY_RECEIPT_META)
                .expect("meta")
                .expect("value"),
        )
        .expect("entries");
        for entry in &mut value {
            if entry.0 == 0 {
                entry.1 = cbor::uint(99);
            }
        }
        let bad_version = crate::wire::encode(&cbor::CdeValue::Map(value)).expect("encode");
        vault
            .set_meta(ROTATION_APPLY_RECEIPT_META, &bad_version)
            .expect("store bad version");
        assert!(RotationApplyReceipt::load(&vault).is_err());

        vault
            .set_meta(ROTATION_APPLY_RECEIPT_META, &[0xa1, 0x00, 0x01])
            .expect("store short");
        assert!(RotationApplyReceipt::load(&vault).is_err());

        // begin_apply 必须原子地清掉旧 receipt 并落 applying。
        RotationApplyReceipt::store(&receipt, &vault).expect("store again");
        RotationApplyPhase::Applied.store(&vault).expect("applied");
        RotationApplyPhase::begin_apply(&vault).expect("begin");
        assert_eq!(
            RotationApplyPhase::load(&vault).expect("phase"),
            Some(RotationApplyPhase::Applying)
        );
        assert_eq!(RotationApplyReceipt::load(&vault).expect("receipt"), None);
        assert!(apply_phase_barrier(&vault));
    }

    #[test]
    fn restart_intent_is_durable_before_assignment_without_secrets() {
        let (_dir, vault) = fresh();
        let intent = RestartIntent {
            previous_rotation_id: [0x71; 16],
            request_id: [0x72; 16],
            from_epoch: 3,
            previous_candidate_epoch: 4,
            expected_control_rev: 9,
            manager_rev: 2,
            freeze_seq: 12,
            instance_id: [0x73; 16],
            freeze_barrier: true,
        };
        let journal =
            RotationJournal::record_restart_intent(&vault, intent.clone()).expect("record intent");
        assert_eq!(journal.phase, RotationPhase::RestartIntentRecorded);
        assert_eq!(journal.to_epoch, 0, "分配前不得写候选 epoch");
        assert!(journal.rotation_id.is_none());
        assert!(journal.candidate_root.is_none());
        assert!(journal.restart_password.is_none());
        assert!(journal.recovery_seed.is_none());
        assert!(journal.manager_seed.is_none());
        let loaded = RotationJournal::load(&vault)
            .expect("load")
            .expect("journal");
        assert_eq!(loaded.phase, journal.phase);
        assert_eq!(loaded.request_id, journal.request_id);
        assert_eq!(loaded.from_epoch, journal.from_epoch);
        assert_eq!(loaded.to_epoch, journal.to_epoch);
        assert_eq!(loaded.control_rev, journal.control_rev);
        assert_eq!(
            loaded.previous_candidate_epoch,
            journal.previous_candidate_epoch
        );
        assert_eq!(
            loaded.restart_previous_rotation_id,
            journal.restart_previous_rotation_id
        );
        assert_eq!(
            loaded.restart_intent, journal.restart_intent,
            "公开 intent 必须逐字段 durable"
        );
        // 已有 journal 时不得再落第二份 intent（否则会把两次请求混在一起）。
        assert!(RotationJournal::record_restart_intent(&vault, journal_intent()).is_err());
        assert!(
            RotationJournal::freeze(&vault, 3, [0x74; 16], 9, [0u8; 32]).is_err(),
            "也不能被普通 rotate freeze 覆盖"
        );
    }

    fn journal_intent() -> RestartIntent {
        RestartIntent {
            previous_rotation_id: [0x81; 16],
            request_id: [0x82; 16],
            from_epoch: 5,
            previous_candidate_epoch: 6,
            expected_control_rev: 10,
            manager_rev: 2,
            freeze_seq: 13,
            instance_id: [0x83; 16],
            freeze_barrier: true,
        }
    }

    #[test]
    fn journal_roundtrips_with_secrets_and_activate_fields() {
        let (_dir, vault) = fresh();
        let mut journal = frozen(&vault, [0x21; 16]);
        let request_id = journal.request_id();
        journal
            .mark_begun([0x31; 16], 9, [0x41; 16])
            .expect("begun");
        let root = RootSecretV1::new([0x55; 16], 2, Secret32::from_array([0x66; 32]));
        journal.set_candidate(root.clone(), [0x77; 32], 2, true);
        journal.set_activate_new_manager(vec![1, 2, 3], [0x88; 64], [0x99; 64], 3);
        journal.save(&vault).expect("save");
        // 无明文旁路：vault.meta 里的 journal 必须是本地封装密文，不能出现候选根明文。
        let blob = vault
            .get_meta(ROTATION_JOURNAL_META)
            .expect("meta")
            .expect("blob");
        assert!(
            !blob.windows(32).any(|window| window == [0x66u8; 32]),
            "journal 必须以本地封装密文落盘"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(&blob[..4], b"MNKW", "Linux journal 必须带 MNKW 外层头");
        let loaded = RotationJournal::load(&vault)
            .expect("load")
            .expect("journal");
        assert_eq!(loaded.phase, RotationPhase::Begun);
        assert_eq!(loaded.request_id(), request_id);
        assert_eq!(loaded.rotation_id, Some([0x31; 16]));
        assert_eq!(loaded.freeze_seq, Some(9));
        assert!(loaded.r_prime_backed_up);
        assert_eq!(
            loaded
                .candidate_root
                .expect("root")
                .encode()
                .expect("encode"),
            root.encode().expect("root encode")
        );
        assert_eq!(loaded.new_manager_transition, Some(vec![1, 2, 3]));
        assert_eq!(loaded.old_manager_signature, Some([0x88; 64]));
        assert_eq!(loaded.control_chain_rev, Some(3));
    }

    #[test]
    fn phase_cannot_skip_and_request_id_is_stable() {
        let (_dir, vault) = fresh();
        let mut journal = frozen(&vault, [0x22; 16]);
        let first = journal.request_id();
        assert_eq!(first, journal.request_id());
        assert!(
            journal.advance(RotationPhase::Integrated).is_err(),
            "Frozen 不能直接到 Integrated"
        );
        journal
            .mark_begun([0x32; 16], 4, [0x42; 16])
            .expect("begun");
        assert_eq!(journal.phase, RotationPhase::Begun);
    }

    #[test]
    fn integrate_allows_local_pending_edits() {
        let (_dir, vault) = fresh();
        set_confirmed_highwater(&vault, 5).expect("highwater");
        vault
            .set_meta(
                menghunote_storage::META_SYNC_INTEGRATED_SEQ,
                &5u64.to_be_bytes(),
            )
            .expect("integrated");
        create_body_doc(&vault, "note-1").expect("doc");
        let body = menghunote_storage::BodyDoc::create("note-1", 1).expect("body");
        body.insert_scalars(0, "pending").expect("text");
        let pending = body
            .pending_update(&loro::VersionVector::default())
            .expect("pending update");
        menghunote_storage::append_updates(
            &vault,
            "note-1",
            0,
            &[menghunote_storage::UpdateRecord::new("msg-1", pending)],
        )
        .expect("local update");
        menghunote_storage::enqueue_sync_intent(
            &vault,
            "msg-1",
            "batch-1",
            "note-1",
            1,
            1,
            &[0xAA; 32],
        )
        .expect("pending outbox");
        let mut journal = frozen(&vault, [0x23; 16]);
        journal
            .mark_begun([0x33; 16], 5, [0x43; 16])
            .expect("begun");
        // 冻结后继续产生的本地编辑（pending/encrypted）不阻断轮换；它们推广后重包。
        journal
            .mark_integrated(&vault, 5, 5)
            .expect("本地 pending 不应阻断 ready");
        assert_eq!(journal.phase, RotationPhase::Integrated);
    }

    #[test]
    fn integrate_requires_highwater_to_cover_s() {
        let (_dir, vault) = fresh();
        set_confirmed_highwater(&vault, 3).expect("highwater");
        let mut journal = frozen(&vault, [0x24; 16]);
        journal
            .mark_begun([0x34; 16], 5, [0x44; 16])
            .expect("begun");
        assert!(journal.mark_integrated(&vault, 5, 5).is_err());
    }

    #[test]
    fn received_ahead_of_integrated_is_allowed_when_s_covered() {
        let (_dir, vault) = fresh();
        set_confirmed_highwater(&vault, 5).expect("highwater");
        vault
            .set_meta(
                menghunote_storage::META_SYNC_RECEIVED_SEQ,
                &6u64.to_be_bytes(),
            )
            .expect("received");
        vault
            .set_meta(
                menghunote_storage::META_SYNC_INTEGRATED_SEQ,
                &5u64.to_be_bytes(),
            )
            .expect("integrated");
        let mut journal = frozen(&vault, [0x26; 16]);
        journal
            .mark_begun([0x36; 16], 5, [0x46; 16])
            .expect("begun");
        // received 超前只说明 >S 的入站事件尚未整合；≤S 已覆盖且无 inbox 缺口时允许 ready。
        // quarantine/因果洞由 `rotation::ensure_c_phase_gate` 的 inbox 查询单独拦截。
        journal
            .mark_integrated(&vault, 5, 5)
            .expect("received 超前但 ≤S 已覆盖时不应阻断");
        assert_eq!(journal.phase, RotationPhase::Integrated);
    }
}
