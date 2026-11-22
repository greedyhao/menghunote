//! 同步的**协调状态**（ADR 0009 Phase 1 / 切片 4c-2；无会话补丁见下）。
//!
//! ## 这一片在做什么
//!
//! `DevLocal::sync_once` 会 `push → pull → integrate → ack`：其中 `integrate` 跳过本地
//! 会话直接往 SQLite 追加外部行。所以 UI 必须先把 `integrate` 与本地写入协调好：
//!
//! - **有活动文档**（有人打开了笔记）：先让该文档进入**外部提交闸门 Held**
//!   （切片 3a），worker 才允许跑同步（切片 4c-2 的 `DevLocalRunner`）；
//! - **没有活动文档**（空库 / 还没打开任何笔记）：没有会话可以 begin/adopt，走
//!   [`SyncOnceTarget::NoSession`] 的**无会话同步**——不持闸，但同步期间冻结结构写入，
//!   结束后按结构 head 重投影并自动打开第一篇（见 `AppShell::dev_sync_finish`）。
//!
//! ## ADR 0012 S2a：手动同步 = **可暂停的两阶段**（网络阶段不持闸）
//!
//! 有活动文档时的手动「同步」不再整轮持闸，而是：
//!
//! ```text
//! Prepare（网络：encrypt/push/pull，不持闸，可编辑/切换）
//!   -> NeedGate（worker 停下等授权）
//!   -> 当前活动文档 begin/poll 到 Held
//!   -> GrantGate（带 op/授权代际/head/会话纪元）
//!   -> Integrate（worker 复核后只 integrate）
//!   -> Integrated
//!   -> prepare_committed_delta → adopt → end（释放闸门）
//!   -> Finish（worker push tail + ack）
//!   -> Completed
//! ```
//!
//! 每个阶段都由 worker 重新构造 client/engine（事实只在 DB：outbox/inbox/cursor）；授权与
//! 取消走**独立控制通道**（[`crate::dev_sync::service::SyncControl`]），不会被普通命令队列挡住。
//! Held 只覆盖 `integrate + adopt`；网络阶段 UI 保持可编辑/可切换。切换文档后 NeedGate 以
//! **当时**的活动 session 为准（见 [`SyncOnceStage::NeedGate`]）。
//!
//! 旧整轮 `SyncOnce`（持闸后一次跑完）仍保留给**发布基线**与无会话路径；手动同步的新路径
//! 由 [`SyncOnceStage::Preparing`] / [`SyncOnceStage::Integrating`] 等阶段标识。
//!
//! ## 结构树与其它文档（ADR 0012 S2b）
//!
//! 依赖顺序在 **worker 的 Integrate 阶段内**收敛（结构 → 只投影 → 正文公平轮转 →
//! 同轮重试 deferred），UI **不再**发"补一轮网络同步"来修依赖：
//!
//! - 其它 body 文档：本轮 integrate 也会推进它们，但没有打开的会话，UI 不 adopt；
//!   下次打开时由 `SessionDriver::open` 从 durable journal 重放（不读旧缓存 session，
//!   天然正确）；changed heads 触发既有物化/索引接口更新。
//! - 结构树：worker 集成结构 head 后在同一次 Integrate 内用
//!   `StructureWriter::commit`（无新 op，**只做投影**）重建 `tree/notes` 与 body 行；
//!   UI 收尾仍比较结构 head，变了才重投影 + `refresh_tree`（幂等，不假刷新）。
//! - 投影失败：worker 明确报错（"结构投影失败"），UI **不**解闸/保持写禁用，等用户
//!   重试收尾；绝不把失败显示成"已刷新"。
//! - 无会话同步（没有活动文档）没有可 adopt 的正文：收尾按结构 head 重投影，并在树里
//!   出现笔记时**自动打开第一篇**（空库第一次同步的完整闭环，同样不需要补轮）。
//!
//! ## 发布基线也走这条状态机（ADR 0009 切片 4c-4）
//!
//! 「发布基线」与同步**共用**同一条持闸状态机，只换 worker 命令
//! （`SyncOnce` / `PublishInitialPair`）与报告种类（[`GatedTask`]）：
//! 发布同样要先 `Held`（把活动文档 flush 到 durable）再派发，收尾同样走
//! `prepare → adopt → end` 释放闸门——发布的 worker 内部会跑一轮同步，所以"闸门释放前
//! 不能有本地写入"与"已 durable 的外部行必须先交付"这两条约束和同步完全一致。
//! 发布**永远走 `Held` 路径**（要求有活动文档），不进 [`SyncOnceTarget::NoSession`]。
//!
//! ## 失败语义
//!
//! - worker 失败：先交付**已 durable** 的外部行（`prepare → adopt`），再 `end` 释放闸门；
//!   正文不变、闸门不残留。
//! - 交付/释放本身失败：**保持 Held**（fail-closed，UI 不静默解冻），状态记为
//!   `Delivering`，用户再点「同步」会重试收尾。
//! - 无会话路径没有闸门可残留：失败只保留错误、释放 busy 状态，后续操作不冻结。
//! - 重复命令：同步在途时 `dev_sync_sync_once` 直接返回明确错误（Busy 语义）。

