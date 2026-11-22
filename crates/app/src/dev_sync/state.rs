//! dev-sync 的**纯状态机**：没有 GPUI、没有 IO、没有线程、没有时钟。
//!
//! ADR 0009 的 Phase 1 只有一条铁律：**面板看到什么必须能从状态推出来**。
//! 所以"单操作在途 / Busy 拒绝新命令 / Progress 可合并 / Completed 与 Stopped 不丢"
//! 全部落在本文件，`service.rs` 只负责线程与通道，`panel.rs` 只做投影。
//!
//! ## 状态语义
//!
//! | 状态 | 含义 |
//! | --- | --- |
//! | `Uninitialized` | 还没有任何操作 |
//! | `Setup` | 正在建/加入初始状态（切片 4） |
//! | `WaitingBackup` | Create 成功，等用户确认恢复码备份（ADR 0009「用户真实确认恢复码备份」） |
//! | `Ready` | 空闲可用 |
//! | `WaitingIME` / `Flushing` / `Reconciling` | 外部提交闸门的中间态（ADR 0009 草案；切片 2 接） |
//! | `Busy { op_id, kind }` | 有操作在途（**唯一**允许在途的操作） |
//! | `Paused { reason }` | 已取消/被闸门暂停 |
//! | `Failed { reason }` | 最近一次操作明确失败（面板显示原文） |
//! | `Closing` | 已请求 Stop，worker 正在收尾 |
//!
//! ## 事件纪律
//!
//! - **Progress 可合并**：同一 op 的进度只保留最新一条（面板只需要最新相位）；
//! - **Completed / Stopped 不丢**：逐条进邮箱，直到被 [`SyncState::pull_events`] 取走；
//! - **迟到事件丢弃**：超时后 runner 才返回的 `Completed`、重复的 `Stopped` 都不能
//!   再改状态（否则面板会把"超时失败"画成"成功"）。
//!
//! ## 关闭票据（`StopTicket` / `CloseState`）
//!
//! `request_close` 只等**明确的目标 durable 确认**，不等网络同步（ADR 0009）：
//!
//! 1. `Stop` 时若没有在途 op → `Ready`（没有待确认的目标）；
//! 2. 在途 op 完成且返回 `Ok` → `Ready`（本切片把 `Ok` 定义为"DurableAck 已确认"）；
//! 3. 在途 op 返回 `Err` 或 worker 在确认前停掉 → `Blocked(原因)`，**不假装 Ready**。

use std::fmt;

use menghunote_sync::http_relay::MemberView;
use menghunote_sync::DeviceId;

/// 一次操作/一张关闭票据的编号（进程内单调递增；**不是** durable 标识）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId(u64);

impl OpId {
    /// 未分配（只用于 runner 上报 Progress 时占位：worker 会统一改写成真实 op_id）。
    pub const UNASSIGNED: Self = Self(0);

    /// 构造（测试与 worker 用；业务代码不应自造）。
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// 原始值（日志/面板显示）。
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }

    /// 是否已分配（`UNASSIGNED` = 未分配）。
    #[must_use]
    pub const fn is_assigned(self) -> bool {
        self.0 != 0
    }
}

impl fmt::Display for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// 设备 ID 的短展示形式（前 8 位小写 hex；设备 ID 是公开标识，不含秘密）。
///
/// 面板与 worker 共用同一套展示（notice/进度/行内 id 都只显示这一段），避免任何
/// 位置把完整设备 id（更不用说公钥/证书）带进 UI 或日志。
#[must_use]
pub fn short_device_id(device: &DeviceId) -> String {
    menghunote_sync::ids::hex_of(device)
        .chars()
        .take(8)
        .collect()
}

/// 重复移除（目标此前已是 revoked、`changed=false`）时 worker 摘要里的固定标记。
///
/// `auth_rev` 只对**真变化**推进一次（ADR 0010 幂等语义），所以面板不能一律宣称
/// "认证已刷新"——两边靠这个常量对齐，避免任何一处写死字符串后各说各话。
pub const REVOKE_UNCHANGED_MARKER: &str = "认证未再次推进";

/// 面板/worker 能发的命令（ADR 0009 §数据结构与 API）。
///
/// `Cancel` / `Stop` 是**控制**命令，其余是**操作**命令；操作命令严格串行，
/// 同一时刻只允许一个在途（见 [`SyncState::try_begin`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// 新建身份 + 初始 checkpoint（切片 4）。
    Create,
    /// 用 vault_id + 恢复码加入（切片 4）。
    Join,
    /// 完整 push/pull/integrate/ACK 一轮（切片 4）。
    ///
    /// **必须**带 [`HeldGate`] 派发：它认定"有活动文档，闸门已 Held"。
    SyncOnce,
    /// ADR 0012 S2a：**可暂停的两阶段**同步（Prepare → 等授权 → Integrate → 等交付 →
    /// Finish）。
    ///
    /// 与 [`Self::SyncOnce`] 的差别：本命令在**网络阶段不持闸**（派发时没有
    /// `HeldGate`），worker 完成 push/pull 后发 [`Event::NeedGate`] 并停下等
    /// [`crate::dev_sync::service::SyncControl::GrantGate`]；UI 此时才短持闸并授权。
    /// 授权在 worker 侧复核 op/授权代际/head，然后才 `integrate`。
    SyncTwoPhase,
    /// 完整 push/pull/integrate/ACK 一轮，**没有 UI 会话**（无活动文档：空库/没打开笔记）。
    ///
    /// 与 [`Self::SyncOnce`] 是两条互斥路径：本命令**不需要也不接受** [`HeldGate`]
    /// （没有活动文档就没有闸门目标）。只有 `AppShell` 在判定"没有活动文档"时才允许
    /// 派发；同步期间宿主必须冻结结构写入（新建/重命名/删除/移动）。
    SyncOnceNoSession,
    /// 发布初始双 checkpoint 基线（切片 4）。
    PublishInitialPair,
    /// 读取一次设备列表（**只读快照**；ADR 0010 切片 2a-2）。
    ///
    /// 刷新只由命令触达（面板打开/同步完成/用户点刷新），没有任何后台轮询。
    ListDevices,
    /// 移除一台设备对 relay 的访问（**不轮换密钥**；ADR 0010 切片 2a-2）。
    RevokeDevice {
        /// 目标设备 ID（公开标识；本机/非 Active 目标由 UI + worker 双重拒绝）。
        target: DeviceId,
    },
    /// 轮换（Phase 2）。
    Rotate,
    /// 恢复/重试被暂停的操作（Phase 2）。
    Resume,
    /// 取消在途操作（本切片只记意图，见 [`InFlight::cancel_requested`]）。
    Cancel,
    /// 请求关闭 worker（关闭票据见 [`SyncState::poll_close`]）。
    Stop,
}

impl Command {
    /// 命令的种类（面板显示 / 状态机分类用；不含 op_id）。
    #[must_use]
    pub const fn kind(self) -> CommandKind {
        match self {
            Self::Create => CommandKind::Create,
            Self::Join => CommandKind::Join,
            Self::SyncOnce => CommandKind::SyncOnce,
            Self::SyncTwoPhase => CommandKind::SyncTwoPhase,
            Self::SyncOnceNoSession => CommandKind::SyncOnceNoSession,
            Self::PublishInitialPair => CommandKind::PublishInitialPair,
            Self::ListDevices => CommandKind::ListDevices,
            Self::RevokeDevice { .. } => CommandKind::RevokeDevice,
            Self::Rotate => CommandKind::Rotate,
            Self::Resume => CommandKind::Resume,
            Self::Cancel => CommandKind::Cancel,
            Self::Stop => CommandKind::Stop,
        }
    }

    /// 是不是控制命令（`Cancel` / `Stop`）。
    ///
    /// 控制命令不受"单操作在途"限制：`Stop` 必须在 busy 时也能发出去，否则关不掉。
    #[must_use]
    pub const fn is_control(self) -> bool {
        matches!(self, Self::Cancel | Self::Stop)
    }
}

