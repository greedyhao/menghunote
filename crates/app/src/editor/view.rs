//! 编辑器视图：`EditorController` + `SessionDriver` 的接线、键盘导航、状态栏。
//!
//! ## 线程模型
//!
//! `SessionDriver::pump()` 只做内存操作（读 ticker、检查 250ms 固定窗口、
//! 收会话 / B 物化事件），不碰 SQLite。它仍然放在**主线程**上跑，但由
//! `cx.spawn` + `background_executor().timer(..)` 驱动：
//!
//! ```text
//! 主线程: spawn(future) → timer(50ms).await（后台线程计时）→ 回到主线程 pump()
//! ```
//!
//! 因此 UI 线程不会被 IO 阻塞；`flush` 的计时也**不在这里**——那是
//! `SessionDriver` 既有的 250ms / 64 KiB 调度（ADR 0002 §9），app 不另写一套。
//! `driver.close(timeout)` 只在窗口关闭时调用，且有界（见 [`CLOSE_TIMEOUT`]）。
//!
//! ## 切片 6b：IME 输入闭环
//!
//! 平台文本输入（含 IME 组合）由 `impl EntityInputHandler for EditorView`（`input` 模块）
//! 接入；本模块负责它背后那些**必须串行、必须可观测**的动作：
//!
//! 1. **组合闸门**：第一次 preedit 到达时 `driver.begin_composition()`，commit/cancel 时
//!    `end_composition()` 并按序把返回的 `LiveDelta` 应用镜像（**只**从返回值取，
//!    不再走 `DriverEvent::Remote`，避免同一条 delta 应用两次）；
//! 2. **一次 commit 只有一个 `EditRequest`**：所有编辑都过 [`EditorView::submit_request`]，
//!    那里打日志（`请求#N … edits=M`）——日志就是"只发一次"的证据；
//! 3. **组合期主动不发**：组合中若还有用户命令（快捷键 / 别的路径），先挂起，
//!    `end_composition()` 之后用**同一 `command_id`** 重试（会话侧幂等）；
//!    万一仍收到 `Rejected{CompositionActive}`，不把它交给控制器（否则会进 `needs_resync`）；
//! 4. **Undo/Redo**：`Ctrl+Z` / `Ctrl+Shift+Z` 走会话唯一的 `UndoManager`，
//!    组合态下先取消组合（丢弃 overlay，不提交）再撤销。
//!
//! ## 为什么 `scroll_y` 放在实体里
//!
//! 滚动位置是**用户可观察状态**（键盘翻页要基于它，render 也要读它），
//! 所以放实体字段 + `notify()`；而折行结果与上一帧几何只影响绘制，放在
//! [`EditorShared`] 里（见 `shared` 模块的说明）。

use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, px, App, Context, FocusHandle, Focusable, FontWeight, KeyDownEvent, MouseButton,
    MouseDownEvent, Pixels, Render, SharedString, Window,
};
use menghunote_editor::{
    Bias, DisplayByte, EditorController, EditorError, GraphemeStep, InputDocument, Selection,
    SourceByte, StyleKind, UnicodeScalar,
};
#[cfg(feature = "dev-sync")]
use menghunote_storage::session::{
    AdoptError, AdoptOutcome, AppliedHeads, ExternalCommitLease, ExternalCommitTicket, GateError,
    GateState, PreparedCommittedDelta,
};
use menghunote_storage::session::{
    CloseReport, DriverEvent, EditOutcome, EditRequest, LiveDelta, RejectReason, SessionDriver,
};

use super::element::EditorElement;
use super::layout::LayoutFrame;
use super::shared::EditorShared;
use super::theme;

/// 会话驱动 pump 间隔：只做内存操作，50 ms 足够跟上 250 ms 的 flush 窗口。
const PUMP_INTERVAL: Duration = Duration::from_millis(50);
/// 关闭窗口时的有界排空上限（ADR 0002 §9：`close` 必须有界）。
const CLOSE_TIMEOUT: Duration = Duration::from_millis(1500);

/// 状态栏右侧的固定提示。
const HINT: &str =
    "切片 6b：可输入 · Ctrl+Z 撤销 / Ctrl+Shift+Z 重做 · 中文走 IME（组合文本上屏前不落库）";

/// IME 组合状态。
///
/// preedit 本身在 `EditorController` 的 overlay 里（[`EditorController::set_preedit`]），
/// 这里只记"平台这一次组合"在 app 侧需要的东西：闸门是否已开、IME 内部选区、
/// 以及组合期被挂起的待重试请求。
#[derive(Default)]
struct Composition {
    /// 是否已经调过 `driver.begin_composition()`（避免把深度叠加两次）。
    active: bool,
    /// preedit 内的 UTF-16 选区（IME 自己的光标位置，目前只用于日志）。
    selected: Option<Range<usize>>,
    /// 因 `CompositionActive` 未被受理、等组合结束后用**同一 `command_id`** 重试的请求。
    pending_retry: Option<EditRequest>,
}

/// 编辑器窗口的主视图。
/// 编辑器对宿主（`AppShell`）的少量事件。
///
/// 用事件而不是回调：侧栏要知道"按了 F2 / 点进了正文"，但不该把 shell 的引用塞进
/// 编辑器（依赖方向反了）。宿主 `cx.subscribe` 即可。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorEvent {
    /// 按了 `F2`：请宿主重命名侧栏当前选中的节点。
    RenameRequested,
    /// 正文被按下鼠标：宿主可用它取消行内编辑。
    PointerDown,
    /// 按了 `Ctrl+K` / `Ctrl+F`：请宿主打开搜索浮层。
    SearchRequested,
}

/// 这个按键是不是"打开搜索"的快捷键（**纯函数**，便于单测）。
///
/// 规则：`Ctrl` + `K` 或 `Ctrl` + `F`，且**不带** Alt / Shift / Cmd。
/// 显式排除 Shift 是为了不和"Ctrl+Shift+F"这类全局搜索/格式化快捷键撞车。
#[must_use]
pub fn is_search_shortcut(keystroke: &gpui::Keystroke) -> bool {
    let modifiers = keystroke.modifiers;
    modifiers.control
        && !modifiers.alt
        && !modifiers.shift
        && !modifiers.platform
        && matches!(keystroke.key.as_str(), "k" | "f")
}

impl gpui::EventEmitter<EditorEvent> for EditorView {}

pub struct EditorView {
    controller: EditorController,
    driver: SessionDriver,
    focus_handle: FocusHandle,
    shared: Rc<EditorShared>,
    /// 已向下滚过的像素数（非负，0 = 文档顶部）。
    scroll_y: f32,
    /// 上下移动时保持的视觉列（相对行首的像素）；水平定位会清空。
    preferred_x: Option<Pixels>,
    /// 拖拽锚点（按下时的 scalar）；`None` = 当前没有拖拽。
    drag_anchor: Option<UnicodeScalar>,
    /// 是否已经做过首帧聚焦（避免每帧重复 `window.focus`）。
    focused_once: bool,
    /// 关闭排空结果（留痕用，只在 stderr 打一行）。
    close_report: Option<CloseReport>,
    /// 状态栏展示的 vault 路径。
    vault_label: String,
    /// IME 组合状态（preedit 只活在控制器 overlay 里，绝不进 CRDT）。
    composition: Composition,
    /// 已发出的 `EditRequest` 计数：日志里用来证明"一次 commit 只有一个请求"。
    request_seq: u64,
    /// 下一次绘制选区时是否把几何明细打到 stderr（拖拽/点击时置位，task-19 诊断用）。
    dump_selection: bool,
    /// 上一次观察到的「外部提交闸门持有中」状态（ADR 0009 切片 3b）。
    ///
    /// 只用于**探测状态变化**（变化时重绘一次，让 `editor-sync-held` 标记与状态栏文案
    /// 跟上）；真正的冻结判定每次都直接问驱动，绝不用这份缓存。
    held: bool,
    /// 已被驱动受理的**本地**编辑代数（ADR 0012 S3 自动同步的触发源）。
    ///
    /// 只在 `EditOutcome::Applied` 上自增：远端 adopt 不经过 `submit_request`，
    /// 所以"远端 ACK 不触发自动同步"是结构性的，不依赖调用点自觉。
    #[cfg(feature = "dev-sync")]
    local_edit_seq: u64,
}