use std::time::Duration;

use gpui::Entity;
use menghunote_storage::session::{ExternalCommitLease, ExternalCommitTicket};

use crate::editor::view::EditorView;

use super::autosync::OpOrigin;
use super::state::{Command, OpId};

/// 同步期间 shell 的泵间隔（只影响 dev-only 面板/状态机，不是业务时钟）。
pub const SYNC_PUMP_INTERVAL: Duration = Duration::from_millis(40);

/// 持闸任务种类：同一条状态机跑「同步」与「发布基线」（ADR 0009 切片 4c-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GatedTask {
    /// 一轮完整同步（`encrypt_pending → push → pull → integrate → ack`）。
    #[default]
    SyncOnce,
    /// 发布初始基线（worker 内部先跑一轮同步，再发两份同 epoch checkpoint）。
    PublishBaseline,
}

impl GatedTask {
    /// 中文短标签（面板/notice/错误文案用）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SyncOnce => "同步一次",
            Self::PublishBaseline => "发布初始基线",
        }
    }

    /// 面板持闸任务行的前缀（「同步：…」/「发布：…」）。
    #[must_use]
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::SyncOnce => "同步",
            Self::PublishBaseline => "发布",
        }
    }

    /// 对应的 worker 命令（两条都必须带 `HeldGate`；无会话路径不是本枚举）。
    #[must_use]
    pub const fn command(self) -> Command {
        match self {
            Self::SyncOnce => Command::SyncOnce,
            Self::PublishBaseline => Command::PublishInitialPair,
        }
    }

    /// 并发拒绝文案（重复命令/在途时显示；不谎称"成功"）。
    #[must_use]
    pub const fn busy_label(self) -> &'static str {
        match self {
            Self::SyncOnce => "同步已在进行（持闸同步不允许并发）",
            Self::PublishBaseline => "发布基线已在进行（持闸任务不允许并发）",
        }
    }
}

