//! relay 客户端抽象（ADR 0006 §5）。
//!
//! 引擎只依赖这个**语义**接口；`HttpRelayClient`（[`crate::http_relay`]）负责 JSON/HTTP/
//! token 刷新，测试用 `FakeRelay` 直接实现它（见 `tests/support/fake_relay.rs`）。
//!
//! 为什么拆两层：
//!
//! - 引擎测试不需要真实网络（FakeRelay + 真 SQLite，不 sleep）；
//! - HTTP 层可以单独用 mock transport 验证请求形状/token 刷新/错误映射；
//! - 真实 relay 的 `SignatureHook` 目前是 fail-closed（`crates/relay/src/sig.rs`），
//!   真实端到端联调是 relay 侧接线后的独立任务，不能把它塞进本 crate 的测试。

use crate::error::SyncError;
use crate::{DeviceId, ObjectId, VaultId};
use menghunote_sync_proto::{ActivationCommitV1, ActivationTV1, DeviceCertV1, ObjectCommit};

/// 同步必须暂停的原因（ADR §5：暂停后**不得**推进游标或 ACK）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseReason {
    /// relay head 低于本地已确认高水位（疑似回滚）。
    RollbackDetected,
    /// relay `log_id` 与本地 pinned 值不同（restore/换实例）。
    LogIdChanged,
    /// relay 活动 epoch 与本地根不匹配（需要先 bootstrap/rotation）。
    EpochMismatch,
    /// 本地已收到的前缀已被 GC（`gc_floor` 超过 `received_seq`）。
    CursorExpired,
    /// 日志里出现本任务范围外的事件（asset/checkpoint/control），不能静默跳过。
    UnsupportedEvent,
    /// 游标已过期且 checkpoint 快速追赶失败；不得回退普通 pull（会立即再次过期）。
    CatchupFailed,
    /// 事件缺少 ObjectCommit 签名（真实 relay 还没在 log 事件里带签名）。
    MissingSignature,
    /// 轮换阶段 A–C 已冻结：禁止再 push/encrypt（A 事务编辑仍可继续）。
    RotationFrozen,
    /// relay 在同一 `relay_seq` 上返回了与本地 durable 证据不同的历史。
    ///
    /// 触发点是 engine 已真实下载并落盘的 ASSET（skipped-asset 记录 HMAC 通过）
    /// 再次拉取同一 seq 时身份字段不一致：本地记录未被篡改，是 relay 重写了历史，
    /// 必须暂停等待人工处理，不能报成本地数据损坏。
    RelayHistoryRewritten,
}

impl core::fmt::Display for PauseReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = match self {
            Self::RollbackDetected => "检测到 relay 回滚（head 低于已确认高水位）",
            Self::LogIdChanged => "relay log_id 变化（恢复/换实例）",
            Self::EpochMismatch => "relay epoch 与本地根不匹配",
            Self::CursorExpired => "游标前缀已被 GC",
            Self::UnsupportedEvent => "日志含本任务范围外的事件类型",
            Self::CatchupFailed => "checkpoint 快速追赶失败（游标已过期，等待重试）",
            Self::MissingSignature => "日志事件缺少 ObjectCommit 签名",
            Self::RotationFrozen => "轮换已冻结，禁止 push/encrypt",
            Self::RelayHistoryRewritten => {
                "relay 改写了已消费 seq 的历史（与本地 durable 证据冲突）"
            }
        };
        f.write_str(text)
    }
}

/// relay 状态（`GET P/state` 的客户端视图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayState {
    /// vault 身份。
    pub vault_id: VaultId,
    /// 日志实例（restore 必须换新）。
    pub log_id: [u8; 16],
    /// relay 实例 id。
    pub instance_id: [u8; 16],
    /// 当前活动 epoch。
    pub epoch: u64,
    /// 控制链版本。
    pub control_rev: u64,
    /// 鉴权版本（revoke 后递增）。
    pub auth_rev: u64,
    /// 已分配的最大 relay_seq。
    pub head_seq: u64,
    /// 日志前缀 GC 下界。
    pub gc_floor: u64,
    /// §30.3：当前 frozen/staging 轮换 id（响应丢失时先查询）。
    pub current_rotation_id: Option<[u8; 16]>,
}

