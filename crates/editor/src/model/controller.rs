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
use std::ops::Range;

use menghunote_storage::session::{
    Applied, CommandId, DeltaSplice, DurableAck, EditOutcome, EditRequest, Generation, LiveDelta,
    LiveRev, OpenSnapshot, RejectReason, ScalarEdit, SessionEpoch,
};
use menghunote_text::{units, DisplayByte, SourceByte, UnicodeScalar};
use ropey::Rope;

use crate::model::block::BlockRun;
use crate::model::error::EditorError;
use crate::model::input::InputDocument;
use crate::model::markdown::{
    MarkdownDocument, MarkdownImage, MarkdownMermaid, MarkdownTable, ProjectionCellReveal,
};
use crate::model::object::ObjectBlock;
use crate::model::projection::ProjectionMap;
use crate::model::selection::Selection;
use crate::model::style::StyleRun;
use crate::model::table_cell_edit;
use crate::model::table_edit;

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

/// 一个正在就地编辑的表格单元格（ADR 0019 S3b2）。
///
/// 用**行号**（表格对象行）+ 行列地址定位，而不是缓存 cell 的源码区间：cell 的
/// 起止字节会随每次输入变化（cell 文本被解析器裁掉首尾空白、表格加列等），缓存旧
/// 坐标必然错位。每次需要区间时都从**最新解析**重新取，因此“加行/加列后的新 cell”
/// 天然可用。
#[derive(Debug, Clone, PartialEq, Eq)]
struct CellEditState {
    /// 表格对象行（表头源码行号，0 起）。
    line: usize,
    /// 行下标（0 = 表头，1..=n = 数据行）。
    row: usize,
    /// 列下标（0 起）。
    column: usize,
    /// 进入编辑前的 cell 源码原文（Esc 取消时逐字节恢复）。
    original_text: String,
    /// 进入编辑前的 cell 源码区间（**会话/镜像基线坐标**，字节）。
    original_span: crate::SourceRange,
    /// 进入编辑前的 cell 起止 scalar（提交 `ScalarEdit` 用；也是基线坐标）。
    original_start: UnicodeScalar,
    original_end: UnicodeScalar,
    /// 进入编辑前整篇镜像的**字节**长度（提交时据此算 cell 区域的新长度）。
    base_len: usize,
}

/// 表格单元格编辑对外的地址（行 0 = 表头；ADR 0019 S3b2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableCellTarget {
    /// 表格对象行（表头源码行号，0 起）。
    pub line: usize,
    /// 行下标（0 = 表头）。
    pub row: usize,
    /// 列下标（0 起）。
    pub column: usize,
}