/// 一次同步走到哪一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOnceStage {
    /// ADR 0012 S2a：已派发两阶段 Prepare，worker 在网络阶段（**不持闸**）。
    ///
    /// UI 可继续编辑/切换文档；收到 `NeedGate` 才进入 [`Self::NeedGate`]。
    Preparing,
    /// 两阶段 Prepare 完成，worker 已停下等授权；UI 要在**当前**活动文档上持闸。
    NeedGate,
    /// 已在当前活动文档上 `begin_external_commit`，等闸门 `Held`（`WaitingIME` /
    /// `Flushing` 都算这一步）。
    ///
    /// 旧持闸路径（发布基线）也用它；两阶段路径由 [`Self::NeedGate`] 进入。
    WaitingGate,
    /// 两阶段：已发 `GrantGate`，worker 正在 `integrate`（UI 已 Held）。
    Integrating,
    /// 已派发（或即将派发）旧整轮同步命令 / 发布基线，worker 在跑完整一轮。
    ///
    /// 两阶段路径不用它（阶段见 `Preparing/NeedGate/Integrating/Finishing`）。
    WorkerRunning,
    /// 两阶段：已交付并释放闸门，worker 在收尾（push tail + ack）。
    Finishing,
    /// 两阶段：已取消/授权失败，闸门已释放（或从未持有），等 worker 的真实回执收尾。
    ///
    /// **不是**"已安全结束"：worker 仍可能在外部写边界内，不能派发第二操作。
    Cancelling,
    /// worker 已回执/失败，正在把已 durable 的外部增量交付进 live 并释放闸门。
    ///
    /// 只有持闸路径会进这个阶段（无会话路径没有闸门，也没有 adopt）。
    Delivering,
}

impl SyncOnceStage {
    /// 中文短标签（面板显示用）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Preparing => "网络同步（不持闸）",
            Self::NeedGate => "等待持闸授权",
            Self::WaitingGate => "等待持闸",
            Self::Integrating => "整合中（持闸）",
            Self::WorkerRunning => "同步中",
            Self::Finishing => "收尾（已解闸）",
            Self::Cancelling => "等待真实收尾",
            Self::Delivering => "交付已落盘变化",
        }
    }
}

/// 本轮同步针对谁：**有活动文档**（持闸）或**没有活动文档**（无会话）。
///
/// 两条路径的差别是结构性的，不是开关：
///
/// | | 持闸（[`Self::Held`]） | 无会话（[`Self::NoSession`]） |
/// | --- | --- | --- |
/// | 闸门 | `begin/poll/end` 都要 | **没有**（没有会话可持） |
/// | worker 证明 | 必须带 `HeldGate` | 一律不带（带了一律拒绝） |
/// | 交付/adopt | `prepare_committed_delta` → `adopt` | 不 adopt（没有打开的视图） |
/// | 正文冻结 | 整个同步期间输入被冻结 | 没有编辑器，无冻结对象 |
/// | 结构写入 | 由 `write_structure` 拒绝 | 同样由 `write_structure` 拒绝 |
#[derive(Debug, Clone)]
pub enum SyncOnceTarget {
    /// 持闸同步：闸门、delta、adopt 都针对这篇活动文档。
    Held {
        /// 活动文档的 body doc id。
        doc_id: String,
        /// 活动文档的编辑器实体（切换文档被本片禁止，所以它可以安全地活到同步结束）。
        view: Entity<EditorView>,
        /// `begin_external_commit` 发出的票据。
        ticket: ExternalCommitTicket,
    },
    /// 无会话同步：UI 没有活动文档（空库/还没打开笔记），没有可 begin/adopt 的会话。
    ///
    /// 为什么安全：没有编辑器会话就没有本地 live 写入，集成期间不会与本地 flush 争
    /// `head_seq`；结构写入由 `AppShell::write_structure` 在同步期间明确拒绝。
    NoSession,
    /// ADR 0012 S2a 两阶段：网络阶段**还没有**闸门；收到 `NeedGate` 后由 UI 在当时的
    /// 活动文档上 begin，字段才逐步填上（保持"同一时刻最多一个 op"）。
    TwoPhase {
        /// 已持闸的活动文档视图（`None` = 还没 begin）。
        view: Option<Entity<EditorView>>,
        /// 已持闸的活动文档 body doc id（`None` = 还没 begin）。
        doc_id: Option<String>,
        /// `begin_external_commit` 发出的票据（`None` = 还没 begin）。
        ticket: Option<ExternalCommitTicket>,
    },
}