/// 日志事件类型（ADR §5：update/asset/checkpoint/control）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayLogKind {
    /// 数据 update。
    Update,
    /// 资产对象。
    Asset,
    /// checkpoint。
    Checkpoint,
    /// 控制面转换。
    Control,
}

impl RelayLogKind {
    /// 解析 wire 字符串；未知类型返回 `None`（由调用方转成暂停）。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "update" => Some(Self::Update),
            "asset" => Some(Self::Asset),
            "checkpoint" => Some(Self::Checkpoint),
            "control" => Some(Self::Control),
            _ => None,
        }
    }
}

/// 一条日志事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayLogEvent {
    /// 送达顺序（仅 relay 解读）。
    pub relay_seq: u64,
    /// 事件类型。
    pub kind: RelayLogKind,
    /// 关联对象（update/asset 有）。
    pub object_id: Option<ObjectId>,
    /// 对象所属 epoch。
    pub epoch: u64,
    /// 事件载荷：UPDATE 为 `ObjectCommit` 的 CDE 字节。
    pub event: Vec<u8>,
    /// `ObjectCommit` 的设备签名。
    ///
    /// **真实 relay 缺口**：当前 `GET log` 的事件只带 commit、不带签名（`objects.signed_commit`
    /// 列存的是 commit 字节）。引擎要求签名才能验收，因此这里的 `None` 会让同步暂停；
    /// relay 侧补上签名（或提供可取的签名端点）后该字段才有值。
    pub signature: Option<[u8; 64]>,
}

/// 一页日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayLogPage {
    /// 升序事件。
    pub events: Vec<RelayLogEvent>,
    /// 指最后返回事件的游标（无事件时回显请求游标）。
    pub next_cursor: String,
    /// 本页覆盖到的水位。
    pub through_seq: u64,
    /// 查询时的 head。
    pub query_head: u64,
    /// 当前活动 epoch。
    pub epoch: u64,
    /// 当前 gc_floor。
    pub gc_floor: u64,
}

/// 一次对象提交（`POST /updates` 的一项）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateSubmission {
    /// 对象名（opaque 32B）。
    pub object_id: ObjectId,
    /// 已签名的提交记录（CDE）。
    pub commit: ObjectCommit,
    /// `commit.encode()` 的字节（避免重复编码）。
    pub signed_commit: Vec<u8>,
    /// 设备对 `object_commit_signing_input(commit)` 的签名。
    pub signature: [u8; 64],
}

/// 一项提交结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    /// 对应对象。
    pub object_id: ObjectId,
    /// 结果。
    pub status: UpdateStatus,
}

/// 提交状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// 首次接受；`relay_seq` 只对 UPDATE/ASSET 等会进 log 的对象存在
    /// （MANIFEST/KEY_SLOT 等控制对象可能不分配送达序号）。
    Accepted {
        /// 分配的送达序号（relay 未分配时为 `None`）。
        relay_seq: Option<u64>,
    },
    /// 完全相同字节的重复提交（幂等）。
    Duplicate,
    /// 业务失败（整批鉴权/epoch 错误由传输层错误表示）。
    Failed {
        /// 统一错误码。
        code: Option<String>,
        /// 是否可重试。
        retryable: bool,
    },
}

/// 一项对象提交（任意类型：UPDATE/SNAPSHOT/MANIFEST/ASSET/KEY_SLOT）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectSubmission {
    /// 对象名（opaque 32B）。
    pub object_id: ObjectId,
    /// 已签名的提交记录（CDE）。
    pub commit: ObjectCommit,
    /// `commit.encode()` 的字节。
    pub signed_commit: Vec<u8>,
    /// 设备对 `object_commit_signing_input(commit)` 的签名。
    pub signature: [u8; 64],
}