/// 命令种类（`Command` 去掉"这次是第几号 op"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandKind {
    /// 见 [`Command::Create`]。
    Create,
    /// 见 [`Command::Join`]。
    Join,
    /// 见 [`Command::SyncOnce`]。
    SyncOnce,
    /// 见 [`Command::SyncTwoPhase`]。
    SyncTwoPhase,
    /// 见 [`Command::SyncOnceNoSession`]。
    SyncOnceNoSession,
    /// 见 [`Command::PublishInitialPair`]。
    PublishInitialPair,
    /// 见 [`Command::ListDevices`]。
    ListDevices,
    /// 见 [`Command::RevokeDevice`]。
    RevokeDevice,
    /// 见 [`Command::Rotate`]。
    Rotate,
    /// 见 [`Command::Resume`]。
    Resume,
    /// 见 [`Command::Cancel`]。
    Cancel,
    /// 见 [`Command::Stop`]。
    Stop,
}

impl CommandKind {
    /// 中文短标签（面板按钮/状态行用）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Create => "创建",
            Self::Join => "加入",
            Self::SyncOnce => "同步一次",
            Self::SyncTwoPhase => "同步一次（两阶段）",
            Self::SyncOnceNoSession => "同步一次（无会话）",
            Self::PublishInitialPair => "发布初始基线",
            Self::ListDevices => "刷新设备",
            Self::RevokeDevice => "移除设备",
            Self::Rotate => "轮换",
            Self::Resume => "恢复",
            Self::Cancel => "取消",
            Self::Stop => "停止",
        }
    }
}

/// 为什么拒绝一条新命令（[`SyncState::try_begin`] 的错误载荷）。
///
/// 面板必须能说清"为什么点不动"，不能只给一个"忙"字：
/// 是在途占着、正在关闭、还是 worker 已经停了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyReason {
    /// 已有操作在途（单操作串行）。
    InFlight,
    /// 已请求关闭（`Stop` 之后不再接新命令）。
    Closing,
    /// worker 已停止（不再接任何命令）。
    WorkerStopped,
    /// 没有在途操作，`Cancel` 没有目标。
    Idle,
    /// 操作 deadline 已过：worker 仍在外部写边界内，**不派发第二操作**（ADR 0012 S1）。
    Cancelling,
}

impl BusyReason {
    /// 中文说明（面板/日志用）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::InFlight => "已有操作在途（单操作串行）",
            Self::Closing => "正在关闭，不再接受新命令",
            Self::WorkerStopped => "worker 已停止",
            Self::Idle => "没有可取消的操作",
            Self::Cancelling => "已超时/取消，等待 worker 真实收尾（不派发第二操作）",
        }
    }
}

/// 命令被拒绝（ADR 0009 `try_command(Command) -> Result<OpId, Busy>`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy {
    /// 拒绝原因。
    pub reason: BusyReason,
    /// 当前在途/正在收尾的 op（若有）。
    pub current: Option<OpId>,
}

impl Busy {
    /// 构造。
    #[must_use]
    pub const fn new(reason: BusyReason, current: Option<OpId>) -> Self {
        Self { reason, current }
    }
}

impl fmt::Display for Busy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.current {
            Some(op) => write!(f, "{}（当前 {}）", self.reason.label(), op),
            None => write!(f, "{}", self.reason.label()),
        }
    }
}

impl std::error::Error for Busy {}

/// 关闭票据（`request_stop` 的返回值）。
///
/// 只对**同一次**关闭有效：新的 `Stop` 会递增 epoch，旧票据的 `poll_close` 一律
/// `Blocked`（防止拿旧票读出新关闭的 Pending/Ready）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopTicket {
    /// 关闭轮次（每次 `Stop` 递增）。
    epoch: u64,
    /// 这次 `Stop` 自己的 op（worker 会以 `Stopped { op_id }` 回执）。
    op_id: OpId,
}

impl StopTicket {
    /// 关闭轮次。
    #[must_use]
    pub const fn epoch(self) -> u64 {
        self.epoch
    }

    /// 这次关闭的 op id。
    #[must_use]
    pub const fn op_id(self) -> OpId {
        self.op_id
    }
}

/// 关闭门禁状态（ADR 0009 `poll_close(ticket) -> CloseState`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseState {
    /// 还等在途目标的 durable 确认。
    Pending,
    /// 明确目标都已确认 durable（或本来就没有待确认目标）：可以关。
    Ready,
    /// 无法确认（失败/worker 先停了）：**不假装 Ready**。
    Blocked(String),
}

impl CloseState {
    /// 是否已经可以关闭。
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// 是否存在阻断原因。
    #[must_use]
    pub fn blocked_reason(&self) -> Option<&str> {
        match self {
            Self::Blocked(reason) => Some(reason),
            Self::Pending | Self::Ready => None,
        }
    }

    /// 面板/日志文案。
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Pending => "等待 durable 确认".to_owned(),
            Self::Ready => "可关闭".to_owned(),
            Self::Blocked(reason) => format!("不可关闭：{reason}"),
        }
    }
}

/// 进度相位（本切片是抽象相位；切片 4 由真实 runner 填具体值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// 准备（读身份/校验参数）。
    Preparing,
    /// 传输（连接/handshake/收发）。
    Transport,
    /// push。
    Pushing,
    /// pull。
    Pulling,
    /// integrate。
    Integrating,
    /// 落盘与 durable 确认。
    Durability,
}

impl Phase {
    /// 中文短标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Preparing => "准备",
            Self::Transport => "传输",
            Self::Pushing => "推送",
            Self::Pulling => "拉取",
            Self::Integrating => "整合",
            Self::Durability => "落盘",
        }
    }
}

/// 两阶段同步的**阶段**（ADR 0012 S2a；比 `Phase` 更粗，专门给授权/交付状态机断言）。
///
/// ```text
/// Prepare -> NeedGate -> Integrate -> Integrated -> Finish -> Completed
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPhase {
    /// 已派发 Prepare：网络阶段，**不持闸**，UI 可编辑/切换。
    Prepare,
    /// Prepare 完成，worker 已停下等 `GrantGate`（UI 此时才短持闸）。
    NeedGate,
    /// 已发出授权，worker 正在 `integrate`（UI 已 Held）。
    Integrate,
    /// 集成完成，worker 等 UI `adopt` 并 `end` 后的 Finish（UI 仍 Held）。
    Integrated,
    /// UI 已交付并释放闸门，worker 正在收尾（push tail + ack）。
    Finish,
}

impl SyncPhase {
    /// 中文短标签（面板/报告用）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Prepare => "网络同步（不持闸）",
            Self::NeedGate => "等待持闸授权",
            Self::Integrate => "整合中（持闸）",
            Self::Integrated => "等待交付（持闸）",
            Self::Finish => "收尾（已解闸）",
        }
    }
}

/// 两阶段同步在服务快照里的可见状态（授权绑定 + 阶段断言用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwoPhaseStatus {
    /// 这次两阶段操作。
    pub op_id: OpId,
    /// 当前阶段。
    pub phase: SyncPhase,
    /// worker 在 `NeedGate` 里给出的授权代际（`GrantGate` 必须原样带回）。
    pub authorization_epoch: Option<u64>,
    /// 集成阶段报告里"活动文档 head 变了"的 heads（UI 交付依据；可空）。
    pub changed_heads: Vec<String>,
    /// `NeedGate` 上报的 durable 已接收数（"已接收·待应用"）。
    pub stored: usize,
    /// `NeedGate`/`Integrated` 上报的仍待应用数（真实 deferred，不跨它提前 ACK）。
    pub deferred: usize,
    /// `Integrated` 上报的本轮已应用对象数（续批的"有进展"判据之一）。
    pub integrated: usize,
}

