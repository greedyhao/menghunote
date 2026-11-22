//! 编辑器纯模型控制器（ADR 0003 §7 切片 3）。
//!
//! ## 职责
//!
//! - 持有打开快照（[`OpenSnapshot`]）与 Rope 镜像；镜像只按**确认**增量更新；
//! - source selection（anchor / head 都是 [`UnicodeScalar`]）；
//! - Markdown 语义扫描（[`MarkdownDocument`]）+ 当前显示投影（[`ProjectionMap`]）：
//!   source 变时重解析；selection 变时只重建投影（caret 进入构造会展开标记，
//!   因此**移动 caret 不改 source 但可能改 display 长度**，app 据此决定是否重排）；
//! - 语义样式区间（`StyleRun`，display 坐标）：随投影一起重建，app 每帧直接借用；
//! - 块级语义区间（`BlockRun`，source 坐标）：每行一个 run，仅 source 变时重建；
//! - 生成本地编辑请求（[`EditRequest`]）；本地输入**乐观写入**镜像一次，
//!   `Applied.local_echo = true` 时只确认不再二次应用；
//! - IME 输入文档（[`InputDocument`]）与 preedit：preedit 永不进入 source；
//! - 按 grapheme 的交互式移动 / 删除（grapheme 只作为**交互边界**计算，
//!   产生的 [`ScalarEdit`] 仍然是 scalar 单位）。
//!
//! ## 为什么镜像"乐观写入 + 只确认一次"
//!
//! 输入法 / 键入要求即时可见，所以本地编辑在 `compose` 时就写进镜像；会话随后
//! 发回的 `Applied` 是**回声**（`local_echo = true`），按 `splice` 再应用一次会
//! 直接翻倍。远端 / 撤销等非本地命令（`local_echo = false`）才是镜像的唯一写入源，
//! 必须应用。两种路径都由 [`EditorController::apply_outcome`] 收口。
//!
//! ## S1 的边界（切片 4 再接）
//!
//! - 单飞行：一条本地命令确认前不允许再 `compose`（[`EditorError::CommandInFlight`]）；
//! - 基准过期 / 请求被拒 / `live_rev` 跳跃时**不猜**，控制器进入
//!   [`EditorError::ResyncRequired`]，等调用方按新的 `OpenSnapshot` 重建
//!   （`adopt_snapshot`）；
//! - 本地命令未确认时收到远端 [`LiveDelta`]：镜像含乐观输入，而 delta 按 live
//!   文档坐标给出，S1 **不做并发 rebase**，直接标记需要重建；
//! - 背压（[`RejectReason::Backpressure`]）不销毁状态：请求未被受理，镜像的乐观
//!   输入与 pending 都保留，可用同一 `EditRequest` 重试；
//! - 组合期间收到镜像变化会直接取消组合（preedit 不重映射，也不产生编辑）。
//!
//! ## 投影重建时机与代价（S2-3a）
//!
//! 控制器**急切**维护 `source → MarkdownDocument → ProjectionMap` 三者同步：
//!
//! - **source 变**（本地编辑、IME 提交、远端 delta、undo/redo）：重扫 Markdown
//!   并重建投影与语义样式区间（`refresh_markdown`）；
//! - **selection 变**（移动 caret / 设选区）：复用已缓存的 `MarkdownDocument`，
//!   只重建投影与样式区间（`refresh_projection`），不重解析；
//! - **块级语义 `block_runs`** 只依赖 source，随 `refresh_markdown` 重建；
//!   selection 变化**不**触发它（标题级别 / 块类型与 caret 无关）；
//! - 读 API（[`EditorController::display_text`] / `projection` / 双向映射）只借用
//!   当前结果，**不做惰性重建**；任何变更返回后三者都已同步。
//!
//! **v1 代价**：每次编辑 / caret 移动都做一次 O(n) 的全量重建（扫描 + 段表）。
//! 对当前目标（桌面笔记、单篇文档）可接受；后续可做增量 Markdown 扫描（按行/块
//! 失效重扫）与按块投影缓存，对外 API 不变。注意 caret 移动虽不改 source，仍会
//! 因显现/隐藏构造而改变 `display_len`，app 必须按 `display_len` 变化决定重排。

use std::borrow::Cow;

use menghunote_storage::session::{
    Applied, CommandId, DeltaSplice, DurableAck, EditOutcome, EditRequest, Generation, LiveDelta,
    LiveRev, OpenSnapshot, RejectReason, ScalarEdit, SessionEpoch,
};
use menghunote_text::{units, DisplayByte, SourceByte, UnicodeScalar};
use ropey::Rope;

