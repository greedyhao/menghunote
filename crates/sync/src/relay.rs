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
use menghunote_sync_proto::{DeviceCertV1, ObjectCommit};

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
    /// 事件缺少 ObjectCommit 签名（真实 relay 还没在 log 事件里带签名）。
    MissingSignature,
}

impl core::fmt::Display for PauseReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = match self {
            Self::RollbackDetected => "检测到 relay 回滚（head 低于已确认高水位）",
            Self::LogIdChanged => "relay log_id 变化（恢复/换实例）",
            Self::EpochMismatch => "relay epoch 与本地根不匹配",
            Self::CursorExpired => "游标前缀已被 GC",
            Self::UnsupportedEvent => "日志含本任务范围外的事件类型",
            Self::MissingSignature => "日志事件缺少 ObjectCommit 签名",
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
    /// 首次接受，返回 relay_seq。
    Accepted {
        /// 分配的送达序号。
        relay_seq: u64,
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

    /// `POST P/updates`（≤256 项；整批一次鉴权，逐项结果）。
    fn submit_updates(
        &mut self,
        items: Vec<UpdateSubmission>,
    ) -> Result<Vec<UpdateOutcome>, SyncError>;

    /// `POST P/acks`。
    fn ack(&mut self, received_seq: u64, integrated_seq: u64, epoch: u64) -> Result<(), SyncError>;

    /// 查设备证书（验 `ObjectCommit` 用）。
    ///
    /// 真实 relay 目前没有该端点/控制链查询（ADR §5 `GET P/control` 尚未落客户端），
    /// HTTP 实现返回 [`SyncError::Unsupported`]；FakeRelay 提供证书。
    fn device_cert(&mut self, device_id: &DeviceId) -> Result<Option<DeviceCertV1>, SyncError>;
}