/// 一次 `PUT P/key-slots/{slot_id}` 的输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotUpload {
    /// 槽 id（也是 KEY_SLOT 对象 id）。
    pub slot_id: ObjectId,
    /// 槽所属 epoch。
    pub epoch: u64,
    /// 槽类型。
    pub kind: menghunote_sync_proto::SlotKind,
    /// `CDE(SlotDescriptorV1)`。
    pub descriptor: Vec<u8>,
    /// 设备签名的待签字节（本实现 = `descriptor`，见 `checkpoint` 模块文档）。
    pub signed_message: Vec<u8>,
    /// 设备签名（64B）。
    pub signature: [u8; 64],
}

/// relay 返回的槽记录（`GET P/key-slots` 的一项）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRecord {
    /// 槽 id。
    pub slot_id: ObjectId,
    /// 槽所属 epoch。
    pub epoch: u64,
    /// 槽类型。
    pub kind: menghunote_sync_proto::SlotKind,
    /// `CDE(SlotDescriptorV1)`。
    pub descriptor: Vec<u8>,
    /// `staging` / `active` / `retiring`。
    pub state: String,
    /// 槽集版本（activate 的 CAS 基准）。
    pub slot_set_rev: u64,
    /// retiring 槽的删除时间（Unix 秒）。
    pub retire_at: Option<u64>,
}

/// B 侧 latest self grant 的原始证据（外签/recipient/epoch 校验用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfGrantEvidence {
    pub epoch: u64,
    pub device_id: DeviceId,
    pub manager_rev: u64,
    pub hpke_enc: [u8; 32],
    pub ciphertext: Vec<u8>,
    pub signed_header: Vec<u8>,
    pub signature: Vec<u8>,
    pub package_digest: Option<[u8; 32]>,
}

/// `/control` 里**已复算**的 activation T 与旧/新管理双签（ADR 0006 §34）。
///
/// 只有 [`RelayClient::verified_activation_t`] 的实现能产出它：T 的双签、字段合法性
/// 与"钉在已验证控制链上"都已由实现方（真实客户端 = `HttpRelayClient`）复算过。
/// 调用方拿到的 `None` 表示**无法复算**，绝不表示"验证通过"（必须 fail-closed）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedActivationT {
    /// canonical CDE 解码后的 T（20 字段已校验）。
    pub activation_t: ActivationTV1,
    /// 旧管理 key 对 `activation_t_signing_input(T)` 的 64B 签名。
    pub old_t_signature: Vec<u8>,
    /// 新管理 key 对同一输入的 64B 签名。
    pub new_t_signature: Vec<u8>,
}

/// `/control` 中一条 `epoch.activated` 的**完整授权材料**（epoch 链的一环）。
///
/// 设备离线跨过多个 epoch 后，本地 `current_epoch` 只会落后于链尾：latest T 的
/// `from_epoch` 不再是本地 epoch。此时必须先沿控制链逐条验证每一环（T 双签、T 与
/// rotation view 绑定、`ActivationCommitV1` 摘要），才能用链尾的 T 授权 apply。
///
/// 本结构由 [`RelayClient::epoch_activations`] 产出：实现方（真实客户端 =
/// [`crate::http_relay::HttpRelayClient`]）负责把 T 钉在**已验证**控制链的位置上，
/// 并把 `rotation view + primary/fallback 公开事实` 重建为 `ActivationCommitV1`；
/// 消费方（[`crate::rotation_apply::verify_epoch_activation_chain`]）再复算全部绑定，
/// 不信任 relay 自报。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochActivationLink {
    /// 该 activation 记录在控制链上的 rev（必须等于 `T.expected_control_rev`）。
    pub control_rev: u64,
    /// 链上**前一条**记录（`control_rev - 1`）的 `record_hash`；必须等于 `T.prev_control_hash`。
    pub prev_control_hash: [u8; 32],
    /// `GET /rotations/{id}` 视图（必须是 `activated`）。
    pub view: RotationView,
    /// `/control` 里已复算的 activation T 与旧/新管理双签。
    pub evidence: VerifiedActivationT,
    /// 由 view + primary/fallback checkpoint 公开事实重建的 `ActivationCommitV1`。
    ///
    /// relay 只把它的 SHA256 记在 view 键 20；消费方必须重新编码并与该摘要逐字节比对。
    pub activation_commit: ActivationCommitV1,
}

