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
    Entity, ExternalPaths, FocusHandle, Focusable, ImageSource, KeyDownEvent, MouseButton,
    MouseDownEvent, ObjectFit, Pixels, Point, Render, SharedString, Window,
};
use menghunote_editor::model::frontmatter;
use menghunote_editor::model::markdown::MarkdownDrawing;
use menghunote_editor::{
    BacktickAction, Bias, BlockKind, DisplayByte, EditorController, EditorError, GraphemeStep,
    InputDocument, MarkdownImage, MarkdownLineKind, MarkdownMermaid, MarkdownTableAlignment,
    Selection, SourceByte, SourceRange, StyleKind, TableCellInline, TableCellStyle, UnicodeScalar,
    FENCE_CARET_OFFSET, FENCE_INSERT, PAIR_CARET_OFFSET, PAIR_INSERT, SINGLE_CARET_OFFSET,
    SINGLE_INSERT, TASK_BOX_DISPLAY,
};
use menghunote_storage::rename_links::{plan_wiki_link_renames, scalar_edits_for_renames};
#[cfg(feature = "sync-ui")]
use menghunote_storage::session::{
    AdoptError, AdoptOutcome, AppliedHeads, ExternalCommitLease, ExternalCommitTicket, GateError,
    GateState, PreparedCommittedDelta,
};
use menghunote_storage::session::{
    CloseReport, DriverEvent, EditOutcome, EditRequest, FlushReason, LiveDelta, LiveRev,
    RejectReason, ScalarEdit, SessionDriver,
};
use menghunote_storage::MaterializeOutcome;

use crate::drawing::drawing_asset_key;
use crate::note_properties::NotePropertiesPanel;
use crate::sidebar_rename::{RenameEvent, RenameInput};

use super::clipboard::{
    choose_paste, AssetRefSummary, ImportPayload, ImportReport, ImportSource, ImportedSummary,
    PasteAction, SharedImportWriter, MARKDOWN_METADATA_PREFIX,
};
use super::element::EditorElement;
use super::geometry::{
    ObjectPaint, ObjectPaintContent, ObjectPaintState, TableCellLinkPaint, TableCellPaint,
    TableDragAxis, TableDragHandlePaint, TablePaint, TableRowPaint,
};
use super::layout::LayoutFrame;
use super::link::{self, LinkTarget};
use super::link_settings;
use super::mermaid_preview::{MermaidCacheKey, MermaidPreviewState, MermaidService};
use super::object_image::{parse_asset_hash, ImageService, ImageState, SharedAssetProvider};
use super::shared::EditorShared;
use super::theme;

/// 会话驱动 pump 间隔：只做内存操作，50 ms 足够跟上 250 ms 的 flush 窗口。
const PUMP_INTERVAL: Duration = Duration::from_millis(50);
/// 关闭窗口时的有界排空上限（ADR 0002 §9：`close` 必须有界）。
const CLOSE_TIMEOUT: Duration = Duration::from_millis(1500);
/// 图片放大遮罩的缩放上下限（有界，滚轮再猛也不越界）。
const IMAGE_ZOOM_MIN: f32 = 0.5;
const IMAGE_ZOOM_MAX: f32 = 4.0;

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

/// 正文右键菜单（ADR 0019 S3b4）：正文空白处是「插入 → 表格」，普通链接上是
/// 「打开链接 / 复制链接 / 复制 Markdown」。
///
/// 只存右键**落点**（窗口坐标）与菜单目标：渲染时按编辑器视口夹取，不改变正文
/// caret/选区。插入位置始终是打开菜单时的正文 caret/选区（右键本身不落 caret）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct EditorContextMenu {
    /// 右键落点（窗口坐标；菜单面板左上角的锚点）。
    position: Point<Pixels>,
    /// 菜单内容（普通链接 / 正文插入）。
    target: EditorContextMenuTarget,
}

/// 正文右键菜单的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
enum EditorContextMenuTarget {
    /// 正文空白处：「插入 → 表格」/「插入 → 模板…」（模板插件启用时多一项）。
    Insert,
    /// 普通链接：打开 / 复制链接 / 复制 Markdown。
    Link(ExternalLinkHit),
}

/// 正文右键菜单「插入 → 模板…」多一行时增加的高度（像素）。
///
/// 既有 `theme::EDITOR_CONTEXT_MENU_HEIGHT` 是「表头 + 一行」的高度；模板插件启用时
/// 多一行，菜单面板与命中矩形都要同步变高（位图/几何保持同源）。
const INSERT_TEMPLATE_ROW_HEIGHT: f32 = 26.0;

/// 链接右键菜单的三个动作（与菜单项一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkMenuAction {
    /// 打开链接（与 Ctrl/Cmd+单击同一条路径）。
    Open,
    /// 复制链接目标（`dest` 原文）。
    CopyLink,
    /// 复制完整 Markdown 源码（`[text](dest)`）。
    CopyMarkdown,
}

/// 一次普通链接命中的完整信息（点击打开、右键菜单、复制都从这里取）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalLinkHit {
    /// 解析后的目标（`https://…` / `#标题` / `Note` 等；原样，不改写）。
    pub dest: String,
    /// 完整 Markdown 源码（`[text](dest)`；「复制 Markdown」用）。
    pub markdown: String,
}

/// 外部链接确认框（`file:`/UNC 与未知自定义 scheme）。
///
/// 只活在 app 内（最小浮层），不依赖系统级对话框；`remember` 只影响本机偏好文件，
/// 不写正文、不同步。
#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkConfirm {
    /// 原始目标（打开时原样交给系统；**不写日志**）。
    url: String,
    /// 小写 scheme（确认框标题只显示它，不回显路径）。
    scheme: String,
    /// 未知自定义 scheme 才允许「记住」；`file:`/UNC 每次都要确认。
    rememberable: bool,
    /// 本次是否勾选「记住此 scheme」。
    remember: bool,
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
    /// 右键菜单「插入 → 模板…」：请宿主打开模板选择器（覆盖层/需要窗口聚焦）。
    TemplatePickerRequested,
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

/// 一个表格单元格里的**可见链接**（headless 断言点击命中用）。
///
/// 见 [`EditorView::table_cell_render`]：矩形来自绘制用的 `TableCellLinkPaint`。
#[derive(Debug, Clone, PartialEq)]
pub struct TableCellLinkSnapshot {
    /// 链接可见文本在该单元格 display 文本里的字节区间（已按列宽截断裁剪）。
    pub display: Range<usize>,
    /// 目标 URL。
    pub dest: String,
    /// 点击命中矩形（窗口坐标）。
    pub rect: Bounds<Pixels>,
}

/// 一个表格单元格的**本帧绘制快照**（headless 断言行内 Markdown 渲染用）。
///
/// 见 [`EditorView::table_cell_render`]：数据直接来自绘制用的 `TableCellPaint`。
#[derive(Debug, Clone, PartialEq)]
pub struct TableCellRenderSnapshot {
    /// 实际 shaping 的文本（行内 Markdown 的 display；被列宽截断时含 `…`）。
    pub display: String,
    /// display 字节区间上的语义样式段（与绘制用的 `TextRun` 一一对应；空 cell 为空）。
    ///
    /// 注意：headless 的 `NoopTextSystem` 把所有字体解析成同一个 `FontId`，因此
    /// **不能**用 run 的字体 id 断言样式；这里直接给出绘制时使用的语义段。
    pub spans: Vec<(Range<usize>, StyleKind)>,
    /// 单元格内**可见**链接（截断点之后不出现）。
    pub links: Vec<TableCellLinkSnapshot>,
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

/// 一个 Mermaid Object Block 的渲染状态（headless 测试断言用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MermaidObjectState {
    /// 光标/选区仍在该 block 的源码范围内（或内容为空）：不请求渲染。
    Waiting,
    /// 正在后台渲染/光栅化（或尚未请求）。
    Loading,
    /// 已光栅化，可绘制。
    Ready,
    /// 渲染/光栅化失败（显示「Mermaid 渲染失败」占位，源码回退仍可用）。
    Failed,
}

/// 一个 Mermaid Object Block 的只读快照（headless 测试断言状态/几何用）。
#[derive(Debug, Clone, PartialEq)]
pub struct MermaidSnapshot {
    /// 开围栏行行号（0 起；也是对象行）。
    pub line: usize,
    /// 闭围栏行行号（0 起，含）。
    pub end_line: usize,
    /// 整块源码跨度 `(start, end)`（UTF-8 bytes，不含末尾换行）。
    pub span: (usize, usize),
    /// 围栏之间的内容源码跨度。
    pub content_span: (usize, usize),
    /// info string（`mermaid`）。
    pub info: String,
    /// 渲染状态。
    pub state: MermaidObjectState,
    /// 就绪时的逻辑显示尺寸；未就绪为 `None`。
    pub size: Option<(f32, f32)>,
    /// 当前投影里的 display 区间（`None` = 没有投影成对象，源码可见）。
    pub display: Option<(usize, usize)>,
    /// 是否处于选中态（单击选中 / 选区相交）。
    pub selected: bool,
    /// 是否在显式源码模式。
    pub editing_source: bool,
    /// 源码是否直接可见（源码模式 / 未投影）。
    pub source_visible: bool,
    /// 占位文案（加载中/失败；就绪为 `None`）。
    pub label: Option<String>,
}

/// 一个内嵌画布 Object Block 的只读快照（headless 测试断言状态/几何用）。
///
/// 与 [`MermaidSnapshot`] 同一定位：数据直接来自绘制路径的同源派生状态
/// （合成资产 key → `ImageService`），不复制一套渲染逻辑。
#[derive(Debug, Clone, PartialEq)]
pub struct DrawingSnapshot {
    /// 对象行行号（0 起）。
    pub line: usize,
    /// 整行源码跨度 `(start, end)`（UTF-8 bytes，不含行尾空白/换行）。
    pub span: (usize, usize),
    /// 目标笔记标题（`|` 之前）。
    pub target: String,
    /// 别名（`|` 之后的显示文本；没有别名时 `None`）。
    pub alias: Option<String>,
    /// 预览请求/解码状态（与图片共用 `ImageService`）。
    pub state: ObjectImageState,
    /// 就绪时的逻辑显示尺寸；未就绪为 `None`。
    pub size: Option<(f32, f32)>,
    /// 当前投影里的 display 区间（`None` = 没有投影成对象，源码可见）。
    pub display: Option<(usize, usize)>,
    /// 是否处于选中态（单击选中 / 选区相交）。
    pub selected: bool,
    /// 是否在显式源码模式。
    pub editing_source: bool,
    /// 源码是否直接可见（源码模式 / 未投影）。
    pub source_visible: bool,
    /// 占位文案（加载中/缺失/失败；就绪为 `None`）。
    pub label: Option<String>,
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
    /// 最近一次指针位置（窗口坐标）；刚开始拖拽时等于按下位置。
    pub position: Point<Pixels>,
    /// 按下把手的指针位置（窗口坐标）：ghost 的锚点（偏移 = `position - press`）。
    pub press: Point<Pixels>,
    /// 指针是否已离开按下点超过 [`theme::TABLE_DRAG_GHOST_MIN_MOVE`]：只影响 ghost
    /// 是否出现（单击把手只看选中反馈，不闪 ghost）。
    pub moved: bool,
}

/// 选中的整行/整列（ADR 0019 S3b6：Obsidian 式选中反馈；headless/诊断可读）。
///
/// 与 [`TableDragState`]（一次鼠标手势的生命周期）**分开保存**：
/// - 按下把手即选中（按下本身已经是“选中这一列/行”，不管随后有没有拖动）；
/// - 拖拽释放后选中**移动到新下标**；原地/无效槽位释放保持原下标；
/// - `Esc` / 点表格外 / 点单元格才清除（见 `EditorView::commit_selection` /
///   `cancel_table_drag` / `click_table_cell`）。
///
/// `index` 与把手同一套寻址：列 = 列下标（0 起）；行 = **数据行**下标（0 起、不含表头）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableAxisSelection {
    /// 表格对象行（表头源码行号）。
    pub line: usize,
    /// 选中的轴（列 / 数据行）。
    pub axis: TableDragAxis,
    /// 选中项下标（列 / 数据行，0 起）。
    pub index: usize,
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

/// 表格单元格的**原地编辑浮层**（ADR 0019 S3b2，2026-11-17 交互修正）。
///
/// 与控制器内部的 `begin_table_cell_edit` 不同：浮层**不进** cell 编辑态，表格始终
/// 是 Object 投影（其它行列可见），输入框只覆盖被点的那一格。提交时一次性调用
/// [`EditorController::replace_table_cell_text`] 产生唯一一条 `EditRequest`。
struct CellEditOverlay {
    /// 表格对象行（表头源码行号，0 起）。
    line: usize,
    /// 行下标（0 = 表头）。
    row: usize,
    /// 列下标（0 起）。
    column: usize,
    /// 复用侧栏重命名的单行 IME 输入框（光标在末尾）。
    input: Entity<RenameInput>,
    /// 输入框 → 宿主的提交/取消/跳格事件（Drop = 取消订阅）。
    _events: gpui::Subscription,
    /// 本帧输入框的窗口矩形（`None` = 这一帧没画出来，例如表格滚出视口）。
    bounds: Option<Bounds<Pixels>>,
    /// 最近一次输入框文本（关闭/切换文档时没有 `Window`，不能现场读实体）。
    draft: String,
}

/// 图片放大预览的运行态（`scale` 由滚轮调整，0.5–4.0 有界）。
#[derive(Clone)]
struct ImageZoom {
    /// 已解码图片。
    image: Arc<gpui::RenderImage>,
    /// 显示倍率（相对遮罩可用区域的基准框）。
    scale: f32,
}

pub struct EditorView {
    controller: EditorController,
    driver: SessionDriver,
    focus_handle: FocusHandle,
    shared: Rc<EditorShared>,
    /// 已向下滚过的像素数（非负，0 = 文档顶部）。
    scroll_y: f32,
    /// 笔记开头的属性面板（frontmatter 块）；由宿主创建并注入，面板只弱引用本视图。
    properties: Option<Entity<NotePropertiesPanel>>,
    /// 上下移动时保持的视觉列（相对行首的像素）；水平定位会清空。
    preferred_x: Option<Pixels>,
    /// 拖拽锚点（按下时的 scalar）；`None` = 当前没有拖拽。
    drag_anchor: Option<UnicodeScalar>,
    /// 是否已经做过首帧聚焦（避免每帧重复 `window.focus`）。
    focused_once: bool,
    /// 关闭排空结果（留痕用，只在 stderr 打一行）。
    close_report: Option<CloseReport>,
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
    /// Mermaid 预览：后台渲染/光栅化 + LRU 缓存（ADR 0019 M2）。
    mermaid: MermaidService,
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
    /// `row` 与控制器同一套寻址（0 = 表头）。选中单元格**不移动正文 caret**；
    /// 单击（含双击）即在 cell 矩形上浮出编辑输入框，表格其余行列仍然可见。
    selected_table_cell: Option<(usize, usize, usize)>,
    /// 表格单元格原地编辑浮层（`None` = 不在编辑）。
    cell_edit: Option<CellEditOverlay>,
    /// 下一帧把焦点交给单元格输入框（事件回调里拿不到 `&mut Window`）。
    pending_cell_focus: bool,
    /// 下一帧把焦点还给正文编辑器（提交/取消浮层后）。
    pending_focus_editor: bool,
    /// 本帧悬浮工具条的**窗口坐标**矩形（元素层用它避免把点击误当成正文命中）。
    toolbar_rect: Option<Bounds<Pixels>>,
    /// 本帧表格右下角「加行/加列」浮层的**窗口坐标**矩形（与 `toolbar_rect` 同样
    /// 用于“点击别当正文命中”，也用于 headless 断言浮层不遮挡单元格）。
    table_actions_rect: Option<Bounds<Pixels>>,
    /// 表格行列拖拽（ADR 0019 S3b5）：进行中的一次拖拽；`None` = 没有拖拽。
    table_drag: Option<TableDragState>,
    /// 选中的整行/整列（ADR 0019 S3b6）：与拖拽手势分开保存，点击把手即选中，
    /// 拖拽结束后移动到新下标；`Esc` / 点表格外 / 点单元格清除。
    table_axis_selection: Option<TableAxisSelection>,
    /// 本帧表格拖拽把手的**命中矩形**（窗口坐标）：元素层据此放行把手上的点击，
    /// 避免同一次按下又被当成“选中单元格 / 整表选中 / 落 caret”。
    table_handle_rects: Vec<Bounds<Pixels>>,
    /// 正文右键菜单（ADR 0019 S3b4：「插入 → 表格」/ 链接动作）；`None` = 没有菜单。
    context_menu: Option<EditorContextMenu>,
    /// 本帧右键菜单面板的**窗口坐标**矩形（点击落在这里时元素层不挪正文 caret）。
    context_menu_rect: Option<Bounds<Pixels>>,
    /// 外部链接确认框（`file:`/UNC 与未知自定义 scheme）；`None` = 没有确认框。
    link_confirm: Option<LinkConfirm>,
    /// 非阻塞链接提示（危险 scheme 拒绝 / 未找到标题）；**不回显路径、不写日志**。
    link_notice: Option<String>,
    /// 链接菜单/确认框动作后吞掉同一次手势的 mouse-up（不移动 caret）。
    swallow_mouse_up: bool,
    /// 放大预览的图片（点击图片/放大按钮后显示；`None` = 关闭）。
    zoomed_image: Option<ImageZoom>,
    /// 已被驱动受理的**本地**编辑代数（ADR 0012 S3 自动同步的触发源）。
    ///
    /// 只在 `EditOutcome::Applied` 上自增：远端 adopt 不经过 `submit_request`，
    /// 所以"远端 ACK 不触发自动同步"是结构性的，不依赖调用点自觉。
    #[cfg(feature = "sync-ui")]
    local_edit_seq: u64,
}

