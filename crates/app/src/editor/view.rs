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

use std::collections::{BTreeSet, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    div, img, point, px, size, AnyElement, App, Bounds, ClipboardItem, Context, CursorStyle,
    ExternalPaths, FocusHandle, Focusable, FontWeight, ImageSource, KeyDownEvent, MouseButton,
    MouseDownEvent, ObjectFit, Pixels, Point, Render, SharedString, Window,
};
use menghunote_editor::{
    Bias, DisplayByte, EditorController, EditorError, GraphemeStep, InputDocument, MarkdownImage,
    Selection, SourceByte, SourceRange, StyleKind, UnicodeScalar, TASK_BOX_DISPLAY,
};
use menghunote_storage::rename_links::{plan_wiki_link_renames, scalar_edits_for_renames};
#[cfg(feature = "dev-sync")]
use menghunote_storage::session::{
    AdoptError, AdoptOutcome, AppliedHeads, ExternalCommitLease, ExternalCommitTicket, GateError,
    GateState, PreparedCommittedDelta,
};
use menghunote_storage::session::{
    CloseReport, DriverEvent, EditOutcome, EditRequest, FlushReason, LiveDelta, LiveRev,
    RejectReason, ScalarEdit, SessionDriver,
};
use menghunote_storage::MaterializeOutcome;

use super::clipboard::{
    choose_paste, AssetRefSummary, ImportPayload, ImportReport, ImportSource, ImportedSummary,
    PasteAction, SharedImportWriter, MARKDOWN_METADATA_PREFIX,
};
use super::element::EditorElement;
use super::geometry::{
    ObjectPaint, ObjectPaintContent, ObjectPaintState, TableCellPaint, TableDragAxis,
    TableDragHandlePaint, TablePaint, TableRowPaint,
};
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

/// 表格边缘浮层的**插入**结构动作（ADR 0019 S3b）：加行 / 加列。
///
/// 删除行列用 [`TableDeleteAxis`]（语义与 caret/选中态修正规则不同，分开实现）；
/// 拖拽重排、合并单元格仍不在本切片范围内。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableStructureAction {
    /// 在最后一条数据行之后插入一条空数据行。
    Row,
    /// 给表头 / 分隔行 / 每条数据行末尾追加一列。
    Column,
}

impl TableStructureAction {
    /// 点击走到表格结构动作（加行 / 加列）的日志名。
    const fn label(self) -> &'static str {
        match self {
            Self::Row => "加行",
            Self::Column => "加列",
        }
    }
}

/// 表格删除动作的轴（ADR 0019 S3b3）：删行 / 删列。
///
/// 与 [`TableStructureAction`]（只插入）分开命名：删除是“去掉已有字节”，caret/选中态
/// 修正规则与插入不同，混在一个枚举里容易把插入路径也走成删除语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableDeleteAxis {
    /// 删除选中的**数据行**（表头行不提供删除）。
    Row,
    /// 删除选中的列（至少保留 1 列）。
    Column,
}

impl TableDeleteAxis {
    /// 日志/提示用的中文名。
    const fn label(self) -> &'static str {
        match self {
            Self::Row => "删除行",
            Self::Column => "删除列",
        }
    }
}

/// 正文右键菜单（ADR 0019 S3b4）：目前只有「插入 → 表格」。
///
/// 只存右键**落点**（窗口坐标）：渲染时按编辑器视口夹取，不改变正文 caret/选区。
/// 插入位置始终是打开菜单时的正文 caret/选区（右键本身不落 caret）。
#[derive(Debug, Clone, Copy, PartialEq)]
struct EditorContextMenu {
    /// 右键落点（窗口坐标；菜单面板左上角的锚点）。
    position: Point<Pixels>,
}

/// 编辑器窗口的主视图。
/// 编辑器对宿主（`AppShell`）的少量事件。
///
/// 用事件而不是回调：侧栏要知道"按了 F2 / 点进了正文"，但不该把 shell 的引用塞进
/// 编辑器（依赖方向反了）。宿主 `cx.subscribe` 即可。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorEvent {
    /// 按了 `F2`：请宿主重命名侧栏当前选中的节点。
    RenameRequested,
    /// 正文被按下鼠标：宿主可用它取消行内编辑。
    PointerDown,
    /// 按了 `Ctrl+K` / `Ctrl+F`：请宿主打开搜索浮层。
    SearchRequested,
    /// 一次粘贴/拖放的资产导入结果（宿主据此写提示行；失败时正文与引用都未改变）。
    ImportReported(Box<ImportReport>),
    /// 点击 wiki 链接：目标与可选的 `#heading`（解析/打开由宿主负责）。
    WikiLinkRequested {
        /// 目标笔记文本（`[[#H]]` 时为空）。
        target: String,
        /// `#` 后的标题（不含 `#`）。
        heading: Option<String>,
    },
    /// 点击普通 Markdown 链接：宿主负责校验 scheme 并打开外链。
    ExternalLinkRequested(String),
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

/// 一个表格 Object Block 的只读快照（headless 测试断言状态用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSnapshot {
    /// 表头行行号（0 起；也是对象行）。
    pub line: usize,
    /// 末行行号（0 起，含）。
    pub end_line: usize,
    /// 完整源码跨度 `(start, end)`（UTF-8 bytes；不含末尾换行）。
    pub span: (usize, usize),
    /// 列数。
    pub columns: usize,
    /// 数据行数（不含表头与分隔行）。
    pub rows: usize,
    /// 当前投影里的 display 区间（`None` = 没有投影成对象，源码可见）。
    pub display: Option<(usize, usize)>,
    /// 是否处于选中态（单击选中 / 选区相交）。
    pub selected: bool,
    /// 是否在显式源码模式。
    pub editing_source: bool,
    /// 源码是否直接可见（源码模式 / 未投影）。
    pub source_visible: bool,
    /// 单击选中的单元格 `(row, column)`（row 0 = 表头；ADR 0019 S3b2）。
    pub selected_cell: Option<(usize, usize)>,
    /// 正在就地编辑的单元格 `(row, column)`；`None` = 不在单元格编辑态。
    pub editing_cell: Option<(usize, usize)>,
}

/// 一次表格行列拖拽的**进行中**状态（ADR 0019 S3b5；headless/诊断可读）。
///
/// 只存在一次鼠标手势的生命周期内：按下把手 → 拖动（`slot` 每次移动更新）→ 释放
/// （一次 `EditRequest`）或取消（`Esc` / 拖出表格：不产生任何请求）。
/// **拖拽期间正文 caret/选区一律不动**（元素层在拖拽时直接放行文本命中）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TableDragState {
    /// 表格对象行（表头源码行号）。
    pub line: usize,
    /// 拖拽轴向（列把手水平拖，行把手垂直拖）。
    pub axis: TableDragAxis,
    /// 被拖动的项：列下标 / 数据行下标（0 起、不含表头）。
    pub from: usize,
    /// 当前插入槽位（列：`0..=columns`；行：`0..=row_count`）；`None` = 已拖出表格。
    pub slot: Option<usize>,
    /// 槽位换算出的**最终下标**（`None` = 原地/越界，释放也不产生编辑）。
    pub target: Option<usize>,
    /// 最近一次指针位置（窗口坐标）；刚开始拖拽时是 `(0, 0)`，第一次移动后更新。
    pub position: Point<Pixels>,
}

/// 一个可见视觉行的度量快照（headless/诊断用）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisualRowMetric {
    /// 视觉行号（0 起，整个文档顺序）。
    pub row: usize,
    /// 该视觉行的 display 字节区间。
    pub bytes: (usize, usize),
    /// 字号（像素）。
    pub font_size: f32,
    /// 行高（像素）。
    pub line_height: f32,
    /// 该行左侧缩进（像素）。
    pub indent: f32,
}