impl EditorView {
    /// 组装视图并启动后台 pump 循环。
    pub fn new(
        controller: EditorController,
        driver: SessionDriver,
        vault_label: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = Self {
            controller,
            driver,
            focus_handle: cx.focus_handle(),
            shared: EditorShared::new(),
            scroll_y: 0.0,
            preferred_x: None,
            drag_anchor: None,
            focused_once: false,
            close_report: None,
            vault_label,
            composition: Composition::default(),
            request_seq: 0,
            dump_selection: false,
            held: false,
            #[cfg(feature = "dev-sync")]
            local_edit_seq: 0,
        };
        view.spawn_pump(cx);
        view
    }

    /// 启动 `pump` 循环：后台计时、主线程收事件。
    fn spawn_pump(&self, cx: &mut Context<Self>) {
        let timers = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            loop {
                timers.timer(PUMP_INTERVAL).await;
                let updated = this.update(cx, |view, cx| {
                    if view.pump() {
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    // 实体已被释放（窗口关闭）：结束循环，不要继续 await。
                    break;
                }
            }
        })
        .detach();
    }

    /// 收一次驱动事件；返回是否影响了可视状态（只有影响时才 `notify`）。
    fn pump(&mut self) -> bool {
        let mut changed = false;
        for event in self.driver.pump() {
            match event {
                DriverEvent::Durable(ack) => {
                    self.controller.apply_durable_ack(&ack);
                    eprintln!(
                        "[menghunote] durable 水位 → live_rev={}（is_saved={}）",
                        ack.through_live_rev.0,
                        self.controller.is_saved()
                    );
                    changed = true;
                }
                DriverEvent::Remote(delta) => match self.controller.apply_live_delta(delta) {
                    Ok(_) => changed = true,
                    Err(error) => {
                        eprintln!("[menghunote] 远端变更未能应用（等待重建）：{error}");
                    }
                },
                // flush 已提交但后台还没写完：状态栏由 `is_saved()` 表达，无需额外处理。
                DriverEvent::FlushScheduled { .. } => {}
                // B 物化只写 `notes.body_md`，本切片视图不读它。
                DriverEvent::Materialized(_) => {}
                DriverEvent::MaterializeFailed { note_id, message } => {
                    eprintln!("[menghunote] B 物化失败 note={note_id}：{message}");
                }
            }
        }
        // 组合期间延后的远端可能因背压没排空：非组合态下每次 pump 都补一次。
        if !self.is_composing() && self.driver.deferred_remote_len() > 0 {
            self.drain_deferred_remote();
            changed = true;
        }
        if changed {
            // 远端变更可能让内容变短，滚动量必须重新夹取，否则会滚到空白区。
            self.clamp_scroll();
        }
        // 闸门状态可能在没有事件的情况下变化（begin/poll/end 由 worker/测试驱动）：
        // 变化时重绘一次，`editor-sync-held` 标记与状态栏文案才不会滞后一帧。
        if self.sync_held_state() {
            changed = true;
        }
        changed
    }

    // ---- 外部提交闸门（ADR 0009 切片 3b）----

    /// 外部提交闸门是否持有中（Held）：期间输入/撤销/粘贴一律**不落到驱动**。
    #[must_use]
    pub fn is_sync_held(&self) -> bool {
        self.driver.is_external_commit_held()
    }

    /// 同步缓存的闸门状态；返回**是否发生变化**（仅用于触发一次重绘与一行日志）。
    fn sync_held_state(&mut self) -> bool {
        let held = self.driver.is_external_commit_held();
        if held == self.held {
            return false;
        }
        self.held = held;
        if held {
            eprintln!(
                "[menghunote] 外部提交持有闸门：冻结输入 / 撤销重做 / 粘贴（Held 结束后自动恢复）"
            );
        } else {
            eprintln!("[menghunote] 外部提交闸门已释放：恢复输入");
        }
        true
    }

    /// Held 期间冻结一个写入入口：返回 `true` 表示调用方必须**直接忽略**本次输入。
    ///
    /// 冻结放在 app 侧（而不是靠驱动拒绝）是为了避免每按一个键就走一次
    /// `Rejected{NotReady}` —— 那会把控制器标成 `needs_resync`（见
    /// [`Self::apply_outcome_guarded`] 的说明），也会刷屏日志。
    ///
    /// 这里**只读驱动**、不动 `held` 缓存：状态变化的重绘由 `pump` /
    /// [`Self::note_gate_state`] 负责（它们变了就 `notify`），否则这条热路径会把
    /// "需要重绘"的信号吃掉。
    fn frozen_by_external_commit(&mut self) -> bool {
        self.driver.is_external_commit_held()
    }

    /// 本帧源码快照（绘制入口）。
    #[must_use]
    pub fn source_text(&self) -> String {
        self.controller.source_text()
    }

    /// 布局用文本：**display**（Markdown 标记按当前 selection 隐藏/显现）。
    pub(crate) fn display_text(&self) -> String {
        self.controller.display_text().into_owned()
    }

    /// 逐**源码行**的度量（字号/行高/是否等宽），来自块级语义 `block_runs()`。
    ///
    /// 标题按 level 放大，围栏代码块用等宽字体，其余正文默认。
    pub(crate) fn block_metrics(&self) -> Vec<super::layout::LineMetric> {
        use menghunote_editor::BlockKind;
        // 引用缩进要看**源码行**的前缀（`>` 的层数），投影里 `>` 是隐藏的。
        let source = self.controller.source_text();
        let source_lines: Vec<&str> = source.split_inclusive('\n').collect();
        self.controller
            .block_runs()
            .iter()
            .enumerate()
            .map(|(index, run)| {
                // 引用行每层 16px；层数从源码行前缀数出来（`> > x` = 2 层）。
                let indent = if run.kind() == BlockKind::BlockQuote {
                    let line = source_lines.get(index).copied().unwrap_or("");
                    super::layout::quote_indent(super::layout::quote_depth(line))
                } else {
                    0.0
                };
                match run.kind() {
                    BlockKind::Heading => {
                        let level = run.level().unwrap_or(1);
                        let font_size = theme::heading_font_size(level);
                        super::layout::LineMetric {
                            font_size,
                            line_height: theme::line_height_for(font_size),
                            mono: false,
                            indent,
                        }
                    }
                    BlockKind::FencedCode => super::layout::LineMetric {
                        font_size: theme::FONT_SIZE,
                        line_height: theme::DEFAULT_LINE_HEIGHT,
                        mono: true,
                        indent,
                    },
                    BlockKind::BlockQuote | BlockKind::ListItem | BlockKind::Paragraph => {
                        super::layout::LineMetric {
                            indent,
                            ..super::layout::LineMetric::body()
                        }
                    }
                }
            })
            .collect()
    }

    /// 本帧的语义样式段（display 字节区间，完整覆盖全文）。
    pub(crate) fn style_runs_display(&self) -> Vec<(std::ops::Range<usize>, StyleKind)> {
        self.controller
            .style_runs()
            .iter()
            .map(|run| (run.start().get()..run.end().get(), run.kind()))
            .collect()
    }

    /// 当前选区在 **display** 坐标下的字节区间（`None` = 换算失败，不画）。
    pub(crate) fn selection_in_display_range(&self) -> Option<std::ops::Range<usize>> {
        let (start, end) = self.controller.selection_in_display()?;
        Some(start.get()..end.get())
    }

    /// 光标在 **display** 坐标下的字节位置。
    pub(crate) fn display_caret(&self) -> Option<usize> {
        let source = self.source_byte_of_scalar(self.controller.selection().head())?;
        self.controller
            .source_to_display(SourceByte::new(source))
            .map(|display| display.get())
    }

    /// source scalar → source 字节。
    fn source_byte_of_scalar(&self, scalar: UnicodeScalar) -> Option<usize> {
        let text = self.controller.source_text();
        scalar.to_source_byte(&text).ok().map(|byte| byte.get())
    }

    /// source scalar → display 字节（布局坐标系）。
    fn display_byte_of_scalar(&self, scalar: UnicodeScalar) -> Option<usize> {
        let source = self.source_byte_of_scalar(scalar)?;
        self.controller
            .source_to_display(SourceByte::new(source))
            .map(|display| display.get())
    }

    /// display 字节（布局坐标系）→ source scalar。
    fn scalar_at_display_byte(&self, display: usize) -> Option<UnicodeScalar> {
        self.scalar_at_display_byte_biased(display, Bias::Right)
    }

    /// 带偏置的 display 字节 → scalar（命中时用：右半 `Left`、左半 `Right`）。
    fn scalar_at_display_byte_biased(&self, display: usize, bias: Bias) -> Option<UnicodeScalar> {
        let source = self
            .controller
            .display_to_source_biased(DisplayByte::new(display), bias)?
            .get();
        let text = self.controller.source_text();
        scalar_of_byte(&text, source)
    }