/// staged grant 集合的一个 leaf：relay `grants_digest` 口径下的
/// `(device_id, SHA256(signed_header))`。
///
/// 设备端没有 management realm（relay `GET /rotations/{r}/grants` 仅 management 可读），
/// 因此真实客户端通常只能证明 `grants_count == 1` 的单 leaf 情形；完整 leaf 集合只
/// 出现在具备 management 凭据的调用方/测试夹具里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantLeafEvidence {
    /// 收件设备。
    pub device_id: DeviceId,
    /// `SHA256(grant.signed_header)`（relay `grants_digest` 的叶子）。
    pub signed_header_sha256: [u8; 32],
}

impl GrantLeafEvidence {
    /// 从 `signed_header` 原字节构造 leaf。
    #[must_use]
    pub fn new(device_id: DeviceId, signed_header: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            device_id,
            signed_header_sha256: Sha256::digest(signed_header).into(),
        }
    }
}

/// `GET P/checkpoints/{cid}` 返回的 manifest 明文（只读）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointManifest {
    pub checkpoint_id: [u8; 16],
    pub manifest_bytes: Vec<u8>,
}

/// `POST P/checkpoints` 的 writer lease。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointLease {
    /// checkpoint id。
    pub checkpoint_id: menghunote_sync_proto::CheckpointId,
    /// 覆盖的 epoch。
    pub epoch: u64,
    /// 创建时固定的 S。
    pub covered_seq: u64,
    /// 维护 reservation id。
    pub reservation_id: [u8; 16],
    /// reservation 过期时间（Unix 秒）。
    pub expires_at: u64,
}

/// checkpoint ref 的角色（relay `RefRole`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointRole {
    /// snapshot / asset 数据对象（纳入 `refs_digest`）。
    Data,
    /// manifest 对象本体（不纳入 `refs_digest`）。
    Manifest,
}

impl CheckpointRole {
    /// wire 文本值。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Manifest => "manifest",
        }
    }
}

/// 一条 checkpoint ref。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointRef {
    /// 对象 id。
    pub object_id: ObjectId,
    /// 角色。
    pub role: CheckpointRole,
}

/// `POST P/checkpoints/{c}/publish` 的输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointPublish {
    /// checkpoint id。
    pub checkpoint_id: menghunote_sync_proto::CheckpointId,
    /// 覆盖的 S（必须等于创建时固定的 S）。
    pub covered_seq: u64,
    /// manifest 对象 id（必须与 manifest ref 一致）。
    pub manifest_oid: ObjectId,
    /// 公开 refs 的摘要（只覆盖 snapshot/asset）。
    pub refs_digest: [u8; 32],
    /// `CheckpointCommit` 的 CDE 字节。
    pub signed_commit: Vec<u8>,
    /// 设备对 checkpoint 待签字节的签名。
    pub signature: [u8; 64],
}

/// publish 成功后的 relay 视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedCheckpoint {
    /// checkpoint id。
    pub checkpoint_id: menghunote_sync_proto::CheckpointId,
    /// epoch。
    pub epoch: u64,
    /// 覆盖的 S。
    pub covered_seq: u64,
    /// 发布时间（Unix 秒）。
    pub published_at: u64,
}