/// 两阶段目标里的三件套（还没 `begin` 时各字段都是 `None`）。
#[derive(Debug, Clone, Default)]
pub struct TwoPhaseParts {
    /// 已持闸的活动文档视图（`None` = 还没 begin）。
    pub view: Option<Entity<EditorView>>,
    /// 已持闸的活动文档 body doc id（`None` = 还没 begin）。
    pub doc_id: Option<String>,
    /// `begin_external_commit` 发出的票据（`None` = 还没 begin）。
    pub ticket: Option<ExternalCommitTicket>,
}

impl SyncOnceTarget {
    /// 两阶段目标里的视图/文档/票据（不是两阶段目标时返回 `None`）。
    #[must_use]
    pub fn two_phase_parts(&self) -> Option<TwoPhaseParts> {
        match self {
            Self::TwoPhase {
                view,
                doc_id,
                ticket,
            } => Some(TwoPhaseParts {
                view: view.clone(),
                doc_id: doc_id.clone(),
                ticket: ticket.clone(),
            }),
            _ => None,
        }
    }
}

/// 在途的同步（同一时刻最多一个）。
#[derive(Debug)]
pub struct ActiveSyncOnce {
    /// 本任务种类（同步 / 发布基线；决定派发哪条命令、报告怎么分类）。
    pub task: GatedTask,
    /// 本轮是谁发起的（手动 / 自动；ADR 0012 S3 的调度与状态 UX 依据）。
    ///
    /// 自动轮不走面板按钮，收尾要交回 [`Scheduler`](super::scheduler::Scheduler)；
    /// 手动轮清退避、不参与自动续批。两种来源共用同一条单操作槽与两阶段状态机。
    pub origin: OpOrigin,
    /// 自动轮因"等不到输入法空闲"主动 defer（不算失败，不退避）。
    pub auto_deferred: bool,
    /// 是否走 ADR 0012 S2a 的两阶段路径（网络阶段不持闸）。
    pub two_phase: bool,
    /// 是否已经请求取消（等 worker 真实回执；不重复发 Cancel）。
    pub cancel_requested: bool,
    /// worker 在 `NeedGate` 给出的授权代际（`GrantGate` 必须原样带回）。
    pub authorization_epoch: Option<u64>,
    /// 两阶段 Integrate 报告里活动文档变化的 heads。
    pub integrated_heads: Vec<String>,
    /// 是否已经发出 `Finish`（防止重复授权收尾）。
    pub finish_sent: bool,
    /// 已交付进 live 的变化条数（最终报告用）。
    pub delivered_changes: usize,
    /// 失败/取消时的明确原因（等真实回执期间保留）。
    pub pending_error: Option<String>,
    /// 本轮同步的目标（活动文档 / 无会话 / 两阶段）。
    pub target: SyncOnceTarget,
    /// 进入 Held 后拿到的租约（`None` = 还没 Held，或本来就是无会话路径）。
    pub lease: Option<ExternalCommitLease>,
    /// 派发后的 worker op（`None` = 还没派发）。
    pub op_id: Option<OpId>,
    /// 当前阶段。
    pub stage: SyncOnceStage,
    /// 本轮同步是否已经真的在 Held 下派发过（用于"顺序正确"的最终报告）。
    pub dispatched_under_gate: bool,
    /// 同步前的结构文档 head seq（收尾时只有它变了才重投影；不假刷新）。
    pub structure_base_seq: u64,
    /// 是否已经收到**真实的** worker 收尾回执（`Completed` 或 `Stopped`）。
    ///
    /// ADR 0012 S1：只有它为 `true`（或本次任务根本没派发过 op）时，`AppShell` 才允许
    /// `end_external_commit` 解闸。operation deadline 只看门狗，不能把它置 `true`——
    /// 否则会在 worker 仍可能 `integrate` 时制造"库里有、live 没有"的撕裂。
    pub worker_settled: bool,
    /// 是否已经把"已超时、仍在等真实收尾"写进面板（避免每帧重复通知）。
    pub deadline_noted: bool,
    /// worker 是否上报过"结构投影失败"（ADR 0012 S2b）。
    ///
    /// 结构 CRDT 已 durable、但本地 `tree/notes` 投影没追上：UI 必须先交付已 durable 的
    /// 正文，然后**保持写禁用**（不解闸），等重试收尾把投影补上；不得假装刷新成功。
    pub projection_failed: bool,
}