    /// 当前 source selection。
    #[must_use]
    pub fn selection(&self) -> Selection {
        self.controller.selection()
    }

    /// 当前滚动偏移（像素，正 = 已向下滚）。
    #[must_use]
    pub fn scroll_y(&self) -> f32 {
        self.scroll_y
    }

    /// 是否全部已 durable（状态栏 `已保存 / 保存中` 的来源）。
    #[must_use]
    pub fn is_saved(&self) -> bool {
        self.controller.is_saved()
    }

    /// 请求在下一次绘制选区时输出几何明细（task-19 诊断：软折行选区的首末行范围）。
    pub(crate) fn request_selection_dump(&mut self) {
        self.dump_selection = true;
    }

    /// 取走"是否要 dump"的一次性标记。
    pub(crate) fn take_selection_dump(&mut self) -> bool {
        core::mem::take(&mut self.dump_selection)
    }

    /// 当前 preedit（组合文本）；没有组合返回 `None`。
    ///
    /// 它**只**用于绘制 overlay 与状态栏文案，永远不会被写进 source。
    #[must_use]
    pub fn preedit_text(&self) -> Option<&str> {
        self.controller.preedit_text()
    }

    /// 组合是否进行中（控制器有 preedit 或存储侧闸门开着）。
    #[must_use]
    pub fn is_composing(&self) -> bool {
        self.controller.preedit_text().is_some() || self.driver.is_composing()
    }

    /// 当前虚拟输入文档（source 在选区处替换成 preedit）。
    pub(crate) fn input_document(&self) -> Result<InputDocument, EditorError> {
        self.controller.input_document()
    }

    /// **有界排空**：`end_composition()` → flush(Close) → 等 durable → 等 B 物化。
    ///
    /// 窗口关闭与**切换文档**共用这一条路径；`timed_out == true` 表示没排空，
    /// 调用方（切换文档）必须据此**中止切换**并保留现场 —— 不许静默丢编辑。
    pub fn drain_and_close(&mut self, timeout: Duration) -> CloseReport {
        if let Some(report) = &self.close_report {
            return report.clone();
        }
        // 关闭前先结束组合闸门（丢弃 overlay，不产生请求），否则延后的远端导入
        // 会随会话一起丢掉，且 `close` 期间不会自动合并。
        self.controller.clear_preedit();
        if self.composition.active || self.driver.is_composing() {
            self.composition.active = false;
            self.composition.selected = None;
            let deltas = self.driver.end_composition();
            eprintln!(
                "[menghunote] 关闭前结束组合：合并延后远端 {} 条",
                deltas.len()
            );
            self.apply_live_deltas(deltas);
        }
        let report = self.driver.close(timeout);
        eprintln!(
            "[menghunote] 关闭排空：durable={} materialized={} pending_edits={} pending_materialize={} timed_out={}",
            report.durable.is_some(),
            report.materialized,
            report.pending_edits,
            report.pending_materialize,
            report.timed_out
        );
        self.close_report = Some(report.clone());
        report
    }

    /// 关闭窗口时**有界**排空（沿用 5a 的固定上限）。
    pub fn close_session(&mut self) {
        self.drain_and_close(CLOSE_TIMEOUT);
    }

    // ---- 鼠标 ----

    /// 左键按下：命中 → 落光标，并开始一次可能的拖拽。
    pub fn begin_pointer_selection(
        &mut self,
        display_byte: usize,
        bias: Bias,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 入参是**布局（display）字节**：先在 display 空间吸附 grapheme，
        // 再经投影回到 source scalar（隐藏标记后 display 比 source 短）。
        let display_byte = self.snap_display_byte(display_byte);
        let Some(scalar) = self.scalar_at_display_byte_biased(display_byte, bias) else {
            return;
        };
        let text = self.controller.source_text();
        let byte = self.snap_byte(self.source_byte_of_scalar(scalar).unwrap_or(0), &text);
        let Some(scalar) = scalar_of_byte(&text, byte) else {
            return;
        };
        if crate::shell::trace_mouse_enabled() {
            eprintln!(
                "[menghunote] 命中 display_byte={display_byte} → bias={bias:?} → source_byte={byte} → scalar #{}",
                scalar.get()
            );
        }
        self.drag_anchor = Some(scalar);
        self.preferred_x = None;
        window.focus(&self.focus_handle, cx);
        self.request_selection_dump();
        self.commit_selection(Selection::caret(scalar), cx);
    }

    /// 拖拽移动：anchor 保持，head 跟着鼠标。
    pub fn extend_selection_to(&mut self, display_byte: usize, cx: &mut Context<Self>) {
        self.extend_selection_to_biased(display_byte, Bias::Right, cx);
    }

    /// 拖选（带偏置）：与点击命中共用同一套"半格 → 偏置"规则。
    pub fn extend_selection_to_biased(
        &mut self,
        display_byte: usize,
        bias: Bias,
        cx: &mut Context<Self>,
    ) {
        let Some(anchor) = self.drag_anchor else {
            return;
        };
        let display_byte = self.snap_display_byte(display_byte);
        let Some(head) = self.scalar_at_display_byte_biased(display_byte, bias) else {
            return;
        };
        self.preferred_x = None;
        self.request_selection_dump();
        self.commit_selection(Selection::new(anchor, head), cx);
    }

    /// 左键抬起：结束拖拽。
    pub fn end_drag(&mut self) {
        self.drag_anchor = None;
    }

    /// 滚动（像素，正 = 向下滚）：滚轮与后续自动滚动都走这里。
    pub fn scroll_by(&mut self, delta: f32, cx: &mut Context<Self>) {
        if !delta.is_finite() || delta == 0.0 {
            return;
        }
        self.scroll_y = (self.scroll_y + delta).max(0.0);
        self.clamp_scroll();
        cx.notify();
    }

    // ---- 键盘 ----

    /// 方向键 / 翻页 / 行首行尾。
    ///
    /// 只处理**无修饰键**与 `Shift`（扩选）；带 Ctrl / Alt 的组合留给后续切片
    /// （按词移动、命令面板等）。
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        if std::env::var_os("MENGHUNOTE_TRACE_MENU").is_some() {
            eprintln!(
                "[menghunote] key down keystroke={:?} ctrl={} alt={} shift={} platform={}",
                event.keystroke.key,
                modifiers.control,
                modifiers.alt,
                modifiers.shift,
                modifiers.platform
            );
        }

        // Ctrl+K / Ctrl+F：请宿主打开搜索浮层（编辑器不管浮层，只发事件）。
        if is_search_shortcut(&event.keystroke) {
            cx.emit(EditorEvent::SearchRequested);
            window.prevent_default();
            return;
        }

        // F2：请宿主重命名侧栏选中项（编辑器自己不管侧栏，只发事件）。
        if !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && event.keystroke.key.as_str() == "f2"
        {
            cx.emit(EditorEvent::RenameRequested);
            window.prevent_default();
            return;
        }

        // 调试入口（仅 debug 构建）：模拟一次中文 IME 组合。
        //
        // 真实 IME 事件（微软拼音的候选窗 / Esc / 空格上屏）只能人工验；这里用**同一条**
        // app 侧入口（`update_preedit` → `commit_composition` / `cancel_composition`），
        // 让"一次 commit 只有一个 EditRequest""cancel 不产生请求"这两条约束可以被机器验证。
        #[cfg(debug_assertions)]
        if !modifiers.control && !modifiers.alt && !modifiers.platform {
            match event.keystroke.key.as_str() {
                "f8" => {
                    // 只开始组合、不提交：用来肉眼/截图确认 preedit overlay 与状态栏文案。
                    eprintln!("[menghunote][debug] 模拟 IME 组合（仅 overlay，F8）");
                    self.update_preedit("zhongwen", None, cx);
                    return;
                }
                "f9" => {
                    self.debug_ime_compose("zhongwen", "中文", cx);
                    return;
                }
                "f10" => {
                    self.debug_ime_cancel("nihao", cx);
                    return;
                }
                "f11" => {
                    eprintln!("[menghunote][debug] 模拟 Ctrl+Z（F11）");
                    self.undo(cx);
                    return;
                }
                "f12" => {
                    eprintln!("[menghunote][debug] 模拟 Ctrl+Shift+Z（F12）");
                    self.redo(cx);
                    return;
                }
                _ => {}
            }
        }

