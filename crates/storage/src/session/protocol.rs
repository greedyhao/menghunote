//! 文档会话协议（ADR 0003 §5）：`editor` 与 `storage` 之间的**冻结契约**。
//!
//! 本模块**只放类型与常量**，不放实现。目的：在切片 2（storage 会话核）与切片 3
//! （editor 纯模型）并行前，先把端口签名钉死，避免两边各写一套协议。
//!
//! ## 设计约束（来自 ADR 0002 §9 与 ADR 0003 §5）
//!
//! - 所有半开区间**统一相对 `EditRequest.base_live_rev`**，按 `edits` 的顺序排序且互不重叠，
//!   由会话按**逆序**应用；不允许"一会儿原始坐标、一会儿逐 op 坐标"。
//! - 坐标单位是 **Unicode scalar**（不是 UTF-16、不是字节）：`ScalarEdit` 的两端都是
//!   [`UnicodeScalar`]，单位混用必须是编译错误。
//! - `frontier` / `vv` 在协议里是**不透明字节**（`Vec<u8>`），协议不泄露 Loro 内部类型。
//! - `Applied.local_echo = true` 表示这次编辑的正文已经由调用方写进自己的镜像，
//!   **不得**再按 `splice` 二次应用（防本地回声重复）。
//! - "已保存"只由**通过当前 `live_rev` 的 [`DurableAck`]** 决定；物化（B 事务）不参与保存条件。
//! - 背压是**拒绝**而不是丢数据：[`RejectReason::Backpressure`] 表示"本次受理前就被拒"，
//!   此时 **Loro 未被修改**。
//! - composition 闸门（ADR 0002 §7）：组合期间**远端导入延后**（进队列，结束后按序串行合并）、
//!   其它本地正文改动返回 [`RejectReason::CompositionActive`]（瞬态，先 commit/cancel 再重试）。
//!   preedit 不经过本层：会话只关心「组合是否在进行」。

use menghunote_text::UnicodeScalar;

/// 文档 ID（正文文档，例如 `body-<note_id>`）。
pub type DocId = String;

/// 命令 ID：同一次用户编辑的幂等键。
pub type CommandId = String;

/// 文档代际：换代后旧代的 update 不得导入（ADR 0001 §3.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Generation(pub u64);

/// 会话纪元：进程/会话级标识，用于拒绝旧会话的迟到请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionEpoch(pub u64);

/// 本地 live 修订号：**单调递增**，每次成功受理/合并都前进。
///
/// 它是"基准"的比较单位，与 `batch_id`、`command_id`、`relay_seq` 都不是同一个概念。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LiveRev(pub u64);

/// 持久批次号（A 事务提交一次的标识）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BatchId(pub u64);

/// flush 票据号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FlushTicketId(pub u64);

/// 变更来源：本地输入 / 远端导入 / 撤销 / 重做。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeSource {
    /// 用户本地编辑。
    Local,
    /// 远端 update 导入。
    Remote,
    /// 撤销（生成本地新 op）。
    Undo,
    /// 重做（生成本地新 op）。
    Redo,
}

/// 触发 flush 的原因（ADR 0002 §9：250 ms / 64 KiB / Ctrl+S / 切换 / 关闭）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushReason {
    /// 首次 dirty 起固定 250 ms 窗口。
    Interval,
    /// 待提交字节数达到阈值。
    Size,
    /// 用户显式保存（Ctrl+S）。
    Manual,
    /// 切换文档。
    Switch,
    /// 关闭窗口/应用。
    Close,
}

/// 打开文档时返回的快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSnapshot {
    /// 文档 ID。
    pub doc_id: DocId,
    /// 文档代际。
    pub generation: Generation,
    /// 当前会话纪元。
    pub session_epoch: SessionEpoch,
    /// 打开时的 live 修订号（后续 `EditRequest.base_live_rev` 以它为起点）。
    pub live_rev: LiveRev,
    /// live frontier（不透明字节）。
    pub live_frontier: Vec<u8>,
    /// live 版本向量（不透明字节）。
    pub live_vv: Vec<u8>,
    /// 已 durable 的批次号。
    ///
    /// **打开时恒为 `None`**：批次号不跨会话持久化（库里只有 `loro_updates.batch_id`）。
    /// 跨会话的持久水位请用 [`OpenSnapshot::durable_head_seq`]；本字段只在会话内有效。
    pub durable_batch: Option<BatchId>,
    /// durable head seq。
    pub durable_head_seq: u64,
    /// LF 归一后的 Markdown 正文（**仅打开/故障恢复允许全量**）。
    pub markdown: String,
}

/// 一次替换：`[start, end)` 半开区间（Unicode scalar），换成 `replacement`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalarEdit {
    /// 起始 scalar 位置（相对 `base_live_rev`）。
    pub start: UnicodeScalar,
    /// 结束 scalar 位置（不含）。
    pub end: UnicodeScalar,
    /// 替换文本。
    pub replacement: String,
}

/// 一次编辑请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditRequest {
    /// 文档 ID。
    pub doc_id: DocId,
    /// 文档代际（不符即拒）。
    pub generation: Generation,
    /// 会话纪元（不符即拒）。
    pub session_epoch: SessionEpoch,
    /// 幂等命令 ID：同一 `command_id` 重复到达必须得到同一结果。
    pub command_id: CommandId,
    /// 本次编辑的基准 live 修订号。
    pub base_live_rev: LiveRev,
    /// 编辑集合：相对 `base_live_rev`，排序且互不重叠，会话按**逆序**应用。
    pub edits: Vec<ScalarEdit>,
    /// 撤销栈用的来源标记（配合 `add_exclude_origin_prefix` 排除后台变更）。
    pub origin: String,
    /// 撤销分组（`None` 表示按会话默认分组）。
    ///
    /// v1 约定：**一个 `command_id` = 一个撤销项**，会话不把多个命令并进同一组。
    /// 编辑器若要实现"连续输入合并成一步"，应在**发送前**把多次输入合并成一个 `EditRequest`。
    /// 本字段保留给将来的多命令分组，v1 会话可以忽略它。
    pub undo_group: Option<String>,
}