/// 一个可点击任务复选框的**视图层**描述（ADR 0019 S3）。
///
/// 只含 source / display 字节与勾选态；窗口坐标矩形由 [`super::geometry::TaskBoxPaint`]
/// 根据本帧 shaping 给出（绘制、命中、headless 断言同源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskCheckbox {
    /// 列表项下标（按 `ParsedMarkdown::items` 顺序）。
    pub item: usize,
    /// 是否已勾选（解析器状态）。
    pub checked: bool,
    /// 任务框源码区间 `(start, end)`（3 字节，含方括号）。
    pub source: (usize, usize),
    /// 占位文本的 display 区间（两连 NBSP；绘制/命中都在这个区间上）。
    pub display: (usize, usize),
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
    /// 本次 Tab 已由 key_down 用来“提交 + 跳到下一格”：平台随后送来的 `\t` 文本输入
    /// 要消费掉，否则制表符会被插进下一格（ADR 0019 S3b2）。
    pending_tab_text_input: bool,
    /// 下一次绘制选区时是否把几何明细打到 stderr（拖拽/点击时置位，task-19 诊断用）。
    dump_selection: bool,
    /// 上一次观察到的「外部提交闸门持有中」状态（ADR 0009 切片 3b）。
    ///
    /// 只用于**探测状态变化**（变化时重绘一次，让 `editor-sync-held` 标记与状态栏文案
    /// 跟上）；真正的冻结判定每次都直接问驱动，绝不用这份缓存。
    held: bool,
    /// 图片资产加载 / 解码 / LRU 缓存（ADR 0019 S2a；后台单线程解码）。
    images: ImageService,
    /// 粘贴/拖放的资产导入实现（ADR 0019 S2b）：编辑器只认能力面，不依赖 storage。
    import_writer: SharedImportWriter,
    /// 上一次对账时正文里有没有 `asset://` 链接。
    ///
    /// 用来在**没有资产链接的普通笔记**上跳过对账（否则每次按键都要跑一次引用查询）；
    /// 删掉最后一条链接的那次编辑必须仍然对账，所以这里保留"上一次有过"的痕迹。
    had_asset_links: bool,
    /// 当前视图里解析不到的 wiki 目标（normalized；app 覆盖灰色虚线下划线）。
    unresolved_wiki_targets: HashSet<String>,
    /// 当前悬停的图片对象源码行（悬浮工具条的触发源）。
    hovered_object_line: Option<usize>,
    /// 单击选中的图片对象源码行（描边/工具条/F2 的目标；选区变化会清空）。
    selected_object_line: Option<usize>,
    /// 单击选中的表格单元格 `(表格对象行, row, column)`（ADR 0019 S3b2）。
    ///
    /// `row` 与控制器同一套寻址（0 = 表头）。选中单元格**不移动正文 caret**；再次单击
    /// 同一格 / 双击 / Enter / F2 进入就地编辑（编辑态在控制器里，见
    /// `EditorController::is_editing_table_cell`）。
    selected_table_cell: Option<(usize, usize, usize)>,
    /// 本帧悬浮工具条的**窗口坐标**矩形（元素层用它避免把点击误当成正文命中）。
    toolbar_rect: Option<Bounds<Pixels>>,
    /// 本帧表格右下角「加行/加列」浮层的**窗口坐标**矩形（与 `toolbar_rect` 同样
    /// 用于“点击别当正文命中”，也用于 headless 断言浮层不遮挡单元格）。
    table_actions_rect: Option<Bounds<Pixels>>,
    /// 表格行列拖拽（ADR 0019 S3b5）：进行中的一次拖拽；`None` = 没有拖拽。
    table_drag: Option<TableDragState>,
    /// 本帧表格拖拽把手的**命中矩形**（窗口坐标）：元素层据此放行把手上的点击，
    /// 避免同一次按下又被当成“选中单元格 / 整表选中 / 落 caret”。
    table_handle_rects: Vec<Bounds<Pixels>>,
    /// 正文右键菜单（ADR 0019 S3b4：「插入 → 表格」）；`None` = 没有菜单。
    context_menu: Option<EditorContextMenu>,
    /// 本帧右键菜单面板的**窗口坐标**矩形（点击落在这里时元素层不挪正文 caret）。
    context_menu_rect: Option<Bounds<Pixels>>,
    /// 放大预览的图片（点击图片/放大按钮后显示；`None` = 关闭）。
    zoomed_image: Option<Arc<gpui::RenderImage>>,
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
        import_writer: SharedImportWriter,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
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
            pending_tab_text_input: false,
            dump_selection: false,
            held: false,
            images: ImageService::new(asset_provider),
            import_writer,
            had_asset_links: false,
            unresolved_wiki_targets: HashSet::new(),
            hovered_object_line: None,
            selected_object_line: None,
            selected_table_cell: None,
            toolbar_rect: None,
            table_actions_rect: None,
            table_drag: None,
            table_handle_rects: Vec::new(),
            context_menu: None,
            context_menu_rect: None,
            zoomed_image: None,
            #[cfg(feature = "dev-sync")]
            local_edit_seq: 0,
        };
        view.spawn_pump(cx);
        // 打开笔记即做一次引用对账（ADR 0019 §3）：断线/崩溃/远端改动之后，
        // 引用账本按当前正文重建一次；集合没变时不会写库。
        view.reconcile_asset_refs_now();
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
        let mut pending_reconcile = false;
        for event in self.driver.pump() {
            match event {
                DriverEvent::Durable(ack) => {
                    self.controller.apply_durable_ack(&ack);
                    eprintln!(
                        "[menghunote] durable 水位 → live_rev={}（is_saved={}）",
                        ack.through_live_rev.0,
                        self.controller.is_saved()
                    );
                    // 正文已经落库：按当前正文整篇重建资产引用（ADR 0019 §3）。
                    // 集合没变时实现方短路，因此这里可以每次都调。
                    pending_reconcile = true;
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
        // 引用对账（正文 durable 之后）：只写"引用账本"，不改正文，因此不需要重绘。
        if pending_reconcile {
            self.reconcile_asset_refs_now();
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

    // ---- 表格 Object Block（ADR 0019 S3a）----

    /// 表格内容高度（不含上下内边距）：表头 + 数据行 × 固定行高。
    ///
    /// 用固定行高而不是逐行 shaping 定量：行高必须能在**布局度量**阶段（还没有
    /// shaping 结果时）给出，且长表格要能按行裁剪而不测量全部单元格。
    fn table_content_height(
        &self,
        table: &menghunote_editor::MarkdownTable,
        _content_width: f32,
    ) -> f32 {
        theme::TABLE_HEADER_HEIGHT + table.row_count() as f32 * theme::TABLE_ROW_HEIGHT
    }

    /// 表格对象的**行高**（内容高度 + 上下内边距）。布局度量与绘制共用这一个函数。
    fn table_row_height(
        &self,
        table: &menghunote_editor::MarkdownTable,
        content_width: f32,
    ) -> f32 {
        self.table_content_height(table, content_width) + theme::TABLE_PADDING * 2.0
    }

    /// 一个表格对象的绘制数据：只 shaping **与视口相交**的行（长表格按行裁剪）。
    ///
    /// `rect` 是表格外框（**元素局部坐标**，与 `viewport_height` 同一个坐标系：
    /// 元素顶部 = 0）。整表都在视口外时调用方根本不会走到这里（按对象行裁剪）。
    fn table_paint(
        &self,
        window: &Window,
        line: usize,
        table: &menghunote_editor::MarkdownTable,
        rect: Bounds<Pixels>,
        viewport_height: f32,
    ) -> TablePaint {
        // 源码只取一次：单元格 shaping 是逐格的，不能在每次调用里都 clone 全文。
        let source = self.controller.source_text();
        let columns = table.columns().max(1);
        let column_width = table_column_width(rect.size.width.as_f32(), columns);
        let header_height = theme::TABLE_HEADER_HEIGHT;
        let row_height = theme::TABLE_ROW_HEIGHT;
        // 元素局部坐标系里的可见纵向范围：`rect` 已经减过 `scroll`，不能再和
        // 内容坐标（scroll..scroll+h）比较，否则滚动到长表格中间会整表变空白。
        let viewport_top = 0.0;
        let viewport_bottom = viewport_height.max(0.0);
        let inner_width = (column_width - theme::TABLE_CELL_PAD_X * 2.0).max(0.0);
        // 全部几何先在元素局部 f32 里算，最后再包 `px()`（避免混用 Pixels 与 f32）。
        let origin_x = rect.origin.x.as_f32();
        let origin_y = rect.origin.y.as_f32();
        let total_width = column_width * columns as f32;

        // 选中的单元格（只影响绘制）：行/列必须来自**当前**表格，陈旧地址直接忽略。
        let selected_cell = self
            .selected_table_cell
            .filter(|(selected_line, row, column)| {
                *selected_line == line && *row < table.total_rows() && *column < columns
            })
            .map(|(_, row, column)| (row, column));
        let mut paint = TablePaint {
            rect,
            columns,
            row_count: table.row_count(),
            header: None,
            rows: Vec::new(),
            row_height,
            header_height,
            column_width,
            selected_cell,
        };
        // 表头行：只有与视口相交时才 shaping（滚过表头后不再测量）。
        if origin_y + header_height > viewport_top && origin_y < viewport_bottom {
            let mut cells = Vec::with_capacity(columns);
            for column in 0..columns {
                let left = origin_x + column_width * column as f32;
                let cell_rect = Bounds::new(
                    point(px(left), px(origin_y)),
                    size(px(column_width), px(header_height)),
                );
                cells.push(TableCellPaint {
                    rect: cell_rect,
                    text: table_cell_paint(
                        window,
                        &source,
                        table.header().cell(column),
                        inner_width,
                    ),
                    align: table.alignment(column),
                });
            }
            paint.header = Some(TableRowPaint {
                rect: Bounds::new(
                    point(px(origin_x), px(origin_y)),
                    size(px(total_width), px(header_height)),
                ),
                cells,
                header: true,
                row: 0,
            });
        }
        for (index, row) in table.rows().iter().enumerate() {
            let top = origin_y + header_height + row_height * index as f32;
            if top + row_height <= viewport_top || top >= viewport_bottom {
                continue;
            }
            let mut cells = Vec::with_capacity(columns);
            for column in 0..columns {
                let left = origin_x + column_width * column as f32;
                let cell_rect = Bounds::new(
                    point(px(left), px(top)),
                    size(px(column_width), px(row_height)),
                );
                cells.push(TableCellPaint {
                    rect: cell_rect,
                    text: table_cell_paint(window, &source, row.cell(column), inner_width),
                    align: table.alignment(column),
                });
            }
            paint.rows.push(TableRowPaint {
                rect: Bounds::new(
                    point(px(origin_x), px(top)),
                    size(px(total_width), px(row_height)),
                ),
                cells,
                header: false,
                // 行下标含表头：数据行 index → row = index + 1（与控制器同一套寻址）。
                row: index + 1,
            });
        }
        paint
    }

    /// 当前应画描边的对象行（显式点选 + 文本选区相交；不含源码模式）。
    fn highlighted_object_lines(&self) -> Vec<usize> {
        let mut lines = self.controller.object_lines_in_display_selection();
        if let Some(line) = self.selected_object_line {
            if self.controller.object_at_line(line).is_some() && !lines.contains(&line) {
                lines.push(line);
            }
        }
        lines
    }

    /// 悬浮工具条当前针对的对象行（显式源码模式时没有工具条）。
    fn toolbar_target_line(&self) -> Option<usize> {
        // 源码模式 / 单元格就地编辑：都不是 Object（工具条与边缘浮层一起隐藏）。
        if self.controller.is_editing_object_source() || self.controller.is_editing_table_cell() {
            return None;
        }
        if let Some(line) = self.hovered_object_line {
            if self.controller.object_at_line(line).is_some() {
                return Some(line);
            }
        }
        self.highlighted_object_lines().first().copied()
    }

    /// 表格右下角「加行/加列」浮层针对的对象行：只对**表格**出现，且必须已投影成
    /// Object（源码模式、非整块表格、图片都不显示）。悬浮或选中都算。
    fn table_actions_target_line(&self) -> Option<usize> {
        let line = self.toolbar_target_line()?;
        self.controller
            .object_at_line(line)
            .filter(|object| object.kind().is_table())
            .map(|_| line)
    }

    /// 本帧要绘制的 Object（图片 + 表格；只含可见行；元素层把它们转成窗口坐标）。
    ///
    /// `scroll` 是本帧实际使用的（已夹取的）滚动量；矩形在**元素局部坐标**里，
    /// 由调用方加上 `bounds.origin`。行高已经等于 `内容高 + 2 × 内边距`，
    /// 所以这里的矩形与 `LayoutFrame` 的行顶/行高几何完全一致。
    /// `viewport_height` 用于把长表格按行裁剪（只 shaping 与视口相交的行）。
    pub(crate) fn object_paints(
        &self,
        window: &Window,
        frame: &LayoutFrame,
        scroll: f32,
        content_width: f32,
        viewport_height: f32,
        visible: Range<usize>,
    ) -> Vec<ObjectPaint> {
        let projection = self.controller.projection();
        let highlighted = self.highlighted_object_lines();
        let mut objects = Vec::new();
        for object in self.controller.objects() {
            // 显式源码模式 / 当前投影没有对象段：这一帧不画。
            let Some(display_range) = projection.object_display_range(object.span()) else {
                continue;
            };
            let row = frame.row_of_byte(display_range.start);
            if row < visible.start || row >= visible.end {
                continue;
            }
            let left = theme::PAD_X + frame.row_indent(row);
            let row_top = theme::PAD_Y - scroll + frame.row_top_offset(row);
            let selected = highlighted.contains(&object.line());
            let (rect, content) = match object.kind() {
                menghunote_editor::ObjectKind::Image => {
                    let Some(image) = self.controller.image_of(object) else {
                        continue;
                    };
                    let content_height = self.object_content_height(image, content_width);
                    let state = parse_asset_hash(image.dest())
                        .map_or(ImageState::Missing, |hash| self.images.state(hash));
                    let display_width = match &state {
                        ImageState::Ready(decoded) => super::object_image::fit_size(
                            decoded.width,
                            decoded.height,
                            content_width,
                        )
                        .map_or(content_width, |(width, _)| width),
                        ImageState::Loading | ImageState::Missing | ImageState::Failed(_) => {
                            content_width.min(theme::OBJECT_PLACEHOLDER_HEIGHT * 8.0)
                        }
                    };
                    let rect = Bounds::new(
                        point(px(left), px(row_top + theme::OBJECT_PADDING)),
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
                    (
                        rect,
                        ObjectPaintContent::Image(Box::new(super::geometry::ImagePaint {
                            image: paint_image,
                            label,
                            state: paint_state,
                        })),
                    )
                }
                menghunote_editor::ObjectKind::Table => {
                    let Some(table) = self.controller.table_of(object) else {
                        continue;
                    };
                    let content_height = self.table_content_height(table, content_width);
                    let rect = Bounds::new(
                        point(px(left), px(row_top + theme::TABLE_PADDING)),
                        size(px(content_width), px(content_height)),
                    );
                    let paint =
                        self.table_paint(window, object.line(), table, rect, viewport_height);
                    (rect, ObjectPaintContent::Table(Box::new(paint)))
                }
            };
            objects.push(ObjectPaint {
                kind: object.kind(),
                index: object.index(),
                line: object.line(),
                end_line: object.end_line(),
                row,
                rect,
                content,
                selected,
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

    /// 当前可见 Object（图片/表格）的**窗口坐标**矩形（`(对象行, 矩形)`，诊断/headless 测试用）。
    ///
    /// 几何来自上一帧 `LayoutFrame`（与绘制/命中同源）；对象滚出可见范围时不在列表里。
    /// 表格矩形的宽度是内容宽度、高度是整个表格高度（点击/断言都共用它）。
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

    /// 当前可见表格对象的绘制行数（诊断/headless：验证长表格按行裁剪）。
    ///
    /// 返回 `(对象行, 是否绘制了表头, 实际 shaping 的数据行数)`。数据行数小于表格
    /// 总行数说明“只画与视口相交的行”；滚出视口后整个对象不在列表里。
    #[must_use]
    pub fn table_paint_rows(&self) -> Vec<(usize, bool, usize)> {
        self.shared.geometry().map_or_else(Vec::new, |geometry| {
            geometry
                .objects()
                .iter()
                .filter_map(|object| {
                    object
                        .table()
                        .map(|table| (object.line, table.header.is_some(), table.rows.len()))
                })
                .collect()
        })
    }

    /// 当前**隐藏态**任务框的视图层描述（ADR 0019 S3；绘制/命中都从这里取）。
    ///
    /// 只返回“投影里确实是 Replace 占位”的项：
    ///
    /// - caret/选区进入 marker 或任务框 → `list_marker_revealed` 为真，源码显现，
    ///   此时返回空（不画复选框，交给文本编辑）；
    /// - 解析器没有给出精确 `task_marker` 区间 → 跳过（宁可不画，也不会点错字节）。
    #[must_use]
    pub fn task_checkboxes(&self) -> Vec<TaskCheckbox> {
        let projection = self.controller.projection();
        let source_len = projection.source_len();
        let mut boxes = Vec::new();
        for (index, item) in self.controller.list_items().iter().enumerate() {
            let Some(span) = item.task_marker else {
                continue;
            };
            if self.controller.list_marker_revealed(index) {
                continue;
            }
            let Ok(source) = SourceRange::new(
                SourceByte::new(span.start),
                SourceByte::new(span.end),
                source_len,
            ) else {
                continue;
            };
            let Some(display) = projection.replace_display_range(source) else {
                continue;
            };
            boxes.push(TaskCheckbox {
                item: index,
                checked: item.task.unwrap_or(false),
                source: (span.start, span.end),
                display: (display.start, display.end),
            });
        }
        boxes
    }

    /// 当前可见任务复选框的**窗口坐标**矩形 `(item, checked, rect)`。
    ///
    /// 几何与绘制/命中同源（上一帧 `EditorGeometry`）；滚出可见范围时不在列表里。
    /// headless 测试用它定位点击，不按字符宽度估算坐标。
    #[must_use]
    pub fn task_checkbox_rects(&self) -> Vec<(usize, bool, Bounds<Pixels>)> {
        self.shared.geometry().map_or_else(Vec::new, |geometry| {
            geometry
                .task_boxes()
                .iter()
                .map(|task| (task.item, task.checked, task.rect))
                .collect()
        })
    }

    /// 点击任务复选框：**只切换源码** `[ ] ↔ [x]`，不移动 caret、不改选区。
    ///
    /// - 一次点击一个 `EditRequest`（控制器 [`EditorController::toggle_task_marker`]），
    ///   进会话撤销组；
    /// - `source` 是命中帧记录的源码区间：与当前解析结果不一致（陈旧帧）就忽略，
    ///   绝不拿旧偏移去改新文本；
    /// - 持闸 / IME 组合中直接忽略（不产生请求）；
    /// - 返回 `true` 表示请求已交给驱动（调用方据此重绘）。
    pub fn toggle_task_checkbox(
        &mut self,
        item: usize,
        source: (usize, usize),
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.pending_enter_text_input = false;
        if self.frozen_by_external_commit() {
            eprintln!("[menghunote] 同步持闸中：忽略任务框点击 item={item}");
            return false;
        }
        if self.is_composing() {
            eprintln!("[menghunote] IME 组合中：忽略任务框点击 item={item}");
            return false;
        }
        if self.controller.is_editing_table_cell() {
            eprintln!("[menghunote] 单元格编辑中：先提交/取消再点任务框 item={item}");
            return false;
        }
        // 防陈旧帧：命中用的矩形来自上一帧，远端变更可能已经改过 item/区间。
        // 只认“当前解析结果里这个 item 的任务框仍是点击时那个 span”，不猜新的偏移。
        let Some(current) = self
            .controller
            .list_items()
            .get(item)
            .and_then(|it| it.task_marker)
        else {
            return false;
        };
        if (current.start, current.end) != source {
            eprintln!(
                "[menghunote] 任务框点击已过期（item={item} span={source:?}，当前=({}, {})），忽略",
                current.start, current.end
            );
            return false;
        }
        let before = self
            .controller
            .list_items()
            .get(item)
            .and_then(|it| it.task);
        match self.controller.toggle_task_marker(item) {
            Ok(Some(request)) => {
                let command_id = request.command_id.clone();
                if let Some(window) = window {
                    window.focus(&self.focus_handle, cx);
                }
                let accepted = self.submit_request(Some(request)).is_some();
                if accepted {
                    // 点任务框与点正文一样结束图片对象的“点选态”，但不改文本 caret。
                    self.selected_object_line = None;
                    let after = self
                        .controller
                        .list_items()
                        .get(item)
                        .and_then(|it| it.task);
                    eprintln!(
                        "[menghunote] 任务框点击 item={item} {before:?} → {after:?} command_id={command_id}（只改 [ ]/[x]，caret 不变）"
                    );
                    cx.notify();
                }
                accepted
            }
            Ok(None) => false,
            Err(error) => {
                eprintln!("[menghunote] 任务框切换被拒：{error:?}");
                false
            }
        }
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

    /// 表格对象的只读快照（headless 测试断言状态/几何用）。
    #[must_use]
    pub fn table_snapshots(&self) -> Vec<TableSnapshot> {
        let projection = self.controller.projection();
        let highlighted = self.highlighted_object_lines();
        self.controller
            .tables()
            .iter()
            .filter(|table| table.is_block())
            .map(|table| {
                let display = projection.object_display_range(table.span());
                let source_visible = display.is_none();
                TableSnapshot {
                    line: table.line(),
                    end_line: table.end_line(),
                    span: (table.span().start().get(), table.span().end().get()),
                    columns: table.columns(),
                    rows: table.row_count(),
                    display: display.map(|range| (range.start, range.end)),
                    selected: highlighted.contains(&table.line()),
                    editing_source: self.controller.object_source_edit_line() == Some(table.line()),
                    selected_cell: self
                        .selected_table_cell
                        .filter(|(line, row, column)| {
                            *line == table.line()
                                && *row < table.total_rows()
                                && *column < table.columns()
                        })
                        .map(|(_, row, column)| (row, column)),
                    editing_cell: self
                        .controller
                        .table_cell_edit_target()
                        .filter(|target| target.line == table.line())
                        .map(|target| (target.row, target.column)),
                    source_visible,
                }
            })
            .collect()
    }

    /// 可见视觉行的度量快照。
    ///
    /// headless 用来验证“Object 把多行源码压成一个 display 行之后，后面各行仍取到
    /// **自己的**源码行度量”（表格之后的标题字号/列表缩进不能整体错位）。
    #[must_use]
    pub fn visual_row_metrics(&self) -> Vec<VisualRowMetric> {
        self.shared.geometry().map_or_else(Vec::new, |geometry| {
            geometry
                .visible_rows()
                .map(|row| {
                    let line = &geometry.frame.lines[row];
                    VisualRowMetric {
                        row,
                        bytes: (line.bytes.start, line.bytes.end),
                        font_size: line.font_size,
                        line_height: line.line_height,
                        indent: line.indent,
                    }
                })
                .collect()
        })
    }

    /// display 字节 → 源码行号（0 起）。Object 跨多行时 display 行号 ≠ 源码行号。
    ///
    /// 只在**单次点击**路径用（O(n)）；布局度量按 display 行批量换算，另走前缀数组。
    fn source_line_of_display_byte(&self, display: usize) -> Option<usize> {
        let source = self.controller.source_text();
        let byte = self
            .controller
            .projection()
            .source_byte_at_display(DisplayByte::new(display))?
            .get()
            .min(source.len());
        source
            .as_bytes()
            .get(..byte)
            .map(|prefix| prefix.iter().filter(|byte| **byte == b'\n').count())
    }

    /// 是否点在**浮层控件**上（悬浮工具条 / 表格边缘浮层 / 正文右键菜单）：
    /// 元素层的鼠标事件在这些控件上不应改 caret，也不应挪 hover 目标。
    pub(crate) fn point_in_toolbar(&self, position: Point<Pixels>) -> bool {
        self.toolbar_rect
            .is_some_and(|rect| rect.contains(&position))
            || self
                .table_actions_rect
                .is_some_and(|rect| rect.contains(&position))
            || self
                .context_menu_rect
                .is_some_and(|rect| rect.contains(&position))
            // 表格行列拖拽把手（ADR 0019 S3b5）：按下把手不得被当成正文命中。
            || self
                .table_handle_rects
                .iter()
                .any(|rect| rect.contains(&position))
    }

    /// 正文右键菜单是否打开（元素层据此在点击别处时先关菜单）。
    pub(crate) fn context_menu_open(&self) -> bool {
        self.context_menu.is_some()
    }

    /// 是否点在正文右键菜单**面板**上（菜单项自己处理点击，元素层放行）。
    pub(crate) fn context_menu_contains(&self, position: Point<Pixels>) -> bool {
        self.context_menu_rect
            .is_some_and(|rect| rect.contains(&position))
    }

    /// 鼠标移动：维护“当前悬停的图片对象行”，变化时重绘（工具条随之出现/消失）。
    pub(crate) fn hover_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        // 拖拽期间不跟着指针改悬停目标：把手/工具条必须稳定停在被拖的那张表上。
        if self.table_drag.is_some() {
            return;
        }
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

    /// 单击 Object（图片/表格）：**只选中块**（描边），不展开源码，也不产生对象内部 caret。
    ///
    /// 表格与图片一样是原子块：caret 统一落在整个块的末尾，点击单元格不会把 caret 放进
    /// 格子里。再次单击已选中的**图片**才打开放大预览（表格没有放大）。
    ///
    /// 表格的**单元格**点击走 [`Self::click_table_cell`]（只选中该格，连 caret 都不动）；
    /// 这里只处理图片与表格的非单元格区域（内边距/边框）。
    pub fn select_object_at(
        &mut self,
        line: usize,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let Some(object) = self.controller.object_at_line(line).cloned() else {
            return;
        };
        let already_selected = self.selected_object_line == Some(line);
        let text = self.controller.source_text();
        let Ok(caret) = SourceByte::new(object.span().end().get()).to_scalar(&text) else {
            return;
        };
        if let Some(window) = window {
            window.focus(&self.focus_handle, cx);
        }
        // caret 落在对象之后（对象内部不产生文本 caret）。
        self.commit_selection(Selection::caret(caret), cx);
        self.selected_object_line = Some(line);
        self.selected_table_cell = None;
        self.preferred_x = None;
        if already_selected && object.kind().is_image() {
            self.open_object_zoom(line, cx);
        }
        cx.notify();
    }

    // ---- 表格单元格选择 / 就地编辑（ADR 0019 S3b2）----

    /// 是否正在就地编辑表格单元格（编辑态由控制器持有）。
    #[must_use]
    pub fn is_editing_table_cell(&self) -> bool {
        self.controller.is_editing_table_cell()
    }

    /// 正在编辑的单元格 `(表格对象行, row, column)`（row 0 = 表头）。
    #[must_use]
    pub fn table_cell_edit_target(&self) -> Option<(usize, usize, usize)> {
        self.controller
            .table_cell_edit_target()
            .map(|target| (target.line, target.row, target.column))
    }

    /// 正在编辑的 cell 在 display 上的落点字节（空 cell 也有；供元素层判断“点在不在这一行”）。
    #[must_use]
    pub fn table_cell_edit_display_anchor(&self) -> Option<usize> {
        self.controller.table_cell_edit_display_anchor()
    }

    /// 单击选中的单元格 `(表格对象行, row, column)`（不进编辑；不移动正文 caret）。
    #[must_use]
    pub fn selected_table_cell(&self) -> Option<(usize, usize, usize)> {
        self.selected_table_cell
    }

    /// 本帧所有可见表格单元格的窗口矩形 `(对象行, row, column, 矩形)`。
    ///
    /// 几何来自 `table_paint` 的 `TableCellPaint.rect`（绘制/命中/测试同源），
    /// 长表格按视口裁剪时只有可见行；不在 Object 模式（源码模式 / 单元格编辑中）
    /// 时表格没有网格，这里也就没有矩形。
    #[must_use]
    pub fn table_cell_rects(&self) -> Vec<(usize, usize, usize, Bounds<Pixels>)> {
        let Some(geometry) = self.shared.geometry() else {
            return Vec::new();
        };
        let mut rects = Vec::new();
        for object in geometry.objects() {
            let Some(table) = object.table() else {
                continue;
            };
            for row in table.header.iter().chain(table.rows.iter()) {
                for (column, cell) in row.cells.iter().enumerate() {
                    rects.push((object.line, row.row, column, cell.rect));
                }
            }
        }
        rects
    }

    /// 单击表格单元格：**只选中这一格**（高亮），表格仍是 Object，**不移动正文 caret**。
    ///
    /// 再次单击同一格 / 双击 / `Enter` / `F2` 才进入就地编辑（[`Self::begin_table_cell_edit`]）。
    /// 点击表格的非单元格区域（内边距/边框）仍然走 S3a 的整块选中（[`Self::select_object_at`]）。
    pub fn click_table_cell(
        &mut self,
        line: usize,
        row: usize,
        column: usize,
        click_count: usize,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.pending_enter_text_input = false;
        // 陈旧帧防护：行列必须仍在**最新解析**的表格范围内，否则只当选块处理。
        let valid = self
            .controller
            .object_at_line(line)
            .filter(|object| object.kind().is_table())
            .and_then(|object| self.controller.table_of(object))
            .is_some_and(|table| {
                row < table.total_rows()
                    && column < table.columns()
                    && table.cell(row, column).is_some()
            });
        if !valid {
            eprintln!("[menghunote] 单元格点击已过期（行 {line} 的 {row},{column}）：忽略");
            return false;
        }
        if click_count >= 2 {
            // 双击 = 选中 + 进入编辑（一次手势里完成，不需要先选一次）。
            self.selected_object_line = Some(line);
            self.selected_table_cell = Some((line, row, column));
            return self.begin_table_cell_edit(line, row, column, window, cx);
        }
        let already = self.selected_table_cell == Some((line, row, column));
        if already {
            // 再次单击已选中的格子 → 进入编辑。
            return self.begin_table_cell_edit(line, row, column, window, cx);
        }
        // 第一次单击：选中该格，不碰正文 caret（caret 可能正在别处打字）。
        self.selected_object_line = Some(line);
        self.selected_table_cell = Some((line, row, column));
        self.preferred_x = None;
        if let Some(window) = window {
            window.focus(&self.focus_handle, cx);
        }
        eprintln!(
            "[menghunote] 选中表格单元格（行 {line} 的 row={row} col={column}）：不进入编辑、不移动 caret"
        );
        cx.notify();
        true
    }

    /// 进入单元格就地编辑（`Enter` / `F2` / 再次单击 / 双击）。
    ///
    /// 只有控制器能改正文：这里只调用 [`EditorController::begin_table_cell_edit`]，随后
    /// 把 caret 拉进视口。进入编辑本身**不产生** `EditRequest`。
    pub fn begin_table_cell_edit(
        &mut self,
        line: usize,
        row: usize,
        column: usize,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.pending_enter_text_input = false;
        if self.frozen_by_external_commit() {
            eprintln!("[menghunote] 同步持闸中：不进入单元格编辑（行 {line}）");
            return false;
        }
        if self.is_composing() {
            eprintln!("[menghunote] IME 组合中：先上屏再进入单元格编辑");
            return false;
        }
        if self.controller.is_editing_object_source() {
            return false;
        }
        match self.controller.begin_table_cell_edit(line, row, column) {
            Ok(true) => {
                self.selected_object_line = Some(line);
                self.selected_table_cell = Some((line, row, column));
                self.preferred_x = None;
                if let Some(window) = window {
                    window.focus(&self.focus_handle, cx);
                }
                self.ensure_caret_visible(self.controller.selection().head());
                eprintln!(
                    "[menghunote] 进入单元格编辑（行 {line} 的 row={row} col={column}）：display 只显示该 cell"
                );
                cx.notify();
                true
            }
            Ok(false) => false,
            Err(error) => {
                eprintln!("[menghunote] 进入单元格编辑被拒：{error}");
                false
            }
        }
    }

    /// 提交当前单元格编辑：控制器给出**唯一**一条 `EditRequest`（没有改动则为 `None`）。
    pub fn commit_table_cell_edit(
        &mut self,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.controller.is_editing_table_cell() {
            return false;
        }
        // 组合中按 Enter/Tab：先取消组合（preedit 的基准在提交后失效），再提交这一格。
        if self.is_composing() {
            self.cancel_composition(cx);
        }
        if self.frozen_by_external_commit() {
            // 持闸时请求会被丢弃，而草稿已经写在镜像里 → 放弃草稿，绝不留分叉镜像。
            self.controller.cancel_table_cell_edit();
            eprintln!("[menghunote] 同步持闸中：放弃未提交的单元格编辑");
            cx.notify();
            return false;
        }
        let target = self.table_cell_edit_target();
        match self.controller.commit_table_cell_edit() {
            Ok(request) => {
                let edits = request.as_ref().map(|request| request.edits.len());
                let command_id = request
                    .as_ref()
                    .map(|request| request.command_id.clone())
                    .unwrap_or_default();
                let accepted = self.submit_request(request).is_some();
                eprintln!(
                    "[menghunote] 提交单元格编辑（{:?}）edits={edits:?} command_id={command_id} accepted={accepted}",
                    target
                );
                if let Some(window) = window {
                    window.focus(&self.focus_handle, cx);
                }
                self.ensure_caret_visible(self.controller.selection().head());
                cx.notify();
                true
            }
            Err(error) => {
                eprintln!("[menghunote] 提交单元格编辑失败：{error}");
                cx.notify();
                false
            }
        }
    }

    /// `Esc`：取消当前单元格编辑（恢复进入编辑前的 cell 文本，**不产生请求**）。
    pub fn cancel_table_cell_edit(&mut self, cx: &mut Context<Self>) -> bool {
        if self.is_composing() {
            self.cancel_composition(cx);
        }
        if self.controller.cancel_table_cell_edit() {
            eprintln!("[menghunote] 取消单元格编辑：cell 文本已恢复");
            self.preferred_x = None;
            self.ensure_caret_visible(self.controller.selection().head());
            cx.notify();
            true
        } else {
            false
        }
    }

    /// `Tab` / `Shift+Tab`：提交当前格并进入下一格（到表格末尾就退出编辑）。
    ///
    /// 一次提交仍然只有一条 `EditRequest`；导航按最新解析的行列数算，因此加行/加列
    /// 之后的新格子也在同一条路径上。
    pub fn advance_table_cell_edit(
        &mut self,
        forward: bool,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.controller.is_editing_table_cell() {
            return false;
        }
        if self.is_composing() {
            self.cancel_composition(cx);
        }
        if self.frozen_by_external_commit() {
            self.controller.cancel_table_cell_edit();
            eprintln!("[menghunote] 同步持闸中：放弃未提交的单元格编辑");
            cx.notify();
            return false;
        }
        match self.controller.advance_table_cell_edit(forward) {
            Ok((request, still_editing)) => {
                let edits = request.as_ref().map(|request| request.edits.len());
                let accepted = self.submit_request(request).is_some();
                let target = self.table_cell_edit_target();
                if still_editing {
                    if let Some(target) = target {
                        self.selected_object_line = Some(target.0);
                        self.selected_table_cell = Some(target);
                    }
                } else {
                    self.selected_table_cell = None;
                }
                if let Some(window) = window {
                    window.focus(&self.focus_handle, cx);
                }
                self.ensure_caret_visible(self.controller.selection().head());
                eprintln!(
                    "[menghunote] Tab 提交单元格 edits={edits:?} accepted={accepted} → 目标={target:?} 仍在编辑={still_editing}"
                );
                cx.notify();
                true
            }
            Err(error) => {
                eprintln!("[menghunote] 单元格 Tab 失败：{error}");
                cx.notify();
                false
            }
        }
    }

    /// 点击 cell 外：先提交（一次 `EditRequest`），再把这次点击落成 caret。
    ///
    /// 提交会让表格重新投影成**一个占位符**（display 由“cell 文本”变成 3 字节占位符），
    /// 所以点击的 display 字节要先按这个差值换算到新的 display，再换回 source：
    /// 否则“点下面那一段文字”会落到错行。
    pub(crate) fn commit_table_cell_edit_and_place_caret(
        &mut self,
        display_byte: usize,
        bias: Bias,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editing_range = self.controller.table_cell_edit_display_range();
        let anchor = self.controller.table_cell_edit_display_anchor();
        self.commit_table_cell_edit(Some(window), cx);
        self.selected_table_cell = None;
        let placeholder = menghunote_editor::OBJECT_PLACEHOLDER_LEN;
        let target = match (editing_range, anchor) {
            (Some(range), _) => {
                if display_byte <= range.start {
                    display_byte
                } else if display_byte >= range.end {
                    range
                        .start
                        .saturating_add(placeholder)
                        .saturating_add(display_byte - range.end)
                } else {
                    range.start
                }
            }
            // 空 cell：显示宽度为 0，点击落到“那一行的落点” → caret 停在表格起点。
            (None, Some(anchor)) => {
                if display_byte <= anchor {
                    display_byte
                } else {
                    anchor.saturating_add(placeholder)
                }
            }
            (None, None) => display_byte,
        };
        let target = target.min(self.controller.display_len().get());
        self.place_caret_from_display(target, bias, window, cx);
        cx.notify();
    }

    /// 表格「加行」（ADR 0019 S3b）：在最后一条数据行之后插入一条空数据行。
    ///
    /// - 只走 [`EditorController::append_table_row`]（一次点击一个 `EditRequest`，
    ///   由会话默认撤销组进 undo）；app 不自己拼 `ScalarEdit`、不改 body；
    /// - **不移动 caret/选区**：点击浮层按钮不落正文 caret（控制器按文本位置重映射：
    ///   表格末尾 → 新表格末尾，caret 绝不落进表格内部）；表格选中态保持不变；
    /// - 表格仍是 Object 模式：不进入源码模式，下一帧按新的行数重算几何；
    /// - 持闸 / IME 组合中直接忽略；行号上已不是整块表格（陈旧帧）也忽略。
    pub fn insert_table_row(
        &mut self,
        line: usize,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.insert_table_structure(TableStructureAction::Row, line, window, cx)
    }

    /// 表格「加列」（ADR 0019 S3b）：给表头 / 分隔行 / 每条数据行末尾追加一列。
    ///
    /// 契约与 [`Self::insert_table_row`] 相同（一次点击一个 `EditRequest`、caret 不变、
    /// 仍是 Object）；没有数据行的表格也能加列。
    pub fn insert_table_column(
        &mut self,
        line: usize,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.insert_table_structure(TableStructureAction::Column, line, window, cx)
    }

    /// 表格结构动作的统一入口（加行 / 加列）。
    fn insert_table_structure(
        &mut self,
        action: TableStructureAction,
        line: usize,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.pending_enter_text_input = false;
        if self.frozen_by_external_commit() {
            eprintln!(
                "[menghunote] 同步持闸中：忽略表格{}（行 {line}）",
                action.label()
            );
            return false;
        }
        if self.is_composing() {
            eprintln!(
                "[menghunote] IME 组合中：忽略表格{}（行 {line}）",
                action.label()
            );
            return false;
        }
        if self.controller.is_editing_object_source() {
            // 源码模式里表格不是 Object（工具条/浮层本来也不显示）：不做结构动作。
            return false;
        }
        if self.controller.is_editing_table_cell() {
            // 单元格编辑期间镜像领先于会话，结构插入的坐标只在提交后才对齐。
            eprintln!(
                "[menghunote] 单元格编辑中：忽略表格{}（行 {line}）",
                action.label()
            );
            return false;
        }
        let result = match action {
            TableStructureAction::Row => self.controller.append_table_row(line),
            TableStructureAction::Column => self.controller.append_table_column(line),
        };
        match result {
            Ok(Some(request)) => {
                let command_id = request.command_id.clone();
                let edits = request.edits.len();
                if let Some(window) = window {
                    window.focus(&self.focus_handle, cx);
                }
                let accepted = self.submit_request(Some(request)).is_some();
                if accepted {
                    // 选中态保持（不进入源码模式、不丢选中）：表格仍是工具条/浮层的目标。
                    eprintln!(
                        "[menghunote] 表格{}（行 {line}）command_id={command_id} edits={edits}（caret/选区不变，仍为 Object）",
                        action.label()
                    );
                    cx.notify();
                }
                accepted
            }
            Ok(None) => false,
            Err(error) => {
                eprintln!("[menghunote] 表格{}被拒：{error:?}", action.label());
                false
            }
        }
    }

    /// 表格「删除行」（ADR 0019 S3b3）：删除当前选中的**数据行**（首版不删表头）。
    ///
    /// - 只走 [`EditorController::delete_table_row`]（一次点击一个 `EditRequest`，
    ///   由会话默认撤销组进 undo）；app 不自己拼 `ScalarEdit`、不改 body；
    /// - 删除后把单元格选中态修正到**上一行同列**（第一条数据行则落到下一行同列，
    ///   没有数据行可落就清空），不留一个看不见的选中目标；
    /// - caret 由控制器按删除区间映射并夹到表格之外（绝不落进表格源码内部）；
    /// - 表格仍是 Object；持闸 / IME 组合 / 单元格编辑中直接忽略；陈旧行号由控制器
    ///   兜底成 no-op。
    pub fn delete_table_row(
        &mut self,
        line: usize,
        row: usize,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        // 删除后的“最近可用格”需要**原选中列**：删了「数量」列上的行，就落回上一行的
        // 「数量」列（不能一律按第 0 列猜）。没有匹配的选中格时退回 0（UI 不会这样调）。
        let column = self
            .selected_table_cell
            .filter(|(selected_line, selected_row, _)| {
                *selected_line == line && *selected_row == row
            })
            .map_or(0, |(_, _, column)| column);
        self.delete_table_structure(TableDeleteAxis::Row, line, row, column, window, cx)
    }

    /// 表格「删除列」（ADR 0019 S3b3）：从表头 / 分隔行 / 所有数据行删掉该列。
    ///
    /// 契约与 [`Self::delete_table_row`] 相同（一次点击一个 `EditRequest`、仍为 Object、
    /// caret 不落进表格）；删除后选中同行的**右侧格**（右邻左移补位），删的是最后一列时
    /// 选左侧格。**至少保留 1 列**：只剩一列时浮层按钮是禁用态，控制器也再兜一层，
    /// 不把表格改成非法结构。
    pub fn delete_table_column(
        &mut self,
        line: usize,
        column: usize,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        // 删列不改变行数：修正选中态时保留原行号（表头行就留在表头）。没有匹配的
        // 选中格时退回第 1 行（有数据行时的第一行）；控制器仍会兜底越界。
        let row = self
            .selected_table_cell
            .filter(|(selected_line, _, selected_column)| {
                *selected_line == line && *selected_column == column
            })
            .map_or(1, |(_, row, _)| row);
        self.delete_table_structure(TableDeleteAxis::Column, line, row, column, window, cx)
    }

    /// 表格删除动作的统一入口（删行 / 删列）。
    fn delete_table_structure(
        &mut self,
        axis: TableDeleteAxis,
        line: usize,
        row: usize,
        column: usize,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.pending_enter_text_input = false;
        if self.frozen_by_external_commit() {
            eprintln!(
                "[menghunote] 同步持闸中：忽略表格{}（行 {line}）",
                axis.label()
            );
            return false;
        }
        if self.is_composing() {
            eprintln!(
                "[menghunote] IME 组合中：忽略表格{}（行 {line}）",
                axis.label()
            );
            return false;
        }
        if self.controller.is_editing_object_source() {
            // 源码模式里表格不是 Object（浮层本来也不显示）：不做结构动作。
            return false;
        }
        if self.controller.is_editing_table_cell() {
            // 单元格编辑期间镜像领先于会话，结构删除的坐标只在提交后才对齐。
            eprintln!(
                "[menghunote] 单元格编辑中：忽略表格{}（行 {line}）",
                axis.label()
            );
            return false;
        }
        let result = match axis {
            TableDeleteAxis::Row => self.controller.delete_table_row(line, row),
            TableDeleteAxis::Column => self.controller.delete_table_column(line, column),
        };
        match result {
            Ok(Some(request)) => {
                let command_id = request.command_id.clone();
                let edits = request.edits.len();
                if let Some(window) = window {
                    window.focus(&self.focus_handle, cx);
                }
                // 删除是否被受理不影响“表格已经变短”这个事实（控制器已乐观改镜像），
                // 选中态一律按最新解析修正，避免留一个不存在的格子。
                let accepted = self.submit_request(Some(request)).is_some();
                self.correct_table_cell_after_delete(axis, line, row, column);
                self.ensure_caret_visible(self.controller.selection().head());
                eprintln!(
                    "[menghunote] 表格{}（行 {line}）command_id={command_id} edits={edits} accepted={accepted} → 选中格={:?}（仍为 Object）",
                    axis.label(),
                    self.selected_table_cell
                );
                cx.notify();
                accepted
            }
            Ok(None) => false,
            Err(error) => {
                eprintln!("[menghunote] 表格{}被拒：{error:?}", axis.label());
                false
            }
        }
    }

    /// 删除行列之后修正单元格选中态：按**最新解析**的表格行列数选最近的可用格。
    ///
    /// 规则来自 `table_cell_edit` 的纯函数（与控制器/测试同一套口径，不在 app 里另写）：
    /// - 删行：上一行同列优先，第一条数据行落到下一行同列；没有数据行 → 清空；
    /// - 删列：同行右侧格优先（右邻左移补位），删末列选左侧格；
    /// - 表格/单元格已不存在 → 清空（不猜坐标）。
    fn correct_table_cell_after_delete(
        &mut self,
        axis: TableDeleteAxis,
        line: usize,
        row: usize,
        column: usize,
    ) {
        self.preferred_x = None;
        let dims = self
            .controller
            .object_at_line(line)
            .filter(|object| object.kind().is_table())
            .and_then(|object| self.controller.table_of(object))
            .map(|table| (table.row_count(), table.columns()));
        let Some((rows, columns)) = dims else {
            self.selected_table_cell = None;
            return;
        };
        let next = match axis {
            TableDeleteAxis::Row => {
                menghunote_editor::cell_after_row_deletion(row, column, rows, columns)
            }
            TableDeleteAxis::Column => {
                menghunote_editor::cell_after_column_deletion(row, column, rows, columns)
            }
        };
        match next {
            Some(cell) => {
                self.selected_table_cell = Some((line, cell.row, cell.column));
                self.selected_object_line = Some(line);
            }
            None => self.selected_table_cell = None,
        }
    }

    // ---- 表格行列拖拽重排（ADR 0019 S3b5）----
    //
    // ## 交互模型（写死，headless 钉住）
    //
    // - 表格**悬浮或选中**时，在边框上浮出小手柄：表头顶部每列一个横向胶囊
    //   （`table-drag-column-{n}`），每条**数据行**左侧一个纵向胶囊（`table-drag-row-{n}`）；
    //   表头固定、不参与行重排，因此没有表头行把手；
    // - 把手压在 1px 边框上（一半在表格外、一半在 padding 里），**不遮挡单元格文字**；
    //   命中带比绘制范围略大（更容易点中），但仍只覆盖边框一圈；
    // - 按下列手 → 拖拽状态；水平/垂直拖到目标列/行的**缝隙** → 画 2px 插入指示线；
    //   释放 = 一次 `EditRequest`（一次 undo），`Esc` / 拖出表格（超出
    //   `TABLE_DRAG_CANCEL_MARGIN`）→ 取消，**不产生任何请求**；
    // - 拖拽期间元素层直接放行文本命中：正文 caret/选区**一动不动**，也不会误触
    //   单元格选中或整表选中（把手是浮层，元素级鼠标按下先于窗口级监听执行）。
    //
    // ## 为什么把把手做成浮层 div 而不是画在元素里
    //
    // 绘制、命中、悬停指针、headless 断言（`cx.debug_bounds`）都走同一份 gpui 元素：
    // 不需要自建 hitbox 清单，也不需要自己判“鼠标进出把手”来切光标（gpui 的
    // `cursor`/`hover` 是一等公民）。真正的手势（移动/释放）仍由元素层的窗口级监听
    // 推进 —— 它天然拿得到窗口坐标，且每帧重注册、跟着最新几何走。

    /// 当前表格拖拽状态（headless/诊断用；`None` = 没有拖拽）。
    #[must_use]
    pub fn table_drag(&self) -> Option<TableDragState> {
        self.table_drag
    }

    /// 是否正在拖拽表格行列（元素层据此放行文本命中）。
    #[must_use]
    pub fn table_drag_active(&self) -> bool {
        self.table_drag.is_some()
    }

    /// 本帧可见的表格拖拽把手（窗口坐标；绘制/命中/headless 断言**同源**）。
    ///
    /// 只返回**当前应当显示**的把手：
    /// - 表格悬浮/选中/正在拖动时才出现（`table_drag_target_line`）；
    /// - 列把手要求表头在本帧可见（把手锚在表头顶边），且列数 ≥ 2；
    /// - 行把手只给**本帧画出来的数据行**（长表格按视口裁剪；看不见的行点不到），
    ///   且数据行数 ≥ 2，**不含表头**。
    #[must_use]
    pub fn table_drag_handles(&self) -> Vec<TableDragHandlePaint> {
        let Some(geometry) = self.shared.geometry() else {
            return Vec::new();
        };
        let Some(line) = self.table_drag_target_line() else {
            return Vec::new();
        };
        let Some(object) = geometry.objects().iter().find(|object| object.line == line) else {
            return Vec::new();
        };
        let Some(table) = object.table() else {
            return Vec::new();
        };
        // 只保留画在**编辑器可见区**内的把手：表格顶边滚出视口时列把手会跑到视口外，
        // 浮层 div 不能被画到编辑区之外（会压到宿主其它 UI 上）。
        let viewport_top = geometry.origin.y.as_f32();
        let viewport_bottom = viewport_top + geometry.viewport_height;
        let on_screen = |rect: &Bounds<Pixels>| {
            let top = rect.origin.y.as_f32();
            top >= viewport_top && top + rect.size.height.as_f32() <= viewport_bottom
        };
        let mut handles = Vec::new();
        if table.columns > 1 && table.header.is_some() {
            for column in 0..table.columns {
                let (Some(paint), Some(hit)) = (
                    table.column_handle_paint(column),
                    table.column_handle_hit(column),
                ) else {
                    continue;
                };
                if !on_screen(&paint) {
                    continue;
                }
                handles.push(TableDragHandlePaint {
                    axis: TableDragAxis::Column,
                    line,
                    index: column,
                    paint,
                    hit,
                });
            }
        }
        if table.row_count > 1 {
            for row in 0..table.row_count {
                // 只给本帧可见的数据行（`row` 是 0 起、不含表头；绘制行是 1 起）。
                if !table.rows.iter().any(|paint| paint.row == row + 1) {
                    continue;
                }
                let (Some(paint), Some(hit)) =
                    (table.row_handle_paint(row), table.row_handle_hit(row))
                else {
                    continue;
                };
                if !on_screen(&paint) {
                    continue;
                }
                handles.push(TableDragHandlePaint {
                    axis: TableDragAxis::Row,
                    line,
                    index: row,
                    paint,
                    hit,
                });
            }
        }
        handles
    }

    /// 拖动中的插入位置指示线（窗口坐标）；没有拖拽 / 已拖出表格 / 原地不动 → `None`。
    #[must_use]
    pub fn table_drop_indicator(&self) -> Option<Bounds<Pixels>> {
        let drag = self.table_drag?;
        drag.target?;
        let slot = drag.slot?;
        let geometry = self.shared.geometry()?;
        let object = geometry
            .objects()
            .iter()
            .find(|object| object.line == drag.line)?;
        let table = object.table()?;
        table.drop_indicator_rect(drag.axis, slot)
    }

    /// 拖拽把手的目标表格行：拖动中恒取被拖的那张表；否则取悬浮/选中的表格。
    fn table_drag_target_line(&self) -> Option<usize> {
        if let Some(drag) = self.table_drag {
            return self
                .controller
                .object_at_line(drag.line)
                .filter(|object| object.kind().is_table())
                .map(|_| drag.line);
        }
        self.table_actions_target_line()
    }

    /// 指针位置的表格拖拽槽位（按被拖表格的**当前**几何算；拖出表格 → `None`）。
    fn table_drag_slot(
        &self,
        line: usize,
        axis: TableDragAxis,
        position: Point<Pixels>,
    ) -> Option<usize> {
        let geometry = self.shared.geometry()?;
        let object = geometry
            .objects()
            .iter()
            .find(|object| object.line == line)?;
        let table = object.table()?;
        match axis {
            TableDragAxis::Column => table.column_drop_slot(position),
            TableDragAxis::Row => table.row_drop_slot(position),
        }
    }

    /// 被拖表格的项数（列数 / 数据行数）；表格已不是整块 Object → `None`。
    fn table_drag_count(&self, line: usize, axis: TableDragAxis) -> Option<usize> {
        let object = self
            .controller
            .object_at_line(line)
            .filter(|object| object.kind().is_table())?;
        let table = self.controller.table_of(object)?;
        Some(match axis {
            TableDragAxis::Column => table.columns(),
            TableDragAxis::Row => table.row_count(),
        })
    }

    /// 按下列手：开始一次表格行列拖拽。
    ///
    /// **不产生任何编辑**（真正的重排在释放时一次提交）；只记下“拖的是哪张表的哪一项”。
    /// 持闸 / IME 组合 / 单元格就地编辑 / 表格源码模式 → 拒绝（不进入拖拽）。
    pub fn begin_table_drag(
        &mut self,
        line: usize,
        axis: TableDragAxis,
        index: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        self.pending_enter_text_input = false;
        if self.frozen_by_external_commit() {
            eprintln!("[menghunote] 同步持闸中：不开始表格行列拖拽（行 {line}）");
            return false;
        }
        if self.is_composing() {
            eprintln!("[menghunote] IME 组合中：不开始表格行列拖拽（行 {line}）");
            return false;
        }
        if self.controller.is_editing_object_source() || self.controller.is_editing_table_cell() {
            // 源码模式里表格不是 Object；单元格编辑期间镜像领先于会话。
            return false;
        }
        let Some(count) = self.table_drag_count(line, axis) else {
            return false;
        };
        if count < 2 || index >= count {
            return false;
        }
        self.selected_object_line = Some(line);
        self.table_drag = Some(TableDragState {
            line,
            axis,
            from: index,
            slot: None,
            target: None,
            position: point(px(0.0), px(0.0)),
        });
        eprintln!(
            "[menghunote] 开始表格{}拖拽（行 {line} 的 #{index}）：拖到目标位置释放，Esc/拖出表格取消",
            table_drag_axis_label(axis)
        );
        cx.notify();
        true
    }

    /// 拖动中：按指针位置更新插入槽位（并重绘指示线）。**不产生任何编辑**。
    pub(crate) fn update_table_drag(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(mut drag) = self.table_drag else {
            return;
        };
        let slot = self.table_drag_slot(drag.line, drag.axis, position);
        let count = self.table_drag_count(drag.line, drag.axis);
        let target = slot
            .zip(count)
            .and_then(|(slot, count)| menghunote_editor::drop_target(drag.from, slot, count));
        let changed = drag.slot != slot || drag.target != target;
        drag.slot = slot;
        drag.target = target;
        drag.position = position;
        self.table_drag = Some(drag);
        if changed {
            cx.notify();
        }
    }

    /// 释放：槽位有效 → **一次** `EditRequest` 重排（一次 undo）；否则取消、零请求。
    pub fn finish_table_drag(
        &mut self,
        position: Point<Pixels>,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.table_drag.take() else {
            return false;
        };
        let slot = self.table_drag_slot(drag.line, drag.axis, position);
        let count = self.table_drag_count(drag.line, drag.axis);
        let target = slot
            .zip(count)
            .and_then(|(slot, count)| menghunote_editor::drop_target(drag.from, slot, count));
        let Some(to) = target else {
            eprintln!(
                "[menghunote] 表格{}拖拽在表格外/原位释放（行 {line}）：取消，不产生请求",
                table_drag_axis_label(drag.axis),
                line = drag.line
            );
            cx.notify();
            return false;
        };
        self.move_table_structure(drag.axis, drag.line, drag.from, to, window, cx)
    }

    /// 取消拖拽（`Esc` / 拖出表格 / 拖拽中按了别的键）：丢弃状态，**不产生任何请求**。
    pub fn cancel_table_drag(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(drag) = self.table_drag.take() else {
            return false;
        };
        eprintln!(
            "[menghunote] 取消表格{}拖拽（行 {line} 的 #{from}）：未产生任何请求",
            table_drag_axis_label(drag.axis),
            line = drag.line,
            from = drag.from
        );
        cx.notify();
        true
    }

    /// 表格重排动作的统一入口（拖拽释放）：一次 `EditRequest` + 选中格修正。
    fn move_table_structure(
        &mut self,
        axis: TableDragAxis,
        line: usize,
        from: usize,
        to: usize,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.pending_enter_text_input = false;
        if self.frozen_by_external_commit() {
            eprintln!(
                "[menghunote] 同步持闸中：忽略表格{}（行 {line}）",
                table_drag_axis_label(axis)
            );
            cx.notify();
            return false;
        }
        if self.is_composing() {
            eprintln!(
                "[menghunote] IME 组合中：忽略表格{}（行 {line}）",
                table_drag_axis_label(axis)
            );
            cx.notify();
            return false;
        }
        if self.controller.is_editing_object_source() || self.controller.is_editing_table_cell() {
            cx.notify();
            return false;
        }
        let result = match axis {
            TableDragAxis::Column => self.controller.move_table_column(line, from, to),
            TableDragAxis::Row => self.controller.move_table_row(line, from, to),
        };
        match result {
            Ok(Some(request)) => {
                let command_id = request.command_id.clone();
                let edits = request.edits.len();
                if let Some(window) = window {
                    window.focus(&self.focus_handle, cx);
                }
                let accepted = self.submit_request(Some(request)).is_some();
                self.correct_table_cell_after_move(axis, line, from, to);
                eprintln!(
                    "[menghunote] 表格{}（行 {line} 的 #{from} → #{to}）command_id={command_id} edits={edits} accepted={accepted} → 选中格={:?}（仍为 Object）",
                    table_drag_axis_label(axis),
                    self.selected_table_cell
                );
                cx.notify();
                accepted
            }
            Ok(None) => {
                eprintln!(
                    "[menghunote] 表格{}（行 {line} 的 #{from} → #{to}）没有产生编辑（no-op）",
                    table_drag_axis_label(axis)
                );
                cx.notify();
                false
            }
            Err(error) => {
                eprintln!(
                    "[menghunote] 表格{}被拒：{error:?}",
                    table_drag_axis_label(axis)
                );
                cx.notify();
                false
            }
        }
    }

    /// 重排之后修正单元格选中态：**选中的内容跟着走**（纯函数在 `table_cell_edit`）。
    fn correct_table_cell_after_move(
        &mut self,
        axis: TableDragAxis,
        line: usize,
        from: usize,
        to: usize,
    ) {
        self.preferred_x = None;
        let dims = self
            .controller
            .object_at_line(line)
            .filter(|object| object.kind().is_table())
            .and_then(|object| self.controller.table_of(object))
            .map(|table| (table.row_count(), table.columns()));
        let Some((rows, columns)) = dims else {
            self.selected_table_cell = None;
            return;
        };
        let Some((selected_line, row, column)) = self.selected_table_cell else {
            return;
        };
        if selected_line != line {
            return;
        }
        let next = match axis {
            TableDragAxis::Row => {
                menghunote_editor::cell_after_row_move(row, column, from, to, rows, columns)
            }
            TableDragAxis::Column => {
                menghunote_editor::cell_after_column_move(row, column, from, to, rows, columns)
            }
        };
        match next {
            Some(cell) => {
                self.selected_table_cell = Some((line, cell.row, cell.column));
                self.selected_object_line = Some(line);
            }
            None => self.selected_table_cell = None,
        }
    }

    // ---- 正文右键菜单（ADR 0019 S3b4：插入表格）----

    /// 在右键落点打开「插入 → 表格」菜单；**不移动正文 caret/选区**。
    ///
    /// - 右键只决定菜单面板的位置，插入位置始终是打开菜单时编辑器里的 caret/选区；
    /// - 单元格就地编辑期间不开菜单（镜像领先于会话，菜单项会绕过那格未提交的草稿）；
    /// - 已在别处打开时直接挪到新落点（同一个菜单，不叠两个面板）。
    pub(crate) fn open_context_menu_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.pending_enter_text_input = false;
        if self.controller.is_editing_table_cell() {
            eprintln!("[menghunote] 单元格编辑中：不打开正文右键菜单（先提交该格）");
            return;
        }
        self.context_menu = Some(EditorContextMenu { position });
        eprintln!(
            "[menghunote] 正文右键菜单（插入 → 表格）at=({:.1},{:.1})",
            position.x.as_f32(),
            position.y.as_f32()
        );
        cx.notify();
    }

    /// 关闭正文右键菜单（`Esc` / 点击菜单外 / 执行菜单项之后）。
    pub(crate) fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            self.context_menu_rect = None;
            cx.notify();
        }
    }

    /// 右键菜单「插入 → 表格」：在**当前 caret/选区**处插入一张默认 GFM 表格
    /// （ADR 0019 S3b4）。
    ///
    /// - 只走 [`EditorController::insert_table`]（一次点击一个 `EditRequest`，
    ///   由会话默认撤销组进 undo）；app 不自己拼 `ScalarEdit`、不改 body；
    /// - caret 落在插入文本之后 = 表格之后（表格对象内部没有文本位置）；
    /// - 插入后清空表格/图片点选态：新表格是普通正文内容，不需要旧的选中目标；
    /// - 持闸 / IME 组合 / 单元格编辑中直接忽略（不产生请求）。
    pub(crate) fn insert_table_from_context_menu(
        &mut self,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.context_menu = None;
        // **故意不立刻清 `context_menu_rect`**：菜单项是元素级 on_mouse_down（先跑），
        // 元素层的窗口级左键监听随后还会收到同一次 mouse-down；保留命中矩形让
        // `point_in_toolbar` 继续放行这次点击，避免“插入后又被这次点击挪 caret”。
        // 下一帧 render 开头会把它清掉（菜单已经关了）。
        self.pending_enter_text_input = false;
        if self.frozen_by_external_commit() {
            eprintln!("[menghunote] 同步持闸中：忽略插入表格（不产生请求）");
            cx.notify();
            return false;
        }
        if self.is_composing() {
            eprintln!("[menghunote] IME 组合中：忽略插入表格（不产生请求）");
            cx.notify();
            return false;
        }
        if self.controller.is_editing_table_cell() {
            eprintln!("[menghunote] 单元格编辑中：忽略插入表格（不产生请求）");
            cx.notify();
            return false;
        }
        match self.controller.insert_table() {
            Ok(request) => {
                let command_id = request.command_id.clone();
                let edits = request.edits.len();
                if let Some(window) = window {
                    window.focus(&self.focus_handle, cx);
                }
                let accepted = self.submit_request(Some(request)).is_some();
                self.selected_object_line = None;
                self.selected_table_cell = None;
                self.preferred_x = None;
                self.ensure_caret_visible(self.controller.selection().head());
                eprintln!(
                    "[menghunote] 插入表格（右键菜单）command_id={command_id} edits={edits} accepted={accepted}（caret 在表格之后）"
                );
                cx.notify();
                accepted
            }
            Err(error) => {
                eprintln!("[menghunote] 插入表格失败：{error}");
                cx.notify();
                false
            }
        }
    }

    /// 打开图片放大预览（图片已解码时）。
    pub fn open_object_zoom(&mut self, line: usize, cx: &mut Context<Self>) {
        let Some(image) = self.controller.block_image_at_line(line) else {
            return;
        };
        let Some(hash) = parse_asset_hash(image.dest()) else {
            return;
        };
        let ImageState::Ready(decoded) = self.images.state(hash) else {
            return;
        };
        self.zoomed_image = Some(Arc::clone(&decoded.image));
        self.selected_object_line = Some(line);
        cx.notify();
    }

    /// 关闭图片放大预览。
    pub fn close_object_zoom(&mut self, cx: &mut Context<Self>) {
        if self.zoomed_image.take().is_some() {
            cx.notify();
        }
    }

    /// 工具条「编辑源码」：把目标对象切换到源码模式（ADR 0019 §1，图片/表格通用）。
    pub fn enter_selected_object_source_edit(&mut self, cx: &mut Context<Self>) {
        let Some(line) = self
            .selected_object_line
            .filter(|line| self.controller.object_at_line(*line).is_some())
            .or_else(|| self.highlighted_object_lines().first().copied())
            .or_else(|| {
                self.hovered_object_line
                    .filter(|line| self.controller.object_at_line(*line).is_some())
            })
        else {
            return;
        };
        self.enter_object_source_edit(line, cx);
    }

    /// 进入指定对象行的源码模式（图片 `![...]` / 整张表格源码）。
    pub fn enter_object_source_edit(&mut self, line: usize, cx: &mut Context<Self>) {
        // 源码模式与单元格编辑互斥：进入源码模式先丢掉单元格选中/编辑态。
        self.selected_table_cell = None;
        let kind = self
            .controller
            .object_at_line(line)
            .map(menghunote_editor::ObjectBlock::kind);
        match self.controller.begin_object_source_edit(line) {
            Ok(true) => {
                self.selected_object_line = None;
                self.hovered_object_line = None;
                eprintln!(
                    "[menghunote] {}对象 → 源码模式（行 {line}）",
                    kind.map_or("未知", |kind| kind.label())
                );
                self.ensure_caret_visible(self.controller.selection().head());
                cx.notify();
            }
            Ok(false) => {}
            Err(error) => eprintln!("[menghunote] 进入对象源码模式失败：{error}"),
        }
    }

    /// 退出对象源码模式（`Esc` / 点击块外 / `Ctrl+Enter`）。
    pub fn exit_object_source_edit(&mut self, cx: &mut Context<Self>) {
        if self.controller.end_object_source_edit() {
            eprintln!("[menghunote] 对象 ← 退出源码模式（恢复图片/表格）");
            self.ensure_caret_visible(self.controller.selection().head());
            cx.notify();
        }
    }

    /// 工具条「复制 Markdown」：复制对象的**完整源码跨度**（图片/表格通用）。
    pub fn copy_object_markdown(&mut self, line: usize, cx: &mut Context<Self>) {
        let Some(object) = self.controller.object_at_line(line) else {
            return;
        };
        let source = self.controller.source_text();
        let Some(markdown) = source
            .get(object.span().start().get()..object.span().end().get())
            .map(str::to_owned)
        else {
            return;
        };
        eprintln!(
            "[menghunote] 复制{} Markdown（{} 字节）",
            object.kind().label(),
            markdown.len()
        );
        // 元数据标记：粘贴时"本应用 Markdown 文本"优先级最高（ADR 0019 §2）。
        cx.write_to_clipboard(ClipboardItem::new_string_with_metadata(
            markdown,
            MARKDOWN_METADATA_PREFIX.to_owned(),
        ));
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

    /// 逐**视觉行**的度量（字号/行高/是否等宽/缩进），来自块级语义 + 列表块树。
    ///
    /// 返回值与 `display_text()` 的 `\n` 切分一一对应（`wrap_with` 正是按 display 行
    /// 取度量）。**display 行号 ≠ 源码行号**：Object Block（图片/表格）把整块源码压成
    /// 一个占位符，后面的 display 行会前移。因此这里先用投影把“display 行首字节”
    /// 映射回源码字节，再用源码行的语义算度量；否则表格之后的标题/列表字号会整体错位。
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
        // 源码行起点（升序），用于 display 字节 → 源码行号二分。
        let mut source_line_starts: Vec<usize> = vec![0];
        for (index, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                source_line_starts.push(index + 1);
            }
        }
        let display = self.controller.display_text();
        let projection = self.controller.projection();
        let line_infos = self.controller.line_infos();
        let items = self.controller.list_items();
        // 键包含字号与等宽标记：同一个 "• " 在标题行/正文行宽度不同，不能混用。
        let mut marker_widths: HashMap<(u32, bool, String), f32> = HashMap::new();

        let object_edit_line = self.controller.object_source_edit_line();
        // 单元格就地编辑：该行 display 是 cell 文本（不是表格网格），行高必须按普通文本
        // 度量 —— 否则表头格会撑成整张表的高度，caret/选区/点击全部错位。
        let cell_edit_line = self
            .controller
            .table_cell_edit_target()
            .map(|target| target.line);
        let metrics: Vec<super::layout::LineMetric> = display
            .split_inclusive('\n')
            .scan(0usize, |display_byte, raw| {
                let byte = *display_byte;
                *display_byte += raw.len();
                let source_byte = projection
                    .source_byte_at_display(menghunote_editor::DisplayByte::new(byte))
                    .map_or(0, |byte| byte.get());
                let index = source_line_starts
                    .partition_point(|start| *start <= source_byte)
                    .saturating_sub(1)
                    .min(source_lines.len().saturating_sub(1));
                Some((index, byte))
            })
            .map(|(index, _display_byte)| {
                let run = self.controller.block_runs().get(index);
                let source_line = source_lines.get(index).copied().unwrap_or("");
                // 整块 Object（且不在源码模式）：行高 = 图片/表格高度 + 内边距。
                // 这是布局、绘制、命中三处**唯一**的高度来源（不在这里估字符宽度）。
                if object_edit_line != Some(index) && cell_edit_line != Some(index) {
                    if let Some(object) = self.controller.object_at_line(index) {
                        let line_height = match object.kind() {
                            menghunote_editor::ObjectKind::Image => {
                                self.controller.image_of(object).map_or(
                                    theme::OBJECT_PLACEHOLDER_HEIGHT + theme::OBJECT_PADDING * 2.0,
                                    |image| self.object_row_height(image, content_width),
                                )
                            }
                            menghunote_editor::ObjectKind::Table => {
                                self.controller.table_of(object).map_or(
                                    theme::TABLE_HEADER_HEIGHT + theme::TABLE_PADDING * 2.0,
                                    |table| self.table_row_height(table, content_width),
                                )
                            }
                        };
                        return super::layout::LineMetric {
                            font_size: theme::FONT_SIZE,
                            line_height,
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

                let kind = run.map_or(BlockKind::Paragraph, |run| run.kind());
                let (mut font_size, mut line_height, mut mono) = match kind {
                    BlockKind::Heading => {
                        let level = run.and_then(|run| run.level()).unwrap_or(1);
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
                        .map(|item| {
                            if self.controller.list_marker_revealed(list.item) {
                                self.controller
                                    .list_marker_source(list.item)
                                    .unwrap_or_default()
                                    .to_owned()
                            } else {
                                let mut text = self
                                    .controller
                                    .list_marker_text(list.item)
                                    .unwrap_or_default();
                                // ADR 0019 S3：任务项的内容列还要跨过 `[ ]` 占位
                                // （自绘复选框）与 `]` 之后的那段源空白；否则续行
                                // hanging indent 会停在复选框下面，与正文错位。
                                if let Some(span) = item.task_marker {
                                    text.push_str(TASK_BOX_DISPLAY);
                                    if let Some(rest) = source.get(span.end..) {
                                        let ws = rest
                                            .bytes()
                                            .take_while(|byte| *byte == b' ' || *byte == b'\t')
                                            .count();
                                        text.push_str(&rest[..ws]);
                                    }
                                }
                                text
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
                if matches!(kind, BlockKind::Heading) {
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
            .collect();
        metrics
    }

    /// wiki 链接的目标（已归一化；供宿主解析）。
    pub fn wiki_targets(&self) -> Vec<String> {
        let source = self.controller.source_text();
        self.controller
            .wiki_links()
            .iter()
            .map(|link| normalize_wiki_target(link.target_text(&source)))
            .collect()
    }

    /// 所有 wiki 链接的 **display** 字节区间（用于精确点击/手形光标）。
    pub fn wiki_display_ranges(&self) -> Vec<(usize, usize, String, Option<String>)> {
        let source = self.controller.source_text();
        let mut ranges = Vec::new();
        for link in self.controller.wiki_links() {
            let Some(start) = self.controller.source_to_display(link.display().start()) else {
                continue;
            };
            let Some(end) = self.controller.source_to_display(link.display().end()) else {
                continue;
            };
            if start >= end {
                continue;
            }
            let target = link.target_text(&source).to_owned();
            let heading = link.heading().and_then(|span| {
                source
                    .get(span.start().get()..span.end().get())
                    .map(str::to_owned)
            });
            ranges.push((start.get(), end.get(), target, heading));
        }
        ranges
    }

    /// 命中 wiki 链接：`display` 必须严格落在可见内容区间内。
    ///
    /// 末尾边界（`byte == end`）不算命中，这样点链接末尾会正常进入/显示源码，
    /// 而不是直接跳转；上一行/邻近行的 display 字节也不会误命中。
    pub fn wiki_link_at_display(&self, display: usize) -> Option<(String, Option<String>)> {
        self.wiki_display_ranges()
            .into_iter()
            .find(|(start, end, _, _)| *start <= display && display < *end)
            .map(|(_, _, target, heading)| (target, heading))
    }

    /// 所有普通链接的 **display** 可见内容区间（点击/手形用）。
    pub fn external_link_display_ranges(&self) -> Vec<(usize, usize, String)> {
        let mut ranges = Vec::new();
        for link in self.controller.links() {
            let Some(start) = self.controller.source_to_display(link.content().start()) else {
                continue;
            };
            let Some(end) = self.controller.source_to_display(link.content().end()) else {
                continue;
            };
            if start >= end {
                continue;
            }
            ranges.push((start.get(), end.get(), link.dest().to_owned()));
        }
        ranges
    }

    /// 命中普通链接；只接受可见文本区间（末尾边界不跳转）。
    pub fn external_link_at_display(&self, display: usize) -> Option<String> {
        self.external_link_display_ranges()
            .into_iter()
            .find(|(start, end, _)| *start <= display && display < *end)
            .map(|(_, _, dest)| dest)
    }

    /// 宿主更新“解析不到的目标集合”；变化时触发重绘/重排样式。
    pub fn set_unresolved_wiki_targets(
        &mut self,
        targets: HashSet<String>,
        cx: &mut Context<Self>,
    ) {
        if self.unresolved_wiki_targets != targets {
            self.unresolved_wiki_targets = targets;
            cx.notify();
        }
    }

    /// 本帧的语义样式段（display 字节区间，完整覆盖全文）。
    pub(crate) fn style_runs_display(&self) -> Vec<(std::ops::Range<usize>, StyleKind)> {
        let mut runs: Vec<(std::ops::Range<usize>, StyleKind)> = self
            .controller
            .style_runs()
            .iter()
            .map(|run| (run.start().get()..run.end().get(), run.kind()))
            .collect();
        if self.unresolved_wiki_targets.is_empty() {
            return runs;
        }
        let source = self.controller.source_text();
        let mut overrides = Vec::new();
        for link in self.controller.wiki_links() {
            let target = normalize_wiki_target(link.target_text(&source));
            if !self.unresolved_wiki_targets.contains(&target) {
                continue;
            }
            let Some(start) = self.controller.source_to_display(link.display().start()) else {
                continue;
            };
            let Some(end) = self.controller.source_to_display(link.display().end()) else {
                continue;
            };
            if start < end {
                overrides.push((start.get()..end.get(), StyleKind::WikiLinkUnresolved));
            }
        }
        for (range, kind) in overrides {
            runs = replace_style_range(runs, range, kind);
        }
        runs
    }

    /// 命中一条 wiki 链接：返回 `(target, heading)`（源码切片，不含括号/alias）。
    ///
    /// `byte` 是源码字节；点击落在 display 文本上时由几何映射回来，因此 alias
    /// 可见时命中的是显示区间，但这里仍用整条链接的 source span 判定。
    pub fn wiki_link_at_source(&self, byte: SourceByte) -> Option<(String, Option<String>)> {
        let source = self.controller.source_text();
        let link = self.controller.wiki_links().iter().find(|link| {
            let span = link.span();
            span.start() <= byte && byte < span.end()
        })?;
        let target = link.target_text(&source).to_owned();
        let heading = link.heading().and_then(|span| {
            source
                .get(span.start().get()..span.end().get())
                .map(str::to_owned)
        });
        Some((target, heading))
    }

    /// 发出 wiki 链接请求（宿主负责解析目标并打开）。
    pub fn request_wiki_link(
        &mut self,
        target: String,
        heading: Option<String>,
        cx: &mut Context<Self>,
    ) {
        cx.emit(EditorEvent::WikiLinkRequested { target, heading });
    }

    /// 发出普通外链请求（宿主负责 scheme 校验并打开系统浏览器）。
    pub fn request_external_link(&mut self, url: String, cx: &mut Context<Self>) {
        cx.emit(EditorEvent::ExternalLinkRequested(url));
    }

    /// 重命名联动：通过**当前活动 driver** 改写正文里的 wiki 链接 target。
    ///
    /// 为什么必须走这里而不是 storage 的“另开一个会话”：活动笔记的 live 状态只在
    /// 本视图的 driver 里；另开会话会拿旧快照写正文，把在途编辑覆盖掉（stale 覆盖）。
    /// 本方法复用同一个 `EditRequest` 通道，再 `flush(Manual)` 等 durable + B 物化，
    /// 因此 `links` 会在返回前刷新（不直接写 `body_md` / `links`）。
    ///
    /// `linked_targets` 是 `links` 里解析到被重命名笔记的 target 原文（见
    /// [`menghunote_storage::rename_links::link_sources_for_note`]）。返回改写的链接数。
    pub fn rewrite_wiki_link_targets(
        &mut self,
        old_title: &str,
        new_title: &str,
        linked_targets: &BTreeSet<String>,
        timeout: Duration,
    ) -> Result<usize, String> {
        if self.frozen_by_external_commit() {
            return Err("同步持闸中，未改写当前笔记".to_owned());
        }
        if self.is_composing() {
            return Err("输入法组合中，未改写当前笔记".to_owned());
        }
        let source = self.controller.source_text();
        let renames = plan_wiki_link_renames(&source, old_title, new_title, linked_targets);
        if renames.is_empty() {
            return Ok(0);
        }
        let edits = scalar_edits_for_renames(&source, &renames)?;
        let selection = self.controller.selection();
        let request = self
            .controller
            .compose_edits(edits.clone())
            .map_err(|error| format!("构造链接改写请求失败：{error}"))?;
        let Some(outcome) = self.submit_request(Some(request)) else {
            return Err("链接改写请求未被受理".to_owned());
        };
        let target_live_rev = match outcome {
            EditOutcome::Applied(applied) => applied.live_rev,
            EditOutcome::Rejected(rejected) => {
                return Err(format!("链接改写被拒：{:?}", rejected.reason));
            }
            EditOutcome::Stale(stale) => {
                return Err(format!(
                    "链接改写基准过期（current_live_rev={}）",
                    stale.current_live_rev.0
                ));
            }
        };
        // `compose_edits` 会把 caret 移到最后一处替换之后；这里按同一组编辑把
        // 用户原来的选区映射回来，避免自动改写把正在编辑的光标甩走。
        let mapped = Selection::new(
            UnicodeScalar::new(transform_scalar(selection.anchor().get(), &edits)),
            UnicodeScalar::new(transform_scalar(selection.head().get(), &edits)),
        );
        if let Err(error) = self.controller.set_selection(mapped) {
            eprintln!("[menghunote] 链接改写后恢复选区失败：{error}");
        }
        self.driver.flush(FlushReason::Manual);
        self.await_link_materialize(target_live_rev, timeout)?;
        Ok(renames.len())
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
        // 关闭/切换前先把未提交的单元格编辑落成一条请求（持闸中则放弃草稿）：
        // 草稿只活在镜像里，不提交就等于静默丢掉用户刚打的字。
        if self.controller.is_editing_table_cell() {
            if self.frozen_by_external_commit() {
                self.controller.cancel_table_cell_edit();
                eprintln!("[menghunote] 关闭/切换前放弃未提交的单元格编辑（同步持闸中）");
            } else {
                match self.controller.commit_table_cell_edit() {
                    Ok(request) => {
                        let edits = request.as_ref().map(|request| request.edits.len());
                        self.submit_request(request);
                        eprintln!("[menghunote] 关闭/切换前提交单元格编辑 edits={edits:?}");
                    }
                    Err(error) => eprintln!("[menghunote] 关闭/切换前提交单元格编辑失败：{error}"),
                }
            }
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
        // 关闭/切换前最后对一次账：正文已 flush，引用按最终正文重建（ADR 0019 §3）。
        self.reconcile_asset_refs_now();
        self.close_report = Some(report.clone());
        report
    }

    /// 关闭窗口时**有界**排空（沿用 5a 的固定上限）。
    ///
    /// 图片解码线程随视图 Drop 关闭（worker 收到 `Shutdown` 后退出，不等待未完成的解码）。
    pub fn close_session(&mut self) {
        self.drain_and_close(CLOSE_TIMEOUT);
    }

    /// 手动 flush 后等待 durable + B 物化（重命名联动改写用）。
    ///
    /// 不走 `close`：活动文档的会话必须继续可用。等待条件与 `close` 同一口径：
    /// ACK 覆盖本次 target，且该 ACK 对应的物化结果已经返回（`body_seq >= head_seq`）。
    /// 与 `close` 一样是**有界**的，超时返回错误而不是挂死 UI。
    fn await_link_materialize(&mut self, target: LiveRev, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut durable_head: Option<u64> = None;
        let mut materialized_seq: Option<u64> = None;
        let mut materialize_error: Option<String> = None;
        loop {
            for event in self.driver.pump() {
                match event {
                    DriverEvent::Durable(ack) => {
                        self.controller.apply_durable_ack(&ack);
                        if ack.through_live_rev >= target {
                            durable_head = Some(
                                durable_head.map_or(ack.head_seq, |head| head.max(ack.head_seq)),
                            );
                        }
                    }
                    DriverEvent::Materialized(outcome) => {
                        let body_seq = match outcome {
                            MaterializeOutcome::Applied { body_seq, .. }
                            | MaterializeOutcome::Stale { body_seq, .. } => body_seq,
                        };
                        materialized_seq =
                            Some(materialized_seq.map_or(body_seq, |seq| seq.max(body_seq)));
                    }
                    DriverEvent::MaterializeFailed { note_id, message } => {
                        // 事件可能先于 Durable 到达；先记下，等 ACK 后统一报错。
                        materialize_error =
                            Some(format!("links 物化失败（note={note_id}）：{message}"));
                    }
                    DriverEvent::Remote(delta) => {
                        if let Err(error) = self.controller.apply_live_delta(delta) {
                            eprintln!("[menghunote] 链接改写期间远端变更未能应用：{error}");
                        }
                    }
                    DriverEvent::FlushScheduled { .. } => {}
                }
            }
            if let Some(head) = durable_head {
                if let Some(message) = materialize_error {
                    return Err(message);
                }
                if materialized_seq.is_some_and(|seq| seq >= head) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err("等待正文落库/物化超时".to_owned());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
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
        // 点击块外：正在编辑对象源码时先退出（ADR 0019：Esc / 点击块外 / Ctrl+Enter）。
        //
        // 判定按**源码行**而不是 display 行：表格在 display 上只占一行，但源码是
        // 多行，点击表格源码里的任意一行都算“块内”，不能把源码模式踢掉。
        if let Some(editing) = self.controller.object_source_edit_line() {
            let range = self
                .controller
                .object_at_line(editing)
                .map(|object| object.line()..=object.end_line());
            let clicked_line = self.source_line_of_display_byte(display_byte);
            let inside = match (range, clicked_line) {
                (Some(range), Some(line)) => range.contains(&line),
                _ => false,
            };
            if !inside {
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
        if event.keystroke.key.as_str() != "tab" {
            // 同理：Tab 的 `\t` 回声没来也不能留到下一次输入。
            self.pending_tab_text_input = false;
        }
        // 表格行列拖拽（ADR 0019 S3b5）：`Esc` 取消这次拖拽（**不产生任何请求**）；
        // 其它按键先取消拖拽再继续处理（拖拽不接受键盘改目标，也不能和输入抢状态）。
        if self.table_drag.is_some() {
            if event.keystroke.key.as_str() == "escape" {
                window.prevent_default();
                self.cancel_table_drag(cx);
                return;
            }
            self.cancel_table_drag(cx);
        }

        // 正文右键菜单（ADR 0019 S3b4）：Esc 关闭；其它按键先关菜单再继续处理
        // （不吞用户输入，也不让菜单停在过期位置）。
        if self.context_menu.is_some() {
            self.close_context_menu(cx);
            if event.keystroke.key.as_str() == "escape" {
                window.prevent_default();
                return;
            }
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

        // 单元格就地编辑（ADR 0019 S3b2）：Enter 提交 / Tab 下一格 / Esc 取消。
        //
        // 这三个键在编辑态里**优先**于一切正文语义（Enter 不再换行、Tab 不再缩进、
        // Esc 不再退对象源码模式）：否则用户按 Enter 想提交，结果把表格行切断。
        if self.controller.is_editing_table_cell() && !modifiers.alt && !modifiers.platform {
            match (event.keystroke.key.as_str(), modifiers.control) {
                ("enter", false) => {
                    // 平台可能仍补发一次 `\n` 文本输入：标记一下让 insert_text 消费掉。
                    self.pending_enter_text_input = true;
                    window.prevent_default();
                    self.commit_table_cell_edit(Some(window), cx);
                    return;
                }
                ("tab", false) => {
                    // 平台可能仍补发一次 `\t` 文本输入：标记一下让 insert_text 消费掉。
                    self.pending_tab_text_input = true;
                    window.prevent_default();
                    self.advance_table_cell_edit(!modifiers.shift, Some(window), cx);
                    return;
                }
                ("escape", false) => {
                    window.prevent_default();
                    self.cancel_table_cell_edit(cx);
                    return;
                }
                // Ctrl+Z / Ctrl+Shift+Z：撤销的是“这次未提交的编辑”（= 取消），
                // 不能把会话的历史撤销按 live 坐标套到领先于会话的镜像上。
                ("z", true) => {
                    window.prevent_default();
                    eprintln!("[menghunote] 单元格编辑中收到撤销：取消本次未提交的编辑");
                    self.cancel_table_cell_edit(cx);
                    return;
                }
                _ => {}
            }
        }

        // F2：选中表格单元格 → 进入单元格编辑；有图片对象可编辑 → 该块源码模式；
        // 否则请宿主重命名侧栏项。
        if !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && event.keystroke.key.as_str() == "f2"
        {
            if let Some((line, row, column)) = self.selected_table_cell {
                if self.begin_table_cell_edit(line, row, column, Some(window), cx) {
                    window.prevent_default();
                    return;
                }
            }
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

        // Esc：只选了单元格（未编辑）时退出单元格选中态（整表选中保留，S3a 行为不变）。
        if event.keystroke.key.as_str() == "escape"
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && self.selected_table_cell.take().is_some()
        {
            eprintln!("[menghunote] Esc：退出单元格选中态（表格仍是 Object）");
            cx.notify();
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
                            cx.write_to_clipboard(ClipboardItem::new_string_with_metadata(
                                text,
                                MARKDOWN_METADATA_PREFIX.to_owned(),
                            ));
                        }
                    }
                }
                // Ctrl+V / Ctrl+Shift+V：剪贴板分流（ADR 0019 §2 / S2b）。
                //
                // `prevent_default`：不让平台把这次按键再当文本输入走一遍
                // （Windows 后端不处理 Ctrl+V，但 web 等后端会派发 paste 事件）。
                ("v", false) => {
                    window.prevent_default();
                    self.paste_from_clipboard(false, cx);
                }
                ("v", true) => {
                    window.prevent_default();
                    self.paste_from_clipboard(true, cx);
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
                // 选着某个单元格时 Enter = 进入该格编辑（与 F2 / 再次单击同义）。
                if let Some((line, row, column)) = self.selected_table_cell {
                    if self.begin_table_cell_edit(line, row, column, Some(window), cx) {
                        self.pending_enter_text_input = true;
                        window.prevent_default();
                        return;
                    }
                }
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
        if !self.controller.is_editing_table_cell() {
            self.selected_object_line = None;
        }
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
        // 平台回声必须先消费，**不能**顺手清掉选中态：单元格编辑提交后 Enter/Tab 仍可能
        // 补发一次 `\n` / `\t` 文本输入，那会把刚提交的表格选中态清掉、或把制表符
        // 插进下一格。
        if text == "\n" && self.pending_enter_text_input {
            // Enter 已经由 key_down 处理（结构编辑或普通换行），这里只消费平台的回声。
            self.pending_enter_text_input = false;
            eprintln!("[menghunote] 已消费平台重复的 Enter 文本输入（避免双换行）");
            return;
        }
        if text == "\t" && self.pending_tab_text_input {
            self.pending_tab_text_input = false;
            eprintln!("[menghunote] 已消费平台重复的 Tab 文本输入（避免把制表符插进单元格）");
            return;
        }
        // 单元格就地编辑期间打的是这一格：整表仍是当前对象（工具条/加行加列在提交后
        // 还要用），不能因为一次按键就把选中态清掉。
        if !self.controller.is_editing_table_cell() {
            self.selected_object_line = None;
        }
        self.pending_enter_text_input = false;
        self.pending_tab_text_input = false;
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

    // ---- 粘贴 / 拖放导入（ADR 0019 §2/§3，S2b）----

    /// 光标（选区起点）在 **source 字节**坐标下的位置。
    ///
    /// 导入前拿它当 `asset_refs.start_byte` 的基址；插入前会**重算并比对**，
    /// 因此"从读剪贴板到真正插入之间光标被挪走/切了笔记"不会写出错位引用。
    fn current_source_offset(&self) -> Result<usize, String> {
        let source = self.controller.source_text();
        self.controller
            .selection()
            .start()
            .to_source_byte(&source)
            .map(|byte| byte.get())
            .map_err(|error| format!("光标位置换算失败：{error}"))
    }

    /// 导入前的闸门：同步持闸 / IME 组合中一律**不插**（ADR 0019 §2：切笔记/IME 不偷插）。
    ///
    /// `allow_cell_text` 区分两条路径：单元格就地编辑期间**纯文本**粘贴是合法的
    /// （走 `insert_text` → 草稿，经 GFM 转义），但资产导入（文件/图片）仍然禁止
    /// （它会按会话坐标插一段 Markdown 并落库引用）。
    fn import_blocked_reason(&mut self, allow_cell_text: bool) -> Option<String> {
        if self.frozen_by_external_commit() {
            return Some("同步持闸中，暂不能粘贴/拖放导入".to_owned());
        }
        if self.is_composing() {
            return Some("输入法组合中：请先上屏或按 Esc".to_owned());
        }
        if !allow_cell_text && self.controller.is_editing_table_cell() {
            return Some("单元格编辑中：请先提交（Enter）或取消（Esc）".to_owned());
        }
        None
    }

    /// `Ctrl+V` / `Ctrl+Shift+V` / 平台 paste 的**唯一入口**。
    ///
    /// 分流规则见 [`super::clipboard::choose_paste`]：本应用 Markdown 元数据 → 文件 →
    /// 图片 → 普通文本，每次只选一种；`force_text` 强制纯文本。纯文本仍走
    /// [`Self::insert_text`]（与打字完全同一条路径），需要落库的负载交给注入的
    /// [`super::clipboard::AssetImportWriter`]。
    pub(crate) fn paste_from_clipboard(&mut self, force_text: bool, cx: &mut Context<Self>) {
        if let Some(reason) = self.import_blocked_reason(true) {
            // 连剪贴板都不读：持闸/组合期间不产生任何写入或日志噪音。
            eprintln!("[menghunote] 粘贴被忽略：{reason}");
            return;
        }
        let Some(item) = cx.read_from_clipboard() else {
            eprintln!("[menghunote] 粘贴：剪贴板为空或不可读");
            return;
        };
        let action = choose_paste(item.entries(), force_text);
        self.apply_paste(action, ImportSource::Clipboard, cx);
    }

    /// 平台直接送达的 paste（例如 web 的 DOM paste 事件）：与 `Ctrl+V` 完全同路。
    ///
    /// Windows 后端把 `Ctrl+V` 当普通按键送进来（`on_key_down` 处理），这里主要覆盖
    /// "平台把 paste 当输入事件"的后端；两条路都走同一套分流/事务/回滚。
    pub(crate) fn paste_item(&mut self, item: ClipboardItem, cx: &mut Context<Self>) {
        if let Some(reason) = self.import_blocked_reason(true) {
            eprintln!("[menghunote] 平台粘贴被忽略：{reason}");
            return;
        }
        let action = choose_paste(item.entries(), false);
        self.apply_paste(action, ImportSource::Clipboard, cx);
    }

    /// 拖放：只接受 `ExternalPaths`（GPUI 已把平台文件拖放转成这个载荷）。
    ///
    /// **用当前 caret 作为插入点**（不看落点）：不弹文件对话框、不读相对路径、
    /// 不递归目录；目录/超限会让整批失败（正文与引用都不动）。
    pub(crate) fn import_dropped_paths(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        let payload = ImportPayload::Files(paths.paths().to_vec());
        self.apply_paste(PasteAction::Import(payload), ImportSource::Drop, cx);
    }

    /// 分流结果落地：纯文本自己插，资产导入走宿主能力面。
    fn apply_paste(&mut self, action: PasteAction, source: ImportSource, cx: &mut Context<Self>) {
        match action {
            PasteAction::None => {
                eprintln!("[menghunote] {}：剪贴板里没有可导入的内容", source.label());
                let report = ImportReport {
                    source,
                    result: Err("剪贴板里没有可插入的内容".to_owned()),
                };
                cx.emit(EditorEvent::ImportReported(Box::new(report)));
                cx.notify();
            }
            // 纯文本：行为与打字一致（插入光标处/替换选区，一个撤销单元）。
            PasteAction::Text(text) => self.insert_text(&text, cx),
            PasteAction::Import(payload) => self.import_assets(payload, source, cx),
        }
    }

    /// 资产导入：读+校验 → 落资产/引用 → **一个**编辑事务 → 对账或回滚。
    fn import_assets(
        &mut self,
        payload: ImportPayload,
        source: ImportSource,
        cx: &mut Context<Self>,
    ) {
        let report = match self.try_import(&payload, source) {
            Ok(summary) => {
                eprintln!(
                    "[menghunote] {}导入完成：{} 项 / {} 字节 / 引用 {} 条",
                    source.label(),
                    summary.items,
                    summary.byte_len,
                    summary.refs
                );
                Ok(summary)
            }
            Err(reason) => {
                eprintln!(
                    "[menghunote] {}导入失败（{}）：{reason}",
                    source.label(),
                    payload.category()
                );
                Err(reason)
            }
        };
        cx.emit(EditorEvent::ImportReported(Box::new(ImportReport {
            source,
            result: report,
        })));
        cx.notify();
    }

    /// 导入的核心路径（同步、可单测）：任何失败都发生在正文写入之前，或已回滚。
    fn try_import(
        &mut self,
        payload: &ImportPayload,
        _source: ImportSource,
    ) -> Result<ImportedSummary, String> {
        if let Some(reason) = self.import_blocked_reason(false) {
            return Err(reason);
        }
        let doc_id = self.controller.doc_id().to_owned();
        let body_seq = self.driver.durable_head_seq();
        let anchor = self.current_source_offset()?;
        // 1) 读 + 校验 + 资产 + 引用（重验 note 归属在任何写入之前）。
        let stored =
            self.import_writer
                .stage_and_store(&doc_id, body_seq, anchor as u64, payload)?;
        // 2) 重验锚点与文档：从读剪贴板到真正插入之间不能"偷插"到别处。
        if self.import_blocked_reason(false).is_some()
            || self.controller.doc_id() != doc_id
            || self.driver.durable_head_seq() != body_seq
            || self.current_source_offset()? != anchor
        {
            self.import_writer.rollback(&stored);
            return Err("插入点已变化，请重试".to_owned());
        }
        // 3) 一个编辑事务（整批 Markdown 一次替换 = 一个撤销组）。
        let request = match self.controller.compose_replace_selection(&stored.markdown) {
            Ok(request) => request,
            Err(error) => {
                self.import_writer.rollback(&stored);
                return Err(format!("插入失败：{error}"));
            }
        };
        match self.submit_request(request) {
            Some(EditOutcome::Applied(_)) => {
                // 4) 正文已提交：按当前正文整篇对账（预写的引用被精确集合取代）。
                let source_text = self.controller.source_text();
                match self.import_writer.commit(&stored, &source_text) {
                    Ok(summary) => Ok(ImportedSummary {
                        items: stored.items,
                        byte_len: stored.byte_len,
                        refs: summary.refs,
                    }),
                    Err(reason) => {
                        // 正文已经落库：不回滚正文/引用（下次对账会修好），只报告。
                        eprintln!("[menghunote] 导入后引用对账失败（正文已插入）：{reason}");
                        Ok(ImportedSummary {
                            items: stored.items,
                            byte_len: stored.byte_len,
                            refs: stored.refs.len(),
                        })
                    }
                }
            }
            other => {
                self.import_writer.rollback(&stored);
                Err(match other {
                    Some(EditOutcome::Rejected(rejected)) => {
                        format!("插入被拒（{:?}）", rejected.reason)
                    }
                    Some(EditOutcome::Stale(stale)) => {
                        format!("插入基准过期（当前 live_rev={}）", stale.current_live_rev.0)
                    }
                    _ => "插入未生效（输入被冻结或编辑请求为空）".to_owned(),
                })
            }
        }
    }

    /// 按当前正文**整篇重建引用**（打开笔记 / 正文 durable / 关闭排空后触发）。
    ///
    /// 返回 `None` = 没有对应笔记（正常跳过）或对账失败（已记日志）。
    /// 集合没变时由实现方短路（不写库），因此可以放心在每次 durable 后调用。
    pub fn reconcile_asset_refs_now(&mut self) -> Option<AssetRefSummary> {
        if self.frozen_by_external_commit() {
            return None;
        }
        let doc_id = self.controller.doc_id().to_owned();
        let body_seq = self.driver.durable_head_seq();
        let source = self.controller.source_text();
        self.had_asset_links = source.contains("asset://");
        match self.import_writer.reconcile(&doc_id, body_seq, &source) {
            Ok(summary) => summary,
            Err(reason) => {
                eprintln!("[menghunote] 引用对账失败：{reason}");
                None
            }
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
        self.controller.objects().iter().find_map(|object| {
            let range = self.controller.object_display_range(object.line())?;
            let hit = match step {
                GraphemeStep::Backward => caret == range.end,
                GraphemeStep::Forward => caret == range.start,
            };
            hit.then_some(object.line())
        })
    }

    /// 删除一个对象的完整源跨度（一次 `EditRequest`），并清空对象选中态。
    fn delete_object_block(&mut self, line: usize, cx: &mut Context<Self>) {
        let label = self
            .controller
            .object_at_line(line)
            .map_or("未知", |object| object.kind().label());
        match self.controller.delete_object(line) {
            Ok(Some(request)) => {
                self.selected_object_line = None;
                self.hovered_object_line = None;
                eprintln!("[menghunote] 删除{label}对象（行 {line}）：整段源码一次删除");
                self.submit_request(Some(request));
            }
            Ok(None) => {
                self.selected_object_line = None;
                eprintln!("[menghunote] 删除{label}对象（行 {line}）：没有可删区间，忽略");
            }
            Err(error) => eprintln!("[menghunote] 删除{label}对象失败：{error}"),
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
        // Object Block（ADR 0019 §1/S3a，图片与表格同一条路径）：对象内部没有文本 caret。
        // - 块已被点选 → 直接删整个源跨度；
        // - 折叠 caret 紧贴对象 → **先选块**（本次按键不删）；
        // - 文本选区跨过对象 → 走普通“删选区”（source 区间已包含完整 Markdown）。
        if !self.controller.is_editing_object_source() && !self.controller.is_editing_table_cell() {
            if let Some(line) = self.selected_object_line {
                if self.controller.object_at_line(line).is_some() {
                    self.delete_object_block(line, cx);
                    return;
                }
            }
            if self.controller.selection().is_empty() {
                if let Some(line) = self.adjacent_object_line(step) {
                    self.selected_object_line = Some(line);
                    eprintln!(
                        "[menghunote] 删除（{step:?}）：先选中对象（行 {line}），再按一次删除"
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
            // 两种情况都返回 None：空选区 + 空替换是 no-op；单元格就地编辑期间输入只写
            // 镜像草稿（提交时才产生唯一一条请求）。留痕避免被误读成"丢请求"。
            eprintln!("[menghunote] 本次输入没有产生 EditRequest（no-op 或已写入单元格草稿）");
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
        let applied = matches!(&outcome, EditOutcome::Applied(_));
        match self.controller.apply_outcome(outcome) {
            Ok(_) => {}
            Err(error) => eprintln!("[menghunote] 编辑结局未能应用：{error}"),
        }
        if applied {
            // 正文变了：引用账本按当前正文重建（ADR 0019 §3：普通编辑 / 撤销 / 重做后）。
            // 没有资产链接的笔记在这里直接短路，只有真带链接的笔记才会查/写引用。
            self.reconcile_after_edit();
        }
    }

    /// 正文变更后按当前镜像整篇对账（只在"现在或刚才有 `asset://`"时才真的做）。
    fn reconcile_after_edit(&mut self) {
        let source = self.controller.source_text();
        if !self.had_asset_links && !source.contains("asset://") {
            return;
        }
        self.reconcile_asset_refs_now();
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
        // 单元格选中态同理：正文 caret 动了就不再是“选着某一格”（Enter/F2 的入口
        // 必须跟着可见的高亮走，不能留着一个看不见的目标）。
        self.selected_table_cell = None;
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
        // 单元格就地编辑是“模态”式的（display 只剩这一格）：状态栏必须说明当前在干什么
        // 以及怎么出去，否则用户会以为表格被删掉了。
        let cell = self
            .table_cell_edit_target()
            .map_or(String::new(), |(line, row, column)| {
                format!(" · 单元格编辑（第 {line} 行表 row={row} col={column}）")
            });
        format!(
            "{} · {saved}{cell} · {position} · {rows} 显示行 · {}",
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
    /// 图片放大预览遮罩（点击图片/工具条放大按钮打开；点击任意处关闭）。
    fn render_image_zoom(
        &self,
        image: Arc<gpui::RenderImage>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("image-zoom-overlay")
            .debug_selector(|| "image-zoom-overlay".to_string())
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(0x000000cc))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, _window, cx| {
                if event.keystroke.key == "escape" {
                    view.close_object_zoom(cx);
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
                    view.close_object_zoom(cx);
                }),
            )
            .child(
                div()
                    .id("image-zoom-image")
                    .debug_selector(|| "image-zoom-image".to_string())
                    .max_w(gpui::relative(0.9))
                    .max_h(gpui::relative(0.9))
                    .child(
                        img(ImageSource::Render(image))
                            .object_fit(ObjectFit::Contain)
                            .max_w(gpui::relative(0.9))
                            .max_h(gpui::relative(0.9)),
                    ),
            )
            .into_any_element()
    }

    /// 悬浮工具条（ADR 0019 §1）：图片是「放大 + 编辑源码」，表格只有「编辑源码」。
    ///
    /// 位置来自**上一帧几何**里该对象的矩形（窗口坐标减去元素原点 = 元素局部坐标），
    /// 因此滚动/重排后一帧内自动跟随；对象滚出可见范围时不渲染。
    /// 表格行列拖拽把手（ADR 0019 S3b5）：悬浮/选中时浮在表格边框上的小胶囊。
    ///
    /// debug selector：列把手 `table-drag-column-{n}`、行把手 `table-drag-row-{n}`
    /// （`n` = 列下标 / 数据行下标，0 起、不含表头）；插入指示线 `table-drop-indicator`。
    ///
    /// 每个把手是**元素级** `on_mouse_down`：在窗口级正文监听之前跑，因此按把手
    /// 绝不会先选中单元格 / 落 caret；命中矩形同时写进 `table_handle_rects`，
    /// 让同一次按下的窗口级监听继续放行。
    fn render_table_drag_handles(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.table_handle_rects.clear();
        let Some(geometry) = self.shared.geometry() else {
            return Vec::new();
        };
        let handles = self.table_drag_handles();
        if handles.is_empty() {
            return Vec::new();
        }
        // 元素层用窗口坐标判“这次按下是不是落在把手上”。
        self.table_handle_rects
            .extend(handles.iter().map(|handle| handle.hit));
        let mut elements = Vec::with_capacity(handles.len());
        for handle in handles {
            let local_left = (handle.paint.origin.x - geometry.origin.x).as_f32();
            let local_top = (handle.paint.origin.y - geometry.origin.y).as_f32();
            let width = handle.paint.size.width.as_f32();
            let height = handle.paint.size.height.as_f32();
            let (line, axis, index) = (handle.line, handle.axis, handle.index);
            let axis_name = match axis {
                TableDragAxis::Column => "column",
                TableDragAxis::Row => "row",
            };
            let selector = format!("table-drag-{axis_name}-{index}");
            let element_id = SharedString::from(format!("{selector}-{line}"));
            elements.push(
                div()
                    .id(element_id)
                    .debug_selector(move || selector.clone())
                    .absolute()
                    .left(px(local_left))
                    .top(px(local_top))
                    .w(px(width))
                    .h(px(height))
                    .rounded(px(3.0))
                    .bg(theme::table_drag_handle_bg())
                    .hover(|style| style.bg(theme::table_drag_handle_hover_bg()))
                    // 抓取手势的指针（gpui 原生：命中链变化时自动切换）。
                    .cursor(CursorStyle::OpenHand)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _event: &MouseDownEvent, window, cx| {
                            // 把键盘焦点留在正文：拖拽中按 Esc 取消要靠编辑器的 key down。
                            window.focus(&view.focus_handle, cx);
                            view.begin_table_drag(line, axis, index, cx);
                        }),
                    )
                    .into_any_element(),
            );
        }
        elements
    }

    /// 表格行列拖拽的插入位置指示线（ADR 0019 S3b5；debug selector `table-drop-indicator`）。
    ///
    /// 2px 竖线（列）/ 横线（行），画在**目标缝隙**上；原地不动或拖出表格时不画
    /// （此时释放 = 取消）。
    fn render_table_drop_indicator(&self) -> Option<AnyElement> {
        let geometry = self.shared.geometry()?;
        let rect = self.table_drop_indicator()?;
        let local_left = (rect.origin.x - geometry.origin.x).as_f32();
        let local_top = (rect.origin.y - geometry.origin.y).as_f32();
        Some(
            div()
                .id("table-drop-indicator")
                .debug_selector(|| "table-drop-indicator".to_string())
                .absolute()
                .left(px(local_left))
                .top(px(local_top))
                .w(px(rect.size.width.as_f32()))
                .h(px(rect.size.height.as_f32()))
                .rounded(px(1.0))
                .bg(theme::table_drag_indicator())
                .into_any_element(),
        )
    }

    fn render_object_toolbar(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let line = self.toolbar_target_line()?;
        let geometry = self.shared.geometry()?;
        let object = geometry
            .objects()
            .iter()
            .find(|object| object.line == line)?;
        // 表格没有“放大”（ADR 0019 S3a：工具条只需要编辑源码）。
        let has_zoom = object.kind.is_image();
        let button_count = if has_zoom { 2.0 } else { 1.0 };
        let (viewport_width, _) = self.shared.viewport();
        // 每枚 28px 按钮 + 按钮间 4px gap + 左右各 4px padding。
        let toolbar_width =
            theme::OBJECT_TOOLBAR_BUTTON_WIDTH * button_count + 4.0 * (button_count - 1.0) + 8.0;
        let toolbar_height = theme::OBJECT_TOOLBAR_HEIGHT;
        let target_line = line;
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
                .rounded(px(6.0))
                .text_size(px(15.0))
                .text_color(theme::object_toolbar_text())
                .hover(|style| style.bg(theme::sidebar_selected_bg()))
                .child(SharedString::from(label))
        };

        let zoom = has_zoom.then(|| {
            button("object-toolbar-zoom", "⊕").on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, _event: &MouseDownEvent, _window, cx| {
                    view.open_object_zoom(target_line, cx);
                }),
            )
        });
        let edit = button("object-toolbar-edit", "</>").on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
                view.enter_selected_object_source_edit(cx);
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
                .items_center()
                .h(px(toolbar_height))
                .p(px(4.0))
                .gap(px(4.0))
                .bg(theme::object_toolbar_bg())
                .border(px(1.0))
                .border_color(theme::object_toolbar_border())
                .rounded(px(8.0))
                .overflow_hidden()
                .when_some(zoom, |this, zoom| this.child(zoom))
                .child(edit)
                .into_any_element(),
        )
    }

    /// 表格右下角的「+ 行 / + 列」浮层（ADR 0019 S3b）。
    ///
    /// 位置与可见性（写死，headless 测试钉住）：
    ///
    /// - 只对**已投影的整块表格**出现，且只在悬浮或选中时出现（源码模式不出现）；
    /// - `+ 行` / `+ 列` 恒显示；**删除行 / 删除列只在选着某个单元格时显示**（ADR 0019
    ///   S3b3），避免悬浮时误点删除；选中表头行时不提供「删除行」，只剩一列时「删除列」
    ///   是禁用态（弱化文字、点击不产生请求）；
    /// - 锚在表格**右下角外侧**：浮层顶边在表格底边之下，因此**不遮挡任何单元格文本**；
    ///   表格底边滚出视口时不画（此时“右下角”本来就不在屏幕上）；
    /// - 与悬浮工具条同处表格右上角时，浮层再往下让开工具条，避免两个胶囊叠在一起。
    ///
    /// 点击走 [`Self::insert_table_row`] / [`Self::insert_table_column`] /
    /// [`Self::delete_table_row`] / [`Self::delete_table_column`]（一次点击一个
    /// `EditRequest`，一个撤销组）；`table_actions_rect` 让元素层放行这些点击。
    fn render_table_actions(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let line = self.table_actions_target_line()?;
        let geometry = self.shared.geometry()?;
        let object = geometry
            .objects()
            .iter()
            .find(|object| object.line == line)?;
        // 表格底边不在视口内（长表格被滚掉下半截）→ 不给边缘操作，避免浮层压住可见单元格。
        let (viewport_width, viewport_height) = self.shared.viewport();
        let button_width = theme::TABLE_ACTIONS_BUTTON_WIDTH;
        // 删除按钮只对“选着某个**仍存在**的单元格”显示（陈旧坐标一律不显示删除）。
        let table_dims = self
            .controller
            .object_at_line(line)
            .filter(|object| object.kind().is_table())
            .and_then(|object| self.controller.table_of(object))
            .map(|table| (table.total_rows(), table.columns()));
        let delete_cell = self
            .selected_table_cell
            .filter(|(selected_line, row, column)| {
                *selected_line == line
                    && table_dims.is_some_and(|(rows, columns)| *row < rows && *column < columns)
            });
        // 首版明确不删表头：选中表头格时不提供「删除行」。
        let delete_row_enabled = delete_cell.is_some_and(|(_, row, _)| row >= 1);
        // 至少保留 1 列：只剩一列时按钮禁用（点击只记提示，不产生请求）。
        let delete_column_enabled =
            delete_cell.is_some() && table_dims.is_some_and(|(_, columns)| columns > 1);
        let delete_row_available = delete_cell.is_some() && delete_row_enabled;
        let delete_column_available = delete_cell.is_some();
        let delete_button_width = theme::TABLE_ACTIONS_DELETE_BUTTON_WIDTH;
        // 宽度 = 按钮 + 按钮间 gap + 左右内边距 + 1px 边框（`Bounds` 含边框），与下面 div 的
        // 实际尺寸**逐像素一致**：右边缘才能正好贴住表格右边缘。
        let insert_only_width =
            button_width * 2.0 + 4.0 + theme::TABLE_ACTIONS_PAD * 2.0 + 1.0 * 2.0;
        let delete_button_count =
            usize::from(delete_row_available) + usize::from(delete_column_available);
        let full_width =
            insert_only_width + delete_button_count as f32 * (delete_button_width + 4.0);
        // 窄窗口（编辑区比 4 枚按钮还窄）时隐藏删除组，优先保住 S3b1 契约：
        // 浮层必须整体夹在编辑区内。删除入口在正常宽度下才出现（已知限制，见 ADR）。
        let delete_visible = delete_cell.is_some() && full_width <= viewport_width - theme::PAD_X;
        let width = if delete_visible {
            full_width
        } else {
            insert_only_width
        };
        let height = theme::TABLE_ACTIONS_HEIGHT;
        let table_left = (object.rect.origin.x - geometry.origin.x).as_f32();
        let table_top = (object.rect.origin.y - geometry.origin.y).as_f32();
        let table_bottom = table_top + object.rect.size.height.as_f32();
        let table_right = table_left + object.rect.size.width.as_f32();
        // 先按“表格底边之下”定位，再避开右上角的悬浮工具条；最后夹进视口。
        let toolbar_bottom = table_top + theme::OBJECT_TOOLBAR_GAP + theme::OBJECT_TOOLBAR_HEIGHT;
        let mut local_top = (table_bottom + theme::TABLE_ACTIONS_GAP).max(toolbar_bottom + 4.0);
        if local_top + height > viewport_height - theme::PAD_Y {
            // 底边已经在视口下沿（或屏幕上放不下浮层）：宁可不画，也不压住单元格。
            return None;
        }
        local_top = local_top.max(theme::PAD_Y);
        let local_left = (table_right - width).clamp(
            theme::PAD_X,
            (viewport_width - width - theme::PAD_X).max(theme::PAD_X),
        );
        // 元素层用窗口坐标判“点击是否落在浮层上”，避免同一次点击又挪光标。
        self.table_actions_rect = Some(Bounds::new(
            point(
                geometry.origin.x + px(local_left),
                geometry.origin.y + px(local_top),
            ),
            size(px(width), px(height)),
        ));

        let button = |id: &'static str, label: &'static str, enabled: bool, min_width: f32| {
            let mut element = div()
                .id(id)
                .debug_selector(move || id.to_string())
                .flex()
                .items_center()
                .justify_center()
                .h_full()
                .min_w(px(min_width))
                .px(px(4.0))
                .rounded(px(theme::TABLE_ACTIONS_RADIUS))
                .text_size(px(12.0))
                .text_color(if enabled {
                    theme::table_actions_text()
                } else {
                    // 禁用态（只剩 1 列时的「删除列」）：弱化文字，不给 hover 反馈。
                    theme::table_actions_text_disabled()
                });
            if enabled {
                element = element.hover(|style| style.bg(theme::sidebar_selected_bg()));
            }
            element.child(SharedString::from(label))
        };

        let target_line = line;
        let add_row = button("table-insert-row", "+ 行", true, button_width).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, _event: &MouseDownEvent, window, cx| {
                view.insert_table_row(target_line, Some(window), cx);
            }),
        );
        let add_column = button("table-insert-column", "+ 列", true, button_width).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, _event: &MouseDownEvent, window, cx| {
                view.insert_table_column(target_line, Some(window), cx);
            }),
        );
        let mut bar = div()
            .id("table-actions")
            .debug_selector(|| "table-actions".to_string())
            .absolute()
            .left(px(local_left))
            .top(px(local_top))
            .flex()
            .flex_row()
            .items_center()
            .w(px(width))
            .h(px(height))
            .p(px(theme::TABLE_ACTIONS_PAD))
            .gap(px(4.0))
            .bg(theme::table_actions_bg())
            .border(px(1.0))
            .border_color(theme::table_actions_border())
            .rounded(px(8.0))
            .overflow_hidden()
            .child(add_row)
            .child(add_column);

        // 删除行列只在“选着某个单元格”时出现（悬浮表格不显示，避免误删）；
        // 窄到放不下整条浮层时整组隐藏（见上面的 `delete_visible`）。
        if let Some((_, row, column)) = delete_cell.filter(|_| delete_visible) {
            if delete_row_available {
                let delete_row = button("table-delete-row", "删除行", true, delete_button_width)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _event: &MouseDownEvent, window, cx| {
                            view.delete_table_row(target_line, row, Some(window), cx);
                        }),
                    );
                bar = bar.child(delete_row);
            }
            // 「删除列」永远渲染（让用户知道有这个动作）；只剩 1 列时是禁用态，
            // 点击只记提示、不产生任何请求（控制器再兜一层，不把表格改成非法结构）。
            let column_enabled = delete_column_enabled;
            let delete_column = button(
                "table-delete-column",
                "删除列",
                column_enabled,
                delete_button_width,
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, _event: &MouseDownEvent, window, cx| {
                    if !column_enabled {
                        eprintln!("[menghunote] 表格只剩 1 列：已禁用「删除列」（至少保留 1 列）");
                        return;
                    }
                    view.delete_table_column(target_line, column, Some(window), cx);
                }),
            );
            bar = bar.child(delete_column);
        }

        Some(bar.into_any_element())
    }

    /// 正文右键菜单面板（ADR 0019 S3b4）：「插入」分组 + 「表格」一项。
    ///
    /// 位置 = 右键落点（窗口坐标）按编辑器视口夹取；点菜单不移动正文 caret
    /// （命中矩形写进 `context_menu_rect`，元素层会放行这些点击）。
    fn render_context_menu(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.context_menu?;
        let geometry = self.shared.geometry()?;
        let (viewport_width, viewport_height) = self.shared.viewport();
        let width = theme::EDITOR_CONTEXT_MENU_WIDTH;
        let height = theme::EDITOR_CONTEXT_MENU_HEIGHT;
        let local_left = (menu.position.x - geometry.origin.x).as_f32().clamp(
            theme::PAD_X,
            (viewport_width - width - theme::PAD_X).max(theme::PAD_X),
        );
        let local_top = (menu.position.y - geometry.origin.y).as_f32().clamp(
            theme::PAD_Y,
            (viewport_height - height - theme::PAD_Y).max(theme::PAD_Y),
        );
        self.context_menu_rect = Some(Bounds::new(
            point(
                geometry.origin.x + px(local_left),
                geometry.origin.y + px(local_top),
            ),
            size(px(width), px(height)),
        ));

        let item = div()
            .id("editor-context-insert-table")
            .debug_selector(|| "editor-context-insert-table".to_string())
            .flex()
            .items_center()
            .h(px(24.0))
            .px(px(8.0))
            .rounded(px(4.0))
            .text_size(px(13.0))
            .text_color(theme::object_toolbar_text())
            .hover(|style| style.bg(theme::sidebar_selected_bg()))
            .child(SharedString::from("表格"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _event: &MouseDownEvent, window, cx| {
                    view.insert_table_from_context_menu(Some(window), cx);
                }),
            );

        Some(
            div()
                .id("editor-context-menu")
                .debug_selector(|| "editor-context-menu".to_string())
                .absolute()
                .left(px(local_left))
                .top(px(local_top))
                .flex()
                .flex_col()
                .w(px(width))
                .h(px(height))
                .p(px(4.0))
                .gap(px(2.0))
                .bg(theme::object_toolbar_bg())
                .border(px(1.0))
                .border_color(theme::object_toolbar_border())
                .rounded(px(8.0))
                .overflow_hidden()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(18.0))
                        .px(px(8.0))
                        .text_size(px(11.0))
                        .text_color(theme::table_actions_text_disabled())
                        .child(SharedString::from("插入")),
                )
                .child(item)
                .into_any_element(),
        )
    }
}

/// 把单元格源码文本变成绘制用纯文本（首版不解析行内格式）。
///
/// 只做两件事：保留源码原文（已由解析器去掉首尾空白）+ 把 `\|` 还原成 `|`
/// （转义只是源码层的列边界保护，不应显示成反斜杠）。行内的 `**`/`` ` `` 等标记
/// 首版原样显示（ADR 0019 S3a 明确：单元格不做富文本）。
#[must_use]
pub(crate) fn table_cell_display_text(source: &str, cell: SourceRange) -> String {
    source
        .get(cell.start().get()..cell.end().get())
        .unwrap_or_default()
        .replace("\\|", "|")
}

/// 一个单元格的 shaping 文本：源码原文（去 `\|` 转义）→ 按列宽截断。
///
/// 首版纯文本：单段 `Plain` 样式（表格内不做行内富文本）。空 cell 返回 `None`
/// （不 shaping，也不画字形）。
#[must_use]
pub(crate) fn table_cell_paint(
    window: &Window,
    source: &str,
    cell: Option<SourceRange>,
    inner_width: f32,
) -> Option<gpui::ShapedLine> {
    let text = table_cell_display_text(source, cell?);
    if text.is_empty() {
        return None;
    }
    let spans = [(text.len(), StyleKind::Plain)];
    super::glyph::shape_row_truncated_at(
        window,
        &text,
        &spans,
        theme::TABLE_FONT_SIZE,
        false,
        inner_width,
    )
}

/// 表格列宽：均分内容宽度，但每列不小于主题最小列宽（窄窗口下由内容遮罩裁剪）。
///
/// 抽成**纯函数**便于单测：`f32` 的 NaN/负数/零宽度都必须收敛到最小列宽，
/// 绝不把非有限值带进布局（`Bounds::new` 之后会变成 panic 或不可见矩形）。
#[must_use]
pub(crate) fn table_column_width(content_width: f32, columns: usize) -> f32 {
    let columns = columns.max(1) as f32;
    if !content_width.is_finite() || content_width <= 0.0 {
        return theme::TABLE_MIN_COLUMN_WIDTH;
    }
    (content_width / columns).max(theme::TABLE_MIN_COLUMN_WIDTH)
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
        // 工具条/表格边缘浮层/右键菜单每帧重建：目标对象/几何变化时自动跟随，
        // None 时清空命中矩形。
        self.toolbar_rect = None;
        self.table_actions_rect = None;
        self.context_menu_rect = None;
        self.table_handle_rects.clear();
        let toolbar = self.render_object_toolbar(cx);
        let table_actions = self.render_table_actions(cx);
        let context_menu = self.render_context_menu(cx);
        // 表格行列拖拽把手/插入指示线（ADR 0019 S3b5）：绘制在正文与表格之上、
        // 悬浮工具条与浮层之下（后者更靠近边缘，不能被把手压住）。
        let drag_handles = self.render_table_drag_handles(cx);
        let drop_indicator = self.render_table_drop_indicator();
        let element = EditorElement::new(cx.entity(), Rc::clone(&self.shared));

        div()
            .size_full()
            .relative()
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
                    // 文件拖放（ADR 0019 §2）：GPUI 把平台 CF_HDROP/IDropTarget 转成
                    // `ExternalPaths`；用**当前 caret** 作为插入点，不弹对话框。
                    .on_drop(cx.listener(|view, paths: &ExternalPaths, _window, cx| {
                        view.import_dropped_paths(paths, cx);
                    }))
                    // 正文被按下：通知宿主（它用这个取消侧栏的行内编辑）。
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_view, _event: &MouseDownEvent, _window, cx| {
                            cx.emit(EditorEvent::PointerDown);
                        }),
                    )
                    .child(element)
                    .children(drag_handles)
                    .when_some(drop_indicator, |this, indicator| this.child(indicator))
                    .when_some(toolbar, |this, toolbar| this.child(toolbar))
                    .when_some(table_actions, |this, actions| this.child(actions))
                    .when_some(context_menu, |this, menu| this.child(menu)),
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
            .when_some(self.zoomed_image.clone(), |this, image| {
                this.child(self.render_image_zoom(image, cx))
            })
    }
}

/// 表格拖拽轴的中文名（日志用）。
const fn table_drag_axis_label(axis: TableDragAxis) -> &'static str {
    match axis {
        TableDragAxis::Column => "列重排",
        TableDragAxis::Row => "行重排",
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

/// wiki target 归一化：去掉首尾空白/`.md`/路径首尾 `/`，统一小写。
fn normalize_wiki_target(target: &str) -> String {
    target
        .trim()
        .trim_end_matches(".md")
        .trim_matches('/')
        .to_lowercase()
}

/// 把 source scalar 坐标按一组编辑映射到编辑后的坐标（恢复活动笔记选区用）。
///
/// 编辑按起点升序、互不重叠；落在被替换区间内的位置夹到替换文本内部，
/// 其余位置按前面编辑的净位移平移。
fn transform_scalar(position: usize, edits: &[ScalarEdit]) -> usize {
    let mut delta: i64 = 0;
    for edit in edits {
        let start = edit.start.get();
        let end = edit.end.get();
        if position <= start {
            break;
        }
        let inserted = edit.replacement.chars().count();
        if position <= end {
            let inside = position.saturating_sub(start);
            let mapped = start as i64 + delta + inside.min(inserted) as i64;
            return usize::try_from(mapped).unwrap_or(position);
        }
        delta += inserted as i64 - (end - start) as i64;
    }
    usize::try_from(position as i64 + delta).unwrap_or(position)
}

/// 把 runs 中与 `range` 相交的部分换成 `kind`（保留区间外原样式）。
fn replace_style_range(
    mut runs: Vec<(std::ops::Range<usize>, StyleKind)>,
    range: std::ops::Range<usize>,
    kind: StyleKind,
) -> Vec<(std::ops::Range<usize>, StyleKind)> {
    let mut result = Vec::with_capacity(runs.len() + 2);
    for (run_range, run_kind) in runs.drain(..) {
        if run_range.end <= range.start || range.end <= run_range.start {
            result.push((run_range, run_kind));
            continue;
        }
        let start = run_range.start.max(range.start);
        let end = run_range.end.min(range.end);
        if run_range.start < start {
            result.push((run_range.start..start, run_kind));
        }
        if start < end {
            result.push((start..end, kind));
        }
        if end < run_range.end {
            result.push((end..run_range.end, run_kind));
        }
    }
    result.sort_by_key(|(range, _)| range.start);
    result
}

#[cfg(test)]
mod tests {
    use super::{table_cell_display_text, table_column_width};
    use crate::editor::theme;
    use menghunote_editor::{SourceByte, SourceRange};

    /// 表格列宽：正常均分；NaN / 负数 / 0 列宽收敛到最小列宽；列数为 0 也按 1 列。
    #[test]
    fn table_column_width_is_never_degenerate() {
        assert_eq!(table_column_width(300.0, 3), 100.0);
        assert_eq!(
            table_column_width(90.0, 3),
            theme::TABLE_MIN_COLUMN_WIDTH,
            "窄窗口下不能小于最小列宽"
        );
        assert_eq!(table_column_width(300.0, 0), 300.0, "列数下限是 1");
        assert_eq!(
            table_column_width(f32::NAN, 3),
            theme::TABLE_MIN_COLUMN_WIDTH
        );
        assert_eq!(
            table_column_width(f32::INFINITY, 3),
            theme::TABLE_MIN_COLUMN_WIDTH
        );
        assert_eq!(table_column_width(-10.0, 2), theme::TABLE_MIN_COLUMN_WIDTH);
    }

    /// 单元格显示文本：源码原样 + 只还原 `\|` 转义；行内标记首版原样显示。
    #[test]
    fn table_cell_display_text_keeps_source_and_unescapes_pipes() {
        let source = "| c\\|d | **e** | `f` |";
        let cell = |start: usize, end: usize| {
            SourceRange::new(SourceByte::new(start), SourceByte::new(end), source.len())
                .expect("测试区间必须合法")
        };
        // `c\|d`（字节 2..6）。
        assert_eq!(table_cell_display_text(source, cell(2, 6)), "c|d");
        // `**e**` 首版原样显示（不做富文本）。
        assert_eq!(table_cell_display_text(source, cell(9, 14)), "**e**");
        // 空 cell / 越界区间不 panic。
        assert_eq!(table_cell_display_text(source, cell(5, 5)), "");
        let empty = SourceRange::new(SourceByte::new(0), SourceByte::new(0), source.len())
            .expect("空区间合法");
        assert_eq!(table_cell_display_text(source, empty), "");
    }
}