/// bootstrap 返回的一份基线视图（relay `/bootstrap` 字段 0/1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapCheckpoint {
    /// checkpoint id。
    pub checkpoint_id: menghunote_sync_proto::CheckpointId,
    /// epoch。
    pub epoch: u64,
    /// 覆盖的 S。
    pub covered_seq: u64,
    /// manifest 对象 id（relay 已发布基线必有）。
    pub manifest_oid: ObjectId,
    /// 发布时承诺的 refs digest（relay 响应字段 4；旧库可能缺）。
    pub refs_digest: Option<[u8; 32]>,
    /// 该 S 对应的 relay 游标（`next_cursor` 语义，可直接喂给 `/log`）。
    pub cursor: String,
    /// ADR §20：relay 返回的 `CheckpointCommit` CDE（bootstrap 响应键 6）。
    pub signed_commit: Option<Vec<u8>>,
    /// ADR §20：`CheckpointCommit` 设备签名（bootstrap 响应键 7）。
    pub signature: Option<[u8; 64]>,
}

/// bootstrap read lease（`/bootstrap` 字段 5/6，续租用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapLease {
    /// lease id。
    pub lease_id: [u8; 16],
    /// 到期 Unix 秒。
    pub expires_at: u64,
    /// 保护的下界 floor_seq。
    pub floor_seq: u64,
}

/// `POST P/bootstrap` 的完整结果：primary/fallback + head/epoch + lease。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapBundle {
    /// 最新有效基线（优先使用）。
    pub primary: BootstrapCheckpoint,
    /// 次新有效基线（primary 损坏时回退）。
    pub fallback: BootstrapCheckpoint,
    /// relay head_seq。
    pub head_seq: u64,
    /// relay gc_floor。
    pub gc_floor: u64,
    /// 当前 epoch。
    pub epoch: u64,
    /// 读 lease。
    pub lease: BootstrapLease,
    /// §30.3：bootstrap 是否走了 degraded（`?degraded=1`，只允许显式请求）。
    pub degraded: bool,
}

/// 轮换 begin 的客户端 wire（canonical body + 管理签名；ADR §28.2 阶段 B）。
#[derive(Debug, Clone)]
pub struct RotationBegin {
    pub request_id: [u8; 16],
    pub expected_epoch: u64,
    pub expected_control_rev: u64,
    pub revoke: Vec<[u8; 16]>,
    /// 唯一 canonical body（CDE），管理签名覆盖 `management_request_cde(POST,path,body)`。
    pub signed_body: Vec<u8>,
    pub signature: [u8; 64],
}

/// relay rotation 视图（`/rotations` 响应键 0；字段随 relay 增量扩展）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationView {
    pub rotation_id: [u8; 16],
    pub state: String,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub freeze_seq: u64,
    pub revoked_count: u64,
    pub member_set_hash: Option<[u8; 32]>,
    pub grants_count: u64,
    pub grants_digest: Option<[u8; 32]>,
    pub published_checkpoints: u64,
    /// §30.3 键 10–22：control/auth/manager rev、revoked_once、冻结资产摘要、
    /// 成员证书 digest、两份候选 cid、activation commit/时间/control_rev。
    pub control_rev: u64,
    pub auth_rev: u64,
    pub manager_rev: u64,
    pub from_epoch_revoked_once: bool,
    pub freeze_asset_count: u64,
    pub freeze_asset_digest: Option<[u8; 32]>,
    pub member_cert_digest: Option<[u8; 32]>,
    pub primary_checkpoint: Option<[u8; 16]>,
    pub fallback_checkpoint: Option<[u8; 16]>,
    pub activation_commit: Option<Vec<u8>>,
    pub activated_at: Option<u64>,
    pub activated_control_rev: Option<u64>,
    /// 键 23/26：候选集数量与 candidate_ready。
    pub candidate_count: u64,
    pub candidate_ready: bool,
    /// 键 25：维护 reservation id（候选 epoch 上传用）。
    pub reservation_id: Option<[u8; 16]>,
}