        // 撤销 / 重做：走会话唯一的 UndoManager（组合态下先取消组合）。
        if modifiers.control && !modifiers.alt && !modifiers.platform {
            match (event.keystroke.key.as_str(), modifiers.shift) {
                ("z", false) => self.undo(cx),
                ("z", true) => self.redo(cx),
                _ => {}
            }
            return;
        }

        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            return;
        }
        let extend = modifiers.shift;
        match event.keystroke.key.as_str() {
            // 换行：走和 IME 上屏同一条路径（一次 EditRequest）。
            "enter" => self.insert_text("\n", cx),
            // 退格 / Delete：删什么由编辑器按 grapheme 决定（app 不自己拼 ScalarEdit）。
            "backspace" => self.delete_grapheme(GraphemeStep::Backward, cx),
            "delete" => self.delete_grapheme(GraphemeStep::Forward, cx),
            "left" => self.move_grapheme(GraphemeStep::Backward, extend, cx),
            "right" => self.move_grapheme(GraphemeStep::Forward, extend, cx),
            "up" => self.move_rows(-1, extend, window, cx),
            "down" => self.move_rows(1, extend, window, cx),
            "pageup" => self.move_page(false, extend, window, cx),
            "pagedown" => self.move_page(true, extend, window, cx),
            "home" => self.move_to_row_edge(false, extend, window, cx),
            "end" => self.move_to_row_edge(true, extend, window, cx),
            _ => {}
        }
    }

    /// 左右：按 **display** 上的 grapheme 移动（隐藏标记不参与落点）。
    ///
    /// 不能直接用控制器的 source 语义 `move_grapheme`：隐藏态下构造末尾按左键会一步
    /// 落进 `**` 的两个 `*` 之间，把标记劈开。这里先在 display 上取相邻可见 grapheme，
    /// 再映射回 source；映射失败就**保持原 caret**（不跳到 0）。
    fn move_grapheme(&mut self, step: GraphemeStep, extend: bool, cx: &mut Context<Self>) {
        if self.is_composing() {
            self.cancel_composition(cx);
        }
        let anchor = self.controller.selection().anchor();
        let display = self.display_text();
        let Some(caret) = self.display_caret() else {
            return;
        };
        // 已在构造内部（标记可见）时 display 里就含标记，按 display 走即会正常经过 `*`。
        let Some(target) = super::display_edit::step_target(&display, caret, step) else {
            return; // 文档边界：保持原 caret
        };
        let Some(scalar) = self.scalar_at_display_byte(target) else {
            eprintln!("[menghunote] 光标移动：display {target} 映射不到 source，保持原 caret");
            return;
        };
        if crate::shell::trace_mouse_enabled() {
            eprintln!(
                "[menghunote] 左右移动：display {caret} → {target} → scalar #{}",
                scalar.get()
            );
        }
        let next = if extend {
            Selection::new(anchor, scalar)
        } else {
            Selection::caret(scalar)
        };
        self.preferred_x = None;
        self.ensure_caret_visible(scalar);
        self.log_caret(next);
        self.commit_selection(next, cx);
    }

    /// 上下：按**视觉行**移动，并尽量保持视觉列（`preferred_x`）。
    fn move_rows(&mut self, delta: i64, extend: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(geometry) = self.shared.geometry() else {
            return;
        };
        let selection = self.controller.selection();
        let Some(head_byte) = self.display_byte_of_scalar(selection.head()) else {
            return;
        };
        let frame = &geometry.frame;
        let row = frame.row_of_byte(head_byte);
        let x = self
            .preferred_x
            .unwrap_or_else(|| self.caret_x_in_row(frame, row, head_byte, window));
        let last_row = frame.row_count().saturating_sub(1);
        let target_row = super::layout::vertical_target(row, delta, last_row);
        let target_byte = self.byte_at_row_x(frame, target_row, x, window);
        let target_byte = frame.snap_byte_to_grapheme(target_byte);
        let Some(target) = self.scalar_at_display_byte(target_byte) else {
            eprintln!("[menghunote] 上下移动：display {target_byte} 映射不到 source，保持原 caret");
            return;
        };
        self.preferred_x = Some(x);
        let next = if extend {
            Selection::new(selection.anchor(), target)
        } else {
            Selection::caret(target)
        };
        self.commit_selection(next, cx);
    }

    /// PageUp / PageDown：按可见行数翻页（光标跟着走，视口由
    /// `ensure_caret_visible` 拉到位，因此翻页与滚动是同一套几何）。
    fn move_page(
        &mut self,
        forward: bool,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.shared.geometry().map_or(1, |geometry| {
            super::layout::rows_per_page_mixed(
                &geometry.frame,
                geometry
                    .frame
                    .row_top_offset(geometry.frame.row_at_y(geometry.scroll_y)),
                geometry.viewport_height,
            )
        });
        self.move_rows(if forward { rows } else { -rows }, extend, window, cx);
    }

    /// Home / End：当前**视觉行**的行首 / 行尾（不是源码行，符合软折行后的直觉）。
    fn move_to_row_edge(
        &mut self,
        to_end: bool,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = window;
        let Some(geometry) = self.shared.geometry() else {
            return;
        };
        let selection = self.controller.selection();
        let Some(head_byte) = self.display_byte_of_scalar(selection.head()) else {
            return;
        };
        let row = geometry.frame.row_of_byte(head_byte);
        let bytes = geometry.frame.row_bytes(row);
        let target_byte = if to_end { bytes.end } else { bytes.start };
        let Some(target) =
            self.scalar_at_display_byte(geometry.frame.snap_byte_to_grapheme(target_byte))
        else {
            return;
        };
        self.preferred_x = None;
        let next = if extend {
            Selection::new(selection.anchor(), target)
        } else {
            Selection::caret(target)
        };
        self.commit_selection(next, cx);
    }

    // ---- 文本输入 / IME 组合（切片 6b）----

    /// 组合更新：**只改 overlay**（preedit 不进 source、不产生 `EditRequest`）。
    pub(crate) fn update_preedit(
        &mut self,
        text: &str,
        selected: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) {
        if self.frozen_by_external_commit() {
            // Held：连接收组合会走到 `driver.begin_composition()`（3a 会拒绝进入组合），
            // 所以整段忽略；Held 结束后平台会重新发起组合。
            return;
        }
        if !self.composition.active {
            let depth = self.driver.begin_composition();
            self.composition.active = true;
            eprintln!("[menghunote] IME 组合开始（storage 闸门 depth={depth}）");
        }
        self.composition.selected = selected;
        match self.controller.set_preedit(text) {
            Ok(()) => eprintln!(
                "[menghunote] preedit ← {:?}（只更新 overlay，不产生 EditRequest）",
                text
            ),
            Err(error) => eprintln!("[menghunote] 设置 preedit 失败：{error}"),
        }
        cx.notify();
    }

    /// 取消组合：丢弃 overlay，**不发请求**；返回的延后远端仍然要合并。
    pub(crate) fn cancel_composition(&mut self, cx: &mut Context<Self>) {
        let dropped = self.controller.preedit_text().map(str::to_owned);
        self.controller.clear_preedit();
        if let Some(preedit) = dropped.as_deref() {
            eprintln!("[menghunote] IME 取消：丢弃 preedit {preedit:?}（不发 EditRequest）");
        }
        self.finish_composition_bridge();
        cx.notify();
    }

    /// 组合提交：`text` 是平台给的最终文本，**只产生一个 `EditRequest`**。
    pub(crate) fn commit_composition(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.frozen_by_external_commit() {
            // Held：既不结束组合闸门（3a 的状态优先），也不产生编辑请求。
            eprintln!("[menghunote] Held 期间收到 IME 上屏：已忽略（不落库）");
            return;
        }
        let preedit = self.controller.preedit_text().map(str::to_owned);
        // 1) 先结束存储侧闸门：延后的远端在这里合并（合并可能让 preedit 失效）
        self.finish_composition_bridge();
        // 2) 一次替换：组合文本 → 一个 EditRequest
        let request = if preedit.is_some() && self.controller.preedit_text().is_some() {
            self.controller.commit_preedit(text)
        } else {
            // preedit 已被并发远端取消：退回"在光标处插入"，绝不丢用户的字
            self.controller.compose_replace_selection(text)
        };
        match request {
            Ok(request) => {
                eprintln!(
                    "[menghunote] IME 上屏：preedit={:?} → committed={:?}",
                    preedit, text
                );
                self.submit_request(request);
            }
            Err(error) => eprintln!("[menghunote] 提交组合失败：{error}"),
        }
        cx.notify();
    }

    /// 普通文本输入（无组合：打字 / 粘贴）。
    pub(crate) fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.frozen_by_external_commit() {
            // Held：打字/粘贴一律忽略（控制器镜像也不能先乐观改动，否则之后无法对齐）。
            return;
        }
        match self.controller.compose_replace_selection(text) {
            Ok(request) => self.submit_request(request),
            Err(error) => eprintln!("[menghunote] 插入文本失败：{error}"),
        }
        cx.notify();
    }

    /// 本次删除在 **source** 坐标下的最小区间（display 取相邻可见 grapheme）。
    ///
    /// 有选区时删选区；否则按方向取显示上相邻的一个 grapheme。跨构造的选区会把
    /// 段内标记一起删掉——这是允许的（用户显式选中的是真实文本）。
    fn display_delete_source_range(
        &mut self,
        step: GraphemeStep,
    ) -> Option<menghunote_editor::SourceRange> {
        let projection = self.controller.projection();
        let (start, end) = match self.controller.selection_in_display() {
            Some((start, end)) if start != end => (start.get(), end.get()),
            _ => {
                let display = self.display_text();
                let caret = self.display_caret()?;
                let range = match step {
                    GraphemeStep::Backward => super::display_edit::grapheme_before(&display, caret),
                    GraphemeStep::Forward => super::display_edit::grapheme_after(&display, caret),
                }?;
                let range = super::display_edit::non_empty(range)?;
                (range.start, range.end)
            }
        };
        projection.source_range_of_display_range(DisplayByte::new(start)..DisplayByte::new(end))
    }

    /// 退格 / Delete：走与普通输入**完全相同**的一条路径。
    ///
    /// 删哪一段由 [`EditorController::delete_backward`] / [`EditorController::delete_forward`]
    /// 按 grapheme 决定（有选区删选区），app 只负责把返回的 `EditRequest` 交给
    /// [`Self::submit_request`] —— 不在这里拼 `ScalarEdit`，也不二次改文本。
    ///
    /// 组合态按删除键：先取消组合（丢弃 overlay，不发请求），不要让编辑请求去撞闸门。
    fn delete_grapheme(&mut self, step: GraphemeStep, cx: &mut Context<Self>) {
        if self.frozen_by_external_commit() {
            // Held：退格/Delete 一律忽略（连"先把目标设成选区"这一步也不能做）。
            return;
        }
        if self.is_composing() {
            self.cancel_composition(cx);
        }
        // 先按 **display** 定位要删的那一段，再映射回 source 最小区间。
        // 直接走 source 的 `delete_*` 会在构造边界啃掉隐藏标记（S2-5 的 bug）。
        let Some(source_range) = self.display_delete_source_range(step) else {
            eprintln!(
                "[menghunote] 删除（{step:?}）：display 上无可删单元（边界 / 空区间），不产生请求"
            );
            cx.notify();
            return;
        };
        // 把目标区间设成选区：控制器"有选区删选区"的那条路径会精确删掉它，
        // 不需要 app 自己拼 `ScalarEdit`。
        let text = self.controller.source_text();
        let start = SourceByte::new(source_range.start().get()).to_scalar(&text);
        let end = SourceByte::new(source_range.end().get()).to_scalar(&text);
        let (Ok(start), Ok(end)) = (start, end) else {
            eprintln!("[menghunote] 删除（{step:?}）：源区间换算 scalar 失败，放弃本次删除");
            cx.notify();
            return;
        };
        if start == end {
            cx.notify();
            return;
        }
        if let Err(error) = self.controller.set_selection(Selection::new(start, end)) {
            eprintln!("[menghunote] 删除（{step:?}）：设置待删选区失败：{error}");
            cx.notify();
            return;
        }
        let deleted = text
            .get(source_range.start().get()..source_range.end().get())
            .unwrap_or("")
            .to_owned();
        eprintln!(
            "[menghunote] 删除（{step:?}）：source {:?} → 删掉 {deleted:?}",
            source_range
        );
        let outcome = self.controller.delete_backward();
        match outcome {
            Ok(Some(request)) => {
                // 日志里带上 EditRequest 的形状：一次删除永远只有 1 个 ScalarEdit
                let edit = request.edits.first();
                match edit {
                    Some(edit) => eprintln!(
                        "[menghunote] 删除（{step:?}）：edits={} {:#?}..{:#?} → {:?}",
                        request.edits.len(),
                        edit.start.get(),
                        edit.end.get(),
                        edit.replacement
                    ),
                    None => eprintln!("[menghunote] 删除（{step:?}）：请求里没有 ScalarEdit（异常）"),
                }
                self.submit_request(Some(request));
            }
            Ok(None) => eprintln!(
                "[menghunote] 删除（{step:?}）：无可删单元（空文档 / 已在边界 / 选区为空），不产生请求"
            ),
            Err(EditorError::PreeditActive) => {
                // 理论上上面已经 cancel 过；走到这里说明组合又被平台打开，再取消一次即可
                eprintln!("[menghunote] 删除被拒（组合中）：先取消组合，本次按键丢弃");
                self.cancel_composition(cx);
            }
            Err(EditorError::Backpressured(command_id)) => eprintln!(
                "[menghunote] 删除被拒（背压）：command_id={command_id}，等 DurableAck 后再按一次"
            ),
            Err(EditorError::ResyncRequired) => {
                eprintln!("[menghunote] 删除被拒：控制器已失效，需要按新 OpenSnapshot 重建（切片 7）")
            }
            Err(error) => eprintln!("[menghunote] 删除失败：{error}"),
        }
        cx.notify();
    }

    /// 结束存储侧组合闸门：合并延后远端、重试被挂起的请求。
    fn finish_composition_bridge(&mut self) {
        if !self.composition.active && !self.driver.is_composing() {
            self.composition.selected = None;
            return;
        }
        self.composition.active = false;
        self.composition.selected = None;
        let deltas = self.driver.end_composition();
        if !deltas.is_empty() {
            eprintln!(
                "[menghunote] 组合结束：按序应用延后的远端变更 {} 条",
                deltas.len()
            );
        }
        self.apply_live_deltas(deltas);
        self.drain_deferred_remote();
        if let Some(request) = self.composition.pending_retry.take() {
            eprintln!(
                "[menghunote] 组合结束，重试挂起请求 command_id={}",
                request.command_id
            );
            self.submit_request(Some(request));
        }
    }

    /// 把延后的远端变更按到达顺序应用镜像（**唯一**投递口是 `end_composition` /
    /// `merge_deferred_remote` 的返回值）。
    fn apply_live_deltas(&mut self, deltas: Vec<LiveDelta>) {
        for delta in deltas {
            if let Err(error) = self.controller.apply_live_delta(delta) {
                eprintln!("[menghunote] 延后的远端变更未能应用：{error}");
            }
        }
    }

    /// 背压恢复：非组合态下再排空一次延后队列。
    fn drain_deferred_remote(&mut self) {
        if self.driver.is_composing() || self.driver.deferred_remote_len() == 0 {
            return;
        }
        let deltas = self.driver.merge_deferred_remote();
        if !deltas.is_empty() {
            eprintln!("[menghunote] 补排空延后远端 {} 条", deltas.len());
        }
        self.apply_live_deltas(deltas);
    }

    /// **所有** `EditRequest` 的唯一出口：组合期挂起、`CompositionActive` 不污染控制器、
    /// 每次请求打一行日志（`请求#N … edits=M` 就是"一次 commit 只有一个请求"的证据）。
    fn submit_request(&mut self, request: Option<EditRequest>) {
        if self.frozen_by_external_commit() {
            // 兜底：任何绕到这里的请求（例如组合结束时重试的挂起请求）在 Held 期间都
            // 不能被驱动受理。`Rejected{NotReady}` 会让控制器 `needs_resync`，
            // 因此这里**主动丢弃**而不是让驱动去拒绝。
            if let Some(request) = request.as_ref() {
                eprintln!(
                    "[menghunote] Held 期间丢弃编辑请求 command_id={}（输入已冻结）",
                    request.command_id
                );
            }
            return;
        }
        let Some(request) = request else {
            // 空选区 + 空替换是 no-op：不产生请求，但要留痕，避免被误读成"丢请求"。
            eprintln!("[menghunote] 本次输入是 no-op（空选区 + 空文本），不产生 EditRequest");
            return;
        };
        self.request_seq = self.request_seq.saturating_add(1);
        let seq = self.request_seq;

        if self.driver.is_composing() {
            eprintln!(
                "[menghunote] 请求#{seq} 挂起：组合进行中 command_id={}",
                request.command_id
            );
            self.composition.pending_retry = Some(request);
            return;
        }

        let command_id = request.command_id.clone();
        let edits = request.edits.len();
        let outcome = self.driver.apply(request.clone());
        let composition_rejected = matches!(
            &outcome,
            EditOutcome::Rejected(rejected) if rejected.reason == RejectReason::CompositionActive
        );
        if composition_rejected {
            eprintln!(
                "[menghunote] 请求#{seq} 未受理（CompositionActive）：挂起 command_id={command_id}，组合结束后用同一 id 重试"
            );
            self.composition.pending_retry = Some(request);
            return;
        }
        match &outcome {
            EditOutcome::Applied(applied) => {
                #[cfg(feature = "dev-sync")]
                {
                    // ADR 0012 S3：本地编辑代数（自动同步尾缘 debounce 的触发源）。
                    self.local_edit_seq = self.local_edit_seq.saturating_add(1);
                }
                eprintln!(
                    "[menghunote] 请求#{seq} 受理 command_id={command_id} edits={edits} local_echo={} live_rev={}",
                    applied.local_echo, applied.live_rev.0
                )
            }
            EditOutcome::Rejected(rejected) => eprintln!(
                "[menghunote] 请求#{seq} 被拒 command_id={command_id} reason={:?}",
                rejected.reason
            ),
            EditOutcome::Stale(stale) => eprintln!(
                "[menghunote] 请求#{seq} 基准过期 command_id={command_id} current_live_rev={}",
                stale.current_live_rev.0
            ),
        }
        self.apply_outcome_guarded(outcome);
    }

    /// 调试：模拟"输入法组合 → 上屏"（F9，仅 debug 构建）。
    ///
    /// 走的是与真实 IME 完全相同的 app 侧路径，因此日志能证明"一次 commit 只有一个请求"。
    #[cfg(debug_assertions)]
    fn debug_ime_compose(&mut self, preedit: &str, committed: &str, cx: &mut Context<Self>) {
        eprintln!("[menghunote][debug] 模拟 IME 组合：preedit ← {preedit:?}（F9）");
        self.update_preedit(preedit, None, cx);
        eprintln!("[menghunote][debug] 模拟 IME 上屏：committed ← {committed:?}");
        self.commit_composition(committed, cx);
    }

    /// 调试：模拟"输入法组合 → 取消"（F10，仅 debug 构建）。取消**不产生** `EditRequest`。
    #[cfg(debug_assertions)]
    fn debug_ime_cancel(&mut self, preedit: &str, cx: &mut Context<Self>) {
        eprintln!("[menghunote][debug] 模拟 IME 组合：preedit ← {preedit:?}（F10）");
        self.update_preedit(preedit, None, cx);
        eprintln!("[menghunote][debug] 模拟 IME 取消（等价 Esc）");
        self.cancel_composition(cx);
    }

    /// `Ctrl+Z`：会话唯一 UndoManager 的撤销；组合态下先取消组合。
    fn undo(&mut self, cx: &mut Context<Self>) {
        if self.frozen_by_external_commit() {
            // Held：撤销也是正文写入，必须整段忽略（驱动会 fail-closed 拒绝）。
            return;
        }
        if self.is_composing() {
            self.cancel_composition(cx);
        }
        match self.driver.undo() {
            Ok(Some(outcome)) => {
                self.log_history("undo", &outcome);
                self.apply_outcome_guarded(outcome);
            }
            Ok(None) => eprintln!("[menghunote] undo：没有可撤销项（不改变文本）"),
            Err(error) => eprintln!("[menghunote] undo 失败：{error:?}"),
        }
        cx.notify();
    }

    /// `Ctrl+Shift+Z`：重做。
    fn redo(&mut self, cx: &mut Context<Self>) {
        if self.frozen_by_external_commit() {
            // Held：同 undo。
            return;
        }
        if self.is_composing() {
            self.cancel_composition(cx);
        }
        match self.driver.redo() {
            Ok(Some(outcome)) => {
                self.log_history("redo", &outcome);
                self.apply_outcome_guarded(outcome);
            }
            Ok(None) => eprintln!("[menghunote] redo：没有可重做项（不改变文本）"),
            Err(error) => eprintln!("[menghunote] redo 失败：{error:?}"),
        }
        cx.notify();
    }

    fn log_history(&self, what: &str, outcome: &EditOutcome) {
        match outcome {
            EditOutcome::Applied(applied) => eprintln!(
                "[menghunote] {what} 生效：live_rev={}（镜像按 splice 更新）",
                applied.live_rev.0
            ),
            EditOutcome::Rejected(rejected) => {
                eprintln!("[menghunote] {what} 未受理：reason={:?}", rejected.reason);
            }
            EditOutcome::Stale(stale) => eprintln!(
                "[menghunote] {what} 基准过期：current_live_rev={}",
                stale.current_live_rev.0
            ),
        }
    }

    /// 把结局交给控制器，但**先**滤掉会让控制器 `needs_resync` 的瞬态拒绝。
    ///
    /// `Rejected{CompositionActive}` 表示"请求未被受理、Loro 未修改"，
    /// 交给控制器只会把它标记成必须重建 —— 这不是我们想要的语义。
    fn apply_outcome_guarded(&mut self, outcome: EditOutcome) {
        if matches!(
            &outcome,
            EditOutcome::Rejected(rejected) if rejected.reason == RejectReason::CompositionActive
        ) {
            eprintln!(
                "[menghunote] 瞬态拒绝（CompositionActive）：不交给控制器，避免 needs_resync"
            );
            return;
        }
        match self.controller.apply_outcome(outcome) {
            Ok(_) => {}
            Err(error) => eprintln!("[menghunote] 编辑结局未能应用：{error}"),
        }
    }

    /// 光标选区在**虚拟输入文档**里的 UTF-16 区间（平台查询用）。
    pub(crate) fn selection_utf16_in_input_document(&self) -> Option<Range<usize>> {
        let source = self.controller.source_text();
        let preedit = self.controller.preedit_text().unwrap_or("").to_owned();
        super::ime_map::InputMapping::new(&source, self.controller.selection(), &preedit)
            .selection_utf16()
    }

    /// 虚拟输入文档的 UTF-16 位置 → 窗口坐标（候选窗用）。
    pub(crate) fn point_for_input_utf16(
        &self,
        utf16: usize,
        window: &Window,
    ) -> Option<gpui::Point<Pixels>> {
        let source = self.controller.source_text();
        let preedit = self.controller.preedit_text().unwrap_or("").to_owned();
        let mapping =
            super::ime_map::InputMapping::new(&source, self.controller.selection(), &preedit);
        let source_byte = mapping.utf16_to_source_byte(utf16)?;
        let position = self.point_for_source_byte(source_byte, window)?;
        if preedit.is_empty() {
            return Some(position);
        }
        // 组合串没有进布局：先定位到组合起点，再按 preedit 的 UTF-16 前缀宽度右移。
        let prefix_bytes = mapping.preedit_prefix_bytes(utf16);
        if prefix_bytes == 0 {
            return Some(position);
        }
        let shaped = super::glyph::shape_row(window, &sanitize_single_line(&preedit));
        let advance = shaped.x_for_index(prefix_bytes.min(preedit.len()));
        Some(gpui::point(position.x + advance, position.y))
    }

    /// 源码字节 → 窗口坐标（行内 x + 行顶 y）；屏外行按需 shaping。
    pub(crate) fn point_for_source_byte(
        &self,
        byte: usize,
        window: &Window,
    ) -> Option<gpui::Point<Pixels>> {
        let geometry = self.shared.geometry()?;
        let frame = &geometry.frame;
        // 入参是 source 字节；布局是 display，所以先映射再定位。
        let byte = self
            .controller
            .source_to_display(SourceByte::new(byte))?
            .get()
            .min(frame.text.len());
        let row = frame.row_of_byte(byte);
        let bytes = frame.row_bytes(row);
        let offset = byte.saturating_sub(bytes.start).min(bytes.len());
        let slice = &frame.text[bytes];
        let local_x = if slice.is_empty() {
            px(0.0)
        } else {
            let spans = geometry.spans_for_row(row);
            super::glyph::shape_row_styled_at(
                window,
                slice,
                &spans,
                geometry.frame.row_font_size(row),
                geometry.frame.lines[row].mono,
            )
            .x_for_index(offset)
        };
        Some(gpui::point(
            geometry.text_left_for_row(row) + local_x,
            geometry.row_top(row),
        ))
    }

    /// 窗口坐标 → 源码字节（几何命中，与本切片点击一致）。
    pub(crate) fn source_byte_at_point(&self, position: gpui::Point<Pixels>) -> Option<usize> {
        let display = self.shared.geometry()?.byte_at(position);
        self.controller
            .display_to_source(DisplayByte::new(display))
            .map(|source| source.get())
    }

    /// 在 **display** 空间把字节吸附到 grapheme 边界（折行表里有现成边界）。
    fn snap_display_byte(&self, byte: usize) -> usize {
        match self.shared.geometry() {
            Some(geometry) => geometry.frame.snap_byte_to_grapheme(byte),
            None => byte,
        }
    }

    /// 源码字节 → 虚拟输入文档的 UTF-16 序号。
    pub(crate) fn source_byte_to_input_utf16(&self, byte: usize) -> Option<usize> {
        let source = self.controller.source_text();
        let preedit = self.controller.preedit_text().unwrap_or("").to_owned();
        super::ime_map::InputMapping::new(&source, self.controller.selection(), &preedit)
            .source_byte_to_utf16(byte)
    }

    /// 虚拟输入文档的 UTF-16 序号 → 源码字节。
    pub(crate) fn virtual_utf16_to_source_byte(&self, utf16: usize) -> Option<usize> {
        let source = self.controller.source_text();
        let preedit = self.controller.preedit_text().unwrap_or("").to_owned();
        super::ime_map::InputMapping::new(&source, self.controller.selection(), &preedit)
            .utf16_to_source_byte(utf16)
    }

    /// 把字节吸附到前一个 grapheme 边界（点击 / 平台选区共用）。
    pub(crate) fn snap_source_byte_to_grapheme(&self, byte: usize) -> usize {
        let text = self.controller.source_text();
        self.snap_byte(byte, &text)
    }

    /// 把光标放到源码字节处（平台设置选区时用）。
    pub(crate) fn place_caret_at_byte(&mut self, byte: usize, cx: &mut Context<Self>) {
        let text = self.controller.source_text();
        let Some(scalar) = scalar_of_byte(&text, byte.min(text.len())) else {
            return;
        };
        self.preferred_x = None;
        self.commit_selection(Selection::caret(scalar), cx);
    }

    // ---- 内部辅助 ----

    /// 提交选区：控制器校验通过后滚动到光标可见并重绘。
    fn commit_selection(&mut self, selection: Selection, cx: &mut Context<Self>) {
        match self.controller.set_selection(selection) {
            Ok(()) => {
                self.ensure_caret_visible(selection.head());
                self.log_caret(selection);
                cx.notify();
            }
            Err(error) => eprintln!("[menghunote] 设置选区失败：{error}"),
        }
    }

    /// 开发期留痕：把"点击 / 键盘 → scalar → 视觉行"打到 stderr。
    ///
    /// 这是 S1 的临时调试手段：切片 5a 没有可交互的 DevTools，而 150% 缩放的小屏上
    /// 状态栏可能被裁掉，日志是第二条证据源。等切片 6 有了 preedit 调试面板再删。
    fn log_caret(&self, selection: Selection) {
        let Some(geometry) = self.shared.geometry() else {
            return;
        };
        let Some(byte) = self.display_byte_of_scalar(selection.head()) else {
            return;
        };
        eprintln!(
            "[menghunote] 光标 scalar #{} → 视觉行 {}（选区 {}..{}）",
            selection.head().get(),
            geometry.frame.row_of_byte(byte),
            selection.start().get(),
            selection.end().get()
        );
    }

    /// 让光标所在视觉行保持在视口内（只做最小滚动）。
    fn ensure_caret_visible(&mut self, head: UnicodeScalar) {
        let Some(geometry) = self.shared.geometry() else {
            return;
        };
        let Some(byte) = self.display_byte_of_scalar(head) else {
            return;
        };
        let row = geometry.frame.row_of_byte(byte);
        // 行高逐行不同：用该行自己的行顶/行高做最小滚动。
        self.scroll_y = super::layout::scroll_to_reveal_range(
            self.scroll_y,
            geometry.frame.row_top_offset(row),
            geometry.frame.row_height(row),
            geometry.viewport_height,
        );
        self.clamp_scroll();
    }

    /// 把滚动量夹到 `[0, 内容高度 - 视口高度]`。
    fn clamp_scroll(&mut self) {
        let viewport = self.shared.viewport().1;
        let content = self
            .shared
            .geometry()
            .map_or(0.0, |geometry| geometry.frame.content_height());
        self.scroll_y = super::layout::clamp_scroll(self.scroll_y, content, viewport);
    }

    /// 字节 → 前一个 grapheme 边界（优先用折行表里已算好的边界）。
    fn snap_byte(&self, byte: usize, text: &str) -> usize {
        match self.shared.geometry() {
            Some(geometry) if geometry.frame.text == text => {
                geometry.frame.snap_byte_to_grapheme(byte)
            }
            _ => floor_char_boundary(text, byte.min(text.len())),
        }
    }

    /// 在任意视觉行上把像素 x（相对行首）换成源码字节。
    ///
    /// 目标行可能不在当前绘制范围内（翻页时必然如此），因此这里**按需 shaping**，
    /// 而不是退回"字符数 × 平均宽度"。
    fn byte_at_row_x(&self, frame: &LayoutFrame, row: usize, x: Pixels, window: &Window) -> usize {
        let bytes = frame.row_bytes(row);
        let slice = &frame.text[bytes.clone()];
        if slice.is_empty() {
            return bytes.start;
        }
        let spans = self.spans_for_row(row);
        let (font_size, mono) = self.row_metrics(row);
        let shaped = super::glyph::shape_row_styled_at(window, slice, &spans, font_size, mono);
        let index = shaped.closest_index_for_x(x.max(px(0.0)));
        bytes.start + index.min(slice.len())
    }

    /// 光标在视觉行内的像素 x（相对行首）。
    fn caret_x_in_row(
        &self,
        frame: &LayoutFrame,
        row: usize,
        byte: usize,
        window: &Window,
    ) -> Pixels {
        let bytes = frame.row_bytes(row);
        let offset = byte.saturating_sub(bytes.start).min(bytes.len());
        let slice = &frame.text[bytes.clone()];
        if slice.is_empty() {
            return px(0.0);
        }
        let spans = self.spans_for_row(row);
        let (font_size, mono) = self.row_metrics(row);
        super::glyph::shape_row_styled_at(window, slice, &spans, font_size, mono)
            .x_for_index(offset)
    }

    /// 第 `row` 行的 `(字号, 是否等宽)`（来自上一帧几何；没有几何时用正文默认）。
    fn row_metrics(&self, row: usize) -> (f32, bool) {
        self.shared
            .geometry()
            .map_or((theme::FONT_SIZE, false), |geometry| {
                (
                    geometry.frame.row_font_size(row),
                    geometry.frame.lines.get(row).is_some_and(|line| line.mono),
                )
            })
    }

    /// 第 `row` 行的样式分段（来自上一帧几何；没有几何时按纯文本处理）。
    fn spans_for_row(&self, row: usize) -> Vec<(usize, StyleKind)> {
        self.shared
            .geometry()
            .map_or_else(Vec::new, |geometry| geometry.spans_for_row(row))
    }

    /// 状态栏左侧文案：保存状态 / 光标 / 显示行数。
    fn status_text(&self) -> String {
        let saved = if self.driver.is_external_commit_held() {
            // Held：输入被冻结，明确告诉用户为什么按键没反应（不是卡死）。
            "外部提交中，输入已冻结".to_owned()
        } else if let Some(preedit) = self.controller.preedit_text() {
            // 组合期间 preedit 不进 CRDT：明确告诉用户"还没提交"
            format!("输入中，尚未提交（preedit {} 字）", preedit.chars().count())
        } else if self.controller.is_saved() {
            "已保存".to_owned()
        } else {
            "保存中".to_owned()
        };
        let selection = self.controller.selection();
        let position = if selection.is_empty() {
            format!("光标 scalar #{}", selection.head().get())
        } else {
            format!(
                "选区 scalar #{}..#{}（{} 个）",
                selection.start().get(),
                selection.end().get(),
                selection.len()
            )
        };
        let rows = self
            .shared
            .geometry()
            .map_or(0, |geometry| geometry.frame.row_count());
        format!(
            "{} · {saved} · {position} · {rows} 显示行 · {}",
            self.controller.doc_id(),
            self.vault_label
        )
    }
}