/// 增量 splice（用于驱动调用方的 Rope 镜像）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaSplice {
    /// 起始 scalar 位置。
    pub at: UnicodeScalar,
    /// 删除多少个 scalar。
    pub delete_scalars: u64,
    /// 插入的文本。
    pub insert: String,
}

/// 并发信息：只有**真正并发**的远端导入才置 `concurrent = true`。
///
/// v1 实现用"导入时存在未 ACK 的本地编辑"作近似（ADR 0002 §9 要求的严格 VV 因果判定留待后续）；
/// 因此它只能用于**非阻断提示**，不能作为冲突裁决依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MergeInfo {
    /// 本次合并是否与本地未确认编辑并发。
    pub concurrent: bool,
    /// 本次合并涉及的远端变更数。
    pub remote_changes: u32,
}

/// A（CRDT 持久化）与 live 状态已推进；调用方应据此更新 Rope 镜像。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// 对应命令（撤销/重做时为生成的新命令）。
    pub command_id: CommandId,
    /// 受理后的 live 修订号。
    pub live_rev: LiveRev,
    /// 受理后的 live frontier（不透明字节）。
    pub live_frontier: Vec<u8>,
    /// 已确认的 splice（驱动镜像用）。
    pub splice: DeltaSplice,
    /// 若为 `true`，调用方已自行写入该文本，**不得**再按 `splice` 二次应用。
    pub local_echo: bool,
    /// 变更来源。
    pub source: ChangeSource,
    /// 并发信息。
    pub merge: MergeInfo,
}

/// 基准过期：**请求未被应用**，调用方需基于新基准重放（或接受补偿）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleBase {
    /// 被拒命令。
    pub command_id: CommandId,
    /// 当前 live 修订号（新基准）。
    pub current_live_rev: LiveRev,
    /// 可选的补偿 delta（帮助调用方把镜像追到当前状态）。
    pub compensation: Option<LiveDelta>,
}

/// 拒绝原因：**受理前**就失败，`Loro` 未被修改。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// 未 ACK journal 已满：必须先 flush（背压）。
    Backpressure,
    /// 文档不存在。
    UnknownDoc,
    /// 代际不符（可能是旧代 update）。
    WrongGeneration,
    /// 会话纪元不符（旧会话的迟到请求）。
    WrongEpoch,
    /// 区间非法（越界 / 非 scalar 边界 / 重叠）。
    InvalidRange,
    /// 会话尚未就绪（打开中/恢复中）。
    NotReady,
    /// composition（IME 组合）进行中：本次正文改动被闸门拒绝。
    ///
    /// **瞬态**，与 [`RejectReason::Backpressure`] 同性质：请求未被受理、`Loro` 未被修改，
    /// 调用方应先完成平台的 commit/cancel（即 `end_composition`）再重试同一 `command_id`。
    CompositionActive,
}

/// 拒绝：与 [`StaleBase`] 不同，这里**连基准都不成立**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    /// 被拒命令。
    pub command_id: CommandId,
    /// 原因。
    pub reason: RejectReason,
}

/// 一次编辑的三种结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOutcome {
    /// 已受理并推进。
    Applied(Applied),
    /// 基准过期，未应用。
    Stale(StaleBase),
    /// 受理前被拒（含背压），未应用。
    Rejected(Rejected),
}

/// 远端导入等"非请求驱动"的 live 变更事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveDelta {
    /// 文档 ID。
    pub doc_id: DocId,
    /// 变更前 live 修订号。
    pub from_rev: LiveRev,
    /// 变更后 live 修订号。
    pub to_rev: LiveRev,
    /// 变更后的 live frontier。
    pub live_frontier: Vec<u8>,
    /// 驱动镜像的 splice。
    pub splice: DeltaSplice,
    /// 触发它的命令（远端时通常为 `None`）。
    pub cause: Option<CommandId>,
    /// 来源。
    pub source: ChangeSource,
    /// 并发信息。
    pub merge: MergeInfo,
}

/// durable 确认：**只在 WAL 事务提交成功之后**发出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableAck {
    /// 已 durable 的批次。
    pub batch_id: BatchId,
    /// 该批次覆盖到的 live 修订号（`<= live_rev`）。
    pub through_live_rev: LiveRev,
    /// durable head seq。
    pub head_seq: u64,
    /// durable head frontier。
    pub head_frontier: Vec<u8>,
}

/// flush 票据：受理即返回（异步等 ACK，不阻塞 UI）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushTicket {
    /// 票据号。
    pub ticket_id: FlushTicketId,
    /// 本次 flush 的目标 live 修订号。
    pub target_live_rev: LiveRev,
}

/// 撤销/重做栈状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryState {
    /// 是否可撤销。
    pub can_undo: bool,
    /// 可撤销步数。
    pub undo_count: usize,
    /// 是否可重做。
    pub can_redo: bool,
    /// 可重做步数。
    pub redo_count: usize,
}

/// 会话向调用方推送的事件（不含请求的同步回复）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// durable 水位推进。
    Durable(DurableAck),
    /// 远端/后台变更。
    Remote(LiveDelta),
}

/// 端口错误（传输/IO/内部不一致），与"业务拒绝"（[`Rejected`]）区分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortError {
    /// 存储层错误（SQLite/IO/编码）。
    Storage(String),
    /// 未打开的文档。
    NotOpen(DocId),
}