/// 操作的终态结果。
///
/// 注意：`Completed` 事件里的失败**也是终态**（操作结束了，只是没成功）；"未实现"
/// 与"网络失败"都走 `Failed`，面板显示原文，**绝不谎报成功**（ADR 0009「安全暂停优于
/// 看起来成功」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpResult {
    /// 成功（`summary` 是给面板看的简短结果，不含 secret）。
    Ok {
        /// 简短结果。
        summary: String,
    },
    /// 明确失败（`message` 可直接显示，不含 secret）。
    Failed {
        /// 失败原因。
        message: String,
    },
}

impl OpResult {
    /// 是否成功。
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }

    /// 失败原因（成功时为 `None`）。
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Ok { .. } => None,
            Self::Failed { message } => Some(message),
        }
    }

    /// 给面板看的单行文案。
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Ok { summary } => format!("成功：{summary}"),
            Self::Failed { message } => format!("失败：{message}"),
        }
    }
}

/// 派发 `SyncOnce` 时随命令一起交给 worker 的**持闸证明**（ADR 0009 切片 4c-2）。
///
/// 为什么需要它：worker 与 UI 共用同一个 SQLite，`engine.integrate` 会**绕过本地会话**
/// 直接追加外部行；UI 只有在外部提交闸门 `Held` 之后才允它跑。所以 worker 不接收
/// "裸的 SyncOnce"——命令必须带上这张证明，worker 在 integrate 之前复核：
///
/// 1. 证明存在（UI 确实走到过 `Held`，而不是盲发命令）；
/// 2. 库中该文档的 `head_seq` 恰好等于持闸时的 `durable_head_seq`（UI 的冻结基准没有
///    被移动过）。
///
/// 任一条不满足就**明确失败**，不进入网络/integrate（"安全暂停优于看起来成功"）。
///
/// 这是 dev-only 的 in-process 协调：证明由 UI 线程从真实的 [`ExternalCommitLease`]
/// 构造，worker 只读；不跨进程、不上 wire。真正的并发正确性仍由 storage 的闸门
/// （Held 期间本地写入 fail-closed）保证。
///
/// [`ExternalCommitLease`]: menghunote_storage::session::ExternalCommitLease
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldGate {
    /// 持闸的文档（活动文档的 body doc id）。
    pub doc_id: String,
    /// 持闸时已 durable 的 head seq（外部提交的 CAS 基准）。
    pub durable_head_seq: u64,
    /// 闸门代际（旧路径的诊断字段；两阶段路径里也作为授权代际的一部分）。
    pub generation: u64,
    /// 会话纪元（来自 `ExternalCommitLease::session_epoch`）。
    ///
    /// 两阶段授权把它与 `op_id + 授权代际 + durable head` 一起绑定：换文档 / 重开会话
    /// 后旧租约的 epoch 对不上，UI 必须重新 `begin_external_commit`（ADR 0012）。
    pub session_epoch: u64,
}

impl HeldGate {
    /// 构造（调用方必须来自真实的 `ExternalCommitLease`，不得自造）。
    ///
    /// 旧调用点（旧持闸路径/测试）没有 session epoch：默认 `0`；新路径请用
    /// [`Self::with_session_epoch`] 带上真实纪元。
    #[must_use]
    pub fn new(doc_id: impl Into<String>, durable_head_seq: u64, generation: u64) -> Self {
        Self {
            doc_id: doc_id.into(),
            durable_head_seq,
            generation,
            session_epoch: 0,
        }
    }

    /// 补上会话纪元（两阶段授权必须带真实值）。
    #[must_use]
    pub fn with_session_epoch(mut self, session_epoch: u64) -> Self {
        self.session_epoch = session_epoch;
        self
    }
}

/// 一次**成功**操作的产物。
///
/// 返回 `Ok` 即表示"目标 head 已确认 durable"（本切片用抽象事件模拟 DurableAck；
/// 切片 4 由真实 `SyncEngine` 的 ACK 填充）。失败/未实现一律 `Err(String)`，
/// worker 会把它翻成 [`Event::Completed`] 的 `OpResult::Failed`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpOutcome {
    /// 本次操作改动并已 durable 的 head（面板展示 + 关闭票据的确认依据）。
    pub changed_heads: Vec<String>,
    /// 简短结果（不含 secret）。
    pub summary: String,
}

impl OpOutcome {
    /// 空产物（没有 head 变化）。
    #[must_use]
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            changed_heads: Vec::new(),
            summary: summary.into(),
        }
    }

    /// 带上 changed heads。
    #[must_use]
    pub fn with_heads(mut self, heads: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.changed_heads = heads.into_iter().map(Into::into).collect();
        self
    }
}

/// worker → 面板的事件（ADR 0009 `Event`）。
///
/// 只能在 `service.rs` 的 worker 线程里构造；`op_id` 由 worker 统一盖章，
/// runner 上报 `Progress` 时用 [`OpId::UNASSIGNED`] 占位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// 进度（可合并）。
    Progress {
        /// 哪次操作。
        op_id: OpId,
        /// 相位。
        phase: Phase,
        /// 明细（不得含 secret/HTTP body）。
        detail: String,
    },
    /// 操作 deadline 已过：**不是终态**（ADR 0012 S1）。
    ///
    /// 看门狗只能产生这条事件，不能伪造可解闸的 `Completed`：worker 仍可能正在
    /// `integrate`，UI 必须保持 Held，等真实 `Completed`/`Stopped` 回执。
    DeadlineExceeded {
        /// 哪次操作。
        op_id: OpId,
        /// 供面板展示的原因（不含 secret）。
        message: String,
    },
    /// 两阶段同步的 Prepare 完成：worker 已停下，等 UI 持闸并回 `GrantGate`。
    ///
    /// **不是终态**：单操作位仍被占着，UI 必须先在**当前**活动文档上
    /// `begin_external_commit` 拿到 `Held`，再发授权。
    NeedGate {
        /// 哪次操作。
        op_id: OpId,
        /// worker 给出的授权代际：`GrantGate` 必须原样带回（旧授权 = 被拒绝）。
        authorization_epoch: u64,
        /// Prepare 拉取并落成 durable inbox 的事件数（诊断；不含正文）。
        stored: usize,
        /// 因依赖未齐等保持 durable 的对象数（Prepare 阶段通常为 0）。
        deferred: usize,
        /// 给面板看的中文摘要（不得含 secret）。
        summary: String,
    },
    /// 两阶段同步的 Integrate 完成：durable 变化已落库，等 UI 交付后发 Finish。
    ///
    /// **不是终态**：worker 仍占着单操作位并持有本轮授权，UI 必须
    /// `prepare_committed_delta → adopt → end` 之后才允许它 ack。
    Integrated {
        /// 哪次操作。
        op_id: OpId,
        /// 活动文档 head 前进了时包含它（UI 据此断言交付）。
        changed_heads: Vec<String>,
        /// 本轮 integrated 的对象数。
        integrated: usize,
        /// 仍 deferred 的对象数（S2b 的公平续批；本片不做第二轮联网补）。
        deferred: usize,
        /// 给面板看的中文摘要（不得含 secret）。
        summary: String,
    },
    /// 操作结束（成功或失败都算；**不丢**）。
    Completed {
        /// 哪次操作。
        op_id: OpId,
        /// 本次改动/确认的 head。
        changed_heads: Vec<String>,
        /// 终态结果。
        result: OpResult,
    },
    /// worker 已停止（`Stop` 的回执；**不丢**）。
    Stopped {
        /// 这次 `Stop` 的 op id。
        op_id: OpId,
    },
    /// 设备列表快照（ADR 0010 切片 2a-2；**只读投影**，不含 token/密钥）。
    ///
    /// `self_device` / `can_manage` 由 worker 从**受保护身份**读出后一并上报：
    /// UI 线程绝不读身份文件，所以"哪一行是本机""能不能移除"只能由 worker 告诉面板
    /// （两者都只是公开事实：设备 ID 是公开标识，管理密钥有无是一个布尔能力）。
    DevicesListed {
        /// 已验证控制链的成员视图（已按 `device_id` 稳定升序）。
        members: Vec<MemberView>,
        /// 本机设备 ID（面板据此标「本机」并禁止移除该行）。
        self_device: DeviceId,
        /// 本机身份是否持有管理密钥（Join 设备 = `false`，移除按钮置灰）。
        can_manage: bool,
    },
    /// 一次移除设备拿到了 relay 的**明确成功**事实（ADR 0010 切片 2a-2）。
    ///
    /// 失败不走本事件：worker 返回 `Err`，由 `Completed` 的 `OpResult::Failed`
    /// 呈现在 `dev-sync-error`（面板不把失败写成"已移除"）。
    DeviceRevoked {
        /// 被移除的目标设备。
        target: DeviceId,
        /// relay 事实摘要（`changed`/`auth_rev`；成功才发）。
        result: OpResult,
    },
}