/// dev-sync（ADR 0009 切片 3b）：外部提交闸门 / adopt 的**最小转发**。
///
/// 这里只做三件事：转发给 [`SessionDriver`]、刷新 `held`（状态栏与 selector 用）、
/// 让调用方显式 [`Self::dev_sync_pump`]。事件本身由驱动推进队列（`Remote` + `Durable`），
/// 因此 adopt 之后**不需要重开文档**也能看到远端文字。
///
/// 切片 4 的 worker 会从这些方法之上搭（网络/push 都不在这里）。
#[cfg(feature = "dev-sync")]
impl EditorView {
    /// 登记一次外部提交（对当前活动文档）。
    pub fn dev_sync_begin_external(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<ExternalCommitTicket, GateError> {
        let ticket = self.driver.begin_external_commit()?;
        self.note_gate_state(cx);
        Ok(ticket)
    }

    /// 非阻塞推进闸门（`WaitingIME` → `Flushing` → `Held`）。
    pub fn dev_sync_poll_external(
        &mut self,
        ticket: &ExternalCommitTicket,
        cx: &mut Context<Self>,
    ) -> GateState {
        let state = self.driver.poll_external_commit(ticket);
        self.note_gate_state(cx);
        state
    }

    /// adopt 一份已 durable 的外部增量（必须 Held）。
    pub fn dev_sync_adopt_external(
        &mut self,
        lease: &ExternalCommitLease,
        prepared: PreparedCommittedDelta,
        cx: &mut Context<Self>,
    ) -> Result<AdoptOutcome, AdoptError> {
        let outcome = self.driver.adopt_committed_delta(lease, prepared);
        self.note_gate_state(cx);
        outcome
    }

    /// 结束外部提交并释放闸门（`applied` 必须就是库中当前 head）。
    pub fn dev_sync_end_external(
        &mut self,
        lease: ExternalCommitLease,
        applied: AppliedHeads,
        cx: &mut Context<Self>,
    ) -> Result<(), GateError> {
        let result = self.driver.end_external_commit(lease, applied);
        self.note_gate_state(cx);
        result
    }

    /// 取消/放弃一次外部提交（失败路径；已 durable 的交付仍走事件队列）。
    pub fn dev_sync_cancel_external(
        &mut self,
        ticket: &ExternalCommitTicket,
        cx: &mut Context<Self>,
    ) -> Result<(), GateError> {
        let result = self.driver.cancel_external_commit(ticket);
        self.note_gate_state(cx);
        result
    }

    /// 闸门状态变化时立刻重绘（状态栏文案与 `editor-sync-held` 标记不能滞后）。
    fn note_gate_state(&mut self, cx: &mut Context<Self>) {
        if self.sync_held_state() {
            cx.notify();
        }
    }

    /// 把驱动事件推进控制器（测试 / 后续 worker 调用的唯一入口）；返回是否有可视变化。
    pub fn dev_sync_pump(&mut self, cx: &mut Context<Self>) -> bool {
        let changed = self.pump();
        if changed {
            cx.notify();
        }
        changed
    }

    /// 只读：当前镜像正文（测试断言"不重开也能看到远端文字"）。
    #[must_use]
    pub fn dev_sync_current_text(&self) -> String {
        self.controller.source_text()
    }

    /// 闸门是否持有中（Held）。
    #[must_use]
    pub fn dev_sync_is_held(&self) -> bool {
        self.driver.is_external_commit_held()
    }

    /// 当前文档 id（切片 4c-2：持闸同步按它取 `prepare_committed_delta` / 派发
    /// `HeldGate`）。
    #[must_use]
    pub fn dev_sync_doc_id(&self) -> &str {
        self.controller.doc_id()
    }

    /// 当前会话已 durable 的 head seq（诊断与测试断言用；闸门的 CAS 基准）。
    #[must_use]
    pub fn dev_sync_durable_head_seq(&self) -> u64 {
        self.driver.durable_head_seq()
    }

    /// 已被受理的**本地**编辑代数（ADR 0012 S3）。
    ///
    /// 宿主比较前后两拍的值：变了 = 有新的本地编辑；再等 `dev_sync_is_saved()`
    /// 为真（`DurableAck` 追上 `live_rev`）才交给调度器做 2s 尾缘 debounce。
    #[must_use]
    pub fn dev_sync_local_edit_seq(&self) -> u64 {
        self.local_edit_seq
    }

    /// 已确认输入是否全部 durable（`DurableAck` 水位追上 `live_rev`）。
    #[must_use]
    pub fn dev_sync_is_saved(&self) -> bool {
        self.controller.is_saved()
    }

    /// 当前是否处于 IME 组合中（自动同步要等它自然结束，绝不强行结束组合）。
    #[must_use]
    pub fn dev_sync_is_composing(&self) -> bool {
        self.driver.is_composing()
    }

    /// 调试：开始一次 IME 组合（只更新 preedit / 会话组合深度，不产生 `EditRequest`）。
    ///
    /// 为什么需要：真实平台 IME 事件在 headless 里注入不了，而"自动同步遇到组合中要
    /// defer、不强行结束组合"只能靠一个真实组合态来验证（ADR 0012 S3 验收）。
    #[cfg(debug_assertions)]
    pub fn dev_sync_debug_begin_composition(&mut self, cx: &mut Context<Self>) {
        self.update_preedit("组合", None, cx);
    }

    /// 调试：提交当前 IME 组合（走与平台上屏同一条 `commit_composition` 路径）。
    #[cfg(debug_assertions)]
    pub fn dev_sync_debug_commit_composition(&mut self, text: &str, cx: &mut Context<Self>) {
        self.commit_composition(text, cx);
    }
}

impl Focusable for EditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            self.focused_once = true;
            // `focus_self` 走 `window.defer`：等本帧元素树建好再设焦点，
            // 比在 render 里直接 `window.focus` 更稳（首帧就能收到方向键）。
            cx.focus_self(window);
        }

        let status = SharedString::from(self.status_text());
        let element = EditorElement::new(cx.entity(), Rc::clone(&self.shared));

        div()
            .size_full()
            .flex()
            .flex_col()
            // 测试元数据（只读断言用）：当前打开的文档视图（`AppShell` 里只有一个）。
            .debug_selector(|| "editor-current-doc".to_string())
            .bg(theme::window_bg())
            .font_family(theme::FONT_FAMILY)
            .text_size(px(theme::FONT_SIZE))
            .line_height(px(theme::LINE_HEIGHT))
            .text_color(theme::text_color())
            .child(
                div()
                    .id("menghunote-editor-surface")
                    .flex_1()
                    .w_full()
                    .min_h(px(0.0))
                    .track_focus(&self.focus_handle)
                    .on_key_down(cx.listener(Self::on_key_down))
                    // 正文被按下：通知宿主（它用这个取消侧栏的行内编辑）。
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_view, _event: &MouseDownEvent, _window, cx| {
                            cx.emit(EditorEvent::PointerDown);
                        }),
                    )
                    .child(element),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .gap_3()
                    .px_3()
                    .py_1()
                    .bg(theme::status_bar_bg())
                    .text_size(px(12.0))
                    .text_color(theme::status_bar_text())
                    .font_weight(FontWeight::NORMAL)
                    // 测试元数据：`editor-sync-held` **仅** Held 时存在（headless 断言用）。
                    // 同一个 div 只能记一个 selector，根视图已占用 `editor-current-doc`，
                    // 所以挂在这层；只加 id 不改任何布局。
                    .when(self.driver.is_external_commit_held(), |this| {
                        this.debug_selector(|| "editor-sync-held".to_string())
                    })
                    .child(status)
                    .child(SharedString::from(HINT)),
            )
    }
}

