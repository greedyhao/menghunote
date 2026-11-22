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
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, point, px, size, AnyElement, App, Bounds, ClipboardItem, Context, FocusHandle, Focusable,
    FontWeight, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, Render, SharedString,
    Window,
};
use menghunote_editor::{
    Bias, DisplayByte, EditorController, EditorError, GraphemeStep, InputDocument, MarkdownImage,
    Selection, SourceByte, SourceRange, StyleKind, UnicodeScalar,
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
use super::geometry::{ObjectPaint, ObjectPaintState};
use super::layout::LayoutFrame;
use super::object_image::{parse_asset_hash, ImageService, ImageState, SharedAssetProvider};
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

/// 图片对象的状态（只读快照，测试/诊断用；不参与绘制决策）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectImageState {
    /// 资产不存在 / 只有元数据（待同步）。
    Missing,
    /// 正在后台解码。
    Loading,
    /// 已解码。
    Ready,
    /// 解码失败（损坏 / 超预算 / 不支持）。
    Failed,
}

/// 一个图片 Object Block 的只读快照（headless 测试断言状态/几何用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectSnapshot {
    /// 源码行号（0 起）。
    pub line: usize,
    /// 完整源码跨度 `(start, end)`（UTF-8 bytes）。
    pub span: (usize, usize),
    /// Markdown 目标 URL（`asset://<hex>` 等）。
    pub dest: String,
    /// 资产/解码状态。
    pub state: ObjectImageState,
    /// 当前投影里的 display 区间（`None` = 没有投影成对象，源码可见）。
    pub display: Option<(usize, usize)>,
    /// 是否处于选中态（单击选中 / 选区相交）。
    pub selected: bool,
    /// 是否在显式源码模式。
    pub editing_source: bool,
    /// 源码是否直接可见（源码模式 / 未投影）。
    pub source_visible: bool,
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
    /// 本次 Enter 已由 key_down 落成编辑：平台随后送来的同一个 `\n` 文本输入要消费掉，
    /// 否则一次按键会变成两个 EditRequest（结构编辑 + 普通换行）。
    pending_enter_text_input: bool,
    /// 下一次绘制选区时是否把几何明细打到 stderr（拖拽/点击时置位，task-19 诊断用）。
    dump_selection: bool,
    /// 上一次观察到的「外部提交闸门持有中」状态（ADR 0009 切片 3b）。
    ///
    /// 只用于**探测状态变化**（变化时重绘一次，让 `editor-sync-held` 标记与状态栏文案
    /// 跟上）；真正的冻结判定每次都直接问驱动，绝不用这份缓存。
    held: bool,
    /// 图片资产加载 / 解码 / LRU 缓存（ADR 0019 S2a；后台单线程解码）。
    images: ImageService,
    /// 当前悬停的图片对象源码行（悬浮工具条的触发源）。
    hovered_object_line: Option<usize>,
    /// 单击选中的图片对象源码行（描边/工具条/F2 的目标；选区变化会清空）。
    selected_object_line: Option<usize>,
    /// 本帧悬浮工具条的**窗口坐标**矩形（元素层用它避免把点击误当成正文命中）。
    toolbar_rect: Option<Bounds<Pixels>>,
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
        asset_provider: SharedAssetProvider,
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
            pending_enter_text_input: false,
            dump_selection: false,
            held: false,
            images: ImageService::new(asset_provider),
            hovered_object_line: None,
            selected_object_line: None,
            toolbar_rect: None,
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
        // 图片后台解码完成：重绘一次，图片/占位与行高在下一次 prepaint 生效。
        if self.images.take_completed() > 0 {
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
    ///
    /// 整行独占的图片在这里是占位符（`U+FFFC`），不是源码；测试用它断言
    /// “选中对象不出现源码”。
    #[must_use]
    pub fn display_text(&self) -> String {
        self.controller.display_text().into_owned()
    }

    // ---- 图片 Object Block（ADR 0019 S2a）----

    /// 图片在编辑区里的内容高度（像素）：已解码按内容宽度等比，否则占位框高度。
    fn object_content_height(&self, image: &MarkdownImage, content_width: f32) -> f32 {
        let Some(hash) = parse_asset_hash(image.dest()) else {
            return theme::OBJECT_PLACEHOLDER_HEIGHT;
        };
        match self.images.state(hash) {
            ImageState::Ready(decoded) => {
                super::object_image::fit_size(decoded.width, decoded.height, content_width)
                    .map_or(theme::OBJECT_PLACEHOLDER_HEIGHT, |(_, height)| height)
            }
            ImageState::Loading | ImageState::Missing | ImageState::Failed(_) => {
                theme::OBJECT_PLACEHOLDER_HEIGHT
            }
        }
    }

    /// 图片行的**行高**（内容高度 + 上下内边距）。布局度量与绘制共用这一个函数。
    fn object_row_height(&self, image: &MarkdownImage, content_width: f32) -> f32 {
        self.object_content_height(image, content_width) + theme::OBJECT_PADDING * 2.0
    }

    /// 图片 alt 文案（源码原样，去掉首尾空白；过长截断，避免占位框里溢出）。
    fn object_alt_text(&self, image: &MarkdownImage) -> String {
        let source = self.controller.source_text();
        let alt = source
            .get(image.alt().start().get()..image.alt().end().get())
            .unwrap_or_default()
            .trim();
        if alt.is_empty() {
            return String::new();
        }
        let mut text: String = alt.chars().take(80).collect();
        if alt.chars().count() > 80 {
            text.push('…');
        }
        text
    }

    /// 占位文案：优先 alt，否则给状态说明（ADR 0019：加载失败显示 alt/名称占位）。
    fn object_placeholder_label(&self, image: &MarkdownImage, state: ImageState) -> String {
        let alt = self.object_alt_text(image);
        if !alt.is_empty() {
            return alt;
        }
        match state {
            ImageState::Loading => "图片加载中…".to_owned(),
            ImageState::Missing => "图片缺失或待同步".to_owned(),
            ImageState::Failed(_) => "图片无法预览".to_owned(),
            ImageState::Ready(_) => String::new(),
        }
    }

    /// 当前应画描边的对象行（显式点选 + 文本选区相交；不含源码模式）。
    fn highlighted_object_lines(&self) -> Vec<usize> {
        let mut lines = self.controller.object_lines_in_display_selection();
        if let Some(line) = self.selected_object_line {
            if self.controller.block_image_at_line(line).is_some() && !lines.contains(&line) {
                lines.push(line);
            }
        }
        lines
    }

    /// 悬浮工具条当前针对的对象行（显式源码模式时没有工具条）。
    fn toolbar_target_line(&self) -> Option<usize> {
        if self.controller.is_editing_object_source() {
            return None;
        }
        if let Some(line) = self.hovered_object_line {
            if self.controller.block_image_at_line(line).is_some() {
                return Some(line);
            }
        }
        self.highlighted_object_lines().first().copied()
    }

    /// 本帧要绘制的图片对象（只含可见行；元素层把它们转成窗口坐标）。
    ///
    /// `scroll` 是本帧实际使用的（已夹取的）滚动量；矩形在**元素局部坐标**里，
    /// 由调用方加上 `bounds.origin`。行高已经等于 `内容高 + 2 × OBJECT_PADDING`，
    /// 所以这里的矩形与 `LayoutFrame` 的行顶/行高几何完全一致。
    pub(crate) fn object_paints(
        &self,
        window: &Window,
        frame: &LayoutFrame,
        scroll: f32,
        content_width: f32,
        visible: Range<usize>,
    ) -> Vec<ObjectPaint> {
        let projection = self.controller.projection();
        let highlighted = self.highlighted_object_lines();
        let mut objects = Vec::new();
        for (index, image) in self.controller.images().iter().enumerate() {
            if !image.is_block() {
                continue;
            }
            let Some(display_range) = projection.object_display_range(image.span()) else {
                continue;
            };
            let row = frame.row_of_byte(display_range.start);
            if row < visible.start || row >= visible.end {
                continue;
            }
            let content_height = self.object_content_height(image, content_width);
            let state = parse_asset_hash(image.dest())
                .map_or(ImageState::Missing, |hash| self.images.state(hash));
            let display_width = match &state {
                ImageState::Ready(decoded) => {
                    super::object_image::fit_size(decoded.width, decoded.height, content_width)
                        .map_or(content_width, |(width, _)| width)
                }
                ImageState::Loading | ImageState::Missing | ImageState::Failed(_) => {
                    content_width.min(theme::OBJECT_PLACEHOLDER_HEIGHT * 8.0)
                }
            };
            let left = theme::PAD_X + frame.row_indent(row);
            let top = theme::PAD_Y - scroll + frame.row_top_offset(row) + theme::OBJECT_PADDING;
            let rect = Bounds::new(
                point(px(left), px(top)),
                size(px(display_width), px(content_height)),
            );
            let (paint_image, paint_state, label) = match state {
                ImageState::Ready(decoded) => (
                    Some(Arc::clone(&decoded.image)),
                    ObjectPaintState::Ready,
                    None,
                ),
                ImageState::Loading => (
                    None,
                    ObjectPaintState::Loading,
                    Some(self.object_placeholder_label(image, ImageState::Loading)),
                ),
                ImageState::Missing => (
                    None,
                    ObjectPaintState::Missing,
                    Some(self.object_placeholder_label(image, ImageState::Missing)),
                ),
                ImageState::Failed(error) => (
                    None,
                    ObjectPaintState::Failed,
                    Some(self.object_placeholder_label(image, ImageState::Failed(error))),
                ),
            };
            let label = label.filter(|text| !text.is_empty()).map(|text| {
                let spans = [(text.len(), StyleKind::Plain)];
                super::glyph::shape_row_styled_at(
                    window,
                    &text,
                    &spans,
                    theme::OBJECT_LABEL_FONT_SIZE,
                    false,
                )
            });
            objects.push(ObjectPaint {
                index,
                line: image.line(),
                row,
                rect,
                image: paint_image,
                label,
                state: paint_state,
                selected: highlighted.contains(&image.line()),
            });
        }
        objects
    }

    /// 请求加载当前文档里所有整行独占图片的资产（render 每帧调用，幂等）。
    fn request_object_images(&self) {
        for image in self.controller.block_images() {
            if let Some(hash) = parse_asset_hash(image.dest()) {
                self.images.request(hash);
            }
        }
    }

    /// 测试/诊断用：等后台解码收敛（有界）。生产路径由 pump + `take_completed` 驱动。
    pub fn settle_images(&mut self, timeout: Duration, cx: &mut Context<Self>) -> bool {
        let settled = self.images.wait_idle(timeout);
        if settled {
            cx.notify();
        }
        settled
    }

    /// 当前可见图片对象的**窗口坐标**矩形（`(源码行, 矩形)`，诊断/headless 测试用）。
    ///
    /// 几何来自上一帧 `LayoutFrame`（与绘制/命中同源）；图片滚出可见范围时不在列表里。
    #[must_use]
    pub fn object_rects(&self) -> Vec<(usize, Bounds<Pixels>)> {
        self.shared.geometry().map_or_else(Vec::new, |geometry| {
            geometry
                .objects()
                .iter()
                .map(|object| (object.line, object.rect))
                .collect()
        })
    }

    /// 图片对象的只读快照（headless 测试断言几何/状态用）。
    #[must_use]
    pub fn object_snapshots(&self) -> Vec<ObjectSnapshot> {
        let projection = self.controller.projection();
        let highlighted = self.highlighted_object_lines();
        self.controller
            .images()
            .iter()
            .filter(|image| image.is_block())
            .map(|image| {
                let state = parse_asset_hash(image.dest())
                    .map_or(ImageState::Missing, |hash| self.images.state(hash));
                let display = projection.object_display_range(image.span());
                let source_visible = display.is_none();
                ObjectSnapshot {
                    line: image.line(),
                    span: (image.span().start().get(), image.span().end().get()),
                    dest: image.dest().to_owned(),
                    state: match state {
                        ImageState::Missing => ObjectImageState::Missing,
                        ImageState::Loading => ObjectImageState::Loading,
                        ImageState::Ready(_) => ObjectImageState::Ready,
                        ImageState::Failed(_) => ObjectImageState::Failed,
                    },
                    display: display.map(|range| (range.start, range.end)),
                    selected: highlighted.contains(&image.line()),
                    editing_source: self.controller.object_source_edit_line() == Some(image.line()),
                    source_visible,
                }
            })
            .collect()
    }

    /// 是否能命中对象层（元素层的鼠标事件在工具条上不应改光标）。
    pub(crate) fn point_in_toolbar(&self, position: Point<Pixels>) -> bool {
        self.toolbar_rect
            .is_some_and(|rect| rect.contains(&position))
    }

    /// 鼠标移动：维护“当前悬停的图片对象行”，变化时重绘（工具条随之出现/消失）。
    pub(crate) fn hover_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if self.point_in_toolbar(position) {
            return;
        }
        let line = self
            .shared
            .geometry()
            .and_then(|geometry| geometry.object_line_at(position));
        if line != self.hovered_object_line {
            self.hovered_object_line = line;
            cx.notify();
        }
    }

    /// 单击图片对象：**只选中块**（描边），不展开源码，也不产生对象内部 caret。
    pub fn select_object_at(
        &mut self,
        line: usize,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let Some(image) = self.controller.block_image_at_line(line).cloned() else {
            return;
        };
        let text = self.controller.source_text();
        let Ok(caret) = SourceByte::new(image.span().end().get()).to_scalar(&text) else {
            return;
        };
        if let Some(window) = window {
            window.focus(&self.focus_handle, cx);
        }
        // caret 落在图片之后（对象内部不产生文本 caret）。
        self.commit_selection(Selection::caret(caret), cx);
        self.selected_object_line = Some(line);
        self.preferred_x = None;
        cx.notify();
    }

    /// 工具条「编辑源码」：把目标对象切换到源码模式（ADR 0019 §1）。
    pub fn enter_selected_object_source_edit(&mut self, cx: &mut Context<Self>) {
        let Some(line) = self
            .selected_object_line
            .filter(|line| self.controller.block_image_at_line(*line).is_some())
            .or_else(|| self.highlighted_object_lines().first().copied())
            .or_else(|| {
                self.hovered_object_line
                    .filter(|line| self.controller.block_image_at_line(*line).is_some())
            })
        else {
            return;
        };
        self.enter_object_source_edit(line, cx);
    }

    /// 进入指定图片行的源码模式。
    pub fn enter_object_source_edit(&mut self, line: usize, cx: &mut Context<Self>) {
        match self.controller.begin_object_source_edit(line) {
            Ok(true) => {
                self.selected_object_line = None;
                self.hovered_object_line = None;
                eprintln!("[menghunote] 图片对象 → 源码模式（行 {line}）");
                self.ensure_caret_visible(self.controller.selection().head());
                cx.notify();
            }
            Ok(false) => {}
            Err(error) => eprintln!("[menghunote] 进入图片源码模式失败：{error}"),
        }
    }

    /// 退出图片源码模式（`Esc` / 点击块外 / `Ctrl+Enter`）。
    pub fn exit_object_source_edit(&mut self, cx: &mut Context<Self>) {
        if self.controller.end_object_source_edit() {
            eprintln!("[menghunote] 图片对象 ← 退出源码模式（恢复图片/占位）");
            self.ensure_caret_visible(self.controller.selection().head());
            cx.notify();
        }
    }

    /// 工具条「复制 Markdown」：复制对象的**完整源码跨度**。
    pub fn copy_object_markdown(&mut self, line: usize, cx: &mut Context<Self>) {
        let Some(image) = self.controller.block_image_at_line(line) else {
            return;
        };
        let source = self.controller.source_text();
        let Some(markdown) = source
            .get(image.span().start().get()..image.span().end().get())
            .map(str::to_owned)
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(markdown));
        eprintln!(
            "[menghunote] 复制图片 Markdown（{} 字节，hash 前缀 {}）",
            image.dest().chars().count(),
            image.dest().chars().take(16).collect::<String>()
        );
    }

    /// 工具条「复制 Markdown」：作用于当前工具条目标对象。
    pub fn copy_selected_object_markdown(&mut self, cx: &mut Context<Self>) {
        if let Some(line) = self.toolbar_target_line() {
            self.copy_object_markdown(line, cx);
        }
    }

    /// 当前文本选区在 **source** 坐标下的区间（复制用；对象行会包含完整源码跨度）。
    #[must_use]
    pub fn selected_source_text(&self) -> Option<String> {
        let (start, end) = self.controller.selection_in_display()?;
        let source = self.controller.projection().source_range_of_display_range(
            DisplayByte::new(start.get())..DisplayByte::new(end.get()),
        )?;
        let text = self.controller.source_text();
        text.get(source.start().get()..source.end().get())
            .map(str::to_owned)
    }

    /// 逐**源码行**的度量（字号/行高/是否等宽/缩进），来自块级语义 + 列表块树。
    ///
    /// 列表几何（ADR 0019 S1）：
    /// - **首行**：`indent = 引用缩进 + depth × LIST_INDENT`；bullet/number 就在
    ///   display 文本行首，所以 `wrap_indent = indent + marker 显示宽度`；
    /// - **续行/子段落/内部围栏**：`indent = wrap_indent = 引用缩进 + depth × LIST_INDENT
    ///   + marker 显示宽度`，即 hanging indent，正好对齐内容列；
    /// - marker 显示宽度用**真实 shaping**（与绘制同一字体/字号）测量，不用字符数估算；
    ///   caret 进入 marker、显示源码 `- ` 时宽度会变，`WrapCache` 的度量键包含
    ///   `indent/wrap_indent`，因此会自动重排。
    pub(crate) fn block_metrics(
        &self,
        window: &Window,
        content_width: f32,
    ) -> Vec<super::layout::LineMetric> {
        use menghunote_editor::{BlockKind, MarkdownLineKind};
        use std::collections::HashMap;

        // 引用缩进要看**源码行**的前缀（`>` 在投影里是隐藏的）。
        let source = self.controller.source_text();
        let source_lines: Vec<&str> = source.split_inclusive('\n').collect();
        let line_infos = self.controller.line_infos();
        let items = self.controller.list_items();
        // 键包含字号与等宽标记：同一个 "• " 在标题行/正文行宽度不同，不能混用。
        let mut marker_widths: HashMap<(u32, bool, String), f32> = HashMap::new();

        let object_edit_line = self.controller.object_source_edit_line();
        self.controller
            .block_runs()
            .iter()
            .enumerate()
            .map(|(index, run)| {
                let source_line = source_lines.get(index).copied().unwrap_or("");
                // 整行独占的图片（且不在源码模式）：行高 = 图片/占位高度 + 内边距。
                // 这是布局、绘制、命中三处**唯一**的高度来源（不在这里估字符宽度）。
                if object_edit_line != Some(index) {
                    if let Some(image) = self.controller.block_image_at_line(index) {
                        return super::layout::LineMetric {
                            font_size: theme::FONT_SIZE,
                            line_height: self.object_row_height(image, content_width),
                            mono: false,
                            indent: 0.0,
                            wrap_indent: 0.0,
                            quote_depth: 0,
                        };
                    }
                }
                let quote_depth = super::layout::quote_depth(source_line);
                let quote = super::layout::quote_indent(quote_depth);
                let line_info = line_infos.get(index);

                let (mut font_size, mut line_height, mut mono) = match run.kind() {
                    BlockKind::Heading => {
                        let level = run.level().unwrap_or(1);
                        let size = theme::heading_font_size(level);
                        (size, theme::line_height_for(size), false)
                    }
                    BlockKind::FencedCode => (theme::FONT_SIZE, theme::DEFAULT_LINE_HEIGHT, true),
                    BlockKind::BlockQuote | BlockKind::ListItem | BlockKind::Paragraph => {
                        (theme::FONT_SIZE, theme::DEFAULT_LINE_HEIGHT, false)
                    }
                };
                // 嵌套在列表里的围栏（旧 scanner 看不到的 4+ 空格围栏）也必须是等宽。
                if line_info.is_some_and(|info| {
                    matches!(
                        info.kind,
                        MarkdownLineKind::FencedCode | MarkdownLineKind::IndentedCode
                    )
                }) {
                    mono = true;
                }

                let mut indent = quote;
                let mut wrap_indent = quote;
                // 无缩进的惰性续行（例如删掉 `2.` 的 `.` 后只剩 `2`）仍被 pulldown
                // 归到上一个 item 的 paragraph；编辑体验上它已经不在列表里，不能再按
                // 续行挂到内容列。只有真缩进的续行/子段落才保留 hanging indent。
                let list = line_info
                    .and_then(|info| info.list.as_ref())
                    .filter(|list| {
                        list_indent_applies(
                            list.is_item_start,
                            list.hidden_prefix.is_empty(),
                            source_line.trim().is_empty(),
                        )
                    });
                if let Some(list) = list {
                    let level_indent = quote + list.depth as f32 * theme::LIST_INDENT_PER_LEVEL;
                    let marker = items
                        .get(list.item)
                        .map(|_| {
                            if self.controller.list_marker_revealed(list.item) {
                                self.controller
                                    .list_marker_source(list.item)
                                    .unwrap_or_default()
                                    .to_owned()
                            } else {
                                self.controller
                                    .list_marker_text(list.item)
                                    .unwrap_or_default()
                            }
                        })
                        .unwrap_or_default();
                    // marker 只在 item 首行显示：续行（尤其内部围栏的等宽行）要用
                    // **首行**的字体/字号测宽度，否则 hanging indent 会与内容列错位。
                    let (marker_font_size, marker_mono) = if list.is_item_start {
                        (font_size, mono)
                    } else {
                        let first_line = items.get(list.item).map_or(0, |item| item.first_line);
                        let first_size = self.controller.block_runs().get(first_line).map_or(
                            theme::FONT_SIZE,
                            |first| match first.kind() {
                                BlockKind::Heading => {
                                    theme::heading_font_size(first.level().unwrap_or(1))
                                }
                                _ => theme::FONT_SIZE,
                            },
                        );
                        (first_size, false)
                    };
                    let key = (marker_font_size.to_bits(), marker_mono, marker.clone());
                    let width = if marker.is_empty() {
                        0.0
                    } else if let Some(width) = marker_widths.get(&key) {
                        *width
                    } else {
                        let spans = [(marker.len(), StyleKind::Plain)];
                        let width = super::glyph::shape_row_styled_at(
                            window,
                            &marker,
                            &spans,
                            marker_font_size,
                            marker_mono,
                        )
                        .width()
                        .as_f32();
                        let width = if width.is_finite() {
                            width.max(0.0)
                        } else {
                            0.0
                        };
                        marker_widths.insert(key, width);
                        width
                    };
                    if list.is_item_start {
                        indent = level_indent;
                        wrap_indent = level_indent + width;
                    } else {
                        indent = level_indent + width;
                        wrap_indent = indent;
                    }
                }
                // 标题字号只由 block_runs 决定；行高与之配套（逐行可变）。
                if matches!(run.kind(), BlockKind::Heading) {
                    font_size = font_size.max(theme::FONT_SIZE);
                    line_height = theme::line_height_for(font_size);
                }
                super::layout::LineMetric {
                    font_size,
                    line_height,
                    mono,
                    indent,
                    wrap_indent,
                    quote_depth,
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
    ///
    /// 图片解码线程随视图 Drop 关闭（worker 收到 `Shutdown` 后退出，不等待未完成的解码）。
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
        self.pending_enter_text_input = false;
        // 点击块外：正在编辑图片源码时先退出（ADR 0019：Esc / 点击块外 / Ctrl+Enter）。
        if let Some(editing) = self.controller.object_source_edit_line() {
            let clicked_line = self.shared.geometry().map(|geometry| {
                let row = geometry.frame.row_of_byte(display_byte);
                geometry.frame.lines[row].source_line
            });
            if clicked_line != Some(editing) {
                self.exit_object_source_edit(cx);
                // 退出后 display 变短，旧几何给出的字节需要夹回新 display 长度。
                let display_byte = display_byte.min(self.controller.display_len().get());
                let display_byte = self.snap_display_byte(display_byte);
                self.place_caret_from_display(display_byte, bias, window, cx);
                return;
            }
        }
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

    /// 把 display 字节落成 caret（带偏置；点击路径的公共尾巴）。
    fn place_caret_from_display(
        &mut self,
        display_byte: usize,
        bias: Bias,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scalar) = self.scalar_at_display_byte_biased(display_byte, bias) else {
            return;
        };
        let text = self.controller.source_text();
        let byte = self.snap_byte(self.source_byte_of_scalar(scalar).unwrap_or(0), &text);
        let Some(scalar) = scalar_of_byte(&text, byte) else {
            return;
        };
        self.drag_anchor = Some(scalar);
        self.preferred_x = None;
        window.focus(&self.focus_handle, cx);
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
        if event.keystroke.key.as_str() != "enter" {
            // 平台没有补发 Enter 文本输入时，标记不能留到下一次普通输入。
            self.pending_enter_text_input = false;
        }
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

        // F2：有图片对象可编辑时切换到**该块的源码模式**；否则请宿主重命名侧栏项。
        if !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && event.keystroke.key.as_str() == "f2"
        {
            if self.toolbar_target_line().is_some() {
                self.enter_selected_object_source_edit(cx);
            } else {
                cx.emit(EditorEvent::RenameRequested);
            }
            window.prevent_default();
            return;
        }

        // Esc：正在编辑图片源码时先退出源码模式（对象内部没有文本 caret）。
        if event.keystroke.key.as_str() == "escape"
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && self.controller.is_editing_object_source()
        {
            self.exit_object_source_edit(cx);
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
                // Ctrl+C：选中图片对象时复制完整 Markdown；否则复制选区的源码。
                ("c", false) => {
                    if let Some(line) = self.selected_object_line {
                        self.copy_object_markdown(line, cx);
                        return;
                    }
                    if let Some(text) = self.selected_source_text() {
                        if !text.is_empty() {
                            eprintln!("[menghunote] 复制选区源码（{} 字节）", text.len());
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                    }
                }
                // Ctrl+Enter：退出图片源码模式（与 Esc 等价）。
                ("enter", _) if self.controller.is_editing_object_source() => {
                    self.exit_object_source_edit(cx);
                    window.prevent_default();
                }
                _ => {}
            }
            return;
        }

        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            return;
        }
        let extend = modifiers.shift;
        match event.keystroke.key.as_str() {
            // 换行：列表 item 会同级续写/空项退出；其余情况与 IME 上屏同一条路径。
            // `prevent_default` 阻止平台随后把 Enter 再当文本输入插一个 `\n`
            // （否则一次按键会落两个 EditRequest：结构编辑 + 普通换行）。
            "enter" => {
                window.prevent_default();
                self.enter_newline(cx);
            }
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
        self.pending_enter_text_input = false;
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
        self.pending_enter_text_input = false;
        self.selected_object_line = None;
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

    /// Enter：列表内同级新建 / 空项退出；其余情况插入普通换行。
    ///
    /// **IME 守卫**：组合期间先取消组合并**丢弃本次结构动作**（ADR 0019 §1：
    /// IME 合成期间不触发结构动作）。结构编辑仍走 [`Self::submit_request`]，
    /// 一次按键一个 `EditRequest`。
    fn enter_newline(&mut self, cx: &mut Context<Self>) {
        // 新的 Enter 一律从干净状态开始：上一次平台没有补发 `\n` 时不能吃掉这一次。
        self.pending_enter_text_input = false;
        if self.frozen_by_external_commit() {
            return;
        }
        if self.is_composing() {
            self.cancel_composition(cx);
            return;
        }
        match self.controller.list_enter() {
            Ok(Some(request)) => {
                eprintln!("[menghunote] 列表 Enter：结构编辑 {}", request.command_id);
                self.submit_request(Some(request));
                self.ensure_caret_visible(self.controller.selection().head());
                self.pending_enter_text_input = true;
            }
            Ok(None) => {
                eprintln!("[menghunote] Enter：非列表/续行，按普通换行处理");
                self.insert_text("\n", cx);
                self.pending_enter_text_input = true;
            }
            Err(EditorError::PreeditActive) => {
                eprintln!("[menghunote] 列表 Enter 被拒（组合中）：先取消组合，本次按键丢弃");
                self.cancel_composition(cx);
            }
            Err(error) => eprintln!("[menghunote] 列表 Enter 失败：{error}"),
        }
        cx.notify();
    }

    /// 普通文本输入（无组合：打字 / 粘贴）。
    pub(crate) fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.selected_object_line = None;
        if text == "\n" && self.pending_enter_text_input {
            // Enter 已经由 key_down 处理（结构编辑或普通换行），这里只消费平台的回声。
            self.pending_enter_text_input = false;
            eprintln!("[menghunote] 已消费平台重复的 Enter 文本输入（避免双换行）");
            return;
        }
        self.pending_enter_text_input = false;
        if self.frozen_by_external_commit() {
            // Held：打字/粘贴一律忽略（控制器镜像也不能先乐观改动，否则之后无法对齐）。
            return;
        }
        match self.controller.compose_replace_selection(text) {
            Ok(request) => {
                self.submit_request(request);
            }
            Err(error) => eprintln!("[menghunote] 插入文本失败：{error}"),
        }
        cx.notify();
    }

    /// 附件插入的目标偏移（**不产生编辑**）。
    ///
    /// 先做与 [`Self::insert_attachment_markdown`] 完全相同的闸门检查（持闸 / 组合中），
    /// 再返回选区起点在 **source 字节**坐标下的值——宿主用它算 `asset_refs.start_byte`，
    /// 因此"读文件/换算失败"必须发生在任何落库之前。
    #[cfg(feature = "dev-sync")]
    pub(crate) fn attachment_insert_offset(&mut self) -> Result<usize, String> {
        if self.frozen_by_external_commit() {
            return Err("同步持闸中，暂不能插入附件".to_owned());
        }
        if self.is_composing() {
            return Err("输入法组合中：请先上屏或按 Esc".to_owned());
        }
        let source = self.controller.source_text();
        self.controller
            .selection()
            .start()
            .to_source_byte(&source)
            .map(|byte| byte.get())
            .map_err(|error| format!("光标位置换算失败：{error}"))
    }

    /// 附件插入：在光标/选区处插入一段 Markdown（**一个编辑事务**）。
    ///
    /// 与普通输入走**同一条** [`Self::submit_request`] 路径（Held/组合/闸门语义一致），
    /// 因此"插入成功"在这里就能判定，而"一次插入 = 一个撤销单元"由驱动保证。
    ///
    /// 返回这段 Markdown 在 **source 字节**里的起点（`asset_refs.start_byte` 要用它）。
    /// 失败（持闸 / 组合中 / 编辑被拒）返回人类可读原因，**不产生半链接**：调用方在
    /// 收到错误时会回滚刚写下的 `asset_refs` 行。
    #[cfg(feature = "dev-sync")]
    pub(crate) fn insert_attachment_markdown(
        &mut self,
        markdown: &str,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        if self.frozen_by_external_commit() {
            return Err("同步持闸中，暂不能插入附件".to_owned());
        }
        if self.is_composing() {
            return Err("输入法组合中：请先上屏或按 Esc".to_owned());
        }
        let source = self.controller.source_text();
        let start_byte = self
            .controller
            .selection()
            .start()
            .to_source_byte(&source)
            .map_err(|error| format!("光标位置换算失败：{error}"))?
            .get();
        let request = self
            .controller
            .compose_replace_selection(markdown)
            .map_err(|error| format!("插入附件失败：{error}"))?;
        match self.submit_request(request) {
            Some(EditOutcome::Applied(_)) => {
                cx.notify();
                Ok(start_byte)
            }
            Some(EditOutcome::Rejected(rejected)) => {
                Err(format!("插入附件被拒（{:?}）", rejected.reason))
            }
            Some(EditOutcome::Stale(stale)) => Err(format!(
                "插入附件基准过期（当前 live_rev={}）",
                stale.current_live_rev.0
            )),
            None => Err("插入附件未生效（输入被冻结或编辑请求为空）".to_owned()),
        }
    }

    /// 空列表项行尾的 Backspace 目标：marker 最后一个空格/Tab（source 单字节）。
    ///
    /// 只在“collapsed caret 在该 item marker 之后、且 item 内容只有空白”时生效；
    /// 这样 `1. ` 退格只得到 `1.`（不再渲染列表），而不是把 `1. ` 整段删掉。
    fn empty_list_item_space_range(&self) -> Option<SourceRange> {
        let selection = self.controller.selection();
        if !selection.is_empty() {
            return None;
        }
        let source = self.controller.source_text();
        let head = selection.head().to_source_byte(&source).ok()?.get();
        let line_index = source.as_bytes()[..head]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        let line = self.controller.line_infos().get(line_index)?;
        let list = line.list.as_ref()?;
        let item = self.controller.list_items().get(list.item)?;
        if item.first_line != line_index || head < item.marker.end {
            return None;
        }
        if !source
            .get(item.content_start..line.content.end)?
            .trim()
            .is_empty()
        {
            return None;
        }
        let marker = source.get(item.marker.start..item.marker.end)?;
        let last = *marker.as_bytes().last()?;
        if last != b' ' && last != b'\t' {
            return None;
        }
        let start = item.marker.end.checked_sub(1)?;
        SourceRange::within(
            &source,
            SourceByte::new(start),
            SourceByte::new(item.marker.end),
        )
        .ok()
    }

    /// 本次删除在 **source** 坐标下的最小区间（display 取相邻可见 grapheme）。
    ///
    /// 有选区时删选区；否则按方向取显示上相邻的一个 grapheme。跨构造的选区会把
    /// 段内标记一起删掉——这是允许的（用户显式选中的是真实文本）。
    fn display_delete_source_range(
        &mut self,
        step: GraphemeStep,
    ) -> Option<menghunote_editor::SourceRange> {
        // 空列表项的 Backspace：只删 marker 末尾那一个空格/Tab，不把整个 marker 删掉。
        // 否则 display 上 bullet 是一个 Replace 段，按可见 grapheme 反查会一次删掉
        // `1. `（甚至连同缩进），用户再按 Enter 时只剩一段残留空格。
        if matches!(step, GraphemeStep::Backward) {
            if let Some(range) = self.empty_list_item_space_range() {
                return Some(range);
            }
        }
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
        let range = projection
            .source_range_of_display_range(DisplayByte::new(start)..DisplayByte::new(end))?;
        if matches!(step, GraphemeStep::Backward) {
            Some(self.expand_dangling_list_indent(range))
        } else {
            Some(range)
        }
    }

    /// 删掉列表 marker 最后一个字符时，如果整行只剩缩进空白，就把缩进一起删掉。
    ///
    /// 场景：`  - ` 先删空格得到 `  -`，再删 `-`；普通 grapheme 删除只删 `-`，
    /// 会留下 `  `，随后 Enter 就在一个“空缩进行”上继续。这里只在**删除后整行
    /// 全空白、且原行只包含缩进 + marker 字符**时扩展删除范围，不影响正常文本
    /// 行尾空格（`abc  ` 这种原行含字母，不会触发）。
    fn expand_dangling_list_indent(&self, range: SourceRange) -> SourceRange {
        if !self.controller.selection().is_empty() {
            return range;
        }
        let source = self.controller.source_text();
        let start = range.start().get();
        let end = range.end().get();
        if start > end || end > source.len() {
            return range;
        }
        let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
        let line_end = source[start..]
            .find('\n')
            .map_or(source.len(), |index| start + index);
        let line = &source[line_start..line_end];
        let trimmed = line.trim();
        if trimmed.is_empty()
            || !trimmed.bytes().all(|byte| {
                byte.is_ascii_digit() || matches!(byte, b'-' | b'*' | b'+' | b'.' | b')')
            })
        {
            return range;
        }
        let mut remaining = String::with_capacity(line.len());
        remaining.push_str(&source[line_start..start]);
        remaining.push_str(&source[end..line_end]);
        if !remaining.chars().all(char::is_whitespace) {
            return range;
        }
        SourceRange::within(&source, SourceByte::new(line_start), SourceByte::new(end))
            .unwrap_or(range)
    }

    /// 折叠 caret 是否紧贴某个图片对象（Backspace 在对象之后 / Delete 在对象之前）。
    fn adjacent_object_line(&self, step: GraphemeStep) -> Option<usize> {
        let caret = self.display_caret()?;
        self.controller.block_images().find_map(|image| {
            let range = self.controller.object_display_range(image.line())?;
            let hit = match step {
                GraphemeStep::Backward => caret == range.end,
                GraphemeStep::Forward => caret == range.start,
            };
            hit.then_some(image.line())
        })
    }

    /// 删除一个图片对象的完整源跨度（一次 `EditRequest`），并清空对象选中态。
    fn delete_object_block(&mut self, line: usize, cx: &mut Context<Self>) {
        match self.controller.delete_object(line) {
            Ok(Some(request)) => {
                self.selected_object_line = None;
                self.hovered_object_line = None;
                eprintln!("[menghunote] 删除图片对象（行 {line}）：整段源码一次删除");
                self.submit_request(Some(request));
            }
            Ok(None) => {
                self.selected_object_line = None;
                eprintln!("[menghunote] 删除图片对象（行 {line}）：没有可删区间，忽略");
            }
            Err(error) => eprintln!("[menghunote] 删除图片对象失败：{error}"),
        }
        cx.notify();
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
        // 图片对象（ADR 0019 §1）：对象内部没有文本 caret。
        // - 块已被点选 → 直接删整个源跨度；
        // - 折叠 caret 紧贴对象 → **先选块**（本次按键不删）；
        // - 文本选区跨过对象 → 走普通“删选区”（source 区间已包含完整 Markdown）。
        if !self.controller.is_editing_object_source() {
            if let Some(line) = self.selected_object_line {
                if self.controller.block_image_at_line(line).is_some() {
                    self.delete_object_block(line, cx);
                    return;
                }
            }
            if self.controller.selection().is_empty() {
                if let Some(line) = self.adjacent_object_line(step) {
                    self.selected_object_line = Some(line);
                    eprintln!(
                        "[menghunote] 删除（{step:?}）：先选中图片对象（行 {line}），再按一次删除"
                    );
                    cx.notify();
                    return;
                }
            }
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
    ///
    /// 返回**已交给驱动的结局**（`None` = 没有产生请求：Held 丢弃 / no-op / 组合期挂起）。
    /// 普通输入不关心返回值；附件插入要用它判断"这次事务是否真的生效"。
    fn submit_request(&mut self, request: Option<EditRequest>) -> Option<EditOutcome> {
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
            return None;
        }
        let Some(request) = request else {
            // 空选区 + 空替换是 no-op：不产生请求，但要留痕，避免被误读成"丢请求"。
            eprintln!("[menghunote] 本次输入是 no-op（空选区 + 空文本），不产生 EditRequest");
            return None;
        };
        self.request_seq = self.request_seq.saturating_add(1);
        let seq = self.request_seq;

        if self.driver.is_composing() {
            eprintln!(
                "[menghunote] 请求#{seq} 挂起：组合进行中 command_id={}",
                request.command_id
            );
            self.composition.pending_retry = Some(request);
            return None;
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
            return None;
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
        self.apply_outcome_guarded(outcome.clone());
        Some(outcome)
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
        // 选区一变，图片对象的“点选态”就结束（工具条/描边随之消失）。
        self.selected_object_line = None;
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
        // 对象行没有字形：上下移动一律落到占位符末尾（= 图片之后），不落进源码中间。
        if self
            .shared
            .geometry()
            .is_some_and(|geometry| geometry.object_row(row).is_some())
        {
            return bytes.end;
        }
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

impl EditorView {
    /// 悬浮工具条（ADR 0019 §1）：至少「编辑源码」「复制 Markdown」。
    ///
    /// 位置来自**上一帧几何**里该对象的矩形（窗口坐标减去元素原点 = 元素局部坐标），
    /// 因此滚动/重排后一帧内自动跟随；图片滚出可见范围时不渲染。
    fn render_object_toolbar(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let line = self.toolbar_target_line()?;
        let geometry = self.shared.geometry()?;
        let object = geometry
            .objects()
            .iter()
            .find(|object| object.line == line)?;
        let (viewport_width, _) = self.shared.viewport();
        let toolbar_width = theme::OBJECT_TOOLBAR_BUTTON_WIDTH * 2.0 + 8.0;
        let toolbar_height = theme::OBJECT_TOOLBAR_HEIGHT;
        let local_left = (object.rect.origin.x - geometry.origin.x).as_f32()
            + object.rect.size.width.as_f32()
            - toolbar_width;
        let local_left = local_left.clamp(
            theme::PAD_X,
            (viewport_width - toolbar_width - theme::PAD_X).max(theme::PAD_X),
        );
        let local_top = ((object.rect.origin.y - geometry.origin.y).as_f32()
            + theme::OBJECT_TOOLBAR_GAP)
            .max(theme::PAD_Y);
        // 元素层用窗口坐标判“点击是否落在工具条上”，避免同一次点击又挪光标。
        self.toolbar_rect = Some(Bounds::new(
            point(
                geometry.origin.x + px(local_left),
                geometry.origin.y + px(local_top),
            ),
            size(px(toolbar_width), px(toolbar_height)),
        ));

        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .flex()
                .items_center()
                .justify_center()
                .w(px(theme::OBJECT_TOOLBAR_BUTTON_WIDTH))
                .h_full()
                .text_size(px(12.0))
                .text_color(theme::object_toolbar_text())
                .child(SharedString::from(label))
        };

        let edit = button("object-toolbar-edit", "编辑源码").on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
                view.enter_selected_object_source_edit(cx);
            }),
        );
        let copy = button("object-toolbar-copy", "复制 Markdown").on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
                view.copy_selected_object_markdown(cx);
            }),
        );

        Some(
            div()
                .id("object-toolbar")
                .debug_selector(|| "object-toolbar".to_string())
                .absolute()
                .left(px(local_left))
                .top(px(local_top))
                .flex()
                .flex_row()
                .h(px(toolbar_height))
                .bg(theme::object_toolbar_bg())
                .border(px(1.0))
                .border_color(theme::object_toolbar_border())
                .rounded(px(theme::OBJECT_CORNER_RADIUS))
                .overflow_hidden()
                .child(edit)
                .child(copy)
                .into_any_element(),
        )
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
        // 每帧请求加载当前文档的图片（幂等；解码完成由 pump 触发重绘）。
        self.request_object_images();
        // 工具条每帧重建：目标对象/几何变化时自动跟随，None 时清空命中矩形。
        self.toolbar_rect = None;
        let toolbar = self.render_object_toolbar(cx);
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
                    .child(element)
                    .when_some(toolbar, |this, toolbar| this.child(toolbar)),
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

/// 列表续行是否应该占用列表缩进。
///
/// 无缩进、非空、且不是 item 首行的行是“惰性续行”：删掉 marker 后用户看到的是
/// 普通段落，不能再按列表内容列缩进；真正缩进的续行/子段落仍要 hanging indent。
/// 空白行也不继承缩进——它没有可见字符，但光标/折行仍会落在列表内容列，形成
/// “看不见的缩进”。
fn list_indent_applies(is_item_start: bool, hidden_prefix_empty: bool, blank: bool) -> bool {
    is_item_start || (!blank && !hidden_prefix_empty)
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

#[cfg(test)]
mod list_indent_tests {
    use super::list_indent_applies;

    #[test]
    fn unindented_lazy_continuation_is_not_list_indented() {
        assert!(!list_indent_applies(false, true, false));
    }

    #[test]
    fn indented_continuation_keeps_indent_but_blank_line_does_not() {
        assert!(list_indent_applies(false, false, false));
        assert!(list_indent_applies(true, true, false));
        assert!(!list_indent_applies(false, true, true));
        assert!(!list_indent_applies(false, false, true));
    }
}