impl ActiveSyncOnce {
    /// 从"已 begin 的活动文档"构造（持闸路径）。
    #[must_use]
    pub fn new(
        task: GatedTask,
        doc_id: String,
        view: Entity<EditorView>,
        ticket: ExternalCommitTicket,
        structure_base_seq: u64,
    ) -> Self {
        Self {
            task,
            origin: OpOrigin::Manual,
            auto_deferred: false,
            two_phase: false,
            cancel_requested: false,
            authorization_epoch: None,
            integrated_heads: Vec::new(),
            finish_sent: false,
            delivered_changes: 0,
            pending_error: None,
            target: SyncOnceTarget::Held {
                doc_id,
                view,
                ticket,
            },
            lease: None,
            op_id: None,
            stage: SyncOnceStage::WaitingGate,
            dispatched_under_gate: false,
            worker_settled: false,
            deadline_noted: false,
            structure_base_seq,
            projection_failed: false,
        }
    }

    /// 从"已派发的无会话命令"构造（没有活动文档；没有闸门可等，直接 `WorkerRunning`）。
    #[must_use]
    pub fn new_no_session(op_id: OpId, structure_base_seq: u64) -> Self {
        Self {
            task: GatedTask::SyncOnce,
            origin: OpOrigin::Manual,
            auto_deferred: false,
            two_phase: false,
            cancel_requested: false,
            authorization_epoch: None,
            integrated_heads: Vec::new(),
            finish_sent: false,
            delivered_changes: 0,
            pending_error: None,
            target: SyncOnceTarget::NoSession,
            lease: None,
            op_id: Some(op_id),
            stage: SyncOnceStage::WorkerRunning,
            // 无会话路径**没有**持闸证明：报告里必须如实为 false（不能谎报顺序证明）。
            dispatched_under_gate: false,
            worker_settled: false,
            deadline_noted: false,
            structure_base_seq,
            projection_failed: false,
        }
    }

    /// ADR 0012 S2a：两阶段同步（Prepare 已派发，网络阶段不持闸）。
    #[must_use]
    pub fn new_two_phase(task: GatedTask, structure_base_seq: u64) -> Self {
        Self {
            task,
            origin: OpOrigin::Manual,
            auto_deferred: false,
            two_phase: true,
            cancel_requested: false,
            authorization_epoch: None,
            integrated_heads: Vec::new(),
            finish_sent: false,
            delivered_changes: 0,
            pending_error: None,
            target: SyncOnceTarget::TwoPhase {
                view: None,
                doc_id: None,
                ticket: None,
            },
            lease: None,
            op_id: None,
            stage: SyncOnceStage::Preparing,
            dispatched_under_gate: false,
            worker_settled: false,
            deadline_noted: false,
            structure_base_seq,
            projection_failed: false,
        }
    }

    /// 指定来源（自动轮用；手动默认构造保持 [`OpOrigin::Manual`]）。
    #[must_use]
    pub fn with_origin(mut self, origin: OpOrigin) -> Self {
        self.origin = origin;
        self
    }

    /// 是不是"无活动文档"的无会话同步。
    #[must_use]
    pub fn is_no_session(&self) -> bool {
        matches!(self.target, SyncOnceTarget::NoSession)
    }

    /// 是否仍允许切换文档（ADR 0012 S2a：**网络阶段**可以，NeedGate 之后不行）。
    ///
    /// 旧路径/无会话路径不放开：它们没有"先网络、后持闸"的两段结构。
    #[must_use]
    pub fn blocks_document_switch(&self) -> bool {
        if self.two_phase {
            return self.stage != SyncOnceStage::Preparing;
        }
        true
    }