impl TableCellTarget {
    /// 是否表头行。
    #[must_use]
    pub const fn is_header(self) -> bool {
        self.row == 0
    }
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
    /// 每个列表 marker 在当前 selection 下是否显现（与 projection 同步重建）。
    ///
    /// app 逐行算 gutter 时按行 O(1) 查询，不能对每个 item 重新换算 selection。
    list_marker_revealed: Vec<bool>,
    /// 显式进入源码模式的图片对象所在源码行（ADR 0019 S2a）。
    ///
    /// 普通单击 / 文本选区相交**不会**设置它 —— 对象显现源码只由工具条「编辑源码」
    /// 或 `F2` 触发，`Esc` / 点击块外 / `Ctrl+Enter` 清空（ADR 0019 §1）。
    object_edit_line: Option<usize>,
    /// 正在就地编辑的表格单元格（ADR 0019 S3b2）；`None` = 不在单元格编辑态。
    cell_edit: Option<CellEditState>,
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
        let projection = ProjectionMap::from_markdown_with_lists(&markdown, selection);
        let style_runs = markdown.style_runs(&projection);
        let block_runs = markdown.block_runs()?;
        let list_marker_revealed = markdown.list_marker_reveals(selection)?;
        let mirror = Rope::from_str(&snapshot.markdown);
        Ok(Self {
            snapshot,
            mirror,
            markdown,
            selection,
            projection,
            style_runs,
            block_runs,
            list_marker_revealed,
            object_edit_line: None,
            cell_edit: None,
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
        let projection = ProjectionMap::from_markdown_with_lists(&markdown, selection);
        let style_runs = markdown.style_runs(&projection);
        let block_runs = markdown.block_runs()?;
        let list_marker_revealed = markdown.list_marker_reveals(selection)?;
        self.mirror = Rope::from_str(&snapshot.markdown);
        self.snapshot = snapshot;
        self.markdown = markdown;
        self.selection = selection;
        self.projection = projection;
        self.style_runs = style_runs;
        self.block_runs = block_runs;
        self.list_marker_revealed = list_marker_revealed;
        self.object_edit_line = None;
        self.cell_edit = None;
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

    /// 全部 wiki 链接（L1：点击/未解析样式/后续重命名改写）。
    pub fn wiki_links(&self) -> &[crate::model::markdown::MarkdownWikiLink] {
        self.markdown.wiki_links()
    }

    /// 全部普通链接（外链点击用）。
    pub fn links(&self) -> &[crate::model::markdown::MarkdownLink] {
        self.markdown.links()
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

    /// 逐源码行语义（引用前缀 / 列表归属 / 围栏），来自 ADR 0019 S1 的解析器。
    ///
    /// 与 [`Self::block_runs`] 一样只依赖 source；列表 layout 用它算 gutter 与
    /// hanging indent。app **不要**自己重新扫列表语法。
    #[must_use]
    pub fn line_infos(&self) -> &[menghunote_markdown::LineInfo] {
        self.markdown.line_infos()
    }

    /// 列表块树（按文档顺序）。
    #[must_use]
    pub fn lists(&self) -> &[menghunote_markdown::ListBlock] {
        self.markdown.lists()
    }

    /// 列表项（按文档顺序）。
    #[must_use]
    pub fn list_items(&self) -> &[menghunote_markdown::ListItem] {
        self.markdown.list_items()
    }

    /// 第 `index` 个列表项隐藏态下显示的 marker（bullet / 右对齐编号）。
    #[must_use]
    pub fn list_marker_text(&self, index: usize) -> Option<String> {
        self.markdown.list_marker_text(index)
    }

    /// 第 `index` 个列表项的源码 marker（caret 进入时显示）。
    #[must_use]
    pub fn list_marker_source(&self, index: usize) -> Option<&str> {
        self.markdown.list_marker_source(index)
    }

    /// caret/选区是否落在该列表 marker 上（Typora 规则：进入 marker 才显示源码）。
    ///
    /// O(1)：状态在投影重建时一次性算好（见 `list_marker_reveals`）。
    #[must_use]
    pub fn list_marker_revealed(&self, index: usize) -> bool {
        self.list_marker_revealed
            .get(index)
            .copied()
            .unwrap_or(false)
    }

    // ---- Object Block（ADR 0019 S2a 图片 / S3a 表格）----

    /// 全部图片构造（含混排/行内图片；app 只把 `is_block()` 的当对象渲染）。
    #[must_use]
    pub fn images(&self) -> &[MarkdownImage] {
        self.markdown.images()
    }

    /// 全部表格块（含引用/列表/缩进里、不投影成对象的表格）。
    #[must_use]
    pub fn tables(&self) -> &[MarkdownTable] {
        self.markdown.tables()
    }

    /// 全部整块 Object（图片 + 表格，按源码顺序；投影/选中/源码切换的唯一入口）。
    #[must_use]
    pub fn objects(&self) -> &[ObjectBlock] {
        self.markdown.objects()
    }

    /// 指定源码行上的整块 Object（图片所在行 / 表格表头行）。
    #[must_use]
    pub fn object_at_line(&self, line: usize) -> Option<&ObjectBlock> {
        self.markdown.object_at_line(line)
    }

    /// Object 对应的图片载荷（不是图片对象时 `None`）。
    #[must_use]
    pub fn image_of(&self, object: &ObjectBlock) -> Option<&MarkdownImage> {
        self.markdown.object_image(object)
    }

    /// Object 对应的表格载荷（不是表格对象时 `None`）。
    #[must_use]
    pub fn table_of(&self, object: &ObjectBlock) -> Option<&MarkdownTable> {
        self.markdown.object_table(object)
    }

    /// Object 对应的 Mermaid 载荷（不是 Mermaid 对象时 `None`）。
    #[must_use]
    pub fn mermaid_of(&self, object: &ObjectBlock) -> Option<&MarkdownMermaid> {
        self.markdown.object_mermaid(object)
    }

    /// 全部整块 fenced mermaid（可投影成 Object 的块；后台渲染按 content 取源码）。
    #[must_use]
    pub fn mermaid_blocks(&self) -> &[MarkdownMermaid] {
        self.markdown.mermaid_blocks()
    }

    /// 指定源码行上的整行独占图片（Object Block 候选；图片专用便捷入口）。
    #[must_use]
    pub fn block_image_at_line(&self, line: usize) -> Option<&MarkdownImage> {
        self.markdown.block_image_at_line(line)
    }

    /// 全部整行独占图片（可投影成 Object Block；按文档顺序）。
    pub fn block_images(&self) -> impl Iterator<Item = &MarkdownImage> + '_ {
        self.markdown.block_images()
    }

    /// 整块 Object 在当前投影里的 display 区间（未投影成对象时 `None`）。
    #[must_use]
    pub fn object_display_range(&self, line: usize) -> Option<Range<usize>> {
        let object = self.markdown.object_at_line(line)?;
        self.projection.object_display_range(object.span())
    }

    /// 当前**显式源码模式**的对象行；普通点选/选区相交不会设置它。
    #[must_use]
    pub const fn object_source_edit_line(&self) -> Option<usize> {
        self.object_edit_line
    }

    /// 是否正在编辑某个对象（图片/表格）的源码。
    #[must_use]
    pub const fn is_editing_object_source(&self) -> bool {
        self.object_edit_line.is_some()
    }

    /// 进入某个整块 Object 的**源码模式**（工具条「编辑源码」/ `F2`）。
    ///
    /// 效果：该块不再投影成 Object（完整源码原样显示），caret 落到源码起点。
    /// 行上没有整块 Object 时返回 `Ok(false)`（不改状态）。图片与表格走同一条路径。
    pub fn begin_object_source_edit(&mut self, line: usize) -> Result<bool, EditorError> {
        self.ensure_no_cell_edit()?;
        let Some(object) = self.markdown.object_at_line(line).cloned() else {
            return Ok(false);
        };
        let text = self.mirror.to_string();
        let caret = SourceByte::new(object.span().start().get()).to_scalar(&text)?;
        self.object_edit_line = Some(line);
        self.selection = Selection::caret(caret);
        self.refresh_projection();
        Ok(true)
    }

    /// 退出对象源码模式（`Esc` / 点击块外 / `Ctrl+Enter`）。
    ///
    /// 退出后对象重新显示为图片/表格；caret 放到对象源码末尾（块之后），避免停在
    /// 已被占位符覆盖的源码中间。返回是否真的退出过（未在编辑时 `false`）。
    pub fn end_object_source_edit(&mut self) -> bool {
        let Some(line) = self.object_edit_line.take() else {
            return false;
        };
        if let Some(object) = self.markdown.object_at_line(line).cloned() {
            let text = self.mirror.to_string();
            let mut caret_byte = object.span().end().get();
            // Mermaid：caret 落在 block 源码内会触发“显示源码、不投影 Object”，
            // 因此退出源码模式时把 caret 放到块末尾换行之后（块外），对象重新出现。
            // 图片/表格保持原行为（caret 落在对象末尾）。
            if object.kind().is_mermaid() && text.as_bytes().get(caret_byte) == Some(&b'\n') {
                caret_byte += 1;
            }
            if let Ok(caret) = SourceByte::new(caret_byte).to_scalar(&text) {
                self.selection = Selection::caret(caret);
            }
        }
        self.refresh_projection();
        true
    }

    /// 删除一个整块 Object 的**完整源码跨度**（一次 `ScalarEdit` 事务）。
    ///
    /// 这是 ADR 0019「Delete/Backspace 先选块、再删整个源跨度」的落点：先选中对象
    /// 再由本方法删除；编辑器内部仍走 `compose_edits` 的同一条路径。
    /// 表格删除会一次性去掉表头/分隔行/所有数据行（+ 块内换行）。
    pub fn delete_object(&mut self, line: usize) -> Result<Option<EditRequest>, EditorError> {
        self.ensure_no_cell_edit()?;
        let Some(object) = self.markdown.object_at_line(line).cloned() else {
            return Ok(None);
        };
        let text = self.mirror.to_string();
        let start = SourceByte::new(object.span().start().get()).to_scalar(&text)?;
        let end = SourceByte::new(object.span().end().get()).to_scalar(&text)?;
        self.selection = Selection::new(start, end);
        if self.object_edit_line == Some(line) {
            self.object_edit_line = None;
        }
        self.refresh_projection();
        self.delete_backward()
    }

    /// 与当前（非空）文本选区相交的对象行号（按文档顺序）。
    ///
    /// 折叠 caret 不算相交（边界上的 caret 是普通的文本落点）；对象**内部**没有
    /// 文本 caret，所以只有跨过对象的选区才会命中这里。
    #[must_use]
    pub fn object_lines_in_display_selection(&self) -> Vec<usize> {
        let Some((start, end)) = self.selection_in_display() else {
            return Vec::new();
        };
        if start == end {
            return Vec::new();
        }
        let (start, end) = (start.get(), end.get());
        self.markdown
            .objects()
            .iter()
            .filter(|object| {
                self.projection
                    .object_display_range(object.span())
                    .is_some_and(|range| range.start < end && start < range.end)
            })
            .map(|object| object.line())
            .collect()
    }

    /// 列表 Enter：同级新建 item；空项退出列表。返回 `None` 时调用方按普通换行处理。
    ///
    /// - 只在 item **首行**、caret 位于 marker 之后时接管；续行/子段落不猜测结构，
    ///   交回普通 `\n` 插入（S1 限制）；
    /// - 有非空选区时先按普通替换处理（不在这里猜结构）；
    /// - 编辑仍然走 [`Self::compose_edits`]：一次按键一个 `EditRequest`，
    ///   IME 组合期间由 `ensure_editable` 拒绝（app 必须先取消组合）。
    pub fn list_enter(&mut self) -> Result<Option<EditRequest>, EditorError> {
        self.ensure_editable()?;
        self.ensure_no_cell_edit()?;
        if !self.selection.is_empty() {
            return self.compose_replace_selection("\n");
        }
        let text = self.mirror.to_string();
        let caret = self.selection.head().to_source_byte(&text)?;
        let lines = self.markdown.line_infos();
        let line_index = text[..caret.get()]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        let Some(line) = lines.get(line_index) else {
            return Ok(None);
        };
        let Some(list) = &line.list else {
            return Ok(None);
        };
        let item = &self.markdown.list_items()[list.item];
        if item.first_line != line_index || caret.get() < item.content_start {
            return Ok(None);
        }
        let source = text.as_str();
        let empty = source
            .get(item.content_start..line.content.end)
            .is_some_and(|rest| rest.trim().is_empty());
        let (start, end, replacement) = if empty {
            // 空项退出：删除前导空白 + marker（保留引用前缀），光标落到行首内容区。
            let start = item.hidden_prefix.start;
            let end = item.marker.end;
            (start, end, String::new())
        } else {
            let prefix = source
                .get(line.span.start..item.marker.start)
                .unwrap_or_default();
            let marker = next_marker(source, item);
            let caret = caret.get();
            (caret, caret, format!("\n{prefix}{marker}"))
        };
        let start = SourceByte::new(start).to_scalar(source)?;
        let end = SourceByte::new(end).to_scalar(source)?;
        let edit = ScalarEdit {
            start,
            end,
            replacement,
        };
        self.compose_edits(vec![edit]).map(Some)
    }

    /// 切换第 `index` 个列表项的任务框（`[ ] ↔ [x]`，ADR 0019 S3）。
    ///
    /// 契约（写死，测试钉住）：
    ///
    /// - 只改任务框那 3 个 source 字节（`[ ]` / `[x]` / `[X]`），**不**动缩进、列表
    ///   marker、任务框后的空白、其它正文与换行（逐字不变）；
    /// - 勾选统一写**小写** `x`（`[X]` 视为已勾选；点击后写 `[ ]`，再点写 `[x]`）；
    /// - 一次点击产生**一个** `EditRequest`（单 `ScalarEdit`），由会话默认撤销组进
    ///   undo；不批量改其它 item；
    /// - caret/选区**保持不变**：`compose_edits` 会把 caret 挪到替换文本之后，这里
    ///   在返回请求前恢复原选区（点击复选框不能落正文 caret）；替换等长（3 scalar），
    ///   所以恢复的坐标必然仍然合法；
    /// - IME 组合中由 [`Self::ensure_editable`] 拒绝（调用方应先取消组合）；
    /// - 不是任务项 / 下标越界 → `Ok(None)`（不产生请求，不改文本）。
    pub fn toggle_task_marker(&mut self, index: usize) -> Result<Option<EditRequest>, EditorError> {
        self.ensure_editable()?;
        self.ensure_no_cell_edit()?;
        let Some(item) = self.markdown.list_items().get(index) else {
            return Ok(None);
        };
        let Some(box_span) = item.task_marker else {
            return Ok(None);
        };
        let checked = item.task.unwrap_or(false);
        let replacement = if checked { "[ ]" } else { "[x]" };
        let text = self.mirror.to_string();
        let start = SourceByte::new(box_span.start).to_scalar(&text)?;
        let end = SourceByte::new(box_span.end).to_scalar(&text)?;
        let selection = self.selection;
        let edit = ScalarEdit {
            start,
            end,
            replacement: replacement.to_owned(),
        };
        let request = self.compose_edits(vec![edit])?;
        // 等长替换：原选区坐标不变；恢复后投影会按新 source 重建（显示新勾选态）。
        self.set_selection(selection)?;
        Ok(Some(request))
    }

    /// 表格「加行」（ADR 0019 S3b）：在最后一条数据行之后插入一条**空数据行**。
    ///
    /// 契约（写死，测试钉住）：
    ///
    /// - 只插入、不替换：表头/分隔行/已有数据行**逐字节不变**，也不复制上一行内容；
    /// - 新行的列数与表格一致、每个 cell 为空（[`table_edit::empty_row_text`]）；
    /// - 一次点击产生**一个** `EditRequest`（单 `ScalarEdit`），由会话默认撤销组进
    ///   undo（一次 `Ctrl+Z` 就回到原表）；
    /// - caret/选区**不因结构操作跳走**：端点按插入点做文本位置映射（表格末尾 →
    ///   新表格末尾），caret 绝不会落进表格源码内部（对象内部没有文本位置）；表格
    ///   之外的 caret/选区不受影响；
    /// - 不切换源码模式：表格仍按 Object Block 投影（`object_edit_line` 不变）；
    /// - 不是表格对象 / 行号上没有对象 → `Ok(None)`（不产生请求，不改文本）。
    pub fn append_table_row(&mut self, line: usize) -> Result<Option<EditRequest>, EditorError> {
        self.ensure_editable()?;
        self.ensure_no_cell_edit()?;
        let Some(table) = self.table_object_at(line) else {
            return Ok(None);
        };
        let source = self.mirror.to_string();
        let insertions = table_edit::append_row_insertions(&source, &table);
        self.compose_table_insertions(&source, insertions)
    }

    /// 表格「加列」（ADR 0019 S3b）：给表头、分隔行与**每一条数据行**末尾追加一列。
    ///
    /// 契约（写死，测试钉住）：
    ///
    /// - 每行都在**行内容末尾**插入（只插入、不替换）：已有 cell 的字节、空白与
    ///   对齐标记逐字保留；分隔行追加默认对齐（无冒号）的 `---`；
    /// - 新 cell 为空；没有数据行的表格（只有表头 + 分隔行）也能加列；
    /// - 一次点击产生**一个** `EditRequest`（每条行末尾一个互不重叠的 `ScalarEdit`，
    ///   按位置升序），整批进同一个撤销组；
    /// - caret/选区规则与 [`Self::append_table_row`] 相同（插入点按文本位置映射，
    ///   caret 不会落进表格内部）；
    /// - 不是表格对象 / 行号上没有对象 → `Ok(None)`。
    pub fn append_table_column(&mut self, line: usize) -> Result<Option<EditRequest>, EditorError> {
        self.ensure_editable()?;
        self.ensure_no_cell_edit()?;
        let Some(table) = self.table_object_at(line) else {
            return Ok(None);
        };
        let source = self.mirror.to_string();
        let insertions = table_edit::append_column_insertions(&source, &table);
        self.compose_table_insertions(&source, insertions)
    }

    /// 在当前 caret / 选区处插入一张**默认 GFM 表格**（ADR 0019 S3b4：正文右键
    /// 「插入 → 表格」）。
    ///
    /// 契约（写死，测试钉住）：
    ///
    /// - 插入文本由 [`table_edit::new_table_insertion`] 给出：2 列 × 1 条数据行、
    ///   cell 全空、分隔行 `---`（默认对齐）；**前后保证独占行块**（文档开头/末尾或
    ///   空行处不重复补空行）；
    /// - 一个 `EditRequest`（单 `ScalarEdit`）：`start == end` 是纯插入，非空选区则
    ///   替换选区（与打字语义一致），由会话默认撤销组进 undo；
    /// - caret 落在**插入文本之后** = 表格之后（插入文本恒以行尾结束，表格对象内部
    ///   没有文本位置）；
    /// - 插入完成即按新 source 重扫 Markdown：表格是整块 Object 时下一帧就按 Object
    ///   投影（不切换源码模式；如果在代码块等不投影的上下文里插入，源码仍是合法文本）；
    /// - 不是可编辑状态（持闸/在途命令/IME 组合/单元格编辑）→ 对应错误，不产生请求。
    pub fn insert_table(&mut self) -> Result<EditRequest, EditorError> {
        self.ensure_editable()?;
        self.ensure_no_cell_edit()?;
        let source = self.mirror.to_string();
        let selection = self.selection;
        let start = selection.start().to_source_byte(&source)?.get();
        let end = selection.end().to_source_byte(&source)?.get();
        let replacement = table_edit::new_table_insertion(&source, start, end)?;
        let edit = ScalarEdit {
            start: selection.start(),
            end: selection.end(),
            replacement,
        };
        self.compose_edits(vec![edit])
    }

    /// 行号上的**整块表格**（不是表格 / 没有对象 → `None`）。克隆一份避免借用冲突。
    fn table_object_at(&self, line: usize) -> Option<MarkdownTable> {
        let object = self.markdown.object_at_line(line)?;
        if !object.kind().is_table() {
            return None;
        }
        self.markdown.object_table(object).cloned()
    }

    /// 表格「删除行」（ADR 0019 S3b）：删掉第 `row` 行（0 = 表头）**整行含行尾换行**。
    ///
    /// 契约（写死，测试钉住）：
    ///
    /// - **首版明确不删表头**：`row == 0` → `Ok(None)`（不产生请求、不改正文）；
    /// - 删除区间由 [`table_edit::delete_row_deletion`] 给出：整行内容 + 本行行尾换行，
    ///   因此**不留空行**；表格在文档末尾且没有行尾换行时改删前一行的行尾；
    /// - 一次点击产生**一个** `EditRequest`（单 `ScalarEdit`，替换成空串），由会话默认
    ///   撤销组进 undo（一次 `Ctrl+Z` 恢复原表）；
    /// - caret/选区按**删除区间**做文本位置映射，并夹到表格之外（对象内部没有文本
    ///   位置，绝不让 caret 落进表格源码内部）；表格之外的 caret/选区只按删除量左移；
    /// - 表格仍是 Object：不切换源码模式；
    /// - 不是表格对象 / 行号上没有对象 / 行越界 → `Ok(None)`。
    pub fn delete_table_row(
        &mut self,
        line: usize,
        row: usize,
    ) -> Result<Option<EditRequest>, EditorError> {
        self.ensure_editable()?;
        self.ensure_no_cell_edit()?;
        let Some(table) = self.table_object_at(line) else {
            return Ok(None);
        };
        if row == 0 || row >= table.total_rows() {
            return Ok(None);
        }
        let source = self.mirror.to_string();
        let Some(deletion) = table_edit::delete_row_deletion(&source, &table, row) else {
            return Ok(None);
        };
        self.compose_table_deletions(&source, line, vec![deletion])
    }

    /// 表格「删除列」（ADR 0019 S3b）：从表头、分隔行与所有数据行删掉第 `column` 列。
    ///
    /// 契约（写死，测试钉住）：
    ///
    /// - **至少保留 1 列**：`columns() <= 1` 或 `column` 越界 → `Ok(None)`（不把表格
    ///   改成非法结构：GFM 分隔行至少要有一个分隔 cell）；
    /// - 删除区间由 [`table_edit::delete_column_deletions`] 给出：每行只删本列文本 +
    ///   一侧分隔符与相邻空白，其它 cell 的字节、行首 `|`、分隔行的冒号（对齐）逐字保留；
    /// - 每行一条互不重叠的 `ScalarEdit`（按位置升序），整批进**同一个** `EditRequest`
    ///   = 一个撤销组；
    /// - caret/选区规则与 [`Self::delete_table_row`] 相同（按删除区间映射并夹到表格外）；
    /// - 不是表格对象 / 行号上没有对象 → `Ok(None)`。
    pub fn delete_table_column(
        &mut self,
        line: usize,
        column: usize,
    ) -> Result<Option<EditRequest>, EditorError> {
        self.ensure_editable()?;
        self.ensure_no_cell_edit()?;
        let Some(table) = self.table_object_at(line) else {
            return Ok(None);
        };
        if table.columns() <= 1 || column >= table.columns() {
            return Ok(None);
        }
        let source = self.mirror.to_string();
        let deletions = table_edit::delete_column_deletions(&source, &table, column);
        if deletions.is_empty() {
            return Ok(None);
        }
        self.compose_table_deletions(&source, line, deletions)
    }

    /// 表格「重排行」（ADR 0019 S3b5）：把第 `from` 条**数据行**移动到第 `to` 条。
    ///
    /// 契约（写死，测试钉住）：
    ///
    /// - `from` / `to` 都是**数据行下标**（0 = 第一条数据行；表头与分隔行不参与，恒不动）；
    /// - **只重排、不改内容**：整行内容按槽位移动，槽位之间的行尾换行留在原位 ——
    ///   于是其它行、其它列、表格之外的字节**逐字节不变**，表格总字节数也不变；
    /// - 一次拖动释放产生**一个** `EditRequest`（单条等长替换），由会话默认撤销组进
    ///   undo（一次 `Ctrl+Z` 回到原顺序）；
    /// - caret/选区**不变**：重排是等长置换，表格区间与 scalar 坐标都不动；只有落在
    ///   表格源码内部的位置会被夹到表格末尾（对象内部没有文本落点）；
    /// - 表格仍是 Object（不切换源码模式）；
    /// - 不是表格对象 / 越界 / 原地 / 少于 2 条数据行 → `Ok(None)`（不产生请求）。
    pub fn move_table_row(
        &mut self,
        line: usize,
        from: usize,
        to: usize,
    ) -> Result<Option<EditRequest>, EditorError> {
        self.ensure_editable()?;
        self.ensure_no_cell_edit()?;
        let Some(table) = self.table_object_at(line) else {
            return Ok(None);
        };
        if table.row_count() < 2 || from >= table.row_count() || to >= table.row_count() {
            return Ok(None);
        }
        let source = self.mirror.to_string();
        let replacements = table_edit::move_row_replacements(&source, &table, from, to);
        if replacements.is_empty() {
            return Ok(None);
        }
        self.compose_table_replacements(&source, line, replacements)
    }

    /// 表格「重排列」（ADR 0019 S3b5）：把第 `from` 列移动到第 `to` 列。
    ///
    /// 契约（写死，测试钉住）：
    ///
    /// - 表头 / 分隔行 / **每一条**数据行同时重排该列：cell 文本与分隔行的对齐标记
    ///   （`:-:` 等）一起移动，其它 cell 的字节、行首/行尾 `|` 与空白逐字节保留；
    /// - 每行一条互不重叠的等长替换（按位置升序），整批进**同一个** `EditRequest`
    ///   = 一个撤销组；表格总字节数不变；
    /// - caret/选区规则与 [`Self::move_table_row`] 相同（不变，只夹到表格之外）；
    /// - 缺列的行（GFM 把空列补在行尾）不猜：整次移动放弃（`Ok(None)`）；
    /// - 不是表格对象 / 越界 / 原地 / 少于 2 列 → `Ok(None)`。
    pub fn move_table_column(
        &mut self,
        line: usize,
        from: usize,
        to: usize,
    ) -> Result<Option<EditRequest>, EditorError> {
        self.ensure_editable()?;
        self.ensure_no_cell_edit()?;
        let Some(table) = self.table_object_at(line) else {
            return Ok(None);
        };
        if table.columns() < 2 || from >= table.columns() || to >= table.columns() {
            return Ok(None);
        }
        let source = self.mirror.to_string();
        let replacements = table_edit::move_column_replacements(&source, &table, from, to);
        if replacements.is_empty() {
            return Ok(None);
        }
        self.compose_table_replacements(&source, line, replacements)
    }

    /// 把一组「等长替换」变成一次 `compose_edits`（表格行列重排的公共尾巴）。
    ///
    /// 选区恢复语义：重排**等长**且表格源码区间不变，因此端点坐标本身仍然有效，
    /// 只把落在表格源码**内部**的端点夹到表格末尾（对象内部没有文本落点，与删除/插入
    /// 的既有契约一致）。表格之外的 caret/选区**一个字节都不动**。
    fn compose_table_replacements(
        &mut self,
        source: &str,
        line: usize,
        replacements: Vec<table_edit::TableReplacement>,
    ) -> Result<Option<EditRequest>, EditorError> {
        if replacements.is_empty() {
            return Ok(None);
        }
        let mut edits = Vec::with_capacity(replacements.len());
        for replacement in &replacements {
            let span = replacement.span();
            let start = SourceByte::new(span.start().get()).to_scalar(source)?;
            let end = SourceByte::new(span.end().get()).to_scalar(source)?;
            edits.push(ScalarEdit {
                start,
                end,
                replacement: replacement.text().to_owned(),
            });
        }
        let selection = self.selection;
        let request = self.compose_edits(edits)?;
        let mapped = self.clamp_selection_outside_table(selection, line);
        self.set_selection(mapped)?;
        Ok(Some(request))
    }

    /// 把一组「删除源码区间」变成一次 `compose_edits`（全部替换成空串）。
    ///
    /// 选区恢复语义：端点按**删除区间**做文本位置映射（区间之后左移删除量；落在区间
    /// 内部收敛到区间起点），随后再夹到**删除后的表格之外** —— 与插入不同，删除只会让
    /// 表格变短：caret 在表格之外时只按删除量左移；落在被删区间里（含整表末尾恰好被删
    /// 的情形）收敛到区间起点，通常就是新表格末尾或紧随其后的空行；万一仍落在表格源码
    /// 内部（陈旧/异常坐标），推到新表格末尾，绝不留在源码内部。
    fn compose_table_deletions(
        &mut self,
        source: &str,
        line: usize,
        deletions: Vec<table_edit::TableDeletion>,
    ) -> Result<Option<EditRequest>, EditorError> {
        if deletions.is_empty() {
            return Ok(None);
        }
        let mut edits = Vec::with_capacity(deletions.len());
        for deletion in &deletions {
            let span = deletion.span();
            let start = SourceByte::new(span.start().get()).to_scalar(source)?;
            let end = SourceByte::new(span.end().get()).to_scalar(source)?;
            edits.push(ScalarEdit {
                start,
                end,
                replacement: String::new(),
            });
        }
        let selection = self.selection;
        let request = self.compose_edits(edits)?;
        let mapped = shift_selection_through_deletions(selection, &request.edits);
        let mapped = self.clamp_selection_outside_table(mapped, line);
        self.set_selection(mapped)?;
        Ok(Some(request))
    }

    /// 把选区两端夹到 `line` 上表格的**源码区间之外**（严格在区间内部的位置推到表格末尾）。
    ///
    /// 对象内部没有文本落点：caret 落在表格源码区间里会被投影折回块首，用户下一次输入
    /// 就跑到表格前面去了（S3a/S3b 的既有契约）。
    fn clamp_selection_outside_table(&self, selection: Selection, line: usize) -> Selection {
        let Some(table) = self.table_object_at(line) else {
            return selection;
        };
        let text = self.mirror.to_string();
        let Ok(start) = SourceByte::new(table.span().start().get()).to_scalar(&text) else {
            return selection;
        };
        let Ok(end) = SourceByte::new(table.span().end().get()).to_scalar(&text) else {
            return selection;
        };
        let clamp = |position: UnicodeScalar| {
            if position.get() > start.get() && position.get() < end.get() {
                UnicodeScalar::new(end.get())
            } else {
                position
            }
        };
        Selection::new(clamp(selection.anchor()), clamp(selection.head()))
    }

    /// 把「字节位置 + 插入文本」变成一次 `compose_edits`（全部是空区间插入）。
    ///
    /// 选区恢复语义：端点按**文本位置**映射 —— 插入点在端点之前或正好等于端点时
    /// 端点一起平移（插入点上的 caret 被推到插入文本之后）。于是“表格末尾 → 新表格
    /// 末尾”，caret 永远不会落进表格源码内部（对象内部没有文本位置）。
    fn compose_table_insertions(
        &mut self,
        source: &str,
        insertions: Vec<table_edit::TableInsertion>,
    ) -> Result<Option<EditRequest>, EditorError> {
        if insertions.is_empty() {
            return Ok(None);
        }
        let mut edits = Vec::with_capacity(insertions.len());
        for insertion in &insertions {
            let at = SourceByte::new(insertion.at()).to_scalar(source)?;
            edits.push(ScalarEdit {
                start: at,
                end: at,
                replacement: insertion.text().to_owned(),
            });
        }
        let selection = self.selection;
        let request = self.compose_edits(edits)?;
        let shifted = shift_selection_through_insertions(selection, &request.edits);
        self.set_selection(shifted)?;
        Ok(Some(request))
    }

    // ---- 表格单元格就地编辑（ADR 0019 S3b2）----
    //
    // ## 为什么“编辑中的 cell 文本”先进镜像、提交时只发一条 `ScalarEdit`
    //
    // 编辑中的 cell 文本必须**立刻可见**，而 display 永远由镜像派生（投影层只认
    // source），所以每次按键都写镜像；但这些按键**不产生** `EditRequest`：
    //
    // - 一次单元格编辑在提交（Enter / Tab / 点击别处）时只产生**一条** `ScalarEdit`
    //   （替换整个 cell 源码区间）→ 会话里就是一个撤销组；
    // - `Esc` 取消直接恢复镜像的 cell 区间，**不产生任何请求**，也就没有“撤销一次
    //   取消”的垃圾历史；
    // - 因此“编辑期间镜像领先于会话”是**唯一**的临时不一致，且它有确定的收敛点：
    //   提交（一条请求）或取消（恢复原状）。远端 delta / 撤销 / 重做会在应用前先
    //   放弃这份草稿（见 [`Self::abort_cell_edit_draft`]），绝不带病套用 splice。
    //
    // ## 源码安全（GFM）
    //
    // 进入 cell 的替换文本统一经 [`table_cell_edit::sanitize_cell_text`]：换行 → 空格、
    // 未转义的 `|` → `\|`。否则一行会变成两行 / 一格变成两格，表格结构当场被改坏。

    /// 是否正在就地编辑某个表格单元格。
    #[must_use]
    pub fn is_editing_table_cell(&self) -> bool {
        self.cell_edit.is_some()
    }

    /// 正在编辑的单元格地址（行 0 = 表头）。
    #[must_use]
    pub fn table_cell_edit_target(&self) -> Option<TableCellTarget> {
        self.cell_edit.as_ref().map(|state| TableCellTarget {
            line: state.line,
            row: state.row,
            column: state.column,
        })
    }

    /// 正在编辑的 cell 文本在 display 上的字节区间（空 cell 返回 `None`）。
    ///
    /// app 用它判断“点击是否还落在这个 cell 里”，以及提交后把点击位置换算回新的
    /// display（表格重新变成一个占位符，长度会变）。
    #[must_use]
    pub fn table_cell_edit_display_range(&self) -> Option<Range<usize>> {
        let (start, end) = self.cell_scalars()?;
        let text = self.mirror.to_string();
        let start = UnicodeScalar::new(start).to_source_byte(&text).ok()?;
        let end = UnicodeScalar::new(end).to_source_byte(&text).ok()?;
        let start = self.projection.display_byte_at_source(start)?.get();
        let end = self.projection.display_byte_at_source(end)?.get();
        (start < end).then_some(start..end)
    }

    /// 正在编辑的 cell 在 display 上的**起点**字节（空 cell 也有落点，返回 seam 位置）。
    ///
    /// 与 [`Self::table_cell_edit_display_range`] 的区别：空 cell 的显示宽度是 0，
    /// 但仍有一个“虚拟行”可以点击/落 caret，所以这里不看宽度。
    #[must_use]
    pub fn table_cell_edit_display_anchor(&self) -> Option<usize> {
        let (start, _) = self.cell_scalars()?;
        let text = self.mirror.to_string();
        let start = UnicodeScalar::new(start).to_source_byte(&text).ok()?;
        Some(self.projection.display_byte_at_source(start)?.get())
    }

    /// 进入某个表格单元格的**就地编辑**（`F2` / `Enter` / 再次单击 / 双击）。
    ///
    /// 契约（写死，测试钉住）：
    ///
    /// - 只对**整块表格对象**生效（`ObjectBlock` 且是表格）；不是表格 / 行号上没有
    ///   对象 / 行列越界 → `Ok(false)`，不改任何状态；
    /// - 进入后 display 只显示该 cell 的源码原文（没有 `|`、没有整表源码）：投影把
    ///   表格其余部分全部隐藏（[`ProjectionCellReveal`]）；
    /// - caret 落在 cell 文本末尾；caret/选区此后一律被夹在 cell 内（表格其余部分
    ///   在 display 上不可见，也不接受落点）；
    /// - **不**改写 source：进入编辑本身不产生任何 `EditRequest`；
    /// - 空 cell 也可以进入（编辑时是一行空文本）；但**缺列补出来的**空区间
    ///   （cell 起点已在行内容末尾之外）拒绝进入 —— 往那里插字会把表格写坏。
    pub fn begin_table_cell_edit(
        &mut self,
        line: usize,
        row: usize,
        column: usize,
    ) -> Result<bool, EditorError> {
        if self.needs_resync {
            return Err(EditorError::ResyncRequired);
        }
        if self.preedit.is_some() {
            return Err(EditorError::PreeditActive);
        }
        if let Some(state) = &self.cell_edit {
            if state.line == line && state.row == row && state.column == column {
                return Ok(true);
            }
            // 从另一格切过来（app 的 Tab 会先提交再进入；这里是防御）：先恢复原格。
            self.cancel_table_cell_edit();
        }
        let Some(table) = self.table_object_at(line) else {
            return Ok(false);
        };
        let Some(cell) = table.cell(row, column) else {
            return Ok(false);
        };
        let Some(row_span) = table.row_span(row) else {
            return Ok(false);
        };
        if cell.start() >= row_span.end() {
            // 缺列补出来的空区间：行里根本没有这一格的 `|` 边界，插字会写坏表格。
            return Ok(false);
        }
        let text = self.mirror.to_string();
        let original_start = SourceByte::new(cell.start().get()).to_scalar(&text)?;
        let original_end = SourceByte::new(cell.end().get()).to_scalar(&text)?;
        let original_text = text
            .get(cell.start().get()..cell.end().get())
            .unwrap_or_default()
            .to_owned();
        self.cell_edit = Some(CellEditState {
            line,
            row,
            column,
            original_text,
            original_span: cell,
            original_start,
            original_end,
            base_len: text.len(),
        });
        // caret 落在 cell 文本末尾（空 cell 就是唯一的落点）。
        self.selection = Selection::caret(original_end);
        self.refresh_projection();
        Ok(true)
    }

    /// 提交当前单元格编辑，返回**唯一**一条 `EditRequest`（没有实际改动时返回 `None`）。
    ///
    /// 契约（写死，测试钉住）：
    ///
    /// - 请求只有**一条** `ScalarEdit`：把进入编辑前的 cell 源码区间替换成当前的
    ///   cell 源码区域（会话/镜像基线坐标），因此两侧 `|`、分隔行、其它 cell、对齐、
    ///   行尾换行**逐字节不变**；
    /// - 请求的 `edits` 用的是**进入编辑时**的坐标，而镜像此刻已经等于提交后的文本：
    ///   控制器不再二次应用，只等会话回 `local_echo` 确认（[`Self::apply_outcome`]）；
    /// - 文本与进入时完全相同 → 不发请求（空 op 请求不会推进 `live_rev`，会把单飞行
    ///   的 `pending` 卡死）；
    /// - 提交后 caret 落到**整表末尾**（对象内部没有文本落点，不能停在里面）；
    /// - 表格/单元格在编辑期间消失（语法被改坏）→ 不产生请求，直接退出。
    pub fn commit_table_cell_edit(&mut self) -> Result<Option<EditRequest>, EditorError> {
        let Some(state) = self.cell_edit.clone() else {
            return Ok(None);
        };
        if self.needs_resync {
            return Err(EditorError::ResyncRequired);
        }
        if let Some(command_id) = self.pending.clone() {
            // 单飞行：单元格编辑期间不允许有在途请求，走到这里说明状态不对，不猜。
            return Err(EditorError::CommandInFlight(command_id));
        }
        let Some(replacement) = self.cell_region_text(&state) else {
            // 表格/单元格已经不存在：直接退出编辑态（不产生请求）。
            self.cell_edit = None;
            self.refresh_projection();
            return Ok(None);
        };
        self.cell_edit = None;
        let request = if replacement == state.original_text {
            None
        } else {
            let command_id = self.next_command_id();
            let base_live_rev = self.snapshot.live_rev;
            self.pending = Some(command_id.clone());
            Some(EditRequest {
                doc_id: self.snapshot.doc_id.clone(),
                generation: self.snapshot.generation,
                session_epoch: self.snapshot.session_epoch,
                command_id,
                base_live_rev,
                edits: vec![ScalarEdit {
                    start: state.original_start,
                    end: state.original_end,
                    replacement,
                }],
                origin: LOCAL_ORIGIN.to_owned(),
                undo_group: None,
            })
        };
        self.move_caret_to_object_end(state.line);
        self.refresh_projection();
        Ok(request)
    }

    /// 取消当前单元格编辑：把镜像里的 cell 区域恢复成进入前的原文，**不产生请求**。
    ///
    /// 返回是否真的取消过（不在编辑态时 `false`）。`Esc` / `Ctrl+Z`（编辑中）走这里；
    /// 点击别处走的是**提交**（[`Self::commit_table_cell_edit`]），不是取消。
    pub fn cancel_table_cell_edit(&mut self) -> bool {
        let Some(state) = self.cell_edit.take() else {
            return false;
        };
        if let Some(region) = self.current_cell_region(&state) {
            if region != state.original_text {
                let start = self.mirror.byte_to_char(state.original_span.start().get());
                let end = self
                    .mirror
                    .byte_to_char(state.original_span.start().get() + region.len());
                if start < end {
                    self.mirror.remove(start..end);
                }
                self.mirror.insert(start, &state.original_text);
            }
        }
        // 恢复后重新扫描：失败（理论上不可达）时保持镜像，仍退出编辑态。
        let _ = self.refresh_markdown();
        self.move_caret_to_object_end(state.line);
        self.refresh_projection();
        true
    }

    /// Tab / Shift+Tab：提交当前格并进入下一格（到表格末尾就退出编辑）。
    ///
    /// 返回 `(提交产生的请求, 是否仍在编辑下一格)`。导航按**最新解析**的行列数算，
    /// 因此加行/加列之后的格子也在这条路径上。
    pub fn advance_table_cell_edit(
        &mut self,
        forward: bool,
    ) -> Result<(Option<EditRequest>, bool), EditorError> {
        let Some(state) = self.cell_edit.clone() else {
            return Ok((None, false));
        };
        let request = self.commit_table_cell_edit()?;
        let Some(table) = self.table_object_at(state.line) else {
            return Ok((request, false));
        };
        let next = if forward {
            table_cell_edit::next_cell(state.row, state.column, table.row_count(), table.columns())
        } else {
            table_cell_edit::previous_cell(
                state.row,
                state.column,
                table.row_count(),
                table.columns(),
            )
        };
        let Some(next) = next else {
            // 表头之前 / 末格之后：留在表格外（caret 已经在整表末尾）。
            return Ok((request, false));
        };
        let entered = self.begin_table_cell_edit(state.line, next.row, next.column)?;
        Ok((request, entered))
    }

    /// 用一段完整文本替换某个单元格（app 表格浮层就地编辑的**提交**路径）。
    ///
    /// 与 [`Self::begin_table_cell_edit`] 的区别：**不进入**控制器内部的 cell 编辑态，
    /// 因此表格始终是 Object 投影（display 不会塌缩成一行 cell 文本）。浮层自己持有
    /// 草稿文本，提交时调用这里一次性构造“替换整个 cell 源码区间”的 `EditRequest`。
    ///
    /// 契约：
    ///
    /// - 文本先经 [`table_cell_edit::sanitize_cell_text`]（换行 → 空格、未转义 `|` → `\|`）；
    /// - 与当前 cell 源码逐字相同 → `Ok(None)`（空 op 不会推进 `live_rev`）；
    /// - 一次调用只有**一条** `ScalarEdit`（两侧 `|`、对齐、其它 cell、行尾换行不变），
    ///   会话里就是一个撤销组，caret 落到整表末尾（对象内部没有文本落点）；
    /// - 缺列补出来的空区间（cell 起点在行内容末尾之外）拒绝 → `Ok(None)`，不写坏表格；
    /// - 表格/单元格不存在或行列越界 → `Ok(None)`（陈旧帧不猜）。
    pub fn replace_table_cell_text(
        &mut self,
        line: usize,
        row: usize,
        column: usize,
        text: &str,
    ) -> Result<Option<EditRequest>, EditorError> {
        self.ensure_editable()?;
        self.ensure_no_cell_edit()?;
        let Some(table) = self.table_object_at(line) else {
            return Ok(None);
        };
        let Some(cell) = table.cell(row, column) else {
            return Ok(None);
        };
        let Some(row_span) = table.row_span(row) else {
            return Ok(None);
        };
        if cell.start() >= row_span.end() {
            // 缺列补出来的空区间：行里没有这一格的 `|` 边界，插字会把表格写坏。
            return Ok(None);
        }
        let source = self.mirror.to_string();
        let start_byte = cell.start().get();
        let end_byte = cell.end().get();
        let original = source.get(start_byte..end_byte).unwrap_or_default();
        let replacement = table_cell_edit::sanitize_cell_text(text);
        if replacement == original {
            return Ok(None);
        }
        let start = SourceByte::new(start_byte).to_scalar(&source)?;
        let end = SourceByte::new(end_byte).to_scalar(&source)?;
        let request = self.compose_edits(vec![ScalarEdit {
            start,
            end,
            replacement,
        }])?;
        self.move_caret_to_object_end(line);
        self.refresh_projection();
        Ok(Some(request))
    }

    /// 单元格编辑期间放弃草稿（远端 delta / 撤销 / 重做要按会话坐标改镜像之前调用）。
    ///
    /// 恢复镜像到基线文本（= 会话当前文本），因此随后的 `splice` 坐标重新合法。
    pub(crate) fn abort_cell_edit_draft(&mut self) {
        if self.cell_edit.is_some() {
            self.cancel_table_cell_edit();
        }
    }

    /// 当前 cell 的 `(起 scalar, 止 scalar)`（镜像坐标；不在编辑态/表格没了 → `None`）。
    fn cell_scalars(&self) -> Option<(usize, usize)> {
        let state = self.cell_edit.as_ref()?;
        let table = self.table_object_at(state.line)?;
        let cell = table.cell(state.row, state.column)?;
        let start = self
            .mirror
            .byte_to_char(cell.start().get().min(self.mirror.len_bytes()));
        let end = self
            .mirror
            .byte_to_char(cell.end().get().min(self.mirror.len_bytes()));
        Some((start, end.max(start)))
    }

    /// 镜像里 cell 区域的**当前**文本（按原始区间起点 + 长度增量切）。
    ///
    /// 不能用“最新解析的 cell 区间”切片：解析器会裁掉 cell 首尾空白，用户敲进去的
    /// 前导/尾随空格不在区间里，提交时就会与镜像不一致（会话与镜像分叉）。
    fn current_cell_region(&self, state: &CellEditState) -> Option<String> {
        let text = self.mirror.to_string();
        let delta = text.len() as isize - state.base_len as isize;
        let len = (state.original_span.len() as isize + delta).max(0) as usize;
        let start = state.original_span.start().get();
        if start > text.len() {
            return None;
        }
        // 长度可能因为用户输入越界（极端情况）——夹回文本内，绝不 panic。
        let end = (start + len).min(text.len());
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            return None;
        }
        Some(text[start..end].to_owned())
    }

    /// 提交用的 cell 区域文本（表格/单元格仍在时才有意义）。
    fn cell_region_text(&self, state: &CellEditState) -> Option<String> {
        // 先确认表格与单元格仍在最新解析里（表格被改坏时不再提交）。
        let table = self.table_object_at(state.line)?;
        table.cell(state.row, state.column)?;
        self.current_cell_region(state)
    }

    /// caret 落到某个整块对象的末尾（对象内部没有文本落点）。
    fn move_caret_to_object_end(&mut self, line: usize) {
        let end = self
            .markdown
            .object_at_line(line)
            .map(|object| object.span().end().get())
            .unwrap_or_else(|| self.mirror.len_bytes());
        let end = end.min(self.mirror.len_bytes());
        let scalar = self.mirror.byte_to_char(end);
        self.selection = Selection::caret(UnicodeScalar::new(scalar));
    }

    /// 把选区两端夹进当前 cell 文本（cell 之外在 display 上不可见，不接受落点）。
    fn clamp_selection_to_cell(&self, selection: Selection) -> Selection {
        let Some((start, end)) = self.cell_scalars() else {
            return selection;
        };
        let clamp = |value: UnicodeScalar| UnicodeScalar::new(value.get().clamp(start, end));
        let (anchor, head) = (clamp(selection.anchor()), clamp(selection.head()));
        if anchor == selection.anchor() && head == selection.head() {
            selection
        } else {
            Selection::new(anchor, head)
        }
    }

    /// 单元格文本被解析器裁掉首尾空白后，caret 可能落在区间之外 → 收回 cell 内。
    fn clamp_caret_to_cell(&mut self) {
        let clamped = self.clamp_selection_to_cell(self.selection);
        if clamped != self.selection {
            self.selection = clamped;
            self.refresh_projection();
        }
    }

    /// 当前 cell 编辑对应的投影显现（表格/单元格已不存在 → `None`）。
    fn cell_reveal(&self) -> Option<ProjectionCellReveal> {
        let state = self.cell_edit.as_ref()?;
        let object = self.markdown.object_at_line(state.line)?;
        if !object.kind().is_table() {
            return None;
        }
        let table = self.markdown.object_table(object)?;
        Some(ProjectionCellReveal {
            table: object.span(),
            cell: table.cell(state.row, state.column)?,
        })
    }

    /// 把一组编辑**只写进镜像**（cell 草稿），不产生 `EditRequest`。
    ///
    /// 替换文本先经 [`table_cell_edit::sanitize_cell_text`]（换行 → 空格、未转义 `|`
    /// → `\|`），因此 cell 草稿永远是“表格结构安全”的源码；`start`/`end` 用清理后的
    /// 长度更新 caret，保证 IME / 连续输入的下一个落点正确。
    fn apply_cell_draft_edits(&mut self, edits: Vec<ScalarEdit>) -> Result<(), EditorError> {
        self.ensure_editable()?;
        if edits.is_empty() {
            return Err(EditorError::EmptyEditSet);
        }
        self.validate_edits(&edits)?;
        let Some((cell_start, cell_end)) = self.cell_scalars() else {
            return Err(EditorError::CellEditInactive);
        };
        // caret/选区已经被夹进 cell，但**按 grapheme 删**的目标可能跨出边界（例如 caret
        // 停在 cell 末尾按 Delete，下一个 grapheme 是 cell 之后的空格）：夹回 cell 内，
        // 越界部分直接丢弃（对象内部/表格其它部分没有文本落点）。
        let clamped: Vec<ScalarEdit> = edits
            .into_iter()
            .map(|edit| {
                let start = edit.start.get().clamp(cell_start, cell_end);
                let end = edit.end.get().clamp(start, cell_end);
                ScalarEdit {
                    start: UnicodeScalar::new(start),
                    end: UnicodeScalar::new(end),
                    replacement: edit.replacement,
                }
            })
            .filter(|edit| edit.start != edit.end || !edit.replacement.is_empty())
            .collect();
        if clamped.is_empty() {
            return Ok(());
        }
        let sanitized: Vec<ScalarEdit> = clamped
            .into_iter()
            .map(|edit| ScalarEdit {
                start: edit.start,
                end: edit.end,
                replacement: table_cell_edit::sanitize_cell_text(&edit.replacement),
            })
            .collect();
        self.apply_scalar_edits(&sanitized)?;
        self.clamp_caret_to_cell();
        Ok(())
    }

    /// 一次编辑：有 cell 草稿 → 只写镜像（`Ok(None)`）；否则走普通 `compose_edits`。
    fn submit_or_draft(
        &mut self,
        edits: Vec<ScalarEdit>,
    ) -> Result<Option<EditRequest>, EditorError> {
        if self.cell_edit.is_some() {
            self.apply_cell_draft_edits(edits)?;
            return Ok(None);
        }
        self.compose_edits(edits).map(Some)
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

    /// 带左右偏置的 display → source（命中用；语义见 [`crate::Bias`]）。
    #[must_use]
    pub fn display_to_source_biased(
        &self,
        display: DisplayByte,
        bias: crate::Bias,
    ) -> Option<SourceByte> {
        self.projection.source_byte_at_display_biased(display, bias)
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
        // 单元格草稿还没提交：即使 durable 已经追平当前 live_rev，也不算“已保存”。
        self.pending.is_none()
            && self.cell_edit.is_none()
            && self.durable_through >= self.snapshot.live_rev
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
        // cell 就地编辑：caret/选区只允许落在 cell 文本内（表格其余部分在 display 上
        // 不可见，也没有文本落点）。
        let selection = self.clamp_selection_to_cell(selection);
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
        self.submit_or_draft(vec![edit])
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
        self.submit_or_draft(vec![edit])
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
        // cell 草稿期间任何“通用编辑入口”都必须在会话坐标之外排队（见 cell 编辑小节）。
        self.ensure_no_cell_edit()?;
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
        match self.submit_or_draft(vec![edit]) {
            Ok(request) => Ok(request),
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

    /// 结构类操作（加行加列 / 删除对象 / 任务框 / 列表 Enter / 源码模式）在单元格
    /// 编辑期间一律拒绝：它们会按会话坐标改正文，而镜像此刻还压着未提交的 cell 草稿。
    fn ensure_no_cell_edit(&self) -> Result<(), EditorError> {
        if self.cell_edit.is_some() {
            return Err(EditorError::CellEditActive);
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
        // 图片/表格语法被改坏、行被删掉时，源码模式自动退出（不再有对应对象）。
        if let Some(line) = self.object_edit_line {
            if self.markdown.object_at_line(line).is_none() {
                self.object_edit_line = None;
            }
        }
        // 单元格编辑同理：表格/该格已不存在时退出编辑态（不猜新坐标）。
        if self.cell_edit.is_some() && self.cell_reveal().is_none() {
            self.cell_edit = None;
        }
        // 块级语义只依赖 source：与投影一起在 source 变化时重建。
        self.block_runs = self.markdown.block_runs()?;
        self.refresh_projection();
        Ok(())
    }

    /// selection 变：复用已缓存的 MarkdownDocument，只重建投影与样式区间。
    ///
    /// 投影 / 样式构造都不会失败（选区非法时 `ProjectionMap::from_markdown_with_lists`
    /// 保守退回 identity）；列表 marker 的显现状态与投影一起在这里一次性刷新。
    fn refresh_projection(&mut self) {
        let revealed: Vec<usize> = self.object_edit_line.into_iter().collect();
        let cell = self.cell_reveal();
        self.projection = ProjectionMap::from_markdown_with_lists_and_objects_and_cell(
            &self.markdown,
            self.selection,
            &revealed,
            cell,
        );
        let style_runs = self.markdown.style_runs(&self.projection);
        self.style_runs = style_runs;
        self.list_marker_revealed = self
            .markdown
            .list_marker_reveals(self.selection)
            .unwrap_or_default();
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
        // 镜像里可能压着未提交的 cell 草稿；splice 是会话坐标，必须先恢复基线。
        self.abort_cell_edit_draft();
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

/// 列表 Enter 新 item 的源码 marker：无序沿用 bullet，有序 +1。
fn next_marker(source: &str, item: &menghunote_markdown::ListItem) -> String {
    if item.ordered {
        format!("{}{} ", item.number.saturating_add(1), item.delimiter)
    } else {
        source
            .get(item.marker.start..item.marker.end)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{} ", item.delimiter))
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

/// 把 selection 两端按**一组纯插入**（`start == end`）变换（ADR 0019 S3b）。
///
/// 语义（写死，测试钉住）：
///
/// - 插入点**在端点之前或正好等于端点** → 端点按插入量平移。这是“文本位置不变”的
///   语义：表格选中时 caret 在整表末尾，而新行/新列恰好插在那里，平移后 caret 仍
///   停在**新表格的末尾**；不这样做的话 caret 会落进表格内部（对象内部没有文本
///   位置，投影会把 caret 折回块首，用户下一次输入就跑到表格前面去了）；
/// - 插入点在端点之后 → 不变（表格之前的 caret 不因加行加列而移动）。
///
/// 只接受纯插入：调用方是 [`EditorController::compose_table_insertions`]，
/// 替换/删除走的仍是 [`transform_selection`]。
fn shift_selection_through_insertions(selection: Selection, edits: &[ScalarEdit]) -> Selection {
    let shift = |position: usize| -> usize {
        let mut offset = 0usize;
        for edit in edits {
            debug_assert_eq!(
                edit.start.get(),
                edit.end.get(),
                "结构插入必须都是空区间（纯插入）"
            );
            if edit.start.get() <= position {
                offset = offset.saturating_add(units::scalar_len(&edit.replacement));
            }
        }
        position.saturating_add(offset)
    };
    Selection::new(
        UnicodeScalar::new(shift(selection.anchor().get())),
        UnicodeScalar::new(shift(selection.head().get())),
    )
}

/// 把 selection 两端按**一组删除**（`replacement` 为空）做文本位置映射（ADR 0019 S3b3）。
///
/// 语义（写死，测试钉住）：
///
/// - 端点**在删除区间之后**（含正好等于区间终点）→ 左移删除量（表格外的正文不会被
///   多删/少移）；
/// - 端点**落在删除区间内部** → 收敛到区间起点（该点在新文本里仍然存在；之后由调用方
///   再夹到表格之外）；
/// - 端点**在删除区间之前**（含正好等于区间起点）→ 不变；
/// - 多个区间按位置升序，逐个累计左移量。
///
/// 只接受纯删除：调用方是 [`EditorController::compose_table_deletions`]；插入走的仍是
/// [`shift_selection_through_insertions`]，替换走 [`transform_selection`]。
fn shift_selection_through_deletions(selection: Selection, edits: &[ScalarEdit]) -> Selection {
    let shift = |position: usize| -> usize {
        let mut removed = 0usize;
        for edit in edits {
            debug_assert!(
                edit.replacement.is_empty(),
                "结构性删除必须都是纯删除（替换成空串）"
            );
            let start = edit.start.get();
            let end = edit.end.get();
            let inserted = units::scalar_len(&edit.replacement);
            if position >= end {
                removed =
                    removed.saturating_add(end.saturating_sub(start).saturating_sub(inserted));
            } else if position > start {
                // 删除区间内部：收敛到区间起点（已删除量在它之前，先减掉）。
                return start.saturating_sub(removed);
            }
        }
        position.saturating_sub(removed)
    };
    Selection::new(
        UnicodeScalar::new(shift(selection.anchor().get())),
        UnicodeScalar::new(shift(selection.head().get())),
    )
}