/// 冻结资产分页的一项（relay 只给不透明 object id 与元数据，real sha 在明文里）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenAssetRef {
    pub object_id: ObjectId,
    pub generation: u64,
    pub key_epoch: u64,
    pub part_count: u32,
    pub total_plain_len: u64,
    pub object_digest: [u8; 32],
}

/// `GET /rotations/{r}/assets?after=&limit=` 的一页。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenAssetPage {
    pub assets: Vec<FrozenAssetRef>,
    pub freeze_seq: u64,
    pub asset_set_digest: Option<[u8; 32]>,
    pub total: u64,
}

/// 候选 checkpoint 状态（`GET /checkpoints/{cid}`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointStatus {
    pub checkpoint_id: [u8; 16],
    pub state: String,
    pub role: Option<String>,
    pub covered_seq: u64,
    pub manifest_object_id: Option<ObjectId>,
    pub refs_digest: Option<[u8; 32]>,
    pub candidate_ready: bool,
    pub epoch: u64,
}

/// `/control` 键 5：管理权转换链的一条记录（ADR §27.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerChainEntry {
    pub rev: u64,
    pub pubkey: [u8; 32],
    pub transition: Vec<u8>,
    pub old_rev: u64,
    pub old_signature: Option<[u8; 64]>,
    pub new_signature: Option<[u8; 64]>,
}

/// relay 语义客户端。
///
/// 所有方法都假定调用方已持有有效 token（HTTP 实现内部负责刷新）；`SyncEngine` 只关心
/// 语义结果与错误是否可重试。
pub trait RelayClient {
    /// `GET P/state`。
    fn state(&mut self) -> Result<RelayState, SyncError>;

    /// `GET P/log?after=cursor&limit`。`cursor=None` 表示"从头"，真实 relay 需要
    /// bootstrap 给出初始游标（本 crate 的 `pull` 在无游标时会明确报错）。
    fn log(&mut self, cursor: Option<&str>, limit: u32) -> Result<RelayLogPage, SyncError>;

    /// `GET P/objects/{o}/parts/{n}`：返回 exact MNE1 envelope 字节。
    fn part(&mut self, object_id: &ObjectId, part_no: u32) -> Result<Vec<u8>, SyncError>;

    /// `PUT P/objects/{o}/parts/{n}`：exact 字节幂等上传。
    fn put_part(
        &mut self,
        object_id: &ObjectId,
        part_no: u32,
        envelope: &[u8],
    ) -> Result<(), SyncError>;

    /// §28.1：候选 epoch 上传必须携带维护 reservation id；默认实现不支持（旧 relay）。
    fn put_part_with_reservation(
        &mut self,
        object_id: &ObjectId,
        part_no: u32,
        envelope: &[u8],
        _reservation: Option<&[u8; 16]>,
    ) -> Result<(), SyncError> {
        self.put_part(object_id, part_no, envelope)
    }

    /// `POST P/updates`（≤256 项；整批一次鉴权，逐项结果）。
    fn submit_updates(
        &mut self,
        items: Vec<UpdateSubmission>,
    ) -> Result<Vec<UpdateOutcome>, SyncError>;

    /// 提交单个任意类型对象（snapshot/manifest/asset/key-slot）。
    fn submit_object(&mut self, item: ObjectSubmission) -> Result<UpdateOutcome, SyncError>;

    /// `POST P/acks`。
    fn ack(&mut self, received_seq: u64, integrated_seq: u64, epoch: u64) -> Result<(), SyncError>;

    /// 查设备证书（验 `ObjectCommit` 用）。
    fn device_cert(&mut self, device_id: &DeviceId) -> Result<Option<DeviceCertV1>, SyncError>;