impl Event {
    /// 事件属于哪次 op。
    ///
    /// 设备列表/移除事实是**领域事件**（不带 op 标签），统一返回
    /// [`OpId::UNASSIGNED`]；它们不属于任何在途 op 的进度/终态。
    #[must_use]
    pub const fn op_id(&self) -> OpId {
        match self {
            Self::Progress { op_id, .. }
            | Self::DeadlineExceeded { op_id, .. }
            | Self::NeedGate { op_id, .. }
            | Self::Integrated { op_id, .. }
            | Self::Completed { op_id, .. }
            | Self::Stopped { op_id } => *op_id,
            Self::DevicesListed { .. } | Self::DeviceRevoked { .. } => OpId::UNASSIGNED,
        }
    }

    /// 是否是终态事件（`Completed` / `Stopped`）。
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Stopped { .. })
    }

    /// 把 `Progress` 的 `op_id` 改写成 worker 的真实 op（runner 用 `UNASSIGNED` 占位）。
    ///
    /// 终态事件不改写：它们只能由 worker 自己生成，跑错 op 就是 bug，宁可留着被丢弃。
    /// 领域事件（设备列表/移除事实）原样返回。
    #[must_use]
    pub fn stamped(self, op_id: OpId) -> Self {
        match self {
            Self::Progress {
                phase,
                detail,
                op_id: _,
            } => Self::Progress {
                op_id,
                phase,
                detail,
            },
            Self::NeedGate {
                op_id: _,
                authorization_epoch,
                stored,
                deferred,
                summary,
            } => Self::NeedGate {
                op_id,
                authorization_epoch,
                stored,
                deferred,
                summary,
            },
            Self::Integrated {
                op_id: _,
                changed_heads,
                integrated,
                deferred,
                summary,
            } => Self::Integrated {
                op_id,
                changed_heads,
                integrated,
                deferred,
                summary,
            },
            other => other,
        }
    }
}

/// 面板可见的状态（ADR 0009 `DevSyncState`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevSyncState {
    /// 还没有任何操作。
    Uninitialized,
    /// 建/加入初始状态中。
    Setup,
    /// Create 成功，等用户确认恢复码备份。
    WaitingBackup,
    /// 空闲可用。
    Ready,
    /// 等输入法自然结束（外部提交闸门，切片 2）。
    WaitingIME,
    /// 等 durable flush（外部提交闸门，切片 2）。
    Flushing,
    /// 有操作在途。
    Busy {
        /// 在途 op。
        op_id: OpId,
        /// 在途命令种类。
        kind: CommandKind,
    },
    /// 对账中（外部提交闸门，切片 2）。
    Reconciling,
    /// 操作 deadline 已过 / 用户取消已登记：worker 仍在外部写边界内，**等真实收尾**。
    ///
    /// 不是终态：单操作位仍被占着，新命令一律 `Busy`；UI **不得**解闸（ADR 0012 S1）。
    Cancelling {
        /// 在途 op。
        op_id: OpId,
    },
    /// 已取消/被闸门暂停。
    Paused {
        /// 暂停原因。
        reason: String,
    },
    /// 最近一次操作明确失败。
    Failed {
        /// 失败原因（面板显示原文）。
        reason: String,
    },
    /// 已请求关闭。
    Closing,
}

impl DevSyncState {
    /// 是否有操作在途。
    #[must_use]
    pub const fn is_busy(&self) -> bool {
        // `Cancelling` 仍然占着单操作位（worker 没有停止，随时可能写外部行）。
        matches!(self, Self::Busy { .. } | Self::Cancelling { .. })
    }

    /// 在途 op（若有）。
    #[must_use]
    pub const fn in_flight_op(&self) -> Option<OpId> {
        match self {
            Self::Busy { op_id, .. } => Some(*op_id),
            _ => None,
        }
    }

    /// 面板状态行文案。
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Uninitialized => "未初始化".to_owned(),
            Self::Setup => "初始化中".to_owned(),
            Self::WaitingBackup => "等待恢复码备份确认".to_owned(),
            Self::Ready => "就绪".to_owned(),
            Self::WaitingIME => "等待输入法结束".to_owned(),
            Self::Flushing => "等待落盘".to_owned(),
            Self::Busy { op_id, kind } => format!("执行中：{} {op_id}", kind.label()),
            Self::Reconciling => "对账中".to_owned(),
            Self::Cancelling { op_id } => {
                format!("取消中：等待 worker 真实收尾（{op_id}，闸门保持持有）")
            }
            Self::Paused { reason } => format!("已暂停：{reason}"),
            Self::Failed { reason } => format!("失败：{reason}"),
            Self::Closing => "关闭中".to_owned(),
        }
    }
}

impl fmt::Display for DevSyncState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

/// 面板的一次只读快照（`panel.rs` 只吃这个，不直接碰内部字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSnapshot {
    /// 当前状态。
    pub state: DevSyncState,
    /// 在途 op 与命令种类。
    pub in_flight: Option<(OpId, CommandKind)>,
    /// 最新进度（相位 + 明细）。
    pub progress: Option<(Phase, String)>,
    /// 最近一次终态结果。
    pub last_completed: Option<(OpId, Vec<String>, OpResult)>,
    /// 最近一次错误（成功后清空）。
    pub last_error: Option<String>,
    /// 关闭门禁状态。
    pub close: CloseState,
    /// worker 是否已停止。
    pub worker_stopped: bool,
    /// 已过 deadline 但还没拿到真实终态的 op（`Some` = 只能等，不能解闸）。
    pub deadline_exceeded: Option<OpId>,
    /// 在途 op 是否已被取消（`Cancel`）：两阶段 UI 不得再发起新的授权。
    pub cancel_requested: bool,
    /// 两阶段同步的可见阶段（`None` = 不是两阶段操作）。
    pub two_phase: Option<TwoPhaseStatus>,
    /// 设备列表（只读快照；空 = 还没成功读到过，见 ADR 0010 切片 2a-2）。
    pub devices: Vec<MemberView>,
    /// 本机设备 ID（`None` = worker 还没上报过身份）。
    pub self_device: Option<DeviceId>,
    /// 本机身份是否持有管理密钥（`false` = 移除按钮必须置灰）。
    pub can_manage_devices: bool,
    /// 最近一次**成功**移除的设备与 relay 事实摘要（面板 notice 的来源）。
    pub last_revoked: Option<(DeviceId, OpResult)>,
}

/// 在途操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InFlight {
    op_id: OpId,
    kind: CommandKind,
    /// 已请求取消（本切片的执行体不可中断，只在结束时体现，见 [`SyncState::apply`]）。
    cancel_requested: bool,
}

/// 事件邮箱：`Progress` 合并、`Completed`/`Stopped` 逐条保留。
#[derive(Debug, Default)]
struct Mailbox {
    events: Vec<Event>,
}