impl EditorView {
    /// 组装视图并启动后台 pump 循环。
    pub fn new(
        controller: EditorController,
        driver: SessionDriver,
        _vault_label: String,
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
            properties: None,
            preferred_x: None,
            drag_anchor: None,
            focused_once: false,
            close_report: None,
            composition: Composition::default(),
            request_seq: 0,
            pending_enter_text_input: false,
            dump_selection: false,
            held: false,
            images: ImageService::new(asset_provider),
            // SvgRenderer 只在 worker 线程内构造；这里只传 GPUI 的资产源（bundled fonts）。
            mermaid: MermaidService::new(cx.asset_source().clone()),
            import_writer,
            had_asset_links: false,
            unresolved_wiki_targets: HashSet::new(),
            hovered_object_line: None,
            selected_object_line: None,
            selected_table_cell: None,
            cell_edit: None,
            pending_cell_focus: false,
            pending_focus_editor: false,
            toolbar_rect: None,
            table_actions_rect: None,
            table_drag: None,
            table_axis_selection: None,
            table_handle_rects: Vec::new(),
            context_menu: None,
            context_menu_rect: None,
            link_confirm: None,
            link_notice: None,
            swallow_mouse_up: false,
            zoomed_image: None,
            #[cfg(feature = "sync-ui")]
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
        // Mermaid 后台渲染/光栅化完成：同上（占位 → 图片 / 失败文案）。
        if self.mermaid.take_completed() > 0 {
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

    // ---- 内嵌画布 Object Block（ADR 0025 后续切片）----

    /// 画布预览的合成资产 key（含当前主题 id；provider 负责 scene JSON→PNG）。
    fn drawing_preview_key(&self, drawing: &MarkdownDrawing) -> String {
        drawing_asset_key(drawing.target())
    }

    /// 画布预览的状态（与图片共用 `ImageService`；未请求时是 `Missing`）。
    fn drawing_state(&self, drawing: &MarkdownDrawing) -> ImageState {
        self.images.state(&self.drawing_preview_key(drawing))
    }

    /// 画布对象的显示尺寸（逻辑像素）：就绪按内容宽度等比，否则占位框。
    fn drawing_size_for_state(&self, state: &ImageState, content_width: f32) -> (f32, f32) {
        match state {
            ImageState::Ready(decoded) => {
                super::object_image::fit_size(decoded.width, decoded.height, content_width)
                    .unwrap_or((content_width, theme::OBJECT_PLACEHOLDER_HEIGHT))
            }
            ImageState::Loading | ImageState::Missing | ImageState::Failed(_) => (
                content_width.min(theme::OBJECT_PLACEHOLDER_HEIGHT * 8.0),
                theme::OBJECT_PLACEHOLDER_HEIGHT,
            ),
        }
    }

    /// 画布内容高度（不含上下内边距）。
    fn drawing_content_height(&self, drawing: &MarkdownDrawing, content_width: f32) -> f32 {
        let state = self.drawing_state(drawing);
        self.drawing_size_for_state(&state, content_width).1
    }

    /// 画布对象的**行高**（内容高度 + 上下内边距）。布局度量与绘制共用。
    fn drawing_row_height(&self, drawing: &MarkdownDrawing, content_width: f32) -> f32 {
        self.drawing_content_height(drawing, content_width) + theme::OBJECT_PADDING * 2.0
    }

    /// 画布占位文案：优先 alias（显示文本），否则按状态给固定文案。
    ///
    /// 就绪时返回空串（画的是预览图，不叠文字）。
    fn drawing_placeholder_label(&self, drawing: &MarkdownDrawing, state: ImageState) -> String {
        if let Some(alias) = drawing
            .alias()
            .map(str::trim)
            .filter(|alias| !alias.is_empty())
        {
            let mut text: String = alias.chars().take(80).collect();
            if alias.chars().count() > 80 {
                text.push('…');
            }
            return text;
        }
        match state {
            ImageState::Loading => "画布预览加载中…".to_owned(),
            ImageState::Missing => "画布笔记不存在".to_owned(),
            ImageState::Failed(_) => "画布预览失败".to_owned(),
            ImageState::Ready(_) => String::new(),
        }
    }

    // ---- Mermaid Object Block（ADR 0019 M2）----

    /// 从源码里取 Mermaid 围栏**之间**的内容（渲染输入，不含围栏行）。
    ///
    /// 坐标来自最新解析的 `MarkdownMermaid::content()`；越界时返回空串（渲染器会报
    /// `Empty` → 失败占位），绝不猜偏移。
    fn mermaid_content(&self, mermaid: &MarkdownMermaid) -> String {
        let source = self.controller.source_text();
        source
            .get(mermaid.content().start().get()..mermaid.content().end().get())
            .unwrap_or_default()
            .to_owned()
    }

    /// 当前 source selection 的字节区间（`(start, end)`，折叠时相等）。
    fn selection_source_bytes(&self) -> Option<(usize, usize)> {
        let source = self.controller.source_text();
        let selection = self.controller.selection();
        let start = selection.start().to_source_byte(&source).ok()?.get();
        let end = selection.end().to_source_byte(&source).ok()?.get();
        Some((start, end))
    }

    /// 是否**暂缓**渲染这个 Mermaid block（ADR 0019 M2 收尾）。
    ///
    /// 满足任一条件就等待，不提交渲染请求：
    ///
    /// - 内容为空/纯空白（避免输入 ` ```mermaid ` 后立刻「渲染失败」）；
    /// - 光标/选区仍在该 block 的源码范围内（含两端；非空选区按相交判定）。
    ///
    /// 光标移出后由绘制路径正常 `request`；已有缓存/在途请求不受影响（等待态只是
    /// 视图派生状态，不清理缓存）。
    fn mermaid_render_deferred(&self, mermaid: &MarkdownMermaid, source: &str) -> bool {
        if source.trim().is_empty() {
            return true;
        }
        let Some((start, end)) = self.selection_source_bytes() else {
            return false;
        };
        // 只有**折叠 caret** 在 block 内才等待；非空选区跨过 block 时仍正常渲染
        // （对象保持原子，拖选不应让图变成“等待”占位）。
        if start != end {
            return false;
        }
        let block_start = mermaid.span().start().get();
        let block_end = mermaid.span().end().get();
        // 终点不含：点在块末尾/文档尾时对象保持不变，不突然切成源码。
        block_start <= start && start < block_end
    }

    /// 一个 Mermaid 对象当前应显示的派生状态（Waiting 优先于缓存/在途）。
    fn mermaid_state_for(
        &self,
        mermaid: &MarkdownMermaid,
        content_width: f32,
    ) -> MermaidPreviewState {
        let source = self.mermaid_content(mermaid);
        if self.mermaid_render_deferred(mermaid, &source) {
            return MermaidPreviewState::Waiting;
        }
        self.mermaid
            .state(MermaidCacheKey::new(&source, content_width))
    }

    /// 由渲染状态算显示尺寸（逻辑像素）：就绪按内容宽度等比，其它状态为占位框。
    fn mermaid_size_for_state(
        &self,
        state: &MermaidPreviewState,
        content_width: f32,
    ) -> (f32, f32) {
        match state {
            MermaidPreviewState::Ready(image) => {
                let scale = if image.width > 0.0 {
                    (content_width / image.width).min(1.0)
                } else {
                    1.0
                };
                (
                    (image.width * scale).max(1.0),
                    (image.height * scale).max(1.0),
                )
            }
            MermaidPreviewState::Waiting
            | MermaidPreviewState::Loading
            | MermaidPreviewState::Failed(_) => {
                (content_width.max(1.0), theme::MERMAID_PLACEHOLDER_HEIGHT)
            }
        }
    }

    /// 对象内容宽度（与 prepaint 的 `block_metrics` / `object_paints` 同一个值）。
    fn object_content_width(&self) -> f32 {
        let (viewport_width, _) = self.shared.viewport();
        (viewport_width - theme::PAD_X * 2.0).max(super::layout::MIN_WRAP_WIDTH)
    }

    /// Mermaid 当前的显示尺寸（布局度量与绘制共用）。
    fn mermaid_display_size(&self, mermaid: &MarkdownMermaid, content_width: f32) -> (f32, f32) {
        let state = self.mermaid_state_for(mermaid, content_width);
        self.mermaid_size_for_state(&state, content_width)
    }

    /// Mermaid 内容高度（不含上下内边距）。
    fn mermaid_content_height(&self, mermaid: &MarkdownMermaid, content_width: f32) -> f32 {
        self.mermaid_display_size(mermaid, content_width).1
    }

    /// Mermaid 对象的**行高**（内容高度 + 上下内边距）。布局度量与绘制共用。
    fn mermaid_row_height(&self, mermaid: &MarkdownMermaid, content_width: f32) -> f32 {
        self.mermaid_content_height(mermaid, content_width) + theme::OBJECT_PADDING * 2.0
    }

    /// 占位/失败文案（失败恒为「Mermaid 渲染失败」，加载中为「Mermaid 渲染中…」）。
    fn mermaid_label(state: &MermaidPreviewState) -> Option<String> {
        match state {
            MermaidPreviewState::Ready(_) => None,
            MermaidPreviewState::Waiting => Some("等待光标移出后渲染…".to_owned()),
            MermaidPreviewState::Loading => Some("Mermaid 渲染中…".to_owned()),
            MermaidPreviewState::Failed(_) => Some("Mermaid 渲染失败".to_owned()),
        }
    }

    /// 等 Mermaid 后台渲染收敛（测试/诊断用；有界，生产由 pump + `take_completed` 驱动）。
    pub fn settle_mermaid(&mut self, timeout: Duration, cx: &mut Context<Self>) -> bool {
        let settled = self.mermaid.wait_idle(timeout);
        if settled {
            cx.notify();
        }
        settled
    }

    /// 实际执行过的 Mermaid 渲染次数（诊断/测试：验证同一源码不重复渲染）。
    #[must_use]
    pub fn mermaid_render_count(&self) -> usize {
        self.mermaid.render_count()
    }

    /// 实际提交给后台 worker 的 Mermaid 渲染请求数（诊断/测试：验证等待/缓存命中）。
    #[must_use]
    pub fn mermaid_request_count(&self) -> usize {
        self.mermaid.request_count()
    }

    /// Mermaid 对象的只读快照（headless 测试断言状态/几何用）。
    #[must_use]
    pub fn mermaid_snapshots(&self) -> Vec<MermaidSnapshot> {
        let projection = self.controller.projection();
        let highlighted = self.highlighted_object_lines();
        // 与 prepaint 的 `block_metrics` / `object_paints` 同一个内容宽度（同一 key）。
        let content_width = self.object_content_width();
        self.controller
            .mermaid_blocks()
            .iter()
            .map(|mermaid| {
                // 快照不主动 request，只读等待态/缓存/在途状态。
                let state = self.mermaid_state_for(mermaid, content_width);
                let (state, size, label) = match &state {
                    MermaidPreviewState::Ready(image) => (
                        MermaidObjectState::Ready,
                        Some((image.width, image.height)),
                        None,
                    ),
                    MermaidPreviewState::Waiting => (
                        MermaidObjectState::Waiting,
                        None,
                        Self::mermaid_label(&MermaidPreviewState::Waiting),
                    ),
                    MermaidPreviewState::Loading => (
                        MermaidObjectState::Loading,
                        None,
                        Self::mermaid_label(&MermaidPreviewState::Loading),
                    ),
                    MermaidPreviewState::Failed(_) => (
                        MermaidObjectState::Failed,
                        None,
                        Some("Mermaid 渲染失败".to_owned()),
                    ),
                };
                let display = projection.object_display_range(mermaid.span());
                MermaidSnapshot {
                    line: mermaid.line(),
                    end_line: mermaid.end_line(),
                    span: (mermaid.span().start().get(), mermaid.span().end().get()),
                    content_span: (
                        mermaid.content().start().get(),
                        mermaid.content().end().get(),
                    ),
                    info: mermaid.info().to_owned(),
                    state,
                    size,
                    display: display.as_ref().map(|range| (range.start, range.end)),
                    selected: highlighted.contains(&mermaid.line()),
                    editing_source: self.controller.object_source_edit_line()
                        == Some(mermaid.line()),
                    source_visible: display.is_none(),
                    label,
                }
            })
            .collect()
    }

    /// 内嵌画布对象的只读快照（headless 测试断言状态/占位文案用）。
    ///
    /// 与绘制路径同源：状态来自 `ImageService`（合成 key 含当前主题），尺寸复用
    /// [`Self::drawing_size_for_state`]，占位文案复用
    /// [`Self::drawing_placeholder_label`]。快照**不**主动 request。
    #[must_use]
    pub fn drawing_snapshots(&self) -> Vec<DrawingSnapshot> {
        let projection = self.controller.projection();
        let highlighted = self.highlighted_object_lines();
        let content_width = self.object_content_width();
        self.controller
            .drawings()
            .iter()
            .map(|drawing| {
                let state = self.drawing_state(drawing);
                let snapshot_state = match &state {
                    ImageState::Missing => ObjectImageState::Missing,
                    ImageState::Loading => ObjectImageState::Loading,
                    ImageState::Ready(_) => ObjectImageState::Ready,
                    ImageState::Failed(_) => ObjectImageState::Failed,
                };
                let size = match &state {
                    ImageState::Ready(decoded) => {
                        super::object_image::fit_size(decoded.width, decoded.height, content_width)
                    }
                    _ => None,
                };
                let label = self.drawing_placeholder_label(drawing, state);
                let display = projection.object_display_range(drawing.span());
                DrawingSnapshot {
                    line: drawing.line(),
                    span: (drawing.span().start().get(), drawing.span().end().get()),
                    target: drawing.target().to_owned(),
                    alias: drawing.alias().map(str::to_owned),
                    state: snapshot_state,
                    size,
                    display: display.as_ref().map(|range| (range.start, range.end)),
                    selected: highlighted.contains(&drawing.line()),
                    editing_source: self.controller.object_source_edit_line()
                        == Some(drawing.line()),
                    source_visible: display.is_none(),
                    label: (!label.is_empty()).then_some(label),
                }
            })
            .collect()
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
        // 原 cell 文本必须隐藏，否则浮层输入框会与表格旧数据叠成重影。
        let editing_cell = self
            .cell_edit
            .as_ref()
            .filter(|state| state.line == line)
            .map(|state| (state.row, state.column));
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
                cells.push(table_cell_paint(
                    window,
                    table.header().cell_inline(column),
                    cell_rect,
                    table.alignment(column),
                    inner_width,
                    editing_cell != Some((0, column)),
                ));
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
                cells.push(table_cell_paint(
                    window,
                    row.cell_inline(column),
                    cell_rect,
                    table.alignment(column),
                    inner_width,
                    editing_cell != Some((index + 1, column)),
                ));
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
        if self.controller.is_editing_object_source() || self.is_editing_table_cell() {
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

    /// 本帧要绘制的 Object（图片 / 表格 / Mermaid / 内嵌画布；只含可见行；
    /// 元素层把它们转成窗口坐标）。
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
                        super::glyph::shape_row_styled_at_with_color(
                            window,
                            &text,
                            &spans,
                            theme::OBJECT_LABEL_FONT_SIZE,
                            false,
                            theme::object_label_color(),
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
                menghunote_editor::ObjectKind::Mermaid => {
                    let Some(mermaid) = self.controller.mermaid_of(object) else {
                        continue;
                    };
                    let source = self.mermaid_content(mermaid);
                    // 延迟渲染：光标/选区仍在 block 内、或内容为空时不提交请求；
                    // 等光标移出后，绘制路径才登记 request（每帧幂等，服务侧去重）。
                    let state = if self.mermaid_render_deferred(mermaid, &source) {
                        MermaidPreviewState::Waiting
                    } else {
                        let key = MermaidCacheKey::new(&source, content_width);
                        self.mermaid.request(key, &source);
                        self.mermaid.state(key)
                    };
                    let (display_width, content_height) =
                        self.mermaid_size_for_state(&state, content_width);
                    let rect = Bounds::new(
                        point(px(left), px(row_top + theme::OBJECT_PADDING)),
                        size(px(display_width), px(content_height)),
                    );
                    let (paint_image, paint_state, label) = match &state {
                        MermaidPreviewState::Ready(image) => (
                            Some(Arc::clone(&image.image)),
                            ObjectPaintState::Ready,
                            None,
                        ),
                        MermaidPreviewState::Waiting | MermaidPreviewState::Loading => {
                            (None, ObjectPaintState::Loading, Self::mermaid_label(&state))
                        }
                        MermaidPreviewState::Failed(_) => {
                            (None, ObjectPaintState::Failed, Self::mermaid_label(&state))
                        }
                    };
                    let label = label.filter(|text| !text.is_empty()).map(|text| {
                        let spans = [(text.len(), StyleKind::Plain)];
                        // 失败文案用错误色；加载/等待用占位文案色（深色主题下都可读）。
                        let color = if paint_state == ObjectPaintState::Failed {
                            theme::mermaid_error()
                        } else {
                            theme::mermaid_placeholder_text()
                        };
                        super::glyph::shape_row_styled_at_with_color(
                            window,
                            &text,
                            &spans,
                            theme::OBJECT_LABEL_FONT_SIZE,
                            false,
                            color,
                        )
                    });
                    (
                        rect,
                        ObjectPaintContent::Mermaid(Box::new(super::geometry::ImagePaint {
                            image: paint_image,
                            label,
                            state: paint_state,
                        })),
                    )
                }
                menghunote_editor::ObjectKind::Drawing => {
                    let Some(drawing) = self.controller.drawing_of(object) else {
                        continue;
                    };
                    // 预览不在主线程解析/渲染：这里只读 `ImageService` 的状态，
                    // 请求由 `request_object_images`（render 每帧，幂等）登记。
                    let state = self.drawing_state(drawing);
                    let (display_width, content_height) =
                        self.drawing_size_for_state(&state, content_width);
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
                            Some(self.drawing_placeholder_label(drawing, ImageState::Loading)),
                        ),
                        ImageState::Missing => (
                            None,
                            ObjectPaintState::Missing,
                            Some(self.drawing_placeholder_label(drawing, ImageState::Missing)),
                        ),
                        ImageState::Failed(error) => (
                            None,
                            ObjectPaintState::Failed,
                            Some(
                                self.drawing_placeholder_label(drawing, ImageState::Failed(error)),
                            ),
                        ),
                    };
                    let label = label.filter(|text| !text.is_empty()).map(|text| {
                        let spans = [(text.len(), StyleKind::Plain)];
                        super::glyph::shape_row_styled_at_with_color(
                            window,
                            &text,
                            &spans,
                            theme::OBJECT_LABEL_FONT_SIZE,
                            false,
                            theme::object_label_color(),
                        )
                    });
                    // 与图片共用绘制载荷：都是“一张位图 + 可选占位文案”。
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

    /// 请求加载当前文档里所有整行独占图片 + 内嵌画布的预览（render 每帧调用，幂等）。
    ///
    /// 画布用**合成资产 key**（[`drawing_asset_key`]）向同一个 `ImageService` 登记；
    /// 真正的 scene JSON→PNG 由宿主注入的 provider 在后台线程完成，主线程只读状态。
    fn request_object_images(&self) {
        for image in self.controller.block_images() {
            if let Some(hash) = parse_asset_hash(image.dest()) {
                self.images.request(hash);
            }
        }
        for object in self.controller.objects() {
            if !object.kind().is_drawing() {
                continue;
            }
            let Some(drawing) = self.controller.drawing_of(object) else {
                continue;
            };
            self.images.request(&self.drawing_preview_key(drawing));
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

    /// 当前可见 Object（图片/表格/Mermaid/画布）的**窗口坐标**矩形
    /// （`(对象行, 矩形)`，诊断/headless 测试用）。
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
        if self.is_editing_table_cell() {
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
                        .cell_edit
                        .as_ref()
                        .filter(|state| state.line == table.line())
                        .map(|state| (state.row, state.column)),
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
        // 内嵌画布（ADR 0025 后续切片）：点击预览 = **打开目标画布笔记**。
        // 不移动 caret、不进源码模式、不选中对象（选中态只属于可原地绘制的对象）；
        // 宿主已有的 wiki 打开路径会按标题/路径找到目标画布。
        if object.kind().is_drawing() {
            if let Some(drawing) = self.controller.drawing_of(&object) {
                let target = drawing.target().to_owned();
                cx.emit(EditorEvent::WikiLinkRequested {
                    target,
                    heading: None,
                });
                eprintln!(
                    "[menghunote] 点击画布预览（行 {line}）→ 打开目标画布笔记（不改正文/caret）"
                );
            }
            let had_selection = self.selected_object_line.take().is_some()
                | self.selected_table_cell.take().is_some();
            if had_selection {
                cx.notify();
            }
            return;
        }
        let already_selected = self.selected_object_line == Some(line);
        let text = self.controller.source_text();
        let mut caret_byte = object.span().end().get();
        // Mermaid：caret 在 block 源码内会触发“显示源码、不投影 Object”。单击对象时
        // 把 caret 放到块末尾换行**之后**（即块外），块保持 Object/工具条；块在文档尾
        // 且没有换行时只能落在块尾（此时会显示源码，属边界情况）。
        if object.kind().is_mermaid() && text.as_bytes().get(caret_byte) == Some(&b'\n') {
            caret_byte += 1;
        }
        let Ok(caret) = SourceByte::new(caret_byte).to_scalar(&text) else {
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

    // ---- 表格单元格原地编辑（ADR 0019 S3b2；2026-11-17 改为浮层）----

    /// 是否正在原地编辑表格单元格（浮层输入框存在）。
    ///
    /// 编辑期间表格**仍然是 Object**（其它行列照常绘制），与控制器内部的
    /// `begin_table_cell_edit`（会把 display 压成一行 cell 文本）无关。
    #[must_use]
    pub fn is_editing_table_cell(&self) -> bool {
        self.cell_edit.is_some()
    }

    /// 正在编辑的单元格 `(表格对象行, row, column)`（row 0 = 表头）。
    #[must_use]
    pub fn table_cell_edit_target(&self) -> Option<(usize, usize, usize)> {
        self.cell_edit
            .as_ref()
            .map(|state| (state.line, state.row, state.column))
    }

    /// 单元格输入框当前是否持有键盘焦点（headless 诊断用）。
    #[must_use]
    pub fn table_cell_edit_input_focused(&self, window: &Window, cx: &App) -> bool {
        let Some(state) = self.cell_edit.as_ref() else {
            return false;
        };
        let handle = state.input.read(cx).focus_handle(cx);
        window.focused(cx) == Some(handle)
    }

    /// 本帧编辑输入框的**窗口坐标**矩形（`None` = 不在编辑 / 这一帧没画出来）。
    ///
    /// 输入框矩形就是被点 cell 的矩形（`table_cell_rects()` 同一份几何），
    /// headless 用它断言“输入框只覆盖该 cell、没有遮挡整表”。
    #[must_use]
    pub fn table_cell_edit_input_bounds(&self) -> Option<Bounds<Pixels>> {
        self.cell_edit.as_ref().and_then(|state| state.bounds)
    }

    /// 单击选中的单元格 `(表格对象行, row, column)`（不移动正文 caret）。
    #[must_use]
    pub fn selected_table_cell(&self) -> Option<(usize, usize, usize)> {
        self.selected_table_cell
    }
    /// 清空单元格选中态（点击表格以外的区域、表格结构变化时用）。
    pub(crate) fn clear_table_cell_selection(&mut self, cx: &mut Context<Self>) {
        if self.selected_table_cell.take().is_some() {
            cx.notify();
        }
    }

    /// 本帧所有可见表格单元格的窗口矩形 `(对象行, row, column, 矩形)`。
    ///
    /// 几何来自 `table_paint` 的 `TableCellPaint.rect`（绘制/命中/测试同源），
    /// 长表格按视口裁剪时只有可见行。原地编辑**不会**改变表格投影，所以编辑期间
    /// 这里仍然返回完整网格（输入框就浮在其中一个矩形上）。
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

    /// 某个单元格的**本帧绘制快照**（headless 断言行内 Markdown 渲染用）。
    ///
    /// 全部数据来自 `table_paint` 的 `TableCellPaint`（**绘制同源**，不是重新解析）：
    /// - `display`：实际 shaping 的文本（标记已隐藏；被列宽截断时含 `…`）；
    /// - `spans`：绘制时使用的语义样式段（粗体/斜体/删除线/行内代码/链接）；
    /// - `links`：单元格内**可见**链接的 display 区间 + 目标（截断点之后不出现）。
    ///
    /// 不可见（长表格被视口裁剪 / 该格正在就地编辑）返回 `None`。
    #[must_use]
    pub fn table_cell_render(
        &self,
        line: usize,
        row: usize,
        column: usize,
    ) -> Option<TableCellRenderSnapshot> {
        let geometry = self.shared.geometry()?;
        let object = geometry
            .objects()
            .iter()
            .find(|object| object.line == line)?;
        let table = object.table()?;
        let paint_row = table
            .header
            .iter()
            .chain(table.rows.iter())
            .find(|paint_row| paint_row.row == row)?;
        let cell = paint_row.cells.get(column)?;
        let display = cell
            .text
            .as_ref()
            .map(|text| text.text.to_string())
            .unwrap_or_default();
        // 样式段来自与绘制**同一份**解析结果（`table_cell_paint` 直接把它映射成
        // `TextRun`）；该格没画文字（正在就地编辑 / 空 cell）时没有样式段。
        let spans = if cell.text.is_some() {
            self.controller
                .object_at_line(line)
                .filter(|object| object.kind().is_table())
                .and_then(|object| self.controller.table_of(object))
                .and_then(|table| table.cell_inline(row, column))
                .map(|inline| {
                    inline
                        .spans()
                        .iter()
                        .map(|span| {
                            (
                                span.display.start..span.display.end,
                                table_cell_style_kind(span.kind),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Some(TableCellRenderSnapshot {
            display,
            spans,
            links: cell
                .links
                .iter()
                .map(|link| TableCellLinkSnapshot {
                    display: link.display.clone(),
                    dest: link.dest.clone(),
                    rect: link.rect,
                })
                .collect(),
        })
    }

    /// 正在就地编辑的单元格**草稿文本**（`None` = 不在单元格编辑态）。
    ///
    /// 草稿是 cell 的**源码原文**（`**粗体**` 原样显示，只还原 `\|` 转义），
    /// 提交时才经 `sanitize_cell_text` 写回正文。
    #[must_use]
    pub fn table_cell_edit_draft(&self) -> Option<&str> {
        self.cell_edit.as_ref().map(|state| state.draft.as_str())
    }

    /// 某个单元格的**可编辑显示文本**（源码去掉 `\|` 转义；空 cell = 空串）。
    ///
    /// 缺列补出来的空区间（cell 起点在行内容末尾之外）返回 `None`：往那里插字会写坏
    /// 表格结构，所以单击只选中、不进入编辑。
    fn editable_cell_text(&self, line: usize, row: usize, column: usize) -> Option<String> {
        let object = self
            .controller
            .object_at_line(line)
            .filter(|object| object.kind().is_table())?;
        let table = self.controller.table_of(object)?;
        let cell = table.cell(row, column)?;
        let row_span = table.row_span(row)?;
        if cell.start() >= row_span.end() {
            return None;
        }
        let source = self.controller.source_text();
        Some(table_cell_display_text(&source, cell))
    }

    /// 单击表格单元格：**直接进入原地编辑**（双击同义）。
    ///
    /// 表格仍是 Object（其余行列可见），输入框只覆盖该 cell 矩形；单击**不移动正文
    /// caret**。点击表格的非单元格区域（内边距/边框）仍然走整块选中
    /// （[`Self::select_object_at`]）。
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
        self.selected_object_line = Some(line);
        self.selected_table_cell = Some((line, row, column));
        // 单元格点击优先于整行/整列选中：先退出轴选中，再进入该格编辑（两者不叠加）。
        self.table_axis_selection = None;
        self.preferred_x = None;
        eprintln!(
            "[menghunote] 单击表格单元格（行 {line} 的 row={row} col={column}，click_count={click_count}）：原地编辑，表格仍为 Object"
        );
        self.begin_table_cell_edit(line, row, column, window, cx);
        cx.notify();
        true
    }

    /// 进入单元格原地编辑：在 cell 矩形上创建输入框（**不产生** `EditRequest`）。
    ///
    /// 输入框复用侧栏重命名的 [`RenameInput`]（IME / 选区 / 光标同一套实现），
    /// 光标在 cell 文本末尾；表格保持 Object 投影，整表数据仍可见。
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
        // 已在同一格编辑：只把焦点交回输入框（不重建，避免丢弃 IME 组合）。
        if let Some(state) = self.cell_edit.as_ref() {
            if (state.line, state.row, state.column) == (line, row, column) {
                let input = state.input.clone();
                if let Some(window) = window {
                    input.update(cx, |input, cx| input.focus(window, cx));
                } else {
                    self.pending_cell_focus = true;
                }
                self.pending_focus_editor = false;
                return true;
            }
            // 从另一格切过来：先提交当前格（一次请求），再开新格。
            self.commit_table_cell_edit(None, cx);
        }
        let Some(text) = self.editable_cell_text(line, row, column) else {
            eprintln!(
                "[menghunote] 单元格不可编辑（行 {line} 的 {row},{column}：缺列补出的空区间或已失效）"
            );
            return false;
        };
        let input = cx.new(|cx| RenameInput::new_cell_at_end(&text, cx));
        let events = cx.subscribe(&input, |view, _input, event: &RenameEvent, cx| {
            view.on_cell_input_event(event.clone(), cx);
        });
        self.cell_edit = Some(CellEditOverlay {
            line,
            row,
            column,
            input: input.clone(),
            _events: events,
            bounds: None,
            draft: text.clone(),
        });
        self.selected_object_line = Some(line);
        self.selected_table_cell = Some((line, row, column));
        // 进入单元格编辑即退出整行/整列选中（Enter/直接调用路径也不叠加）。
        self.table_axis_selection = None;
        self.preferred_x = None;
        self.pending_focus_editor = false;
        if let Some(window) = window {
            self.pending_cell_focus = false;
            input.update(cx, |input, cx| input.focus(window, cx));
            // 正文 surface 自己也 `track_focus`：它的自动聚焦监听在祖先层、晚于本监听
            // 执行，会把焦点抢回 EditorView。这里用 prevent_default 抑制那次自动聚焦
            // （只影响这一次鼠标按下，不改变输入法/文本事件）。
            window.prevent_default();
        } else {
            self.pending_cell_focus = true;
        }
        eprintln!(
            "[menghunote] 进入单元格原地编辑（行 {line} 的 row={row} col={column}）：表格仍为 Object，输入框只覆盖该 cell"
        );
        cx.notify();
        true
    }

    /// 输入框事件：Enter 提交 / Tab 跳格 / Esc 取消（事件回调没有 `&mut Window`）。
    fn on_cell_input_event(&mut self, event: RenameEvent, cx: &mut Context<Self>) {
        match event {
            RenameEvent::Commit(text) => {
                self.commit_table_cell_edit_with(text, cx);
            }
            RenameEvent::Advance { forward, text } => {
                self.advance_table_cell_edit_from_input(forward, text, cx);
            }
            RenameEvent::Cancel => {
                self.cancel_table_cell_edit(cx);
            }
            // Cell 模式不会发 Blank（空串是合法提交）。
            RenameEvent::Blank => {}
            RenameEvent::Changed => {
                let text = self
                    .cell_edit
                    .as_ref()
                    .map(|state| state.input.read(cx).text().to_owned());
                if let (Some(state), Some(text)) = (self.cell_edit.as_mut(), text) {
                    state.draft = text;
                }
            }
        }
    }

    /// 提交当前单元格编辑：读取输入框文本 → 控制器唯一一条 `EditRequest`。
    pub fn commit_table_cell_edit(
        &mut self,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(text) = self
            .cell_edit
            .as_ref()
            .map(|state| state.input.read(cx).text().to_owned())
        else {
            return false;
        };
        let committed = self.commit_table_cell_edit_with(text, cx);
        if let Some(window) = window {
            window.focus(&self.focus_handle, cx);
            self.pending_focus_editor = false;
        }
        committed
    }

    /// 用给定文本提交当前格（输入框事件与点击别处共用）。
    ///
    /// 只走 [`EditorController::replace_table_cell_text`]：一次调用一条 `ScalarEdit`
    /// （一个撤销组）；文本没变时返回 `None`、不产生请求。
    fn commit_table_cell_edit_with(&mut self, text: String, cx: &mut Context<Self>) -> bool {
        let Some((line, row, column)) = self.table_cell_edit_target() else {
            return false;
        };
        self.cell_edit = None;
        self.pending_cell_focus = false;
        self.pending_focus_editor = true;
        if self.frozen_by_external_commit() {
            eprintln!("[menghunote] 同步持闸中：放弃未提交的单元格编辑");
            cx.notify();
            return false;
        }
        match self
            .controller
            .replace_table_cell_text(line, row, column, &text)
        {
            Ok(request) => {
                let edits = request.as_ref().map(|request| request.edits.len());
                let command_id = request
                    .as_ref()
                    .map(|request| request.command_id.clone())
                    .unwrap_or_default();
                let accepted = self.submit_request(request).is_some();
                eprintln!(
                    "[menghunote] 提交单元格编辑（行 {line} 的 {row},{column}）edits={edits:?} command_id={command_id} accepted={accepted}"
                );
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

    /// `Esc`：取消当前单元格编辑（**不产生请求**；没人写过正文，丢草稿即可）。
    pub fn cancel_table_cell_edit(&mut self, cx: &mut Context<Self>) -> bool {
        if self.cell_edit.take().is_some() {
            self.pending_cell_focus = false;
            self.pending_focus_editor = true;
            eprintln!("[menghunote] 取消单元格编辑：未提交的文本已丢弃（正文未改动）");
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
        let Some(text) = self
            .cell_edit
            .as_ref()
            .map(|state| state.input.read(cx).text().to_owned())
        else {
            return false;
        };
        self.advance_table_cell_edit_from_input(forward, text, cx);
        if let (Some(window), Some(state)) = (window, self.cell_edit.as_ref()) {
            let input = state.input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
            self.pending_cell_focus = false;
            self.pending_focus_editor = false;
        }
        true
    }

    /// Tab/Shift+Tab 的实现（事件回调没有窗口）：先提交，再按最新表格算下一格。
    ///
    /// `text` 是**按键当时**的输入框文本：平台随后补发的 `\t` 文本输入会落到
    /// 已经关闭的输入框上，不能算进这次提交。
    fn advance_table_cell_edit_from_input(
        &mut self,
        forward: bool,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let Some((line, row, column)) = self.table_cell_edit_target() else {
            return;
        };
        // 先提交（唯一一条请求；没改动就是 `None`）。
        self.commit_table_cell_edit_with(text, cx);
        // 用**最新解析**的表格算下一格（加行/加列后的新格子也能到达）。
        let next = self
            .controller
            .object_at_line(line)
            .filter(|object| object.kind().is_table())
            .and_then(|object| self.controller.table_of(object))
            .and_then(|table| {
                if forward {
                    menghunote_editor::next_cell(row, column, table.row_count(), table.columns())
                } else {
                    menghunote_editor::previous_cell(
                        row,
                        column,
                        table.row_count(),
                        table.columns(),
                    )
                }
            });
        match next {
            Some(next) => {
                let entered = self.begin_table_cell_edit(line, next.row, next.column, None, cx);
                eprintln!(
                    "[menghunote] Tab 提交单元格（行 {line}）→ 下一格 row={} col={} entered={entered}",
                    next.row, next.column
                );
            }
            None => {
                // 表头之前 / 末格之后：退出编辑，caret 在整表末尾（控制器已处理）。
                self.selected_table_cell = None;
                eprintln!("[menghunote] Tab：已到表格边界，退出单元格编辑");
                cx.notify();
            }
        }
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
        if self.is_editing_table_cell() {
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
        if self.is_editing_table_cell() {
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

    /// 当前选中的整行/整列（`None` = 没有轴选中；headless/诊断用）。
    ///
    /// 这是**状态真值**：即使表格被滚出视口、投影失效，也照实返回（绘制侧会
    /// 通过 [`Self::table_axis_selection_rect`] 的几何校验决定画不画）。
    #[must_use]
    pub fn table_axis_selection(&self) -> Option<TableAxisSelection> {
        self.table_axis_selection
    }

    /// 选中行/列的**窗口坐标**矩形（绘制与 headless 断言**同源**）。
    ///
    /// 几何来自本帧 `TablePaint`（`axis_rect`：固定列宽/行高），不另算一套坐标；
    /// 结果裁剪到编辑器可见区（浮层 div 不会被宿主布局裁剪，必须自己裁）。
    /// 单元格原地编辑 / 表格源码模式 / 表格不可见 / 下标越界 → `None`（不画）。
    #[must_use]
    pub fn table_axis_selection_rect(&self) -> Option<Bounds<Pixels>> {
        let selection = self.table_axis_selection?;
        if self.is_editing_table_cell() || self.controller.is_editing_object_source() {
            return None;
        }
        let geometry = self.shared.geometry()?;
        let object = geometry
            .objects()
            .iter()
            .find(|object| object.line == selection.line)?;
        let table = object.table()?;
        let rect = table.axis_rect(selection.axis, selection.index)?;
        self.clip_to_editor_viewport(&geometry, rect)
    }

    /// 拖动中的整行/整列 ghost 矩形（窗口坐标）；未移动 / 拖出表格 → `None`。
    ///
    /// ghost = 被拖项（整列 / 整条数据行）的矩形按 `position - press` 平移，
    /// 再裁剪到编辑器可见区：跟随指针、但不越出编辑区压到宿主 UI。
    #[must_use]
    pub fn table_drag_ghost_rect(&self) -> Option<Bounds<Pixels>> {
        let drag = self.table_drag?;
        if !drag.moved || drag.slot.is_none() {
            return None;
        }
        let geometry = self.shared.geometry()?;
        let object = geometry
            .objects()
            .iter()
            .find(|object| object.line == drag.line)?;
        let table = object.table()?;
        // Obsidian 式小把手：不复制整行/整列，只在表头顶线 / 表格左线上画一个小块：
        // - 列块：y 恒贴表头顶边、x 跟指针（水平拖动）；
        // - 行块：x 恒贴表格左边、y 跟指针（垂直拖动）。
        // 这样既省绘制，也不会在拖到视口外时整块消失。
        let table_left = table.rect.origin.x.as_f32();
        let table_top = table.rect.origin.y.as_f32();
        let table_right = table_left + table.rect.size.width.as_f32();
        let table_bottom = table_top + table.rect.size.height.as_f32();
        let ghost = match drag.axis {
            TableDragAxis::Column => {
                let width = theme::TABLE_DRAG_GHOST_COLUMN_WIDTH;
                let height = theme::TABLE_DRAG_GHOST_COLUMN_HEIGHT;
                let x = (drag.position.x.as_f32() - width / 2.0)
                    .clamp(table_left, (table_right - width).max(table_left));
                Bounds::new(point(px(x), px(table_top)), size(px(width), px(height)))
            }
            TableDragAxis::Row => {
                let width = theme::TABLE_DRAG_GHOST_ROW_WIDTH;
                let height = theme::TABLE_DRAG_GHOST_ROW_HEIGHT;
                let y = (drag.position.y.as_f32() - height / 2.0).clamp(
                    table_top + table.header_height,
                    (table_bottom - height).max(table_top + table.header_height),
                );
                Bounds::new(point(px(table_left), px(y)), size(px(width), px(height)))
            }
        };
        self.clip_to_editor_viewport(&geometry, ghost)
    }

    /// ghost 浮层文案（“移动第 N 列/行 → 第 M 列/行”）；未移动 / 拖出表格 → `None`。
    ///
    /// 与 [`Self::table_drag_ghost_rect`] 同一套可见性条件；`target` 与插入指示线
    /// （`table_drop_indicator`）来自同一个 [`TableDragState`]，三者永远同步。
    #[must_use]
    pub fn table_drag_ghost_label(&self) -> Option<String> {
        let drag = self.table_drag?;
        if !drag.moved || drag.slot.is_none() {
            return None;
        }
        let unit = table_axis_unit(drag.axis);
        let from = drag.from + 1;
        Some(match drag.target {
            Some(to) if to + 1 != from => format!("移动第 {from} {unit} → 第 {} {unit}", to + 1),
            _ => format!("移动第 {from} {unit}"),
        })
    }

    /// 把浮层矩形裁剪到编辑器可见区；裁剪后为空 → `None`。
    fn clip_to_editor_viewport(
        &self,
        geometry: &super::geometry::EditorGeometry,
        rect: Bounds<Pixels>,
    ) -> Option<Bounds<Pixels>> {
        let (width, height) = self.shared.viewport();
        let viewport = Bounds::new(geometry.origin, size(px(width), px(height)));
        let clipped = rect.intersect(&viewport);
        (clipped.size.width.as_f32() > 0.0 && clipped.size.height.as_f32() > 0.0).then_some(clipped)
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

    /// 按下列手：**选中**这一列/行，并开始一次表格行列拖拽（ADR 0019 S3b6）。
    ///
    /// 选中反馈与拖拽是两件事：按下即选中（`table_axis_selection`），但 ghost 只在
    /// 指针移动超过 [`theme::TABLE_DRAG_GHOST_MIN_MOVE`] 后出现 —— 单击把手只看选中，
    /// 拖动才进入“预览 + 落点”。**不产生任何编辑**（真正的重排在释放时一次提交）；
    /// 只记下“拖的是哪张表的哪一项”。
    /// 持闸 / IME 组合 / 单元格就地编辑 / 表格源码模式 → 拒绝（不进入拖拽）。
    pub fn begin_table_drag(
        &mut self,
        line: usize,
        axis: TableDragAxis,
        index: usize,
        position: Point<Pixels>,
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
        if self.controller.is_editing_object_source() || self.is_editing_table_cell() {
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
        self.table_axis_selection = Some(TableAxisSelection { line, axis, index });
        self.table_drag = Some(TableDragState {
            line,
            axis,
            from: index,
            slot: None,
            target: None,
            position,
            press: position,
            moved: false,
        });
        eprintln!(
            "[menghunote] 选中第 {} {}（行 {line}）并开始拖拽：拖到目标位置释放，Esc/拖出表格取消",
            index + 1,
            table_axis_unit(axis)
        );
        cx.notify();
        true
    }

    /// 拖动中：按指针位置更新插入槽位（并重绘指示线/ghost）。**不产生任何编辑**。
    ///
    /// ghost 只在指针离开按下点超过 [`theme::TABLE_DRAG_GHOST_MIN_MOVE`] 后出现
    /// （`moved` 一旦为真就保持到手势结束），因此“点一下把手”不会闪 ghost。
    pub(crate) fn update_table_drag(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(mut drag) = self.table_drag else {
            return;
        };
        let slot = self.table_drag_slot(drag.line, drag.axis, position);
        let count = self.table_drag_count(drag.line, drag.axis);
        let target = slot
            .zip(count)
            .and_then(|(slot, count)| menghunote_editor::drop_target(drag.from, slot, count));
        let dx = position.x.as_f32() - drag.press.x.as_f32();
        let dy = position.y.as_f32() - drag.press.y.as_f32();
        let moved = drag.moved || (dx * dx + dy * dy).sqrt() >= theme::TABLE_DRAG_GHOST_MIN_MOVE;
        let changed = drag.slot != slot || drag.target != target || drag.moved != moved;
        drag.slot = slot;
        drag.target = target;
        drag.position = position;
        drag.moved = moved;
        self.table_drag = Some(drag);
        if changed {
            cx.notify();
        }
    }

    /// 释放：槽位有效 → **一次** `EditRequest` 重排（一次 undo）；否则取消、零请求。
    ///
    /// 重排被接受后，整行/整列选中**跟到新下标**（与插入指示线同一口径）；原地/
    /// 无效槽位释放保持原选中（只是结束手势，不产生请求）。
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
        let accepted = self.move_table_structure(drag.axis, drag.line, drag.from, to, window, cx);
        if accepted {
            self.table_axis_selection = Some(TableAxisSelection {
                line: drag.line,
                axis: drag.axis,
                index: to,
            });
            eprintln!(
                "[menghunote] 整{}选中跟到新位置：第 {} {}（行 {line}）",
                table_axis_unit(drag.axis),
                to + 1,
                table_axis_unit(drag.axis),
                line = drag.line
            );
        }
        accepted
    }

    /// 取消拖拽（`Esc` / 拖出表格 / 拖拽中按了别的键 / 丢失抬起的陈旧手势）：
    /// 丢弃状态，**不产生任何请求**。
    ///
    /// 取消同时清除整行/整列选中：`Esc` 的用户语义就是“退出这次选中/拖拽”，
    /// 而陈旧手势路径意味着用户已经在别处按下了鼠标（等同点表格外）。
    pub fn cancel_table_drag(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(drag) = self.table_drag.take() else {
            return false;
        };
        let had_selection = self.table_axis_selection.take().is_some();
        eprintln!(
            "[menghunote] 取消表格{}拖拽（行 {line} 的 #{from}）：未产生任何请求（清除轴选中={had_selection}）",
            table_drag_axis_label(drag.axis),
            line = drag.line,
            from = drag.from
        );
        cx.notify();
        true
    }

    /// 清除整行/整列选中（`Esc` / 正文 caret 变动 / 点单元格）。**不产生任何请求**。
    pub(crate) fn clear_table_axis_selection(&mut self, cx: &mut Context<Self>) {
        if self.table_axis_selection.take().is_some() {
            eprintln!("[menghunote] 清除整行/整列选中（表格仍是 Object）");
            cx.notify();
        }
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
        if self.controller.is_editing_object_source() || self.is_editing_table_cell() {
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

    // ---- 正文右键菜单（ADR 0019 S3b4：插入表格 / 链接动作）----

    /// 在右键落点打开菜单；**不移动正文 caret/选区**。
    ///
    /// - 普通链接上 → 「打开链接 / 复制链接 / 复制 Markdown」；
    /// - 其它位置 → 原有的「插入 → 表格」；
    /// - 右键只决定菜单面板的位置，插入位置始终是打开菜单时编辑器里的 caret/选区；
    /// - 单元格就地编辑 / 外部链接确认框期间不开菜单（模态浮层优先）；
    /// - 已在别处打开时直接挪到新落点（同一个菜单，不叠两个面板）。
    pub(crate) fn open_context_menu_at(
        &mut self,
        position: Point<Pixels>,
        link: Option<ExternalLinkHit>,
        cx: &mut Context<Self>,
    ) {
        self.pending_enter_text_input = false;
        if self.link_confirm.is_some() {
            return;
        }
        if self.is_editing_table_cell() {
            eprintln!("[menghunote] 单元格编辑中：不打开正文右键菜单（先提交该格）");
            return;
        }
        let target = match link {
            Some(hit) => EditorContextMenuTarget::Link(hit),
            None => EditorContextMenuTarget::Insert,
        };
        let label = match &target {
            EditorContextMenuTarget::Insert => "插入 → 表格/模板",
            EditorContextMenuTarget::Link(_) => "链接动作",
        };
        self.context_menu = Some(EditorContextMenu { position, target });
        // 只记菜单种类与落点：链接目标（URL/路径）**不写日志**。
        eprintln!(
            "[menghunote] 正文右键菜单（{label}）at=({:.1},{:.1})",
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

    /// 链接菜单动作（打开 / 复制链接 / 复制 Markdown）。
    ///
    /// 动作在菜单项的 **mouse-down** 上执行；菜单随即关闭，同一次手势的 mouse-up
    /// 必须被吞掉（[`Self::take_swallow_mouse_up`]），否则会落到下层正文。caret 由
    /// 元素层用「保留 `context_menu_rect` 一帧」放行，不在这里移动。
    fn run_link_menu_action(&mut self, action: LinkMenuAction, cx: &mut Context<Self>) {
        let Some(EditorContextMenu {
            target: EditorContextMenuTarget::Link(hit),
            ..
        }) = self.context_menu.clone()
        else {
            return;
        };
        self.context_menu = None;
        // **故意不立刻清 `context_menu_rect`**：同一次 mouse-down 的窗口级监听随后
        // 还会跑，保留命中矩形让 `point_in_toolbar` 继续放行（不挪 caret）；下一帧
        // render 开头会清掉。
        self.swallow_mouse_up = true;
        match action {
            LinkMenuAction::Open => self.open_link_dest(&hit.dest, cx),
            LinkMenuAction::CopyLink => {
                cx.write_to_clipboard(ClipboardItem::new_string(hit.dest));
                self.link_notice = Some("已复制链接".to_owned());
            }
            LinkMenuAction::CopyMarkdown => {
                cx.write_to_clipboard(ClipboardItem::new_string(hit.markdown));
                self.link_notice = Some("已复制 Markdown".to_owned());
            }
        }
        cx.notify();
    }

    /// 吞掉同一次手势的 mouse-up（链接菜单/确认框动作后调用）。
    pub(crate) fn take_swallow_mouse_up(&mut self) -> bool {
        std::mem::take(&mut self.swallow_mouse_up)
    }

    // ---- 普通链接（外部 / 内部）----

    /// 命中普通链接：返回完整信息（`dest` + 完整 Markdown 源码）。
    ///
    /// 只接受可见文本区间（末尾边界 `display == end` 不算命中），与
    /// [`Self::external_link_display_ranges`] 同一套命中规则。
    pub fn external_link_hit_at_display(&self, display: usize) -> Option<ExternalLinkHit> {
        let source = self.controller.source_text();
        for link in self.controller.links() {
            let Some(start) = self.controller.source_to_display(link.content().start()) else {
                continue;
            };
            let Some(end) = self.controller.source_to_display(link.content().end()) else {
                continue;
            };
            if start.get() <= display && display < end.get() {
                let markdown = source
                    .get(link.span().start().get()..link.span().end().get())
                    .unwrap_or_default()
                    .to_owned();
                return Some(ExternalLinkHit {
                    dest: link.dest().to_owned(),
                    markdown,
                });
            }
        }
        None
    }

    /// 本帧普通链接的**窗口坐标**矩形（headless 点击/断言用；与链接 hitbox 同源）。
    pub fn external_link_rects(&self) -> Vec<(usize, usize, String, Bounds<Pixels>)> {
        let Some(geometry) = self.shared.geometry() else {
            return Vec::new();
        };
        let mut rects = Vec::new();
        for (start, end, dest) in self.external_link_display_ranges() {
            let Some((row, start_x)) = geometry.caret_row_x(start) else {
                continue;
            };
            let Some((end_row, end_x)) = geometry.caret_row_x(end) else {
                continue;
            };
            if row != end_row || end_x <= start_x {
                continue;
            }
            rects.push((
                start,
                end,
                dest,
                Bounds::new(
                    point(start_x, geometry.row_top(row)),
                    size(end_x - start_x, geometry.row_height(row)),
                ),
            ));
        }
        rects
    }

    /// 打开一个普通链接目标（Ctrl/Cmd+单击、右键「打开链接」、`request_external_link`）。
    ///
    /// scheme 策略见 [`super::link`]：允许的 scheme 直接交给系统；`file:`/UNC 与未知
    /// 自定义 scheme 先弹 app 内确认框；危险 scheme 只给提示（不回显路径）；内部链接
    /// 走标题滚动 / 宿主打开笔记（不自动建笔记）。
    pub(crate) fn open_link_dest(&mut self, dest: &str, cx: &mut Context<Self>) {
        match link::classify(dest) {
            LinkTarget::External { .. } => {
                self.link_notice = None;
                link::open_external_url(dest, cx);
            }
            LinkTarget::File { scheme } => {
                self.open_link_confirm(dest, scheme, false, cx);
            }
            LinkTarget::Custom { scheme } => {
                if link_settings::is_remembered(&scheme) {
                    self.link_notice = None;
                    link::open_external_url(dest, cx);
                } else {
                    self.open_link_confirm(dest, scheme, true, cx);
                }
            }
            LinkTarget::Dangerous { scheme } => {
                self.link_notice = Some(link::dangerous_notice(&scheme));
            }
            LinkTarget::Internal { note, heading } => {
                self.open_internal_link(note, heading, cx);
            }
        }
        cx.notify();
    }

    /// 打开确认框（`file:`/UNC 与未知自定义 scheme）。
    fn open_link_confirm(
        &mut self,
        url: &str,
        scheme: String,
        rememberable: bool,
        cx: &mut Context<Self>,
    ) {
        self.link_confirm = Some(LinkConfirm {
            url: url.to_owned(),
            scheme,
            rememberable,
            remember: false,
        });
        self.link_notice = None;
        cx.notify();
    }

    /// 关闭确认框（取消 / `Esc` / 点遮罩）。
    pub(crate) fn close_link_confirm(&mut self, cx: &mut Context<Self>) {
        if self.link_confirm.take().is_some() {
            self.swallow_mouse_up = true;
            cx.notify();
        }
    }

    /// 确认框「打开」：按需记住未知 scheme，然后原样交给系统。
    pub(crate) fn confirm_link_open(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.link_confirm.take() else {
            return;
        };
        self.swallow_mouse_up = true;
        if confirm.rememberable && confirm.remember {
            link_settings::remember(&confirm.scheme);
        }
        self.link_notice = None;
        link::open_external_url(&confirm.url, cx);
        cx.notify();
    }

    /// 内部链接：`#heading` 滚动/定位到标题；相对笔记路径交给宿主打开。
    ///
    /// 解析不到只给**非阻塞提示**，不自动建笔记（与 wiki 链接同一约定）。
    fn open_internal_link(
        &mut self,
        note: Option<String>,
        heading: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let note = note.filter(|note| !note.trim().is_empty());
        let heading = heading.filter(|heading| !heading.trim().is_empty());
        match (note, heading) {
            (None, Some(heading)) => {
                self.scroll_to_heading(&heading, cx);
            }
            (Some(note), heading) => {
                // 相对笔记路径：复用 wiki 链接的解析/打开（宿主负责，不自动建笔记）。
                self.link_notice = None;
                cx.emit(EditorEvent::WikiLinkRequested {
                    target: note,
                    heading,
                });
                cx.notify();
            }
            (None, None) => {
                self.link_notice = Some("链接目标为空".to_owned());
                cx.notify();
            }
        }
    }

    /// 把 caret 移到标题并滚动到标题行顶部；找不到只给非阻塞提示。
    fn scroll_to_heading(&mut self, heading: &str, cx: &mut Context<Self>) -> bool {
        let wanted = link::normalize_heading(heading);
        if wanted.is_empty() {
            self.link_notice = Some("链接标题为空".to_owned());
            cx.notify();
            return false;
        }
        let source = self.controller.source_text().to_owned();
        let mut found = None;
        for run in self.controller.block_runs() {
            if run.kind() != BlockKind::Heading {
                continue;
            }
            if link::heading_text(&source, run.source()).eq_ignore_ascii_case(&wanted) {
                found = Some(run.source().start().get());
                break;
            }
        }
        let Some(start) = found else {
            self.link_notice = Some(format!("未找到标题「{wanted}」"));
            cx.notify();
            return false;
        };
        let Ok(scalar) = SourceByte::new(start).to_scalar(&source) else {
            return false;
        };
        // 跳转语义：标题行**顶部对齐**（不是最小滚动），随后 caret 落到标题行首。
        if let Some(geometry) = self.shared.geometry() {
            if let Some(display) = self.controller.source_to_display(SourceByte::new(start)) {
                let row = geometry.frame.row_of_byte(display.get());
                self.scroll_y = geometry.frame.row_top_offset(row);
                self.clamp_scroll();
            }
        }
        self.link_notice = None;
        self.commit_selection(Selection::caret(scalar), cx);
        true
    }

    /// 外部链接确认框是否打开（元素层据此把点击让给模态浮层）。
    pub(crate) fn link_confirm_open(&self) -> bool {
        self.link_confirm.is_some()
    }

    /// 非阻塞链接提示（危险 scheme / 未找到标题 / 复制结果）；`None` = 没有提示。
    #[must_use]
    pub fn link_notice_text(&self) -> Option<&str> {
        self.link_notice.as_deref()
    }

    /// 清掉非阻塞链接提示（下一次点击/按键时调用）。
    pub(crate) fn clear_link_notice(&mut self) {
        self.link_notice = None;
    }

    /// 右键菜单「插入 → 模板…」：只发事件给宿主（选择器是宿主覆盖层）。
    ///
    /// - 菜单随即关闭；同一次手势的 mouse-up 由 `swallow_mouse_up` 吞掉，**不移动 caret**；
    /// - 持闸 / IME 组合中直接忽略（不打开选择器，避免把组合串/未保存编辑丢掉）；
    /// - 插入位置仍是打开菜单时的 caret/选区（选择器只负责挑模板）。
    pub(crate) fn request_template_picker(&mut self, cx: &mut Context<Self>) {
        self.context_menu = None;
        self.swallow_mouse_up = true;
        self.pending_enter_text_input = false;
        if self.frozen_by_external_commit() {
            self.link_notice = Some(crate::ui_text::templates_insert_failed());
            eprintln!("[menghunote] 同步持闸中：忽略「插入 → 模板…」（不产生请求）");
            cx.notify();
            return;
        }
        if self.is_composing() {
            eprintln!("[menghunote] IME 组合中：忽略「插入 → 模板…」（不产生请求）");
            cx.notify();
            return;
        }
        cx.emit(EditorEvent::TemplatePickerRequested);
        cx.notify();
    }

    /// 宿主插入一段**已渲染**的模板正文（右键「插入 → 模板…」/「从模板新建」/「今日日记」）。
    ///
    /// - 有选区则替换、折叠 caret 则插入；走 [`Self::insert_text`]，因此**一次**
    ///   `EditRequest`（一个撤销组），不自己拼 `ScalarEdit`；
    /// - 持闸 / IME 组合 / 单元格就地编辑中拒绝（返回 `false`，正文零变化）；
    /// - 返回值 = 是否真的产生了已提交的请求（空文本 / no-op 返回 `false`）。
    pub(crate) fn insert_template_text(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if text.is_empty() {
            return false;
        }
        if self.frozen_by_external_commit() || self.is_composing() || self.is_editing_table_cell() {
            return false;
        }
        // 模板是普通正文：清掉对象/单元格的临时选中态，别让旧选中影响后续交互。
        self.selected_object_line = None;
        self.selected_table_cell = None;
        self.table_axis_selection = None;
        self.pending_enter_text_input = false;
        let before = self.request_seq;
        self.insert_text(text, cx);
        self.ensure_caret_visible(self.controller.selection().head());
        self.request_seq != before
    }

    /// 用**整篇替换**写回源文（画布笔记保存路径）。
    ///
    /// 语义与"全选后粘贴"完全一致：先把选区设成整篇（`0..source_scalar_len`），
    /// 再走 [`Self::insert_text`] → `compose_replace_selection`。因此**不新建**
    /// 第二套编辑路径：撤销组、同步 outbox、B 物化、资产引用对账全部沿用现有链路，
    /// 一次保存 = 一条 `EditRequest` = 一个撤销组。
    ///
    /// 返回是否真的产生了**已受理**的请求（调用方据此决定是否关闭覆盖层）：
    ///
    /// - 源文已经等于 `text` → `true`（无需编辑，但保存目标已达成）；
    /// - 持闸 / IME 组合 / 表格单元格编辑中 → `false`（正文零变化）；
    /// - 空文本 + 空文档 → 不产生请求，但目标已达成 → `true`；
    /// - 其余 → 以"替换后源文是否等于 `text`"为准（控制器拒绝时返回 `false`）。
    ///
    /// 光标在第一行末尾、且整篇只有 `---` 时，Enter 自动补成 YAML frontmatter。
    ///
    /// 与 Obsidian 一致：`---` + Enter → `---\n\n---\n`，caret 停在中间空行；
    /// 属性面板随即出现。只认**文档最开头**，正文中间的 `---` 仍是水平线。
    pub fn try_open_frontmatter(&mut self, cx: &mut Context<Self>) -> bool {
        let source = self.controller.source_text();
        if frontmatter::parse(&source).is_some() {
            return false;
        }
        let body = if source == "---" {
            ""
        } else if let Some(rest) = source.strip_prefix("---\n") {
            rest
        } else if let Some(rest) = source.strip_prefix("---\r\n") {
            rest
        } else if let Some(rest) = source.strip_prefix("---") {
            // 已有正文时用户先输入 `---`、再按 Enter；此刻 `---` 和原第一行还在
            // 同一行（换行要等这次 Enter 才插入），所以按前缀 `---` 处理。
            rest
        } else {
            return false;
        };
        let Ok(head_byte) = self.controller.selection().head().to_source_byte(&source) else {
            return false;
        };
        // `---` 后平台可能已经把 caret 放到下一行开头（byte 4）；两种都算
        // “刚输入完分隔符”，否则已有正文时永远不会自动补全。
        if head_byte.get() != 3
            && !(head_byte.get() == 4 && source.as_bytes().get(3) == Some(&b'\n'))
        {
            return false;
        }
        // 已有正文时把正文放到关闭分隔符之后；不要吞掉正文。
        let next = format!("---\n\n---\n{body}");
        if !self.replace_source_text(&next, cx) {
            return false;
        }
        // caret 落在**关闭分隔符之后**的正文起点；中间的空 frontmatter 行是隐藏的，
        // 不能让用户直接在里面输入（属性通过面板编辑）。
        let body_start =
            frontmatter::parse(&next).map_or(next.len(), |frontmatter| frontmatter.body_start);
        let Ok(caret) = SourceByte::new(body_start).to_scalar(&next) else {
            return false;
        };
        self.commit_selection(menghunote_editor::Selection::caret(caret), cx);
        true
    }

    /// 属性面板写回后把 caret 放回正文起点（frontmatter 之后），避免整篇替换
    /// 把滚动位置带到文末。
    pub fn move_caret_to_body_start(&mut self, cx: &mut Context<Self>) {
        let source = self.controller.source_text();
        let Some(frontmatter) = frontmatter::parse(&source) else {
            return;
        };
        let Ok(caret) = SourceByte::new(frontmatter.body_start).to_scalar(&source) else {
            return;
        };
        self.commit_selection(menghunote_editor::Selection::caret(caret), cx);
    }

    pub fn replace_source_text(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if self.frozen_by_external_commit() || self.is_composing() || self.is_editing_table_cell() {
            eprintln!("[menghunote] 画布保存被拒：持闸/IME 组合/单元格编辑中（正文零变化）");
            return false;
        }
        let source = self.controller.source_text();
        if source == text {
            // 已经一致：不制造"删了又插回来"的空撤销项。
            return true;
        }
        // 整篇选区用 **scalar** 坐标（`Selection` 的单位）；`source_len` 是 char 数，
        // 不是字节数——中文/emoji 标题下两者不同，混用会把 caret 落到非边界。
        let scalar_len = source.chars().count();
        let selection = Selection::new(UnicodeScalar::new(0), UnicodeScalar::new(scalar_len));
        if let Err(error) = self.controller.set_selection(selection) {
            eprintln!("[menghunote] 画布保存：设置整篇选区失败：{error}");
            return false;
        }
        // 整篇替换与对象/单元格的临时选中态无关：先清掉，避免保存后还残留工具条。
        self.selected_object_line = None;
        self.selected_table_cell = None;
        self.table_axis_selection = None;
        self.pending_enter_text_input = false;
        self.insert_text(text, cx);
        self.ensure_caret_visible(self.controller.selection().head());
        let applied = self.controller.source_text() == text;
        if !applied {
            eprintln!("[menghunote] 画布保存：整篇替换未生效（正文保持不变）");
        }
        applied
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
        if self.is_editing_table_cell() {
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
        self.zoomed_image = Some(ImageZoom {
            image: Arc::clone(&decoded.image),
            scale: 1.0,
        });
        self.selected_object_line = Some(line);
        cx.notify();
    }

    /// 关闭图片放大预览。
    pub fn close_object_zoom(&mut self, cx: &mut Context<Self>) {
        if self.zoomed_image.take().is_some() {
            cx.notify();
        }
    }
    /// 遮罩滚轮缩放：按滚轮增量乘一个指数因子（围绕居中锚点），夹在 0.5–4.0。
    ///
    /// 返回是否真的改变了倍率（NaN / 0 增量 / 已经在边界都返回 `false`）。
    pub fn zoom_image_by(&mut self, delta_y: f32, cx: &mut Context<Self>) -> bool {
        let Some(zoom) = self.zoomed_image.as_mut() else {
            return false;
        };
        if !delta_y.is_finite() || delta_y == 0.0 {
            return false;
        }
        // 指数因子：一次 60px 滚动约 1.16×；大增量先夹到可计算范围，避免溢出/NaN。
        let factor = (delta_y / 400.0).clamp(-2.0, 2.0).exp();
        let next = (zoom.scale * factor).clamp(IMAGE_ZOOM_MIN, IMAGE_ZOOM_MAX);
        if (next - zoom.scale).abs() < f32::EPSILON {
            return false;
        }
        zoom.scale = next;
        cx.notify();
        true
    }

    /// 当前遮罩缩放倍率（`None` = 遮罩没开；headless 断言用）。
    #[must_use]
    pub fn image_zoom_scale(&self) -> Option<f32> {
        self.zoomed_image.as_ref().map(|zoom| zoom.scale)
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
        // 表格变成源码后不再有“整行/整列”的绘制几何，选中态一并退出。
        self.table_axis_selection = None;
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

    /// 工具条「重新渲染」：丢弃 Mermaid 的缓存终态并重新排队（失败后可重试）。
    ///
    /// 只对 Mermaid 对象生效；在途时返回 `false`（不重复排队）。**不产生任何
    /// `EditRequest`**，也不改正文/selection。
    pub fn retry_object_mermaid(&mut self, line: usize, cx: &mut Context<Self>) -> bool {
        let Some(object) = self.controller.object_at_line(line) else {
            return false;
        };
        if !object.kind().is_mermaid() {
            return false;
        }
        let Some(mermaid) = self.controller.mermaid_of(object) else {
            return false;
        };
        let content_width = self.object_content_width();
        let source = self.mermaid_content(mermaid);
        // 显式「重新渲染」是用户意图，**不受延迟渲染门控**（自动绘制仍然等光标移出）。
        let key = MermaidCacheKey::new(&source, content_width);
        let queued = self.mermaid.retry(key, &source);
        if queued {
            eprintln!("[menghunote] Mermaid 重新渲染（行 {line}）");
            cx.notify();
        }
        queued
    }

    /// 工具条「重新渲染」：作用于当前工具条目标对象（只对 Mermaid 生效）。
    pub fn retry_selected_mermaid(&mut self, cx: &mut Context<Self>) {
        if let Some(line) = self.toolbar_target_line() {
            self.retry_object_mermaid(line, cx);
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
                if object_edit_line != Some(index) {
                    if let Some(object) = self.controller.object_at_line(index) {
                        // Mermaid 在光标/选区进入时**不投影**成 Object（源码可编辑）；
                        // 此时布局必须回落到普通 fenced code 行高，否则 caret 会被
                        // 撑成占位块的高度（用户实测的“大光标”）。
                        if object.kind() == menghunote_editor::ObjectKind::Mermaid
                            && projection.object_display_range(object.span()).is_none()
                        {
                            // 落到下面的普通行度量。
                        } else {
                            let line_height = match object.kind() {
                                menghunote_editor::ObjectKind::Image => {
                                    self.controller.image_of(object).map_or(
                                        theme::OBJECT_PLACEHOLDER_HEIGHT
                                            + theme::OBJECT_PADDING * 2.0,
                                        |image| self.object_row_height(image, content_width),
                                    )
                                }
                                menghunote_editor::ObjectKind::Table => {
                                    self.controller.table_of(object).map_or(
                                        theme::TABLE_HEADER_HEIGHT + theme::TABLE_PADDING * 2.0,
                                        |table| self.table_row_height(table, content_width),
                                    )
                                }
                                menghunote_editor::ObjectKind::Mermaid => {
                                    self.controller.mermaid_of(object).map_or(
                                        theme::MERMAID_PLACEHOLDER_HEIGHT
                                            + theme::OBJECT_PADDING * 2.0,
                                        |mermaid| self.mermaid_row_height(mermaid, content_width),
                                    )
                                }
                                menghunote_editor::ObjectKind::Drawing => {
                                    self.controller.drawing_of(object).map_or(
                                        theme::OBJECT_PLACEHOLDER_HEIGHT
                                            + theme::OBJECT_PADDING * 2.0,
                                        |drawing| self.drawing_row_height(drawing, content_width),
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
                }
                let quote_depth = super::layout::quote_depth(source_line);
                let quote = super::layout::quote_indent(quote_depth);
                let line_info = line_infos.get(index);

                let kind = run.map_or(BlockKind::Paragraph, |run| run.kind());
                let (mut font_size, mut line_height, mut mono) = match kind {
                    BlockKind::ThematicBreak => {
                        (theme::FONT_SIZE, theme::DEFAULT_LINE_HEIGHT, false)
                    }
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

    /// 打开一个普通外链（Ctrl/Cmd+单击、右键「打开链接」与测试的公共入口）。
    ///
    /// scheme 策略 / 确认框 / 内部链接都在编辑器侧统一处理（[`Self::open_link_dest`]）；
    /// 宿主不再参与外链打开。
    pub fn request_external_link(&mut self, url: String, cx: &mut Context<Self>) {
        // 统一收口：scheme 策略 / 确认框 / 内部链接都在编辑器侧处理（见 `open_link_dest`）。
        // 不再经过宿主事件——宿主只负责解析 wiki 目标，外链打开不需要它。
        self.open_link_dest(&url, cx);
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

    /// 宿主注入属性面板；面板弱引用本视图，避免 Entity 强引用环。
    pub fn set_properties_panel(&mut self, panel: Entity<NotePropertiesPanel>) {
        self.properties = Some(panel);
    }

    /// 该 **display** 行首对应的源码行是否是分隔线（绘制层画横线）。
    #[must_use]
    pub fn is_thematic_display_row(&self, display_byte: usize) -> bool {
        let Some(source_byte) = self
            .controller
            .display_to_source(DisplayByte::new(display_byte))
        else {
            return false;
        };
        let source = self.controller.source_text();
        let source_byte = source_byte.get().min(source.len());
        let line = source[..source_byte]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        self.controller
            .line_infos()
            .get(line)
            .is_some_and(|info| info.kind == MarkdownLineKind::ThematicBreak)
    }

    /// 属性面板占用的顶部高度（0 = 没有 frontmatter）。
    ///
    /// 这是面板“作为正文块”参与滚动/布局的唯一来源：`EditorElement` 把正文整体
    /// 下移这么多，面板本身用 `-scroll_y` 平移，于是它像图片/表格一样随正文滚走。
    #[must_use]
    pub fn properties_inset(&self, cx: &App) -> f32 {
        let Some(panel) = self.properties.as_ref() else {
            return 0.0;
        };
        let collapsed = panel.read(cx).is_collapsed();
        let source = self.controller.source_text();
        let Some(frontmatter) = frontmatter::parse(&source) else {
            return 0.0;
        };
        if collapsed {
            NotePropertiesPanel::collapsed_height()
        } else {
            crate::note_properties::panel_height(&frontmatter)
        }
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
        // 关闭/切换前先把未提交的单元格编辑落成一条请求（持闸中则丢弃草稿）：
        // 浮层草稿只活在内存里，不提交就等于静默丢掉用户刚打的字。
        if let Some(state) = self.cell_edit.take() {
            self.pending_cell_focus = false;
            self.pending_focus_editor = false;
            if self.frozen_by_external_commit() {
                eprintln!("[menghunote] 关闭/切换前放弃未提交的单元格编辑（同步持闸中）");
            } else {
                match self.controller.replace_table_cell_text(
                    state.line,
                    state.row,
                    state.column,
                    &state.draft,
                ) {
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
        // 外部链接确认框是**模态**的：`Esc` 关闭，其它按键一律吞掉（不落到正文）。
        if self.link_confirm.is_some() {
            if event.keystroke.key.as_str() == "escape" {
                window.prevent_default();
                self.close_link_confirm(cx);
            }
            return;
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
        // 非阻塞链接提示：任何按键都清掉（它只用于一次性反馈）。
        self.link_notice = None;
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

        // 单元格原地编辑期间焦点在浮层输入框上（Enter/Tab/Esc 由 `RenameInput` 处理）。
        // 只有异常情况下正文会拿到按键：Esc 取消浮层，其余按键全部吞掉，绝不让它们
        // 变成正文编辑（否则会把表格行切断/插入换行）。
        if self.is_editing_table_cell() {
            if event.keystroke.key.as_str() == "escape"
                && !modifiers.control
                && !modifiers.alt
                && !modifiers.platform
            {
                self.cancel_table_cell_edit(cx);
            }
            window.prevent_default();
            return;
        }

        // F2：有图片对象可编辑 → 该块源码模式；否则请宿主重命名侧栏项。
        // 单元格编辑不再需要 F2（单击表格单元格即进入原地编辑）。
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

        // Esc：退出整行/整列选中（ADR 0019 S3b6）。拖拽中的 Esc 已在最前面取消，
        // 这里只处理“选中还在、但没在拖”的状态；清完直接返回，不落到正文/单元格分支。
        if event.keystroke.key.as_str() == "escape"
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && self.table_axis_selection.is_some()
        {
            self.clear_table_axis_selection(cx);
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
                // 选着某个单元格（编辑被取消后仍保留选中态）时 Enter = 重新进入该格编辑。
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
        if !self.is_editing_table_cell() {
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
        if self.try_open_frontmatter(cx) {
            self.pending_enter_text_input = true;
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

    /// 普通文本输入（粘贴 / 内部换行）：**不**触发反引号自动补全。
    pub(crate) fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.insert_text_inner(text, false, cx);
    }

    /// 平台打字的文本输入：允许反引号自动补全（IME 组合不走这里）。
    pub(crate) fn insert_typed_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.insert_text_inner(text, true, cx);
    }

    fn insert_text_inner(&mut self, text: &str, allow_backtick: bool, cx: &mut Context<Self>) {
        // 兜底：单元格浮层编辑期间文本属于输入框。正常路径焦点在输入框、不会走到这里；
        // 万一平台把文本送给了正文，也不能把它插进表格/正文（那会写坏结构）。
        if self.is_editing_table_cell() {
            if let Some(state) = self.cell_edit.as_ref() {
                state.input.update(cx, |input, cx| {
                    input.insert_from_host(text, cx);
                });
            }
            return;
        }
        // 平台回声必须先消费，**不能**顺手清掉选中态：单元格编辑提交后 Enter/Tab 仍可能
        // 补发一次 `\n` / `\t` 文本输入，那会把刚提交的表格选中态清掉、或把制表符
        // 插进下一格。
        if text == "\n" && self.pending_enter_text_input {
            // Enter 已经由 key_down 处理（结构编辑或普通换行），这里只消费平台的回声。
            self.pending_enter_text_input = false;
            eprintln!("[menghunote] 已消费平台重复的 Enter 文本输入（避免双换行）");
            return;
        }
        // 单元格就地编辑期间打的是这一格：整表仍是当前对象（工具条/加行加列在提交后
        // 还要用），不能因为一次按键就把选中态清掉。
        if !self.is_editing_table_cell() {
            self.selected_object_line = None;
        }
        self.pending_enter_text_input = false;
        if self.frozen_by_external_commit() {
            // Held：打字/粘贴一律忽略（控制器镜像也不能先乐观改动，否则之后无法对齐）。
            return;
        }
        // 反引号自动补全只作用于**平台打字**（粘贴走 `insert_text`，不配对）。
        // 规则在 editor 的纯函数里；这里只按行语义过滤（表格/代码块/Html 不自动补）。
        if allow_backtick && text == "`" && self.try_backtick_autocomplete(cx) {
            return;
        }
        match self.controller.compose_replace_selection(text) {
            Ok(request) => {
                self.submit_request(request);
                // Obsidian 式：空笔记第一行刚打成 `---` 就立刻生成 frontmatter 块，
                // 不需要再按 Enter。这里走同一套 `try_open_frontmatter`（含已有正文）。
                if self.try_open_frontmatter(cx) {
                    cx.notify();
                    return;
                }
            }
            Err(error) => eprintln!("[menghunote] 插入文本失败：{error}"),
        }
        cx.notify();
    }

    /// 反引号自动补全：返回 `true` 表示这次输入已被处理（不再走普通替换）。
    ///
    /// 纯规则在 [`menghunote_editor::backtick_action`]；本方法负责上下文过滤与执行：
    ///
    /// - IME 组合中不触发（组合上屏走 `commit_composition`）；
    /// - 非空选区、表格/围栏代码/缩进代码/HTML 行不触发（不破坏结构）；
    /// - “跳过自动补的那个”只移动 caret，不产生 `EditRequest`；
    /// - 其余动作都走 `compose_replace_selection`（一次按键一个撤销组）。
    fn try_backtick_autocomplete(&mut self, cx: &mut Context<Self>) -> bool {
        if self.is_composing() {
            return false;
        }
        let selection = self.controller.selection();
        if !selection.is_empty() {
            return false;
        }
        let source = self.controller.source_text();
        let Ok(caret) = selection
            .head()
            .to_source_byte(&source)
            .map(|byte| byte.get())
        else {
            return false;
        };
        if !self.backtick_line_allows_autocomplete(&source, caret) {
            return false;
        }
        match menghunote_editor::backtick_action(&source, caret) {
            BacktickAction::Plain => false,
            BacktickAction::SkipPaired => {
                // 自动补的那个反引号已经存在，只把 caret 移过去（零请求）。
                self.place_caret_at_byte(caret + 1, cx);
                true
            }
            BacktickAction::InsertPair => {
                self.apply_backtick_edit(caret, PAIR_INSERT, PAIR_CARET_OFFSET, cx);
                true
            }
            BacktickAction::InsertSingle => {
                self.apply_backtick_edit(caret, SINGLE_INSERT, SINGLE_CARET_OFFSET, cx);
                true
            }
            BacktickAction::CompleteFence => {
                self.apply_backtick_edit(caret, FENCE_INSERT, FENCE_CARET_OFFSET, cx);
                true
            }
        }
    }

    /// 当前行是否允许反引号自动补全（表格/代码块/HTML 保持普通输入）。
    fn backtick_line_allows_autocomplete(&self, source: &str, caret: usize) -> bool {
        let line = source[..caret.min(source.len())]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        let Some(info) = self.controller.line_infos().get(line) else {
            return false;
        };
        !matches!(
            info.kind,
            MarkdownLineKind::Table
                | MarkdownLineKind::FencedCode
                | MarkdownLineKind::IndentedCode
                | MarkdownLineKind::Html
        )
    }

    /// 在 caret 处插入反引号文本，并把 caret 放到“原 caret + offset”（同一请求）。
    fn apply_backtick_edit(
        &mut self,
        old_caret: usize,
        insert: &str,
        caret_offset: usize,
        cx: &mut Context<Self>,
    ) {
        match self.controller.compose_replace_selection(insert) {
            Ok(Some(request)) => {
                self.submit_request(Some(request));
                let source = self.controller.source_text();
                if let Some(scalar) =
                    scalar_of_byte(&source, old_caret.saturating_add(caret_offset))
                {
                    self.preferred_x = None;
                    self.commit_selection(Selection::caret(scalar), cx);
                } else {
                    cx.notify();
                }
            }
            Ok(None) => cx.notify(),
            Err(error) => eprintln!("[menghunote] 反引号自动补全失败：{error}"),
        }
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
        if !allow_cell_text && self.is_editing_table_cell() {
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
        // 删除范围跨过行尾（例如整段被隐藏的 frontmatter / 多行选区）：这里只处理
        // “同一行内删掉列表 marker”的窄场景，跨行时直接放行，绝不能用
        // `source[end..line_end]` 切出 end > line_end 的非法区间（真机 panic）。
        if end > line_end || start > line_end {
            return range;
        }
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
        if !self.controller.is_editing_object_source() && !self.is_editing_table_cell() {
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
                #[cfg(feature = "sync-ui")]
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
        // 整行/整列选中同样只属于“点击表格把手”这一种上下文：caret 一动（点正文/
        // 表格内边距/键盘移动）就清掉，不允许高亮停在一个已经离开的目标上。
        self.table_axis_selection = None;
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
}

/// dev-sync（ADR 0009 切片 3b）
/// dev-sync（ADR 0009 切片 3b）：外部提交闸门 / adopt 的**最小转发**。
///
/// 这里只做三件事：转发给 [`SessionDriver`]、刷新 `held`（状态栏与 selector 用）、
/// 让调用方显式 [`Self::dev_sync_pump`]。事件本身由驱动推进队列（`Remote` + `Durable`），
/// 因此 adopt 之后**不需要重开文档**也能看到远端文字。
///
/// 切片 4 的 worker 会从这些方法之上搭（网络/push 都不在这里）。
#[cfg(feature = "sync-ui")]
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
    /// 表格单元格原地编辑输入框：浮在**被点 cell 的矩形**上，只覆盖那一格。
    ///
    /// 矩形来自 `table_cell_rects()`（与表格绘制/命中共用同一份几何）；表格本身仍是
    /// Object，所以其它行列在这一帧照常可见。cell 滚出视口时这一帧不画（`bounds=None`），
    /// 但编辑态不丢，滚回来继续。
    fn render_table_cell_editor(&mut self, _cx: &mut Context<Self>) -> Option<AnyElement> {
        let (line, row, column) = {
            let state = self.cell_edit.as_ref()?;
            (state.line, state.row, state.column)
        };
        let rect = self
            .table_cell_rects()
            .into_iter()
            .find(|(object_line, r, c, _)| *object_line == line && *r == row && *c == column)
            .map(|(_, _, _, rect)| rect);
        let Some(rect) = rect else {
            if let Some(state) = self.cell_edit.as_mut() {
                state.bounds = None;
            }
            return None;
        };
        let origin = self.shared.geometry().map(|geometry| geometry.origin)?;
        let local_left = (rect.origin.x - origin.x).as_f32();
        let local_top = (rect.origin.y - origin.y).as_f32();
        let width = rect.size.width.as_f32();
        let height = rect.size.height.as_f32();
        let state = self.cell_edit.as_mut()?;
        state.bounds = Some(rect);
        let input = state.input.clone();
        Some(
            div()
                .id("table-cell-editor")
                .debug_selector(|| "table-cell-editor".to_string())
                .absolute()
                .left(px(local_left))
                .top(px(local_top))
                .w(px(width))
                .h(px(height))
                // 只覆盖这一格：超长文本（或 IME preedit）裁在 cell 内，不压到邻格。
                .overflow_hidden()
                .child(input)
                .into_any_element(),
        )
    }

    /// 图片放大预览遮罩（点击图片/工具条放大按钮打开；点击任意处关闭）。
    fn render_image_zoom(&self, zoom: ImageZoom, cx: &mut Context<Self>) -> AnyElement {
        // 基准框 = 遮罩的 90%；滚轮倍率直接乘在这个比例上（居中 flex 缩放）。
        // 上限 4.0 时仍是一个有限比例，配合 `overflow_hidden` 裁剪，不会把布局撑爆。
        let fraction = (0.9 * zoom.scale).clamp(0.1, 4.0);
        div()
            .id("image-zoom-overlay")
            .debug_selector(|| "image-zoom-overlay".to_string())
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::image_overlay_bg())
            .overflow_hidden()
            // 覆盖层语义：滚轮/点击都不许漏给下面的编辑器。
            .occlude()
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
            .on_scroll(cx.listener(|view, event: &gpui::ScrollEvent, _window, cx| {
                let delta_y = event.delta.pixel_delta(px(20.0)).y.as_f32();
                view.zoom_image_by(delta_y, cx);
                // **阻断穿透**：遮罩是模态预览，滚轮只缩放图片；底层编辑器不得滚动。
                cx.stop_propagation();
            }))
            .child(
                div()
                    .id("image-zoom-image")
                    .debug_selector(|| "image-zoom-image".to_string())
                    .w(gpui::relative(fraction))
                    .h(gpui::relative(fraction))
                    .child(
                        img(ImageSource::Render(zoom.image))
                            .object_fit(ObjectFit::Contain)
                            .size_full(),
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
            // 选中反馈的一部分：被选中的行/列，其把手换成强调色（普通把手仍是灰色）。
            let axis_selected = self.table_axis_selection.is_some_and(|selection| {
                selection.line == line && selection.axis == axis && selection.index == index
            });
            // 正在拖的就是这个把手 → 抓取中的指针状态。
            let dragging_this = self
                .table_drag
                .is_some_and(|drag| drag.line == line && drag.axis == axis && drag.from == index);
            let background = if axis_selected {
                theme::table_drag_handle_selected_bg()
            } else {
                theme::table_drag_handle_bg()
            };
            let hover_background = if axis_selected {
                theme::table_drag_handle_selected_bg()
            } else {
                theme::table_drag_handle_hover_bg()
            };
            let cursor = if dragging_this {
                CursorStyle::ClosedHand
            } else {
                CursorStyle::OpenHand
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
                    .bg(background)
                    .hover(move |style| style.bg(hover_background))
                    // 抓取手势的指针（gpui 原生：命中链变化时自动切换）。
                    .cursor(cursor)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                            // 把键盘焦点留在正文：拖拽中按 Esc 取消要靠编辑器的 key down。
                            window.focus(&view.focus_handle, cx);
                            view.begin_table_drag(line, axis, index, event.position, cx);
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

    /// 拖动中的整行/整列 ghost + “移动第 N 列/行”浮层（ADR 0019 S3b6）。
    ///
    /// ghost = 被拖项矩形按 `position - press` 平移（几何见 [`Self::table_drag_ghost_rect`]），
    /// 半透明填充 + 强调色边框；旁边跟着一条深色文案浮层，告诉用户“正在拖第几列/行、
    /// 将落到第几列/行”。两个 div **都没有鼠标监听 / hitbox**：绝不拦截鼠标、不改 caret，
    /// 也没有 `cursor`/`hover`，纯绘制；拖出表格 / `Esc` 取消时状态清空 → 一起消失。
    ///
    /// debug selector：预览条 `table-drag-ghost`、文案 `table-drag-ghost-label`。
    fn render_table_drag_ghost(&self) -> Vec<AnyElement> {
        let Some(geometry) = self.shared.geometry() else {
            return Vec::new();
        };
        let Some(rect) = self.table_drag_ghost_rect() else {
            return Vec::new();
        };
        let Some(text) = self.table_drag_ghost_label() else {
            return Vec::new();
        };
        let Some(drag) = self.table_drag else {
            return Vec::new();
        };
        let local_left = (rect.origin.x - geometry.origin.x).as_f32();
        let local_top = (rect.origin.y - geometry.origin.y).as_f32();
        let body = div()
            .id("table-drag-ghost")
            .debug_selector(|| "table-drag-ghost".to_string())
            .absolute()
            .left(px(local_left))
            .top(px(local_top))
            .w(px(rect.size.width.as_f32()))
            .h(px(rect.size.height.as_f32()))
            .rounded(px(4.0))
            .border(px(theme::TABLE_DRAG_GHOST_BORDER))
            .border_color(theme::table_drag_ghost_border())
            .bg(theme::table_drag_ghost_bg())
            .into_any_element();
        // 文案贴着指针（窗口坐标 → 元素局部坐标），不挡被拖项本身；靠近编辑区
        // 右/下边缘时夹住，避免浮层画到状态栏/宿主 UI 上（尺寸取保守上限）。
        let (viewport_width, viewport_height) = self.shared.viewport();
        let label_left = ((drag.position.x - geometry.origin.x).as_f32()
            + theme::TABLE_DRAG_GHOST_LABEL_OFFSET_X)
            .clamp(
                0.0,
                (viewport_width - theme::TABLE_DRAG_GHOST_LABEL_MAX_WIDTH).max(0.0),
            );
        let label_top = ((drag.position.y - geometry.origin.y).as_f32()
            + theme::TABLE_DRAG_GHOST_LABEL_OFFSET_Y)
            .clamp(
                0.0,
                (viewport_height - theme::TABLE_DRAG_GHOST_LABEL_HEIGHT).max(0.0),
            );
        let label = div()
            .id("table-drag-ghost-label")
            .debug_selector(|| "table-drag-ghost-label".to_string())
            .absolute()
            .left(px(label_left))
            .top(px(label_top))
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .bg(theme::table_drag_ghost_label_bg())
            .text_color(theme::table_drag_ghost_label_text())
            .text_size(px(12.0))
            .line_height(px(16.0))
            .child(SharedString::from(text))
            .into_any_element();
        vec![body, label]
    }

    /// 整行/整列选中的高亮（ADR 0019 S3b6；debug selector `table-axis-selection`）。
    ///
    /// 低透明度填充 + 2px 强调色边框：画在表格之上（文字仍然透出来），因此**不会**
    /// 被单元格选中描边盖住（点单元格会先清除轴选中，两者不叠加）。几何来自本帧
    /// `TablePaint`（[`Self::table_axis_selection_rect`]），不另算坐标。
    fn render_table_axis_selection(&self) -> Option<AnyElement> {
        let geometry = self.shared.geometry()?;
        let rect = self.table_axis_selection_rect()?;
        let local_left = (rect.origin.x - geometry.origin.x).as_f32();
        let local_top = (rect.origin.y - geometry.origin.y).as_f32();
        Some(
            div()
                .id("table-axis-selection")
                .debug_selector(|| "table-axis-selection".to_string())
                .absolute()
                .left(px(local_left))
                .top(px(local_top))
                .w(px(rect.size.width.as_f32()))
                .h(px(rect.size.height.as_f32()))
                .border(px(theme::TABLE_AXIS_SELECTION_BORDER))
                .border_color(theme::table_axis_selection_border())
                .bg(theme::table_axis_selection_bg())
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
        // 内嵌画布没有工具条：单击预览直接打开目标画布笔记（ADR 0025 后续切片），
        // 不提供「编辑源码」「放大」等原地动作。
        if object.kind.is_drawing() {
            return None;
        }
        // 表格没有“放大”（ADR 0019 S3a：工具条只需要编辑源码）。
        // Mermaid 没有“放大”，但提供「重新渲染」（失败/改主题后可重试；M2）。
        let has_zoom = object.kind.is_image();
        let has_rerender = object.kind.is_mermaid();
        let button_count = if has_zoom || has_rerender { 2.0 } else { 1.0 };
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
        let rerender = has_rerender.then(|| {
            button("object-toolbar-rerender", "⟳").on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, _event: &MouseDownEvent, _window, cx| {
                    view.retry_object_mermaid(target_line, cx);
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
                .when_some(rerender, |this, rerender| this.child(rerender))
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
        let menu = self.context_menu.clone()?;
        let geometry = self.shared.geometry()?;
        let (viewport_width, viewport_height) = self.shared.viewport();
        let width = theme::EDITOR_CONTEXT_MENU_WIDTH;
        // 「插入」菜单：模板插件启用时多一行「模板…」，面板与命中矩形同步变高。
        let insert_template = matches!(menu.target, EditorContextMenuTarget::Insert)
            && crate::templates::preferences().enabled;
        let height = match menu.target {
            EditorContextMenuTarget::Insert => {
                theme::EDITOR_CONTEXT_MENU_HEIGHT
                    + if insert_template {
                        INSERT_TEMPLATE_ROW_HEIGHT
                    } else {
                        0.0
                    }
            }
            EditorContextMenuTarget::Link(_) => theme::EDITOR_CONTEXT_MENU_LINK_HEIGHT,
        };
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

        let panel = match menu.target {
            EditorContextMenuTarget::Insert => {
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
                    .child(SharedString::from(crate::ui_text::editor_menu_table()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _event: &MouseDownEvent, window, cx| {
                            view.insert_table_from_context_menu(Some(window), cx);
                        }),
                    );
                // 模板插件启用时多一行「模板…」（选择器由宿主渲染；这里只发事件）。
                let template_item = insert_template.then(|| {
                    div()
                        .id("editor-context-insert-template")
                        .debug_selector(|| "editor-context-insert-template".to_string())
                        .flex()
                        .items_center()
                        .h(px(24.0))
                        .px(px(8.0))
                        .rounded(px(4.0))
                        .text_size(px(13.0))
                        .text_color(theme::object_toolbar_text())
                        .hover(|style| style.bg(theme::sidebar_selected_bg()))
                        .child(SharedString::from(crate::ui_text::editor_menu_template()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
                                view.request_template_picker(cx);
                            }),
                        )
                });

                div()
                    .flex()
                    .flex_col()
                    .w(px(width))
                    .h(px(height))
                    .p(px(4.0))
                    .gap(px(2.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .h(px(18.0))
                            .px(px(8.0))
                            .text_size(px(11.0))
                            .text_color(theme::table_actions_text_disabled())
                            .child(SharedString::from(crate::ui_text::editor_menu_insert())),
                    )
                    .child(item)
                    .when_some(template_item, |this, item| this.child(item))
            }
            EditorContextMenuTarget::Link(_) => {
                // 链接菜单：三个动作，全部在 mouse-down 上执行；动作后由
                // `swallow_mouse_up` 吞掉同一次手势的 mouse-up（不移动 caret）。
                let item = |selector: &'static str, label: String, action: LinkMenuAction| {
                    div()
                        .id(selector)
                        .debug_selector(move || selector.to_string())
                        .flex()
                        .items_center()
                        .h(px(24.0))
                        .px(px(8.0))
                        .rounded(px(4.0))
                        .text_size(px(13.0))
                        .text_color(theme::object_toolbar_text())
                        .hover(|style| style.bg(theme::sidebar_selected_bg()))
                        .child(SharedString::from(label))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, _event: &MouseDownEvent, _window, cx| {
                                view.run_link_menu_action(action, cx);
                            }),
                        )
                };

                div()
                    .flex()
                    .flex_col()
                    .w(px(width))
                    .h(px(height))
                    .p(px(4.0))
                    .gap(px(2.0))
                    .child(item(
                        "editor-context-open-link",
                        crate::ui_text::editor_menu_open_link(),
                        LinkMenuAction::Open,
                    ))
                    .child(item(
                        "editor-context-copy-link",
                        crate::ui_text::editor_menu_copy_link(),
                        LinkMenuAction::CopyLink,
                    ))
                    .child(item(
                        "editor-context-copy-markdown",
                        crate::ui_text::editor_menu_copy_markdown(),
                        LinkMenuAction::CopyMarkdown,
                    ))
            }
        };

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
                .child(panel)
                .into_any_element(),
        )
    }
    /// 外部链接确认框（`file:`/UNC 与未知自定义 scheme）。
    ///
    /// 最小 app 内浮层（不依赖系统级对话框）：遮罩 + 面板 + 「取消 / 打开」；
    /// 未知自定义 scheme 多一个「记住此 scheme（仅本机）」勾选项。确认框是模态的：
    /// 遮罩 `occlude` 且各按钮 `stop_propagation`，点击不会落到正文。
    fn render_link_confirm(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let confirm = self.link_confirm.as_ref()?;
        // 只显示 scheme[:host]，**不回显 path/query**（可能含敏感信息）。
        let target = link::display_target(&confirm.url);
        let rememberable = confirm.rememberable;
        let remember = confirm.remember;

        let cancel = div()
            .id("editor-link-confirm-cancel")
            .debug_selector(|| "editor-link-confirm-cancel".to_string())
            .flex()
            .items_center()
            .justify_center()
            .h(px(26.0))
            .px(px(12.0))
            .rounded(px(6.0))
            .text_size(px(12.0))
            .text_color(theme::object_toolbar_text())
            .hover(|style| style.bg(theme::sidebar_selected_bg()))
            .child(SharedString::from("取消"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                    view.close_link_confirm(cx);
                }),
            );

        let open = div()
            .id("editor-link-confirm-open")
            .debug_selector(|| "editor-link-confirm-open".to_string())
            .flex()
            .items_center()
            .justify_center()
            .h(px(26.0))
            .px(px(12.0))
            .rounded(px(6.0))
            .text_size(px(12.0))
            .text_color(theme::link_confirm_primary_text())
            .bg(theme::link_confirm_primary_bg())
            .hover(|style| style.opacity(0.9))
            .child(SharedString::from("打开"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                    view.confirm_link_open(cx);
                }),
            );

        let mut panel = div()
            .id("editor-link-confirm-panel")
            .flex()
            .flex_col()
            .gap(px(10.0))
            .w(px(theme::LINK_CONFIRM_WIDTH))
            .p(px(16.0))
            .rounded(px(10.0))
            .bg(theme::object_toolbar_bg())
            .border(px(1.0))
            .border_color(theme::object_toolbar_border())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_view, _event: &MouseDownEvent, _window, cx| {
                    // 面板内部点击不关闭确认框（只有遮罩/取消才关）。
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .text_size(px(14.0))
                    .text_color(theme::text_color())
                    .child(SharedString::from("打开外部链接？")),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme::table_actions_text_disabled())
                    .child(SharedString::from(format!("目标：{target}"))),
            );
        if rememberable {
            panel = panel.child(
                div()
                    .id("editor-link-confirm-remember")
                    .debug_selector(|| "editor-link-confirm-remember".to_string())
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
                            if let Some(confirm) = view.link_confirm.as_mut() {
                                confirm.remember = !confirm.remember;
                            }
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .w(px(12.0))
                            .h(px(12.0))
                            .rounded(px(3.0))
                            .border(px(1.0))
                            .border_color(theme::object_toolbar_border())
                            .when(remember, |this| this.bg(theme::link_confirm_primary_bg())),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme::table_actions_text_disabled())
                            .child(SharedString::from("记住此 scheme（仅本机）")),
                    ),
            );
        }
        panel = panel.child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(cancel)
                .child(open),
        );

        Some(
            div()
                .id("editor-link-confirm")
                .debug_selector(|| "editor-link-confirm".to_string())
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::link_confirm_overlay())
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, _event: &MouseDownEvent, _window, cx| {
                        // 点遮罩 = 取消；吞掉这次点击，不落到正文。
                        cx.stop_propagation();
                        view.close_link_confirm(cx);
                    }),
                )
                .child(panel)
                .into_any_element(),
        )
    }

    /// 非阻塞链接提示（危险 scheme 拒绝 / 未找到标题 / 复制结果）。
    ///
    /// 浮在状态栏上方，不拦截鼠标（没有交互监听）；下一次点击/按键清除。
    fn render_link_notice(&self) -> Option<AnyElement> {
        let notice = self.link_notice.as_ref()?;
        Some(
            div()
                .id("editor-link-notice")
                .debug_selector(|| "editor-link-notice".to_string())
                .absolute()
                .bottom(px(36.0))
                .left(px(0.0))
                .right(px(0.0))
                .flex()
                .justify_center()
                .child(
                    div()
                        .px(px(10.0))
                        .py(px(4.0))
                        .rounded(px(999.0))
                        .bg(theme::link_notice_bg())
                        .text_size(px(12.0))
                        .text_color(theme::link_notice_text())
                        .child(SharedString::from(notice.clone())),
                )
                .into_any_element(),
        )
    }
}

/// 把单元格源码文本变成**编辑浮层**用的文本（源码原文 + 只还原 `\|` 转义）。
///
/// 单元格就地编辑显示的是**源码**（ADR 0019 S3b7）：`**粗体**` 原样显示成
/// `**粗体**`，用户改的就是真源；只有 GFM 的列边界转义 `\|` 还原成 `|`
/// （否则每敲一个字都要面对反斜杠）。提交时仍走 `sanitize_cell_text` 重新转义，
/// 因此「显示 → 提交」逐字节稳定。
#[must_use]
pub(crate) fn table_cell_display_text(source: &str, cell: SourceRange) -> String {
    source
        .get(cell.start().get()..cell.end().get())
        .unwrap_or_default()
        .replace("\\|", "|")
}

/// 一个单元格的绘制数据：行内 Markdown 的 display 文本（按列宽截断）+ 可见链接矩形。
///
/// 文本与链接**同源**：都来自 [`TableCellInline`]（解析文档时算一次），链接矩形用
/// 截断后 `ShapedLine` 的 `x_for_index` 换算，因此点击与绘制逐像素一致。
///
/// `visible = false`（该格正在就地编辑）时不 shaping、不产生链接矩形：浮层输入框
/// 覆盖在 cell 上，旧文字必须隐藏，否则会与草稿叠成重影。
#[must_use]
pub(crate) fn table_cell_paint(
    window: &Window,
    inline: Option<&TableCellInline>,
    rect: Bounds<Pixels>,
    align: MarkdownTableAlignment,
    inner_width: f32,
    visible: bool,
) -> TableCellPaint {
    let empty = TableCellPaint {
        rect,
        text: None,
        align,
        links: Vec::new(),
    };
    if !visible {
        return empty;
    }
    let Some(inline) = inline else {
        return empty;
    };
    let display = inline.display();
    if display.is_empty() {
        return empty;
    }
    let spans: Vec<(usize, StyleKind)> = inline
        .spans()
        .iter()
        .map(|span| (span.len(), table_cell_style_kind(span.kind)))
        .collect();
    let Some(truncated) = super::glyph::shape_row_truncated_at(
        window,
        display,
        &spans,
        theme::TABLE_FONT_SIZE,
        false,
        inner_width,
    ) else {
        return empty;
    };
    let links = table_cell_link_paints(&truncated, inline, rect, align, inner_width);
    TableCellPaint {
        rect,
        text: Some(truncated.line),
        align,
        links,
    }
}

/// 单元格行内语义 → 正文的 [`StyleKind`]（同一套外观：粗体/斜体/删除线/代码/链接）。
fn table_cell_style_kind(kind: TableCellStyle) -> StyleKind {
    match kind {
        TableCellStyle::Plain => StyleKind::Plain,
        TableCellStyle::Bold => StyleKind::Bold,
        TableCellStyle::Emphasis => StyleKind::Emphasis,
        TableCellStyle::InlineCode => StyleKind::InlineCode,
        TableCellStyle::Strike => StyleKind::Strike,
        TableCellStyle::Link => StyleKind::Link,
    }
}

/// 单元格里的链接 → 窗口矩形（按**截断后**的可见前缀裁剪）。
///
/// 偏移与 `ShapedLine::paint` 的 `TextAlign` 语义一致：左对齐从内边距起点开始，
/// 居中/右对齐在单元格内宽里平移。截断点之后的链接没有画出来，直接丢弃（不可点）。
fn table_cell_link_paints(
    truncated: &super::glyph::TruncatedLine,
    inline: &TableCellInline,
    rect: Bounds<Pixels>,
    align: MarkdownTableAlignment,
    inner_width: f32,
) -> Vec<TableCellLinkPaint> {
    if inline.links().is_empty() || truncated.visible_len == 0 {
        return Vec::new();
    }
    let text_width = truncated.line.width().as_f32();
    let align_offset = match align {
        MarkdownTableAlignment::Center => ((inner_width - text_width) / 2.0).max(0.0),
        MarkdownTableAlignment::Right => (inner_width - text_width).max(0.0),
        MarkdownTableAlignment::None | MarkdownTableAlignment::Left => 0.0,
    };
    let left = rect.origin.x.as_f32() + theme::TABLE_CELL_PAD_X + align_offset;
    let mut paints = Vec::with_capacity(inline.links().len());
    for link in inline.links() {
        let start = link.display.start.min(truncated.visible_len);
        let end = link.display.end.min(truncated.visible_len);
        if start >= end {
            continue;
        }
        let x0 = truncated.line.x_for_index(start).as_f32();
        let x1 = truncated.line.x_for_index(end).as_f32();
        if x1 <= x0 {
            continue;
        }
        paints.push(TableCellLinkPaint {
            rect: Bounds::new(
                point(px(left + x0), rect.origin.y),
                size(px(x1 - x0), rect.size.height),
            ),
            display: start..end,
            dest: link.dest.clone(),
        });
    }
    paints
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
        // 浮层是独立实体：焦点在下一帧补（事件回调里没有 `&mut Window`）。
        // 关闭浮层后把焦点还给正文，否则键盘会继续落到已销毁的输入框上。
        if self.cell_edit.is_some() {
            self.pending_focus_editor = false;
            if self.pending_cell_focus {
                self.pending_cell_focus = false;
                if let Some(state) = self.cell_edit.as_ref() {
                    let input = state.input.clone();
                    input.update(cx, |input, cx| input.focus(window, cx));
                }
            }
        } else if self.pending_focus_editor {
            self.pending_focus_editor = false;
            window.focus(&self.focus_handle, cx);
        }

        // 每帧请求加载当前文档的图片（幂等；解码完成由 pump 触发重绘）。
        self.request_object_images();
        // 工具条/表格边缘浮层/右键菜单每帧重建：目标对象/几何变化时自动跟随，
        // None 时清空命中矩形。
        self.toolbar_rect = None;
        self.table_actions_rect = None;
        self.context_menu_rect = None;
        self.table_handle_rects.clear();
        // 单元格原地编辑输入框：浮在 cell 矩形上，绘制在正文之上、其它浮层之下。
        let cell_editor = self.render_table_cell_editor(cx);
        let toolbar = self.render_object_toolbar(cx);
        let table_actions = self.render_table_actions(cx);
        let context_menu = self.render_context_menu(cx);
        // 外部链接确认框（模态浮层）与非阻塞提示：确认框在最上层，提示在状态栏上方。
        let link_confirm = self.render_link_confirm(cx);
        let link_notice = self.render_link_notice();
        // 表格行列拖拽把手/插入指示线（ADR 0019 S3b5）：绘制在正文与表格之上、
        // 悬浮工具条与浮层之下（后者更靠近边缘，不能被把手压住）。
        let drag_handles = self.render_table_drag_handles(cx);
        let drop_indicator = self.render_table_drop_indicator();
        // 整行/整列选中高亮 + 拖拽 ghost（ADR 0019 S3b6）：选中在正文之上、输入框之下；
        // ghost 跟着指针、在把手/指示线之上、工具条之下（都不拦截鼠标）。
        let axis_selection = self.render_table_axis_selection();
        let drag_ghost = self.render_table_drag_ghost();
        let element = EditorElement::new(cx.entity(), Rc::clone(&self.shared));
        let properties_inset = self.properties_inset(cx);
        let properties = self.properties.as_ref().map(|panel| {
            let height = properties_inset;
            let scroll = self.scroll_y;
            div()
                .absolute()
                .top(px(-scroll))
                .left(px(theme::PAD_X))
                .right(px(theme::PAD_X))
                .h(px(height))
                .child(panel.clone())
        });

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
                    .overflow_hidden()
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
                    .when_some(properties, |this, panel| this.child(panel))
                    .when_some(axis_selection, |this, selection| this.child(selection))
                    .when_some(cell_editor, |this, editor| this.child(editor))
                    .children(drag_handles)
                    .when_some(drop_indicator, |this, indicator| this.child(indicator))
                    .children(drag_ghost)
                    .when_some(toolbar, |this, toolbar| this.child(toolbar))
                    .when_some(table_actions, |this, actions| this.child(actions))
                    .when_some(context_menu, |this, menu| this.child(menu)),
            )
            // 状态/保存提示统一由外层 shell 的底栏承担，编辑器内部不再画一条
            // 会挤压正文高度的灰色状态条（真机反馈：底栏遮挡正文）。
            // `editor-sync-held` 仍保留为 0 尺寸的测试标记，不影响布局。
            .when(self.driver.is_external_commit_held(), |this| {
                this.child(
                    div()
                        .w(px(1.0))
                        .h(px(1.0))
                        .debug_selector(|| "editor-sync-held".to_string()),
                )
            })
            .when_some(link_notice, |this, notice| this.child(notice))
            .when_some(self.zoomed_image.clone(), |this, image| {
                this.child(self.render_image_zoom(image, cx))
            })
            // 确认框是模态的：画在最后（图片放大遮罩之上），保证按钮可达。
            .when_some(link_confirm, |this, confirm| this.child(confirm))
    }
}

/// 表格拖拽轴的中文名（日志用）。
const fn table_drag_axis_label(axis: TableDragAxis) -> &'static str {
    match axis {
        TableDragAxis::Column => "列重排",
        TableDragAxis::Row => "行重排",
    }
}

/// 表格拖拽轴的中文单位（“第 N 列/行”文案与日志用）。
const fn table_axis_unit(axis: TableDragAxis) -> &'static str {
    match axis {
        TableDragAxis::Column => "列",
        TableDragAxis::Row => "行",
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