    /// `PUT P/key-slots/{slot_id}`：登记一个已 complete 的 KEY_SLOT 对象。
    fn put_key_slot(&mut self, upload: SlotUpload) -> Result<SlotRecord, SyncError>;

    /// `GET P/key-slots`：列出可见槽。
    fn list_key_slots(&mut self) -> Result<Vec<SlotRecord>, SyncError>;

    /// `POST P/key-slots/password:activate`：CAS 切换 active password 槽。
    fn activate_password_slot(
        &mut self,
        expected_slot_set_rev: u64,
        slot_id: ObjectId,
        signed_message: Vec<u8>,
        signature: [u8; 64],
    ) -> Result<u64, SyncError>;

    /// `POST P/checkpoints`：申请 writer lease（固定 S）。
    fn create_checkpoint(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
    ) -> Result<CheckpointLease, SyncError>;

    /// §28.1：候选 checkpoint 创建时固定 role（primary/fallback）；默认忽略 role。
    fn create_checkpoint_with_role(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
        _role: Option<&str>,
    ) -> Result<CheckpointLease, SyncError> {
        self.create_checkpoint(estimated_bytes, epoch)
    }

    /// `POST P/checkpoints/{c}/refs`：追加 opaque refs（data + manifest）。
    fn add_checkpoint_refs(
        &mut self,
        checkpoint_id: menghunote_sync_proto::CheckpointId,
        refs: Vec<CheckpointRef>,
    ) -> Result<u64, SyncError>;

    /// `POST P/checkpoints/{c}/publish`。
    fn publish_checkpoint(
        &mut self,
        publish: CheckpointPublish,
    ) -> Result<PublishedCheckpoint, SyncError>;

    /// `POST P/bootstrap`：取两份有效基线 + 各自游标 + read lease。
    fn bootstrap(&mut self) -> Result<BootstrapBundle, SyncError>;

    /// `PATCH P/bootstrap-leases/{l}`：续租（过期 → 410，调用方重新 bootstrap）。
    fn renew_bootstrap_lease(&mut self, lease_id: [u8; 16]) -> Result<BootstrapLease, SyncError>;