use crate::model::block::BlockRun;
use crate::model::error::EditorError;
use crate::model::input::InputDocument;
use crate::model::markdown::MarkdownDocument;
use crate::model::projection::ProjectionMap;
use crate::model::selection::Selection;
use crate::model::style::StyleRun;

/// 本地编辑在 `EditRequest.origin` 里的标记（会话用于排除后台变更）。
const LOCAL_ORIGIN: &str = "local";

/// 逐 grapheme 的移动方向。
///
/// 这是**交互**概念：只在计算时使用，不进入任何持久化坐标（grapheme 不是
/// CRDT 索引，见 ADR 0002 §4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GraphemeStep {
    /// 向文档头。
    Backward,
    /// 向文档尾。
    Forward,
}

/// 一次确认对镜像的实际影响（调用方可据此决定是否重绘）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeEffect {
    /// 按 `splice` 更新了镜像（远端 / 撤销 / 重做等非本地回声）。
    MirrorUpdated,
    /// 本地回声：镜像在 `compose` 时已写入，本次只推进 `live_rev`，未二次应用。
    LocalEchoConfirmed,
    /// 重复 / 过期投递：镜像未改动。
    IgnoredDuplicate,
}

/// 进行中的 IME 组合：被替换的 source 选区 + preedit 文本。
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreeditState {
    range: Selection,
    text: String,
}

/// 一个打开的正文文档的纯模型。
#[derive(Debug, Clone)]
pub struct EditorController {
    snapshot: OpenSnapshot,
    mirror: Rope,
    /// 最近一次 source 的 Markdown 扫描结果；source 变时重建。
    markdown: MarkdownDocument,
    selection: Selection,
    /// 当前显示投影表；source 或 selection 变时重建。
    projection: ProjectionMap,
    /// 当前投影下的语义样式区间；与 projection 同步重建。
    style_runs: Vec<StyleRun>,
    /// 块级语义（每行一个 run）；**只在 source 变化时重建**，selection 不影响。
    block_runs: Vec<BlockRun>,
    preedit: Option<PreeditState>,
    next_command_seq: u64,
    pending: Option<CommandId>,
    needs_resync: bool,
    /// 已 durable 到的 live 修订号（由调用方消费 [`DurableAck`] 时告知）。
    durable_through: LiveRev,
}

impl EditorController {
    /// 用 `OpenSnapshot.markdown` 建立镜像、语义扫描、显示投影与样式区间。
    ///
    /// `markdown` 是 LF 归一后的真源，直接作为 source；本方法不做任何归一化。
    pub fn open(snapshot: OpenSnapshot) -> Result<Self, EditorError> {
        let markdown = MarkdownDocument::parse(&snapshot.markdown)?;
        let selection = Selection::default();
        let projection = ProjectionMap::from_markdown(&markdown, selection);
        let style_runs = markdown.style_runs(&projection);
        let block_runs = markdown.block_runs()?;
        let mirror = Rope::from_str(&snapshot.markdown);
        Ok(Self {
            snapshot,
            mirror,
            markdown,
            selection,
            projection,
            style_runs,
            block_runs,
            preedit: None,
            next_command_seq: 0,
            pending: None,
            needs_resync: false,
            durable_through: LiveRev(0),
        })
    }

    /// 按新的打开快照整体重建（切片 4 在 stale / 故障恢复时用）。
    ///
    /// 命令序号保持单调，避免新旧会话产生相同的 `command_id`。
    pub fn adopt_snapshot(&mut self, snapshot: OpenSnapshot) -> Result<(), EditorError> {
        let markdown = MarkdownDocument::parse(&snapshot.markdown)?;
        let selection = Selection::default();
        let projection = ProjectionMap::from_markdown(&markdown, selection);
        let style_runs = markdown.style_runs(&projection);
        let block_runs = markdown.block_runs()?;
        self.mirror = Rope::from_str(&snapshot.markdown);
        self.snapshot = snapshot;
        self.markdown = markdown;
        self.selection = selection;
        self.projection = projection;
        self.style_runs = style_runs;
        self.block_runs = block_runs;
        self.preedit = None;
        self.pending = None;
        self.needs_resync = false;
        self.durable_through = LiveRev(0);
        Ok(())
    }

    /// 当前打开快照（`live_rev` / `live_frontier` 会随确认单调推进）。
    #[must_use]
    pub fn snapshot(&self) -> &OpenSnapshot {
        &self.snapshot
    }

    /// 当前文档 ID。
    #[must_use]
    pub fn doc_id(&self) -> &str {
        &self.snapshot.doc_id
    }