/// 源码字节 → scalar；非 scalar 边界返回 `None`。
fn scalar_of_byte(text: &str, byte: usize) -> Option<UnicodeScalar> {
    SourceByte::new(byte).to_scalar(text).ok()
}

/// 组合串不可能含换行，但 `shape_line` 对 `\n` 会断言；这里统一换成空格，
/// 保证绘制路径永远安全（不 panic）。
pub(crate) fn sanitize_single_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect()
}

/// 把字节位置退到最近的 `char` 边界（兜底路径，避免落在多字节字符中间）。
fn floor_char_boundary(text: &str, mut byte: usize) -> usize {
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

#[cfg(test)]
mod search_shortcut_tests {
    use super::is_search_shortcut;
    use gpui::Keystroke;

    fn keystroke(text: &'static str) -> Keystroke {
        Keystroke::parse(text).expect("按键解析失败")
    }

    #[test]
    fn ctrl_k_and_ctrl_f_open_search() {
        assert!(is_search_shortcut(&keystroke("ctrl-k")));
        assert!(is_search_shortcut(&keystroke("ctrl-f")));
    }

    #[test]
    fn other_modifier_combinations_do_not_open_search() {
        assert!(!is_search_shortcut(&keystroke("k")), "裸 k 是打字");
        assert!(!is_search_shortcut(&keystroke("alt-k")));
        assert!(!is_search_shortcut(&keystroke("ctrl-shift-k")));
        assert!(!is_search_shortcut(&keystroke("ctrl-alt-f")));
        assert!(!is_search_shortcut(&keystroke("ctrl-j")));
    }
}