    /// `POST P/rotations`（管理 realm；阶段 B）。
    fn begin_rotation(&mut self, _begin: RotationBegin) -> Result<RotationView, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 rotation begin",
        ))
    }

    /// `GET P/rotations/{id}`（管理/设备可读）。
    fn rotation(&mut self, _rotation_id: [u8; 16]) -> Result<RotationView, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 rotation 查询",
        ))
    }

    /// 丢响应后按 request_id 找回（relay task-101 后可用；未实现时返回 Unsupported）。
    fn rotation_for_request(
        &mut self,
        _request_id: [u8; 16],
    ) -> Result<Option<RotationView>, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 request_id 查询 rotation",
        ))
    }

    /// `GET /control` 键 5：管理权转换链（阶段 A 校验）。
    fn management_chain(&mut self) -> Result<Vec<ManagerChainEntry>, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 management chain",
        ))
    }

    /// 阶段 D：冻结 finalized 资产分页（不依赖可回收 log）。
    fn rotation_assets(
        &mut self,
        _rotation_id: [u8; 16],
        _after: Option<[u8; 32]>,
        _limit: u32,
    ) -> Result<FrozenAssetPage, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 frozen asset 分页",
        ))
    }

    /// 阶段 E：候选 checkpoint 状态（ready 前不对外生效）。
    fn checkpoint_status(
        &mut self,
        _checkpoint_id: [u8; 16],
    ) -> Result<CheckpointStatus, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 candidate checkpoint 状态",
        ))
    }

    /// §28.1：当前 active 成员证书的 HPKE 公钥（来自已验证 `/control` 缓存）。
    fn active_member_hpke_keys(&mut self) -> Result<Vec<(DeviceId, [u8; 32])>, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 active 成员 HPKE 列表",
        ))
    }

    /// §30.3：控制链 typed kind（键 4）。
    fn control_kinds(&mut self) -> Result<Vec<(u64, String)>, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 control_kinds",
        ))
    }

    /// B 侧 latest self grant 的原始证据。
    fn self_grant_evidence(&mut self) -> Result<SelfGrantEvidence, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 self_grant_evidence",
        ))
    }

    /// `/control` 中**已复算**的 activation T 与旧/新管理双签（ADR 0006 §34）。
    ///
    /// 这是 B 侧 apply 的**唯一授权来源**：relay 自报的 `ActivationCommitV1` 32B hash
    /// 只用于交叉一致性，不能代替 T。
    ///
    /// 默认 `Ok(None)`：表示该客户端**无法复算** T（历史控制记录不带 T，或实现方
    /// 没有管理 realm/控制链缓存）。`None` 绝不等于"验证通过"——调用方必须
    /// fail-closed（`Paused`，不写库、不推游标）。
    ///
    /// 真实 HTTP 客户端在 [`crate::http_relay::HttpRelayClient`] 里覆盖本方法，
    /// 直接委托它的 inherent `verified_activation_t`（按 `/control` 缓存复算）。
    fn verified_activation_t(
        &mut self,
        _rotation_id: [u8; 16],
    ) -> Result<Option<VerifiedActivationT>, SyncError> {
        Ok(None)
    }

    /// 完整 staged grant leaf 集合（证明本设备 latest grant 的 inclusion）。
    ///
    /// 默认 `Ok(None)`：设备端读不到 management 的 grant 清单，此时只有
    /// `grants_count == 1`（单 leaf 即全集）能复算 `grants_digest`；其余情形
    /// 调用方必须拒绝（无法证明 inclusion）。
    fn grant_leaf_evidence(
        &mut self,
        _rotation_id: [u8; 16],
    ) -> Result<Option<Vec<GrantLeafEvidence>>, SyncError> {
        Ok(None)
    }

    /// `/control` 中全部 `epoch.activated` 的完整授权材料（epoch 链），按控制链 rev 升序。
    ///
    /// 只返回 `to_epoch > from_epoch`（调用方本地可信 epoch）的环：每环的
    /// `view/T/activation_commit` 必须能在本地复算（见
    /// [`crate::rotation_apply::verify_epoch_activation_chain`]）。
    ///
    /// 真实客户端 = [`crate::http_relay::HttpRelayClient`]：从已验证 `/control` 缓存取
    /// T 与它在链上的位置（rev / 前一条 `record_hash`），再按 rotation/cid 重建 commit。
    ///
    /// **默认 `Ok(Vec::new())`**：表示该客户端无法提供可验证的链（不支持/无控制链缓存）。
    /// 空链绝不等于"验证通过"——本地 epoch 落后 relay 时调用方必须 fail-closed（`Paused`），
    /// 不得回退到"只信 latest rotation 的 view"。
    fn epoch_activations(
        &mut self,
        _from_epoch: u64,
    ) -> Result<Vec<EpochActivationLink>, SyncError> {
        Ok(Vec::new())
    }

    /// 只读 checkpoint manifest（不写库）。
    fn checkpoint_manifest(
        &mut self,
        _checkpoint_id: &[u8; 16],
    ) -> Result<CheckpointManifest, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 checkpoint_manifest",
        ))
    }

    /// 丢弃旧 token/控制缓存；下一次请求重新认证（begin/activate 会 bump auth_rev）。
    fn reauthenticate(&mut self) -> Result<(), SyncError> {
        Ok(())
    }

    /// §30.3：`GET /rotations/current`；没有进行中的轮换返回 `None`。
    fn current_rotation(&mut self) -> Result<Option<RotationView>, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 rotation current",
        ))
    }

    /// §30.3：显式 degraded bootstrap（`POST /bootstrap?degraded=1`）。
    fn bootstrap_degraded(&mut self) -> Result<BootstrapBundle, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 degraded bootstrap",
        ))
    }
}