    /// 文档代际。
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.snapshot.generation
    }

    /// 会话纪元。
    #[must_use]
    pub fn session_epoch(&self) -> SessionEpoch {
        self.snapshot.session_epoch
    }

    /// 当前 live 修订号（已确认的基准）。
    #[must_use]
    pub fn live_rev(&self) -> LiveRev {
        self.snapshot.live_rev
    }

    /// source 全量文本（导出 / 调试用；真源仍在会话与磁盘）。
    #[must_use]
    pub fn source_text(&self) -> String {
        self.mirror.to_string()
    }

    /// source 的 scalar 数。
    #[must_use]
    pub fn scalar_len(&self) -> usize {
        self.mirror.len_chars()
    }

    /// 当前 source selection。
    #[must_use]
    pub fn selection(&self) -> Selection {
        self.selection
    }

    /// 当前显示投影表（source 或 selection 变时已重建）。
    #[must_use]
    pub fn projection(&self) -> &ProjectionMap {
        &self.projection
    }

    /// 当前投影下的语义样式区间（display 坐标，完整覆盖显示文本）。
    ///
    /// runs 与 `projection()` 同步重建：source 变（编辑/IME 提交/远端/undo/redo）
    /// 或 selection 变（caret 进出构造）后都保持正确；无样式位置为
    /// `StyleKind::Plain`。app 每帧直接借用，不重复计算。
    #[must_use]
    pub fn style_runs(&self) -> &[StyleRun] {
        &self.style_runs
    }

    /// 块级语义区间（source 坐标，每行一个 run，完整覆盖 `[0, source_len)`）。
    ///
    /// **只在 source 变化时重建**：移动 caret / 改 selection 不会改变块级结构，
    /// 缓存保持有效。标题级别只能通过本 API 获得（`level = Some(1..=6)`）；
    /// 具体字号由 app 主题决定。
    #[must_use]
    pub fn block_runs(&self) -> &[BlockRun] {
        &self.block_runs
    }

    /// 当前逻辑显示文本（Markdown 标记按当前 selection 隐藏/显现）。
    ///
    /// 与 `self.projection().display_text()` 同源；任何编辑 / 选择变更返回后都已同步。
    #[must_use]
    pub fn display_text(&self) -> Cow<'_, str> {
        self.projection.display_text()
    }

    /// 当前逻辑显示长度（UTF-8 bytes，强类型 [`DisplayByte`]）。
    ///
    /// 注意：移动 caret 不改 source，但可能显现/隐藏构造，从而改变本值；
    /// app 应据此决定是否重新排版。
    #[must_use]
    pub fn display_len(&self) -> DisplayByte {
        self.projection.display_len()
    }

    /// source byte → display byte（偏置规则见 [`ProjectionMap`]）。
    #[must_use]
    pub fn source_to_display(&self, source: SourceByte) -> Option<DisplayByte> {
        self.projection.display_byte_at_source(source)
    }

    /// display byte → source byte（偏置规则见 [`ProjectionMap`]）。
    #[must_use]
    pub fn display_to_source(&self, display: DisplayByte) -> Option<SourceByte> {
        self.projection.source_byte_at_display(display)
    }

    /// 把当前 source 选区映射到 display 坐标，返回**有序**的 `(start, end)`。
    ///
    /// 两端按 [`ProjectionMap`] 的 source→display 偏置收敛；折叠选区返回空区间。
    /// scalar 越界（理论上不会发生，selection 恒被校验）或换算失败时返回 `None`。
    #[must_use]
    pub fn selection_in_display(&self) -> Option<(DisplayByte, DisplayByte)> {
        let source = self.markdown.source();
        let start = self.selection.start().to_source_byte(source).ok()?;
        let end = self.selection.end().to_source_byte(source).ok()?;
        let start = self.projection.display_byte_at_source(start)?;
        let end = self.projection.display_byte_at_source(end)?;
        if start.get() > end.get() {
            return None;
        }
        Some((start, end))
    }

    /// 在等确认的命令 ID（S1 单飞行）。
    #[must_use]
    pub fn pending_command(&self) -> Option<&str> {
        self.pending.as_deref()
    }

    /// 是否已失效、必须按新快照重建。
    #[must_use]
    pub fn needs_resync(&self) -> bool {
        self.needs_resync
    }

    /// 进行中的 preedit 文本（没有组合时为 `None`）。
    #[must_use]
    pub fn preedit_text(&self) -> Option<&str> {
        self.preedit.as_ref().map(|state| state.text.as_str())
    }

    /// 已 durable 到的 live 修订号（由调用方消费 [`DurableAck`] 时告知）。
    #[must_use]
    pub fn durable_through(&self) -> LiveRev {
        self.durable_through
    }

    /// 所有已确认输入是否都已 durable。
    ///
    /// 判定规则：**没有待确认命令**且 `durable_through >= live_rev`。
    /// 旧 ACK 只覆盖旧的 `live_rev`，ACK 之后继续输入会重新变成"保存中"，
    /// 直到新 ACK 的 `through_live_rev` 追上当前 `live_rev`。
    #[must_use]
    pub fn is_saved(&self) -> bool {
        self.pending.is_none() && self.durable_through >= self.snapshot.live_rev
    }

    /// 消费一条 durable 确认：水位只前进，不回退（旧 ACK 不会把新输入标成已保存）。
    pub fn apply_durable_ack(&mut self, ack: &DurableAck) {
        if ack.through_live_rev > self.durable_through {
            self.durable_through = ack.through_live_rev;
        }
    }

    /// 设置 selection；两端越界立即报错（不钳制）。
    pub fn set_selection(&mut self, selection: Selection) -> Result<(), EditorError> {
        if self.preedit.is_some() {
            return Err(EditorError::PreeditActive);
        }
        self.check_scalar(selection.anchor())?;
        self.check_scalar(selection.head())?;
        self.selection = selection;
        // 选择变 → 只重建投影（复用 MarkdownDocument）；caret 进出构造会改变隐藏集合。
        self.refresh_projection();
        Ok(())
    }

    /// 把光标折叠到 `at`。
    pub fn set_caret(&mut self, at: UnicodeScalar) -> Result<(), EditorError> {
        self.set_selection(Selection::caret(at))
    }

    /// 按 grapheme 边界移动光标；`extend = true` 时保持 anchor（扩选）。
    ///
    /// 只做交互定位：结果仍是 [`UnicodeScalar`]，不产生任何 grapheme 坐标。
    pub fn move_grapheme(&mut self, step: GraphemeStep, extend: bool) -> Result<(), EditorError> {
        if self.needs_resync {
            return Err(EditorError::ResyncRequired);
        }
        if self.preedit.is_some() {
            return Err(EditorError::PreeditActive);
        }
        let text = self.mirror.to_string();
        let head = self.selection.head();
        let Some(target) = Self::grapheme_target(&text, head, step)? else {
            return Ok(());
        };
        let anchor = if extend {
            self.selection.anchor()
        } else {
            target
        };
        self.selection = Selection::new(anchor, target);
        self.refresh_projection();
        Ok(())
    }

    /// **Backspace 语义**：有非空选区时删除选区，否则删除光标**前**一个 grapheme。
    ///
    /// 这是按键接线（app）应当调用的正式入口；不要在 app 里自己拼 `ScalarEdit`。
    ///
    /// - `Ok(Some(request))`：已经走 [`Self::compose_edits`] 的同一条路径（乐观写入镜像、
    ///   设置 pending、沿用 `base_live_rev` / 排序不重叠 / `UnicodeScalar` 单位），
    ///   调用方把它交给会话，并把 outcome 交回 [`Self::apply_outcome`]；
    /// - `Ok(None)`：无操作（空文档 / 已在文档头 / 无可删 grapheme），**不产生请求**，
    ///   不动 `live_rev`；
    /// - `Err(EditorError::PreeditActive)`：IME 组合中先走 commit/cancel，不要绕过闸门。
    pub fn delete_backward(&mut self) -> Result<Option<EditRequest>, EditorError> {
        self.delete_grapheme(GraphemeStep::Backward)
    }

    /// **Delete 语义**：有非空选区时删除选区，否则删除光标**后**一个 grapheme。
    ///
    /// 与 [`Self::delete_backward`] 对称；`Delete` 键接线用本方法。
    pub fn delete_forward(&mut self) -> Result<Option<EditRequest>, EditorError> {
        self.delete_grapheme(GraphemeStep::Forward)
    }

    /// 按 grapheme 删除：有选区删选区（一个 `ScalarEdit`），否则删光标前/后一整个 grapheme。
    ///
    /// 这是方向参数化的底层原语；按键路径请用 [`Self::delete_backward`] /
    /// [`Self::delete_forward`]。产生的 [`ScalarEdit`] 端点仍是 scalar 序号；
    /// grapheme 只用于**决定删哪一段**，不会进入 CRDT 坐标。
    pub fn delete_grapheme(
        &mut self,
        step: GraphemeStep,
    ) -> Result<Option<EditRequest>, EditorError> {
        self.ensure_editable()?;
        let selection = self.selection;
        if !selection.is_empty() {
            return self.compose_replace_selection("");
        }
        let text = self.mirror.to_string();
        let head = selection.head();
        let Some(target) = Self::grapheme_target(&text, head, step)? else {
            return Ok(None);
        };
        let edit = ScalarEdit {
            start: target.min(head),
            end: target.max(head),
            replacement: String::new(),
        };
        self.compose_edits(vec![edit]).map(Some)
    }

    /// 用 `replacement` 替换当前选区（空选区即插入）。
    ///
    /// 空选区的空替换是 no-op，返回 `Ok(None)`，不产生请求。
    pub fn compose_replace_selection(
        &mut self,
        replacement: &str,
    ) -> Result<Option<EditRequest>, EditorError> {
        let selection = self.selection;
        if selection.is_empty() && replacement.is_empty() {
            return Ok(None);
        }
        let edit = ScalarEdit {
            start: selection.start(),
            end: selection.end(),
            replacement: replacement.to_owned(),
        };
        self.compose_edits(vec![edit]).map(Some)
    }

    /// 提交一组编辑：校验（排序 / 不重叠 / 不越界）→ 乐观写入镜像 → 生成请求。
    ///
    /// - `base_live_rev` 取当前已确认的 `live_rev`（S1 单飞行，因此它一定就是
    ///   会话眼中的当前基准）；
    /// - `command_id` 形如 `doc_id:session_epoch:序号`，在控制器生命周期内唯一，
    ///   作为幂等键；
    /// - `origin` 固定为 `"local"`（供会话排除后台变更的撤销），`undo_group` 为
    ///   `None`（S1 没有 UI 分组，交给会话默认分组）。
    pub fn compose_edits(&mut self, edits: Vec<ScalarEdit>) -> Result<EditRequest, EditorError> {
        self.ensure_editable()?;
        if edits.is_empty() {
            return Err(EditorError::EmptyEditSet);
        }
        self.validate_edits(&edits)?;

        let command_id = self.next_command_id();
        let base_live_rev = self.snapshot.live_rev;
        self.apply_scalar_edits(&edits)?;
        self.pending = Some(command_id.clone());

        Ok(EditRequest {
            doc_id: self.snapshot.doc_id.clone(),
            generation: self.snapshot.generation,
            session_epoch: self.snapshot.session_epoch,
            command_id,
            base_live_rev,
            edits,
            origin: LOCAL_ORIGIN.to_owned(),
            undo_group: None,
        })
    }

    /// 消费一条会话结局。
    ///
    /// - `Applied`：本地回声只确认；非回声按 `splice` 应用一次；重复投递忽略；
    /// - `Stale` / `Rejected`：请求未被应用，控制器标记需要重建并返回错误。
    ///
    /// S1 不做并发合并：本地回声确认时严格按协议**不再**应用 `splice`；若
    /// `Applied.merge.concurrent` 带回了远端变更，那条路径由切片 4 的并发端到端
    /// 测试覆盖（本模型不自行猜测合并偏移）。
    pub fn apply_outcome(&mut self, outcome: EditOutcome) -> Result<OutcomeEffect, EditorError> {
        if self.needs_resync {
            return Err(EditorError::ResyncRequired);
        }
        match outcome {
            EditOutcome::Applied(applied) => self.apply_applied(applied),
            EditOutcome::Stale(stale) => {
                let command_id = stale.command_id;
                let current_live_rev = stale.current_live_rev;
                if self.pending.as_deref() == Some(command_id.as_str()) {
                    self.pending = None;
                }
                self.needs_resync = true;
                self.preedit = None;
                Err(EditorError::StaleBase {
                    command_id,
                    current_live_rev,
                })
            }
            EditOutcome::Rejected(rejected) => {
                let command_id = rejected.command_id;
                let reason = rejected.reason;
                if reason == RejectReason::Backpressure {
                    // 背压是"暂不受理"：请求没有进 Loro、也没有进 journal，镜像里的乐观
                    // 输入与 pending 命令都保留，调用方可用**同一个 EditRequest**（同一
                    // command_id）重试；这不是不可恢复状态，不能标记 needs_resync。
                    return Err(EditorError::Backpressured(command_id));
                }
                if self.pending.as_deref() == Some(command_id.as_str()) {
                    self.pending = None;
                }
                self.needs_resync = true;
                self.preedit = None;
                Err(EditorError::Rejected { command_id, reason })
            }
        }
    }

    /// 应用一条远端 / 后台 live 变更（必须应用，且只应用一次）。
    ///
    /// `from_rev` 与当前 `live_rev` 不连续时返回 [`EditorError::RevGap`] 并标记
    /// 需要重建；`to_rev <= 当前 live_rev` 的重复投递直接忽略。
    ///
    /// **并发边界（S1）**：本地命令尚未确认时镜像已含乐观输入，而 `splice` 是相对
    /// live 文本文档的坐标；此时无法安全叠加，返回
    /// [`EditorError::ConcurrentDeltaWithPending`] 并标记重建，**不做猜测式 rebase**
    /// （切片 4 的端到端并发测试再处理）。
    pub fn apply_live_delta(&mut self, delta: LiveDelta) -> Result<OutcomeEffect, EditorError> {
        if self.needs_resync {
            return Err(EditorError::ResyncRequired);
        }
        if delta.doc_id != self.snapshot.doc_id {
            return Err(EditorError::WrongDoc {
                expected: self.snapshot.doc_id.clone(),
                actual: delta.doc_id,
            });
        }
        if delta.to_rev <= self.snapshot.live_rev {
            return Ok(OutcomeEffect::IgnoredDuplicate);
        }
        if let Some(command_id) = &self.pending {
            self.needs_resync = true;
            self.preedit = None;
            return Err(EditorError::ConcurrentDeltaWithPending(command_id.clone()));
        }
        if delta.from_rev != self.snapshot.live_rev {
            self.needs_resync = true;
            self.preedit = None;
            return Err(EditorError::RevGap {
                expected: self.snapshot.live_rev,
                actual: delta.from_rev,
            });
        }

        self.apply_splice(&delta.splice)?;
        self.snapshot.live_rev = delta.to_rev;
        self.snapshot.live_frontier = delta.live_frontier;
        Ok(OutcomeEffect::MirrorUpdated)
    }

    /// 开始 / 更新 IME 组合。
    ///
    /// 组合期间**不允许**提交正文编辑（[`EditorError::PreeditActive`]），因此
    /// preedit 永远不会变成 `source`，也不会派生出 [`ScalarEdit`]。
    pub fn set_preedit(&mut self, text: &str) -> Result<(), EditorError> {
        if self.needs_resync {
            return Err(EditorError::ResyncRequired);
        }
        if let Some(command_id) = &self.pending {
            return Err(EditorError::CommandInFlight(command_id.clone()));
        }
        if text.is_empty() {
            self.preedit = None;
            return Ok(());
        }
        match &mut self.preedit {
            Some(state) => state.text = text.to_owned(),
            None => {
                self.preedit = Some(PreeditState {
                    range: self.selection,
                    text: text.to_owned(),
                });
            }
        }
        Ok(())
    }

    /// 取消组合：**不修改** source，也不产生请求。
    pub fn clear_preedit(&mut self) {
        self.preedit = None;
    }

    /// 当前输入文档（source 在组合区间处替换成 preedit）；没有组合时等于 source。
    pub fn input_document(&self) -> Result<InputDocument, EditorError> {
        let source = self.mirror.to_string();
        let Some(state) = &self.preedit else {
            return Ok(InputDocument::from_source(&source));
        };
        InputDocument::compose(&source, state.range, &state.text)
    }

    /// IME 提交：把 `committed` 作为一次普通替换提交，随后清空组合状态。
    ///
    /// 只有 `committed`（而不是 preedit 内容）会进入编辑请求；空提交且空选区时
    /// 返回 `Ok(None)` 并结束组合。
    pub fn commit_preedit(&mut self, committed: &str) -> Result<Option<EditRequest>, EditorError> {
        if self.needs_resync {
            return Err(EditorError::ResyncRequired);
        }
        let Some(state) = self.preedit.take() else {
            return Err(EditorError::NoPreedit);
        };
        if state.range.is_empty() && committed.is_empty() {
            return Ok(None);
        }
        let edit = ScalarEdit {
            start: state.range.start(),
            end: state.range.end(),
            replacement: committed.to_owned(),
        };
        match self.compose_edits(vec![edit]) {
            Ok(request) => Ok(Some(request)),
            Err(error) => {
                // 提交失败（例如仍有在飞命令）时恢复组合，避免静默吞掉输入。
                self.preedit = Some(state);
                Err(error)
            }
        }
    }

    // ---- 内部实现 ----

    fn check_scalar(&self, at: UnicodeScalar) -> Result<(), EditorError> {
        let len = self.mirror.len_chars();
        if at.get() > len {
            Err(EditorError::ScalarOutOfBounds {
                index: at.get(),
                len,
            })
        } else {
            Ok(())
        }
    }

    fn ensure_editable(&self) -> Result<(), EditorError> {
        if self.needs_resync {
            return Err(EditorError::ResyncRequired);
        }
        if let Some(command_id) = &self.pending {
            return Err(EditorError::CommandInFlight(command_id.clone()));
        }
        if self.preedit.is_some() {
            return Err(EditorError::PreeditActive);
        }
        Ok(())
    }

    fn validate_edits(&self, edits: &[ScalarEdit]) -> Result<(), EditorError> {
        let len = self.mirror.len_chars();
        for (index, edit) in edits.iter().enumerate() {
            let start = edit.start.get();
            let end = edit.end.get();
            if start > end {
                return Err(EditorError::ReversedEdit { start, end });
            }
            if end > len {
                return Err(EditorError::ScalarOutOfBounds { index: end, len });
            }
            if index > 0 {
                let previous = &edits[index - 1];
                if start < previous.start.get() {
                    return Err(EditorError::EditsNotSorted {
                        previous_start: previous.start.get(),
                        next_start: start,
                    });
                }
                if start < previous.end.get() {
                    return Err(EditorError::OverlappingEdits {
                        previous_end: previous.end.get(),
                        next_start: start,
                    });
                }
            }
        }
        Ok(())
    }

    fn next_command_id(&mut self) -> CommandId {
        let sequence = self.next_command_seq;
        self.next_command_seq = self.next_command_seq.saturating_add(1);
        format!(
            "{}:{}:{}",
            self.snapshot.doc_id, self.snapshot.session_epoch.0, sequence
        )
    }

    fn apply_scalar_edits(&mut self, edits: &[ScalarEdit]) -> Result<(), EditorError> {
        // 与协议一致：按 edits 逆序应用，保证每条编辑的基准坐标仍然有效。
        for edit in edits.iter().rev() {
            let start = edit.start.get();
            let end = edit.end.get();
            if start < end {
                self.mirror.remove(start..end);
            }
            if !edit.replacement.is_empty() {
                self.mirror.insert(start, &edit.replacement);
            }
        }
        // 先更新 caret（依赖新文本的 scalar 长度），再按新 selection 重建投影；
        // 两步都执行完再返回错误，避免 source 已变而投影仍停在旧文本。
        let caret_result = self.update_caret_after_edits(edits);
        let projection_result = self.refresh_markdown();
        caret_result?;
        projection_result
    }

    /// 光标落在最后一条编辑（`start` 最大）的替换文本之后，且要计入前面编辑的净位移。
    fn update_caret_after_edits(&mut self, edits: &[ScalarEdit]) -> Result<(), EditorError> {
        let Some(last) = edits.last() else {
            return Ok(());
        };
        let mut caret =
            i64::try_from(last.start.get()).map_err(|_| EditorError::ArithmeticOverflow)?;
        for edit in &edits[..edits.len() - 1] {
            let inserted = i64::try_from(units::scalar_len(&edit.replacement))
                .map_err(|_| EditorError::ArithmeticOverflow)?;
            let removed = i64::try_from(edit.end.get() - edit.start.get())
                .map_err(|_| EditorError::ArithmeticOverflow)?;
            let delta = inserted
                .checked_sub(removed)
                .ok_or(EditorError::ArithmeticOverflow)?;
            caret = caret
                .checked_add(delta)
                .ok_or(EditorError::ArithmeticOverflow)?;
        }
        let inserted_last = i64::try_from(units::scalar_len(&last.replacement))
            .map_err(|_| EditorError::ArithmeticOverflow)?;
        let caret = caret
            .checked_add(inserted_last)
            .ok_or(EditorError::ArithmeticOverflow)?;
        let caret = usize::try_from(caret).map_err(|_| EditorError::ArithmeticOverflow)?;
        self.check_scalar(UnicodeScalar::new(caret))?;
        self.selection = Selection::caret(UnicodeScalar::new(caret));
        Ok(())
    }

    /// source 变：全量重扫 Markdown + 按当前 selection 重建投影。
    ///
    /// v1 代价 O(source_len)；后续可增量重扫（见模块文档）。
    fn refresh_markdown(&mut self) -> Result<(), EditorError> {
        let text = self.mirror.to_string();
        self.markdown = MarkdownDocument::parse(&text).map_err(EditorError::Markdown)?;
        // 块级语义只依赖 source：与投影一起在 source 变化时重建。
        self.block_runs = self.markdown.block_runs()?;
        self.refresh_projection();
        Ok(())
    }

    /// selection 变：复用已缓存的 MarkdownDocument，只重建投影与样式区间。
    ///
    /// 投影 / 样式构造都不会失败（选区非法时 `ProjectionMap::from_markdown`
    /// 保守退回 identity）。
    fn refresh_projection(&mut self) {
        self.projection = ProjectionMap::from_markdown(&self.markdown, self.selection);
        let style_runs = self.markdown.style_runs(&self.projection);
        self.style_runs = style_runs;
    }

    fn apply_applied(&mut self, applied: Applied) -> Result<OutcomeEffect, EditorError> {
        // 重复 / 过期投递：镜像不再改动（同一 delta 只应用一次）。
        if applied.live_rev <= self.snapshot.live_rev {
            return Ok(OutcomeEffect::IgnoredDuplicate);
        }

        let is_pending = self.pending.as_deref() == Some(applied.command_id.as_str());
        let effect = if is_pending {
            if !applied.local_echo {
                // 本 crate 已乐观写入，协议却说"未本地写入"：两种权威冲突，不猜。
                self.needs_resync = true;
                self.preedit = None;
                return Err(EditorError::MirrorDiverged(applied.command_id));
            }
            // 本地回声：compose 时已写入镜像，禁止再按 splice 二次应用。
            self.pending = None;
            OutcomeEffect::LocalEchoConfirmed
        } else if applied.local_echo {
            // 回声没有对应乐观编辑：说明镜像缺文本，不能假装成功。
            self.needs_resync = true;
            self.preedit = None;
            return Err(EditorError::LocalEchoWithoutPending(applied.command_id));
        } else {
            self.apply_splice(&applied.splice)?;
            OutcomeEffect::MirrorUpdated
        };

        self.snapshot.live_rev = applied.live_rev;
        self.snapshot.live_frontier = applied.live_frontier;
        Ok(effect)
    }

    fn apply_splice(&mut self, splice: &DeltaSplice) -> Result<(), EditorError> {
        let at = splice.at.get();
        let delete =
            usize::try_from(splice.delete_scalars).map_err(|_| EditorError::ArithmeticOverflow)?;
        let end = at
            .checked_add(delete)
            .ok_or(EditorError::ArithmeticOverflow)?;
        let len = self.mirror.len_chars();
        if at > len {
            return Err(EditorError::ScalarOutOfBounds { index: at, len });
        }
        if end > len {
            return Err(EditorError::ScalarOutOfBounds { index: end, len });
        }

        if delete > 0 {
            self.mirror.remove(at..end);
        }
        if !splice.insert.is_empty() {
            self.mirror.insert(at, &splice.insert);
        }

        let inserted = units::scalar_len(&splice.insert);
        self.selection = transform_selection(self.selection, at, end, inserted);
        // 组合基准已改变；S1 不猜测重映射，直接取消组合（不产生任何 ScalarEdit）。
        self.preedit = None;
        self.refresh_markdown()
    }

    /// 计算相邻 grapheme 边界。
    ///
    /// 只把 `&str` 上的 grapheme 边界当作**交互定位**，返回的仍是 scalar 坐标。
    fn grapheme_target(
        text: &str,
        caret: UnicodeScalar,
        step: GraphemeStep,
    ) -> Result<Option<UnicodeScalar>, EditorError> {
        let caret_byte = caret.to_source_byte(text)?.get();
        let boundaries = units::grapheme_boundaries(text);
        let target = match step {
            GraphemeStep::Backward => boundaries
                .iter()
                .copied()
                .take_while(|&byte| byte < caret_byte)
                .last(),
            GraphemeStep::Forward => boundaries.iter().copied().find(|&byte| byte > caret_byte),
        };
        match target {
            None => Ok(None),
            Some(byte) => {
                let scalar = SourceByte::new(byte).to_scalar(text)?;
                Ok(Some(scalar))
            }
        }
    }
}

/// 把 selection 两端按一次 splice 变换（ADR 0002 §8：远端变更必须被变换）。
///
/// 端点落在被删区间内时收敛到 `at`（该点在新文本里仍然存在）；不做"四舍五入"。
fn transform_selection(selection: Selection, at: usize, end: usize, inserted: usize) -> Selection {
    let anchor = transform_position(selection.anchor().get(), at, end, inserted);
    let head = transform_position(selection.head().get(), at, end, inserted);
    Selection::new(UnicodeScalar::new(anchor), UnicodeScalar::new(head))
}

fn transform_position(position: usize, at: usize, end: usize, inserted: usize) -> usize {
    if position <= at {
        position
    } else if position >= end {
        position - (end - at) + inserted
    } else {
        at
    }
}