impl Mailbox {
    /// 入队。同一 op 的 `Progress` 只留最新一条（面板只需要最新相位）。
    fn push(&mut self, event: Event) {
        match event {
            Event::Progress {
                op_id,
                phase,
                detail,
            } => {
                if let Some(Event::Progress {
                    op_id: previous,
                    phase: previous_phase,
                    detail: previous_detail,
                }) = self.events.last_mut()
                {
                    if *previous == op_id {
                        *previous_phase = phase;
                        *previous_detail = detail;
                        return;
                    }
                }
                self.events.push(Event::Progress {
                    op_id,
                    phase,
                    detail,
                });
            }
            terminal => self.events.push(terminal),
        }
    }

    fn drain(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
}

/// dev-sync 状态机（纯数据；`SyncService` 用 `Mutex` 包着它）。
#[derive(Debug)]
pub struct SyncState {
    state: DevSyncState,
    in_flight: Option<InFlight>,
    progress: Option<(Phase, String)>,
    mailbox: Mailbox,
    last_completed: Option<(OpId, Vec<String>, OpResult)>,
    last_error: Option<String>,
    close: CloseState,
    /// 本次关闭要等的在途 op（`None` = 没有待确认目标）。
    close_target: Option<OpId>,
    close_epoch: u64,
    /// 最近一张关闭票据（`Stopped` 之后仍然可查，供重复 `request_stop` 复用）。
    last_ticket: Option<StopTicket>,
    /// 已接受 `Stop` 但还没回执的 op。
    stop_op: Option<OpId>,
    worker_stopped: bool,
    /// 已过 deadline、还没有真实终态的 op（ADR 0012 S1：迟到回执不能丢）。
    deadline_exceeded: Option<OpId>,
    /// 两阶段同步的可见阶段（ADR 0012 S2a）。
    two_phase: Option<TwoPhaseStatus>,
    next_op: u64,
    /// 设备列表（只读快照；ADR 0010 切片 2a-2）。
    devices: Vec<MemberView>,
    /// 本机设备 ID（worker 上报的身份事实）。
    self_device: Option<DeviceId>,
    /// 本机是否有管理密钥（worker 上报的能力位）。
    can_manage_devices: bool,
    /// 最近一次成功移除（面板 notice 的来源；失败走 `last_error`）。
    last_revoked: Option<(DeviceId, OpResult)>,
}

impl Default for SyncState {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncState {
    /// 初始状态：`Uninitialized`，没有票据、没有在途。
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: DevSyncState::Uninitialized,
            in_flight: None,
            progress: None,
            mailbox: Mailbox::default(),
            last_completed: None,
            last_error: None,
            close: CloseState::Pending,
            close_target: None,
            close_epoch: 0,
            last_ticket: None,
            stop_op: None,
            worker_stopped: false,
            deadline_exceeded: None,
            two_phase: None,
            next_op: 1,
            devices: Vec::new(),
            self_device: None,
            can_manage_devices: false,
            last_revoked: None,
        }
    }

    /// 当前状态。
    #[must_use]
    pub const fn state(&self) -> &DevSyncState {
        &self.state
    }

    /// 是否有操作在途。
    #[must_use]
    pub const fn is_busy(&self) -> bool {
        self.in_flight.is_some()
    }

    /// 在途 op（若有；`Cancelling` 也算在途，worker 仍可能写外部行）。
    #[must_use]
    pub const fn in_flight_op(&self) -> Option<OpId> {
        match self.in_flight {
            Some(in_flight) => Some(in_flight.op_id),
            None => None,
        }
    }

    /// worker 是否已停止。
    #[must_use]
    pub const fn worker_stopped(&self) -> bool {
        self.worker_stopped
    }

    /// 两阶段同步的当前阶段（`None` = 不是两阶段操作，或已结束）。
    #[must_use]
    pub fn two_phase(&self) -> Option<&TwoPhaseStatus> {
        self.two_phase.as_ref()
    }

    /// worker 在 `NeedGate` 里给出的授权代际（`GrantGate` 必须原样带回）。
    #[must_use]
    pub fn two_phase_epoch(&self) -> Option<u64> {
        self.two_phase
            .as_ref()
            .and_then(|status| status.authorization_epoch)
    }

    /// 记录"UI 已发出 `GrantGate`"：阶段 `NeedGate → Integrate`。
    ///
    /// 返回 `false` = 当前不是该 op 的 `NeedGate` 阶段（过期/伪造的授权，必须拒绝）。
    pub fn note_grant_sent(&mut self, op_id: OpId) -> bool {
        let Some(status) = self.two_phase.as_mut() else {
            return false;
        };
        if status.op_id != op_id || status.phase != SyncPhase::NeedGate {
            return false;
        }
        status.phase = SyncPhase::Integrate;
        true
    }

    /// 授权消息**发送失败**：把刚推进到 `Integrate` 的阶段退回 `NeedGate`（可重试）。
    ///
    /// 只对同一个 op 的 `Integrate` 阶段生效；worker 没有收到授权，绝不会 integrate。
    pub fn note_grant_failed(&mut self, op_id: OpId) {
        if let Some(status) = self.two_phase.as_mut() {
            if status.op_id == op_id && status.phase == SyncPhase::Integrate {
                status.phase = SyncPhase::NeedGate;
            }
        }
    }

    /// 记录"UI 已发出 `Finish`"：阶段 `Integrated → Finish`。
    ///
    /// 返回 `false` = 当前不是该 op 的 `Integrated` 阶段（授权/交付顺序错误）。
    pub fn note_finish_sent(&mut self, op_id: OpId) -> bool {
        let Some(status) = self.two_phase.as_mut() else {
            return false;
        };
        if status.op_id != op_id || status.phase != SyncPhase::Integrated {
            return false;
        }
        status.phase = SyncPhase::Finish;
        true
    }

    /// 只读快照（面板用）。
    #[must_use]
    pub fn snapshot(&self) -> StateSnapshot {
        StateSnapshot {
            state: self.state.clone(),
            in_flight: self
                .in_flight
                .map(|in_flight| (in_flight.op_id, in_flight.kind)),
            progress: self.progress.clone(),
            last_completed: self.last_completed.clone(),
            last_error: self.last_error.clone(),
            close: self.close.clone(),
            worker_stopped: self.worker_stopped,
            deadline_exceeded: self.deadline_exceeded,
            cancel_requested: self
                .in_flight
                .is_some_and(|in_flight| in_flight.cancel_requested),
            two_phase: self.two_phase.clone(),
            devices: self.devices.clone(),
            self_device: self.self_device,
            can_manage_devices: self.can_manage_devices,
            last_revoked: self.last_revoked.clone(),
        }
    }

    /// 取走邮箱里的事件（`Progress` 已合并，`Completed`/`Stopped` 逐条）。
    #[must_use]
    pub fn pull_events(&mut self) -> Vec<Event> {
        self.mailbox.drain()
    }

    /// 分配一个 op id（只在 [`SyncState::try_begin`] 内用）。
    fn alloc(&mut self) -> OpId {
        let op_id = OpId(self.next_op);
        self.next_op += 1;
        op_id
    }