    /// 活动文档的 doc id（无会话路径 = `None`）。
    #[must_use]
    pub fn document_id(&self) -> Option<&str> {
        match &self.target {
            SyncOnceTarget::Held { doc_id, .. } => Some(doc_id),
            SyncOnceTarget::TwoPhase { doc_id, .. } => doc_id.as_deref(),
            SyncOnceTarget::NoSession => None,
        }
    }
}

/// 一次同步的最终报告（面板文案 + 测试断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOnceReport {
    /// 本次持闸任务的种类（同步 / 发布基线；无会话同步恒为 `SyncOnce`）。
    pub task: GatedTask,
    /// 本次同步的活动文档（无会话同步 = `None`：没有活动文档）。
    pub doc_id: Option<String>,
    /// 是否走**无会话**路径（没有闸门、没有 adopt；ADR 0009 已知边界的补丁）。
    pub no_session: bool,
    /// 是否在 Held 下派发过 worker 命令（顺序正确性证明；无会话路径恒为 `false`）。
    pub dispatched_under_gate: bool,
    /// 交进 live 的已 durable 变化条数（无会话路径恒为 0：没有视图可交付）。
    pub changes: usize,
    /// worker 的中文摘要（推送/拉取/整合/延后）；失败时是 `None`。
    pub worker_summary: Option<String>,
    /// 明确失败原因（失败时**正文不变**、闸门已释放或安全保持）。
    pub error: Option<String>,
    /// 本轮是否重投影了结构树（结构 head 变了才刷新，不假刷新）。
    pub structure_refreshed: bool,
    /// 同步后自动打开的笔记标题（仅无会话路径 + 树里出现笔记）。
    pub opened_note: Option<String>,
    /// 给面板/状态栏看的单行文案（「同步中/已持闸/N 条变化」）。
    pub notice: String,
}

impl SyncOnceReport {
    /// 是否成功（没有错误）。
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

/// worker 上报"结构投影失败"时的稳定标记（ADR 0012 S2b）。
///
/// UI 据此**保持写禁用**（不解闸）并保留重试收尾入口；worker 的错误文案已经由
/// `SyncError::StructureProjection` 生成（"结构投影失败：…"），这里只做稳定匹配。
pub const PROJECTION_FAILURE_MARKER: &str = "结构投影失败";

/// 解闸判据（ADR 0012 S1，**fail-closed**）。
///
/// `end_external_commit` 只在两种情况下允许：
///
/// 1. 本次任务**从未派发过** worker op（`op_id = None`）：没有任何后台执行体可能写外部行；
/// 2. 已收到**真实**收尾回执（`worker_settled = true`）：`Completed`（含失败）或 `Stopped`。
///
/// operation deadline 只看门狗，**不**产生终态，因此不会让这里变成 `true`：worker 若仍卡在
/// `integrate`，UI 必须继续 Held（安全降级：提示用户重启/显式重试收尾），绝不静默解闸。
#[must_use]
pub const fn release_gate_allowed(dispatched_op: Option<OpId>, worker_settled: bool) -> bool {
    match dispatched_op {
        None => true,
        Some(_) => worker_settled,
    }
}

/// 未完成的持闸任务在继续（面板按钮置灰/提示用）。
#[must_use]
pub fn busy_label(task: GatedTask) -> &'static str {
    task.busy_label()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_can_only_be_released_after_a_real_receipt() {
        // 从未派发：没有后台执行体，可以 cancel/end。
        assert!(release_gate_allowed(None, false));
        // 派发过但只看门狗超时：**不得**解闸（worker 可能还在 integrate）。
        assert!(!release_gate_allowed(Some(OpId::new(7)), false));
        // 真实 Completed / Stopped 回执：允许解闸。
        assert!(release_gate_allowed(Some(OpId::new(7)), true));
    }
}