    /// 尝试开始一条命令（ADR 0009 `try_command` 的状态机部分）。
    ///
    /// 规则：
    /// - **操作命令**：已有在途 / 正在关闭 / worker 已停 → `Err(Busy)`；
    /// - **`Stop`**：busy 时也接受（否则关不掉），但正在关闭/已停 → `Err(Busy)`；
    /// - **`Cancel`**：只在 busy 时有意义（返回**在途**的 op id），空闲 → `Err(Busy{Idle})`。
    pub fn try_begin(&mut self, command: Command) -> Result<OpId, Busy> {
        let current = self.in_flight.map(|in_flight| in_flight.op_id);
        if self.worker_stopped {
            return Err(Busy::new(BusyReason::WorkerStopped, current));
        }
        match command {
            Command::Stop => {
                if matches!(self.state, DevSyncState::Closing) || self.stop_op.is_some() {
                    return Err(Busy::new(BusyReason::Closing, current));
                }
                let op_id = self.alloc();
                self.stop_op = Some(op_id);
                self.close_epoch += 1;
                self.last_ticket = Some(StopTicket {
                    epoch: self.close_epoch,
                    op_id,
                });
                self.close_target = current;
                self.close = if self.close_target.is_some() {
                    CloseState::Pending
                } else {
                    // 没有在途 op = 没有待确认的目标（本切片把"已 durable"当作既有事实）。
                    CloseState::Ready
                };
                self.state = DevSyncState::Closing;
                Ok(op_id)
            }
            Command::Cancel => match self.in_flight.as_mut() {
                Some(in_flight) => {
                    let op_id = in_flight.op_id;
                    in_flight.cancel_requested = true;
                    Ok(op_id)
                }
                None => Err(Busy::new(BusyReason::Idle, None)),
            },
            operation => {
                if matches!(self.state, DevSyncState::Closing) {
                    return Err(Busy::new(BusyReason::Closing, current));
                }
                if let Some(in_flight) = self.in_flight {
                    return Err(Busy::new(BusyReason::InFlight, Some(in_flight.op_id)));
                }
                let op_id = self.alloc();
                self.in_flight = Some(InFlight {
                    op_id,
                    kind: operation.kind(),
                    cancel_requested: false,
                });
                self.progress = None;
                // 两阶段同步从 Prepare 起就有可见阶段（网络阶段：不持闸）。
                self.two_phase =
                    (operation.kind() == CommandKind::SyncTwoPhase).then_some(TwoPhaseStatus {
                        op_id,
                        phase: SyncPhase::Prepare,
                        authorization_epoch: None,
                        changed_heads: Vec::new(),
                        stored: 0,
                        deferred: 0,
                        integrated: 0,
                    });
                self.state = DevSyncState::Busy {
                    op_id,
                    kind: operation.kind(),
                };
                Ok(op_id)
            }
        }
    }

    /// 应用一条 worker 事件。
    ///
    /// 迟到的 `Completed`（超时后才跑完）、重复的 `Stopped`、别人的 `Progress` 一律丢弃：
    /// 它们不能再改状态，否则面板会把"失败/关闭"画成"成功"。
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Progress {
                op_id,
                phase,
                detail,
            } => {
                let watched = self
                    .in_flight
                    .map(|in_flight| in_flight.op_id)
                    .or(self.stop_op);
                if watched != Some(op_id) {
                    return;
                }
                self.progress = Some((phase, detail.clone()));
                self.mailbox.push(Event::Progress {
                    op_id,
                    phase,
                    detail,
                });
            }
            Event::NeedGate {
                op_id,
                authorization_epoch,
                stored,
                deferred,
                summary,
            } => {
                let Some(in_flight) = self.in_flight else {
                    return;
                };
                if in_flight.op_id != op_id || in_flight.kind != CommandKind::SyncTwoPhase {
                    // 不是两阶段操作/不是本次 op：不污染状态（但仍原样丢进邮箱会让
                    // 调用方看到一条无主事件，所以直接丢弃）。
                    return;
                }
                self.two_phase = Some(TwoPhaseStatus {
                    op_id,
                    phase: SyncPhase::NeedGate,
                    authorization_epoch: Some(authorization_epoch),
                    changed_heads: Vec::new(),
                    stored,
                    deferred,
                    integrated: 0,
                });
                self.mailbox.push(Event::NeedGate {
                    op_id,
                    authorization_epoch,
                    stored,
                    deferred,
                    summary,
                });
            }
            Event::Integrated {
                op_id,
                changed_heads,
                integrated,
                deferred,
                summary,
            } => {
                let Some(in_flight) = self.in_flight else {
                    return;
                };
                if in_flight.op_id != op_id || in_flight.kind != CommandKind::SyncTwoPhase {
                    return;
                }
                let (epoch, stored) = self.two_phase.as_ref().map_or((None, 0), |status| {
                    (status.authorization_epoch, status.stored)
                });
                self.two_phase = Some(TwoPhaseStatus {
                    op_id,
                    phase: SyncPhase::Integrated,
                    authorization_epoch: epoch,
                    changed_heads: changed_heads.clone(),
                    stored,
                    deferred,
                    integrated,
                });
                self.mailbox.push(Event::Integrated {
                    op_id,
                    changed_heads,
                    integrated,
                    deferred,
                    summary,
                });
            }
            Event::DeadlineExceeded { op_id, message } => {
                // 只对在途 op 生效；重复的超时事件不重复通知。
                let Some(in_flight) = self.in_flight else {
                    return;
                };
                if in_flight.op_id != op_id || self.deadline_exceeded == Some(op_id) {
                    return;
                }
                // **不是终态**：in_flight 保持占位，新命令一律 Busy；UI 必须继续 Held。
                self.deadline_exceeded = Some(op_id);
                self.last_error = Some(message.clone());
                self.state = DevSyncState::Cancelling { op_id };
                self.mailbox
                    .push(Event::DeadlineExceeded { op_id, message });
            }
            Event::Completed {
                op_id,
                changed_heads,
                mut result,
            } => {
                let Some(in_flight) = self.in_flight else {
                    // 没有任何在途 op：这条回执无处安放（例如 worker 停止后才到）。
                    // 不能改状态，但也不能声称它"成功"——面板的 last_error 保留原文。
                    return;
                };
                if in_flight.op_id != op_id {
                    return;
                }
                // 超时后才到的**真实**回执不能丢：记录并注明"超时后真实收尾"，
                // 这样面板不会把超时错误当成最终结论（ADR 0012 S1）。
                let late_after_deadline = self.deadline_exceeded == Some(op_id);
                if late_after_deadline {
                    result = match result {
                        OpResult::Ok { summary } => OpResult::Ok {
                            summary: format!("{summary}（超时后真实收尾，未丢弃）"),
                        },
                        OpResult::Failed { message } => OpResult::Failed {
                            message: format!("{message}（超时后真实收尾，未丢弃）"),
                        },
                    };
                    self.deadline_exceeded = None;
                }
                self.in_flight = None;
                self.progress = None;
                self.two_phase = None;
                self.settle_close(op_id, &result);
                match &result {
                    OpResult::Ok { summary } => {
                        self.last_error = None;
                        if in_flight.cancel_requested {
                            // 本切片的 runner 是同步调用，取消不能真正打断：
                            // 只把"用户按过取消"如实呈现在状态里，不谎称已撤销结果。
                            self.state = DevSyncState::Paused {
                                reason: "已请求取消（执行体不可中断，操作已结束）".to_owned(),
                            };
                        } else {
                            self.state = match in_flight.kind {
                                CommandKind::Create => DevSyncState::WaitingBackup,
                                CommandKind::Stop => DevSyncState::Closing,
                                _ => DevSyncState::Ready,
                            };
                        }
                        let _ = summary;
                    }
                    OpResult::Failed { message } => {
                        self.last_error = Some(message.clone());
                        self.state = DevSyncState::Failed {
                            reason: message.clone(),
                        };
                    }
                }
                self.last_completed = Some((op_id, changed_heads.clone(), result.clone()));
                self.mailbox.push(Event::Completed {
                    op_id,
                    changed_heads,
                    result,
                });
            }
            Event::Stopped { op_id } => {
                if self.worker_stopped {
                    return;
                }
                // 不是这次 `Stop` 的回执：丢弃（防止迟到/伪造的 Stopped 关掉新会话）。
                if self.stop_op.is_some_and(|stop| stop != op_id) {
                    return;
                }
                self.worker_stopped = true;
                self.in_flight = None;
                self.progress = None;
                self.two_phase = None;
                self.stop_op = None;
                // worker 已停止 = "不会再有未来外部写"的可靠回执；超时等待到此为止。
                self.deadline_exceeded = None;
                if self.close_target.is_some() {
                    // 在途 op 的 durable 确认没等到就让 worker 停了：宁可 Blocked。
                    self.close = CloseState::Blocked(
                        "worker 已停止，在途操作的 durable 确认未到达".to_owned(),
                    );
                    self.close_target = None;
                }
                self.state = DevSyncState::Closing;
                self.mailbox.push(Event::Stopped { op_id });
            }
            // 设备列表/移除事实是**只读领域事件**：不改状态机（在途/关闭/结果都不受
            // 影响），只更新面板投影并进邮箱。它们不像 Progress 那样合并——面板需要
            // 看到"列表真的刷新过"这条事实（`Debug` 也不含密钥）。
            Event::DevicesListed {
                members,
                self_device,
                can_manage,
            } => {
                self.devices = members.clone();
                self.self_device = Some(self_device);
                self.can_manage_devices = can_manage;
                self.mailbox.push(Event::DevicesListed {
                    members,
                    self_device,
                    can_manage,
                });
            }
            Event::DeviceRevoked { target, result } => {
                // 只保留成功事实：失败由 `Completed(Failed)` 走 `last_error`。
                if result.is_ok() {
                    self.last_revoked = Some((target, result.clone()));
                }
                self.mailbox.push(Event::DeviceRevoked { target, result });
            }
        }
    }

    /// 在途 op 结束时结算关闭门禁（只对本次关闭的目标 op 生效）。
    fn settle_close(&mut self, op_id: OpId, result: &OpResult) {
        if self.close_target != Some(op_id) {
            return;
        }
        self.close_target = None;
        self.close = match result {
            OpResult::Ok { .. } => CloseState::Ready,
            OpResult::Failed { message } => {
                CloseState::Blocked(format!("目标 durable 未确认：{message}"))
            }
        };
    }

    /// 派发失败（worker 通道已关）时回滚在途状态并留下明确错误。
    pub fn abort_dispatch(&mut self, op_id: OpId, reason: &str) {
        if self
            .in_flight
            .is_some_and(|in_flight| in_flight.op_id == op_id)
        {
            self.in_flight = None;
        }
        if self.stop_op == Some(op_id) {
            self.stop_op = None;
            self.close = CloseState::Blocked(reason.to_owned());
        }
        self.worker_stopped = true;
        self.deadline_exceeded = None;
        self.last_error = Some(reason.to_owned());
        self.state = DevSyncState::Failed {
            reason: reason.to_owned(),
        };
    }

    /// 最近一张关闭票据（`request_stop` 重复调用时复用）。
    #[must_use]
    pub const fn last_ticket(&self) -> Option<StopTicket> {
        self.last_ticket
    }

    /// 取本次（或上一次）关闭的票据；从未发起过关闭时**分配一张新票据**。
    ///
    /// 为什么需要这条：`request_stop` 在"worker 已经停了/状态机拒绝 `Stop`"时仍必须给
    /// 调用方一张可查询的票（否则面板拿不到 `Ready`/`Blocked`，只能永远显示未知）。
    /// 分配票据本身不改在途状态；没有在途 op 时关闭判定直接 `Ready`（没有待确认目标）。
    pub fn stop_ticket(&mut self) -> StopTicket {
        if let Some(ticket) = self.last_ticket {
            return ticket;
        }
        let op_id = self.alloc();
        self.close_epoch += 1;
        let ticket = StopTicket {
            epoch: self.close_epoch,
            op_id,
        };
        self.last_ticket = Some(ticket);
        if self.in_flight.is_none() {
            self.close_target = None;
            self.close = CloseState::Ready;
        }
        ticket
    }

    /// 查关闭门禁（ADR 0009 `poll_close`）。
    ///
    /// 票据只对**同一次**关闭有效：epoch 不匹配直接 `Blocked`，不读新关闭的状态。
    #[must_use]
    pub fn poll_close(&self, ticket: &StopTicket) -> CloseState {
        if ticket.epoch != self.close_epoch || self.last_ticket != Some(*ticket) {
            return CloseState::Blocked("关闭票据已过期（已发起新的关闭流程）".to_owned());
        }
        self.close.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(summary: &str) -> OpResult {
        OpResult::Ok {
            summary: summary.to_owned(),
        }
    }

    #[test]
    fn busy_rejects_a_second_operation_but_busy_stop_is_accepted() {
        let mut state = SyncState::new();
        let first = state.try_begin(Command::Create).expect("第一条命令");
        let busy = state
            .try_begin(Command::SyncOnce)
            .expect_err("在途时必须拒绝");
        assert_eq!(
            busy,
            Busy::new(BusyReason::InFlight, Some(first)),
            "Busy 必须带上在途 op"
        );
        // Stop 是控制命令：busy 时也要能发出去。
        let stop = state
            .try_begin(Command::Stop)
            .expect("busy 时 Stop 必须放行");
        assert!(matches!(state.state(), DevSyncState::Closing));
        assert_eq!(
            state.poll_close(&state.last_ticket().expect("票")),
            CloseState::Pending
        );
        assert_eq!(
            state
                .try_begin(Command::Stop)
                .expect_err("重复 Stop")
                .reason,
            BusyReason::Closing
        );
        assert_eq!(stop, state.last_ticket().expect("票").op_id());
    }

    #[test]
    fn progress_is_coalesced_and_completed_events_survive() {
        let mut state = SyncState::new();
        let op = state.try_begin(Command::SyncOnce).expect("同步");
        for (phase, detail) in [
            (Phase::Preparing, "准备"),
            (Phase::Transport, "连接"),
            (Phase::Pushing, "推送 3 条"),
        ] {
            state.apply(Event::Progress {
                op_id: op,
                phase,
                detail: detail.to_owned(),
            });
        }
        // 迟到/伪造的进度（别人的 op）不能污染面板。
        state.apply(Event::Progress {
            op_id: OpId(op.value() + 100),
            phase: Phase::Durability,
            detail: "伪造".to_owned(),
        });
        state.apply(Event::Completed {
            op_id: op,
            changed_heads: vec!["head-a".to_owned()],
            result: ok("已同步"),
        });
        // 迟到/重复的 Completed 丢弃（不能把状态再改一遍）。
        state.apply(Event::Completed {
            op_id: op,
            changed_heads: Vec::new(),
            result: ok("重复"),
        });

        let events = state.pull_events();
        assert_eq!(
            events.len(),
            2,
            "3 条进度合并成 1 条 + 1 条 Completed：{events:?}"
        );
        assert_eq!(
            events[0],
            Event::Progress {
                op_id: op,
                phase: Phase::Pushing,
                detail: "推送 3 条".to_owned(),
            },
            "Progress 只保留最新一条"
        );
        assert!(matches!(
            events[1],
            Event::Completed {
                result: OpResult::Ok { .. },
                ..
            }
        ));
        assert_eq!(state.state(), &DevSyncState::Ready);
        assert!(state.pull_events().is_empty(), "取走后邮箱必须为空");
    }

    #[test]
    fn create_success_waits_for_backup_and_cancel_shows_paused() {
        let mut state = SyncState::new();
        let create = state.try_begin(Command::Create).expect("创建");
        state.apply(Event::Completed {
            op_id: create,
            changed_heads: vec!["head-1".to_owned()],
            result: ok("身份已建"),
        });
        assert_eq!(state.state(), &DevSyncState::WaitingBackup);

        let sync = state.try_begin(Command::SyncOnce).expect("同步");
        let canceled = state.try_begin(Command::Cancel).expect("取消在途");
        assert_eq!(canceled, sync, "Cancel 返回的是在途 op");
        state.apply(Event::Completed {
            op_id: sync,
            changed_heads: Vec::new(),
            result: ok("跑完了"),
        });
        assert!(
            matches!(state.state(), DevSyncState::Paused { .. }),
            "取消过的操作不能画成 Ready"
        );
        assert_eq!(
            state
                .try_begin(Command::Cancel)
                .expect_err("空闲取消")
                .reason,
            BusyReason::Idle
        );
    }

    #[test]
    fn close_becomes_blocked_when_the_in_flight_op_fails() {
        let mut state = SyncState::new();
        let op = state.try_begin(Command::Join).expect("加入");
        let _stop = state.try_begin(Command::Stop).expect("停止");
        let ticket = state.last_ticket().expect("票");
        assert_eq!(state.poll_close(&ticket), CloseState::Pending);

        state.apply(Event::Completed {
            op_id: op,
            changed_heads: Vec::new(),
            result: OpResult::Failed {
                message: "模拟 durable 未确认".to_owned(),
            },
        });
        match state.poll_close(&ticket) {
            CloseState::Blocked(reason) => assert!(reason.contains("durable 未确认")),
            other => panic!("失败后关闭必须 Blocked，实际 {other:?}"),
        }
        assert!(matches!(state.state(), DevSyncState::Failed { .. }));
    }

    #[test]
    fn two_phase_only_grants_after_need_gate_and_finishes_after_integrated() {
        // ADR 0012 S2a：阶段顺序由状态机把关（错误授权 = 拒绝，不静默接受）。
        let mut state = SyncState::new();
        let op = state.try_begin(Command::SyncTwoPhase).expect("两阶段同步");
        assert_eq!(
            state.snapshot().two_phase.map(|status| status.phase),
            Some(SyncPhase::Prepare),
            "派发即 Enter Prepare（网络阶段可见）"
        );
        assert!(!state.note_grant_sent(op), "Prepare 阶段不能授权");
        assert!(!state.note_finish_sent(op), "Prepare 阶段不能 Finish");

        state.apply(Event::NeedGate {
            op_id: op,
            authorization_epoch: 77,
            stored: 2,
            deferred: 0,
            summary: "网络阶段完成".to_owned(),
        });
        assert_eq!(state.two_phase_epoch(), Some(77));
        assert!(state.is_busy(), "NeedGate 不是终态：单操作位仍占着");
        assert!(!state.note_grant_sent(OpId::new(999)), "别人的 op 不能授权");
        assert!(state.note_grant_sent(op), "NeedGate 之后允许授权");
        assert!(!state.note_grant_sent(op), "重复授权必须拒绝");
        assert!(!state.note_finish_sent(op), "Integrate 阶段不能 Finish");

        state.apply(Event::Integrated {
            op_id: op,
            changed_heads: vec!["doc-1".to_owned()],
            integrated: 1,
            deferred: 0,
            summary: "集成完成".to_owned(),
        });
        assert_eq!(
            state.snapshot().two_phase.map(|status| status.phase),
            Some(SyncPhase::Integrated)
        );
        assert!(state.note_finish_sent(op), "Integrated 之后允许 Finish");
        assert!(!state.note_finish_sent(op), "重复 Finish 必须拒绝");
        assert!(
            state.is_busy(),
            "交付完成前单操作位必须一直被占着（Finish 也要串行）"
        );

        // 真实终态之后：阶段清空、单操作位释放。
        state.apply(Event::Completed {
            op_id: op,
            changed_heads: vec!["doc-1".to_owned()],
            result: OpResult::Ok {
                summary: "两阶段完成".to_owned(),
            },
        });
        assert!(state.snapshot().two_phase.is_none());
        assert!(!state.is_busy());
    }

    #[test]
    fn deadline_is_not_a_terminal_and_the_late_receipt_is_recorded() {
        // ADR 0012 S1：看门狗只能产生 Cancelling，不能伪造可解闸的 Completed。
        let mut state = SyncState::new();
        let op = state.try_begin(Command::SyncOnce).expect("同步");
        state.apply(Event::DeadlineExceeded {
            op_id: op,
            message: "操作超时：超过 120s 未完成".to_owned(),
        });
        assert!(
            matches!(state.state(), DevSyncState::Cancelling { op_id } if *op_id == op),
            "超时必须落到 Cancelling：{:?}",
            state.state()
        );
        assert!(state.is_busy(), "超时后单操作位仍被占着");
        assert_eq!(
            state.snapshot().in_flight,
            Some((op, CommandKind::SyncOnce))
        );
        assert_eq!(state.snapshot().deadline_exceeded, Some(op));
        assert!(
            state.snapshot().last_completed.is_none(),
            "超时不能生成任何 Completed（否则 UI 会解闸）"
        );
        assert!(state
            .snapshot()
            .last_error
            .is_some_and(|error| error.contains("操作超时")));
        // 重复超时事件不重复通知。
        state.apply(Event::DeadlineExceeded {
            op_id: op,
            message: "重复".to_owned(),
        });
        let events = state.pull_events();
        assert_eq!(events.len(), 1, "只保留一条超时事件：{events:?}");
        assert!(events
            .iter()
            .all(|event| !event.is_terminal() && matches!(event, Event::DeadlineExceeded { .. })));

        // 迟到的**真实**回执不能丢：照常落终态，并注明"超时后真实收尾"。
        state.apply(Event::Completed {
            op_id: op,
            changed_heads: vec!["head-late".to_owned()],
            result: ok("已同步 1 个 head"),
        });
        assert!(!state.is_busy(), "真实终态回来后单操作位才释放");
        assert_eq!(state.snapshot().deadline_exceeded, None);
        let completed = state.snapshot().last_completed.expect("迟到回执必须被记录");
        assert_eq!(completed.0, op);
        assert_eq!(completed.1, vec!["head-late".to_owned()]);
        assert_eq!(
            completed.2,
            OpResult::Ok {
                summary: "已同步 1 个 head（超时后真实收尾，未丢弃）".to_owned()
            },
            "迟到回执必须注明它是在超时之后才到的"
        );
        assert_eq!(state.state(), &DevSyncState::Ready);
    }

    #[test]
    fn worker_stopped_is_a_reliable_receipt_and_clears_the_deadline_wait() {
        let mut state = SyncState::new();
        let op = state.try_begin(Command::SyncOnce).expect("同步");
        state.apply(Event::DeadlineExceeded {
            op_id: op,
            message: "操作超时".to_owned(),
        });
        let stop = state.try_begin(Command::Stop).expect("停止");
        state.apply(Event::Stopped { op_id: stop });
        assert!(state.worker_stopped());
        assert!(state.snapshot().deadline_exceeded.is_none());
        // worker 停止时在途目标的 durable 确认没到：关闭票据必须 Blocked，不假装 Ready。
        assert!(matches!(
            state.poll_close(&state.last_ticket().expect("票")),
            CloseState::Blocked(_)
        ));
    }

    #[test]
    fn stopped_after_success_makes_close_ready_and_stale_tickets_are_blocked() {
        let mut state = SyncState::new();
        let op = state.try_begin(Command::SyncOnce).expect("同步");
        let stop = state.try_begin(Command::Stop).expect("停止");
        let ticket = state.last_ticket().expect("票");
        state.apply(Event::Completed {
            op_id: op,
            changed_heads: vec!["head-9".to_owned()],
            result: ok("落盘完成"),
        });
        assert_eq!(state.poll_close(&ticket), CloseState::Ready);

        state.apply(Event::Stopped { op_id: stop });
        assert!(state.worker_stopped());
        assert_eq!(state.poll_close(&ticket), CloseState::Ready);

        // 新的关闭票据：旧票立即失效（不能拿旧票读新关闭）。
        let stale = ticket;
        let _new_stop_ticket = state.last_ticket().expect("票仍在");
        assert_eq!(state.state(), &DevSyncState::Closing);
        // worker 已停 → 不再接新命令。
        assert_eq!(
            state
                .try_begin(Command::Create)
                .expect_err("worker 已停")
                .reason,
            BusyReason::WorkerStopped
        );
        // 造一张 epoch 不匹配的票：一律 Blocked。
        let forged = StopTicket {
            epoch: stale.epoch() + 7,
            op_id: stale.op_id(),
        };
        assert!(matches!(state.poll_close(&forged), CloseState::Blocked(_)));
    }
}
