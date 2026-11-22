//! Markdown 语义扫描 + 可隐藏区间（S2 第一层，**纯模型**）。
//!
//! 依据 ADR 0002 §4/§5：源码即真源，渲染层只是投影。本模块把源码扫成
//! **构造（[`MarkdownConstruct`]）**：每个构造记录整段 source 区间、要隐藏的
//! 标记区间、以及内容区间；再按 caret / 选区算出**当前应隐藏哪些字节区间**。
//! 渲染层（S2 第二层，`crates/app`）只消费这些区间，不自己解析 Markdown。
//!
//! 本模块不依赖 GPUI / storage，也不做任何布局、命中或绘制；单位是 UTF-8 字节，
//! 与 [`menghunote_text::SourceRange`] 同一套强类型坐标。
//!
//! # 支持的子集
//!
//! 块级（逐行扫描；源码按 LF 归一）：
//!
//! | 构造 | 识别规则 | 标记区间 |
//! | --- | --- | --- |
//! | ATX 标题 | 行首（≤3 空格后）1–6 个 `#` + 空格或行尾 | `#` 连同后面的一个空格 |
//! | 引用（可嵌套） | 行首（≤3 空格后）每层 `>` 及可选一个空格 | 每层前缀各一段 |
//! | 列表项 | `- ` / `* ` / `+ ` / `1. `（1–9 位数字 + `. `） | 标记本身 |
//! | 围栏代码块 | 行首（≤3 空格后）≥3 个 `` ` `` 或 `~`，块内**不解析行内** | 开/闭围栏整行 |
//!
//! 行内（逐行扫描，**不跨行**）：
//!
//! | 构造 | 语法 | 标记区间 | 内容区间 |
//! | --- | --- | --- | --- |
//! | 粗体 | `**text**` | 两对 `**` | 中间的 text |
//! | 斜体 | `*text*` / `_text_` | 两个单字符定界符 | 中间的 text |
//! | 行内代码 | `` `text` ``（反引号数相同才闭合） | 两段反引号 | 中间的 text |
//! | 删除线 | `~~text~~` | 两对 `~~` | 中间的 text |
//! | 链接 | `[text](url)` | `[` 与 `](url)` | 中间的 text |
//!
//! 额外规则：
//!
//! - **未闭合 / 不完整标记不产生构造**（因此绝不隐藏）；
//! - 反斜杠转义：`\*`、`` \` ``、`\[` 等被转义的字符不构成标记；
//!   行内代码内部按 CommonMark 视为原文，反斜杠不特殊；
//! - **行内构造的文本内容会递归解析行内构造**（如 `**a `code` b**` 里的
//!   `code`、`**a *b* c**` 里的 `*b*`）；行内代码内部**不解析**任何其它标记
//!   （代码内容始终是原文）；
//! - 相邻构造（`**a****b**`）按最左优先匹配；
//! - 围栏代码块内部不产生任何块级/行内构造；**未闭合围栏不产生构造**
//!   （这一段按保守处理，整段不隐藏也不解析，绝不猜测）。
//!
//! # 明确不支持（不要误以为全覆盖）
//!
//! - 表格、HTML（行内或块级）、图片 `![]()`、脚注、任务列表、Setext 标题、
//!   分隔线、缩进代码块、自动链接；
//! - **同种定界符的嵌套 / 交叠**（如 `**a **b** c**`、`*a *b* c*`）、
//!   `***a***`、`__bold__`；
//! - 行内构造跨行、列表续行、引用惰性续行、围栏代码块嵌套在引用/列表里；
//! - 链接文本里的嵌套方括号、URL 里的括号配对（取第一个未转义的 `)`）；
//! - 脚注/引用式链接等扩展语法；
//! - 任何 Unicode 归一化（NFD/NFC）：范围原样映射，不重写源码。
//!
//! 这些在 S2 后续切片按需增量支持；当前实现宁可**少隐藏**，也不错误隐藏内容。
//!
//! # 显现规则
//!
//! [`MarkdownDocument::hidden_ranges_for_caret`] /
//! [`MarkdownDocument::hidden_ranges_for_selection`] 返回“当前应隐藏的区间”：
//!
//! - caret 落在某构造的整段区间内（含两端）→ 该构造的标记全部可见；
//! - 非空选区与构造整段区间相交 → 该构造的标记全部可见（多行选区会显现所有
//!   相交构造），其它构造的标记保持隐藏；
//! - 返回区间**有序、不重叠、非空、都在 source 范围内**，且只可能是构造的标记
//!   区间（绝不覆盖内容文字）；同一输入的结果确定。
//!
//! 注意：块级构造（标题/引用/列表/围栏）的整段区间覆盖整行，因此 caret 在行内
//! 任何位置都会显现该行的块级标记（与 Typora 式“进入构造显现标记”一致）。

use std::error::Error;
use std::fmt;

use menghunote_markdown::{self as semantic, ParsedMarkdown, Span as SemanticSpan};
use menghunote_text::{CoordError, SourceByte, SourceRange, UnicodeScalar};

use crate::model::block::BlockRun;
use crate::model::object::{ObjectBlock, ObjectKind};
use crate::model::projection::{ProjectionMap, ProjectionPiece};
use crate::model::selection::Selection;
use crate::model::style::StyleRun;

/// 任务框（`[ ]` / `[x]`）在**隐藏态**下的 display 占位文本（ADR 0019 S3）。
///
/// 2 个不换行空格（U+00A0）而不是可见的 `☐`：
///
/// - 绘制层会在该 display 区间上**自绘**复选框（主题色描边/勾），占位必须是不可见的；
///   用可见字符会在字体缺字时露出豆腐块，也会与自绘方框重影；
/// - NBSP 不参与断行，复选框不会在折行处被拆成两半；
/// - 宽度约等于一个方框（正文 15px 等宽字体下约 16px），内容列/续行 hanging indent
///   仍按**真实 shaping** 测量（app `block_metrics` 用同一段文本量宽度），不做字符数估算。
///
/// 它只出现在 display 串里；source 仍是 3 个字节的 `[ ]`/`[x]`，编辑永远写 source。
pub const TASK_BOX_DISPLAY: &str = "\u{00A0}\u{00A0}";

/// Markdown 构造类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConstructKind {
    /// ATX 标题（`#` … `######`）。
    Heading,
    /// 引用（可嵌套，一个构造记录所有层前缀）。
    BlockQuote,
    /// 列表项标记（`- ` / `* ` / `+ ` / `1. `）。
    ListItem,
    /// 围栏代码块（``` / ~~~）。
    FencedCode,
    /// `**bold**`。
    Bold,
    /// `*em*` / `_em_`。
    Emphasis,
    /// `` `code` ``。
    InlineCode,
    /// `~~strike~~`。
    Strike,
    /// `[text](url)`。
    Link,
    /// `[[Note]]` / `[[Note|alias]]`（wiki 链接；L1 只投影/点击）。
    WikiLink,
}

impl ConstructKind {
    /// 是否属于行内构造。
    #[must_use]
    pub const fn is_inline(self) -> bool {
        matches!(
            self,
            Self::Bold
                | Self::Emphasis
                | Self::InlineCode
                | Self::Strike
                | Self::Link
                | Self::WikiLink
        )
    }

    /// 是否属于块级构造（整行/整块范围，内部可再嵌套行内构造）。
    #[must_use]
    pub const fn is_block(self) -> bool {
        matches!(
            self,
            Self::Heading | Self::BlockQuote | Self::ListItem | Self::FencedCode
        )
    }
}

/// 一个 Markdown 构造：整段区间 + 要隐藏的标记区间 + 内容区间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownConstruct {
    kind: ConstructKind,
    span: SourceRange,
    markers: Vec<SourceRange>,
    content: SourceRange,
}

impl MarkdownConstruct {
    /// 校验并构造（区间端点/包含关系不合法时返回 `None`，由调用方保守跳过）。
    fn checked(
        kind: ConstructKind,
        span: SourceRange,
        markers: Vec<SourceRange>,
        content: SourceRange,
    ) -> Option<Self> {
        if span.is_empty() || markers.is_empty() {
            return None;
        }
        let mut previous_end = span.start().get();
        for marker in &markers {
            if marker.is_empty()
                || marker.start().get() < span.start().get()
                || marker.end().get() > span.end().get()
                || marker.start().get() < previous_end
            {
                return None;
            }
            previous_end = marker.end().get();
        }
        if content.start().get() < span.start().get() || content.end().get() > span.end().get() {
            return None;
        }
        Some(Self {
            kind,
            span,
            markers,
            content,
        })
    }

    /// 构造类型。
    #[must_use]
    pub const fn kind(&self) -> ConstructKind {
        self.kind
    }

    /// 整段 source 区间（含标记与内容）。
    #[must_use]
    pub const fn span(&self) -> SourceRange {
        self.span
    }

    /// 要隐藏的标记区间（非空、互不重叠、都在 `span` 内）。
    #[must_use]
    pub fn markers(&self) -> &[SourceRange] {
        &self.markers
    }

    /// 内容区间（不含自己这层的标记；可能为空，例如空标题/空链接文本）。
    #[must_use]
    pub const fn content(&self) -> SourceRange {
        self.content
    }
}

/// 一个普通 Markdown 链接（`[text](url)`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownLink {
    span: SourceRange,
    content: SourceRange,
    dest: String,
}

impl MarkdownLink {
    /// 完整链接区间。
    #[must_use]
    pub const fn span(&self) -> SourceRange {
        self.span
    }

    /// 方括号内文本区间。
    #[must_use]
    pub const fn content(&self) -> SourceRange {
        self.content
    }

    /// 解析后的目标 URL。
    #[must_use]
    pub fn dest(&self) -> &str {
        &self.dest
    }
}

/// 一条 wiki 链接（L1）。
///
/// 与 [`MarkdownImage`] 一样，它是构造之外的**附加元数据**：投影仍通过
/// `ConstructKind::WikiLink` 隐藏括号；这里保留 target/heading/alias 的 source
/// 区间，供 app 做“点击打开/未解析样式/重命名改写”。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownWikiLink {
    span: SourceRange,
    target: SourceRange,
    heading: Option<SourceRange>,
    alias: Option<SourceRange>,
    display: SourceRange,
}

impl MarkdownWikiLink {
    /// 整条链接（含 `[[`/`]]`）。
    #[must_use]
    pub const fn span(&self) -> SourceRange {
        self.span
    }

    /// 目标笔记区间（`[[#H]]` 时为空区间）。
    #[must_use]
    pub const fn target(&self) -> SourceRange {
        self.target
    }

    /// 目标笔记原文（从 source 切片；`[[#H]]` 为空）。
    #[must_use]
    pub fn target_text<'a>(&self, source: &'a str) -> &'a str {
        source
            .get(self.target.start().get()..self.target.end().get())
            .unwrap_or("")
    }

    /// `#` 后的 heading 区间。
    #[must_use]
    pub const fn heading(&self) -> Option<SourceRange> {
        self.heading
    }

    /// 别名的 source 区间。
    #[must_use]
    pub const fn alias(&self) -> Option<SourceRange> {
        self.alias
    }

    /// 投影后可见内容区间。
    #[must_use]
    pub const fn display(&self) -> SourceRange {
        self.display
    }
}

/// 一个图片构造（ADR 0019 S2a）。
///
/// 图片在 S1 里只是一个 `ConstructKind::Image`（editor **不**把它放进
/// `constructs`，避免被当成行内标记投影）。S2a 把它单独建模：
///
/// - [`Self::span`]：完整源码跨度 `![alt](dest)`（半开区间）；
/// - [`Self::alt`]：alt 内容区间（不含方括号），占位/缺失时显示它；
/// - [`Self::dest`]：`pulldown-cmark` 解析出的目标 URL（引用式图片也解析成真实
///   URL）；**只读**，正文逐字节保留；
/// - [`Self::line`]：图片起点所在源码行（0 起），app 用它把对象行映射到布局行；
/// - [`Self::block`]：是否**整行独占**（可以投影成 Object Block）。
///
/// 判定 `block` 的规则（写死，测试钉住）：
///
/// 1. 图片 span 必须从行内容起点开始（行首缩进/列表 marker/引用前缀都会破坏它）；
/// 2. 图片 span 之后到行内容结尾只能是空白（行尾空格/Tab 允许，不产生错位）；
/// 3. 该行必须是普通段落（不是标题/引用/列表/围栏/缩进代码/表格/HTML/分隔线），
///    且不在 [`ParsedMarkdown::opaque_spans`]（代码 / 表格 / 未支持块）里。
///
/// 不满足时 `block = false`：**首版保持源码原样显示**（混排/行内图片），
/// 绝不原子替换半行内容（否则会产生错位）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownImage {
    span: SourceRange,
    alt: SourceRange,
    dest: String,
    line: usize,
    block: bool,
}

impl MarkdownImage {
    /// 完整源码跨度（含 `![` / `](…)`）。
    #[must_use]
    pub const fn span(&self) -> SourceRange {
        self.span
    }

    /// alt 内容区间（不含方括号）。
    #[must_use]
    pub const fn alt(&self) -> SourceRange {
        self.alt
    }

    /// 解析后的目标 URL（`asset://<hex>` 等）。
    #[must_use]
    pub fn dest(&self) -> &str {
        &self.dest
    }

    /// 图片起点所在源码行（0 起）。
    #[must_use]
    pub const fn line(&self) -> usize {
        self.line
    }

    /// 是否整行独占、可投影成 Object Block。
    #[must_use]
    pub const fn is_block(&self) -> bool {
        self.block
    }
}

/// 一行 GFM 表格（表头行或数据行）的 source 语义（ADR 0019 S3a）。
///
/// `cells` 与 [`MarkdownTable::columns`] 等长：缺列由解析器补空区间，
/// 多列（GFM 会丢弃）不存在；cell 区间已去掉首尾空白，**不含** `|` 边界。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownTableRow {
    span: SourceRange,
    line: usize,
    cells: Vec<SourceRange>,
}

impl MarkdownTableRow {
    /// 整行内容区间（不含换行）。
    #[must_use]
    pub const fn span(&self) -> SourceRange {
        self.span
    }

    /// 源码行号（0 起）。
    #[must_use]
    pub const fn line(&self) -> usize {
        self.line
    }

    /// 每列 cell 的源码区间（长度 = 表格列数）。
    #[must_use]
    pub fn cells(&self) -> &[SourceRange] {
        &self.cells
    }

    /// 第 `column` 列的 cell 区间；越界返回 `None`（不猜列，也不 panic）。
    #[must_use]
    pub fn cell(&self, column: usize) -> Option<SourceRange> {
        self.cells.get(column).copied()
    }
}

/// 单元格就地编辑的**投影显现**（ADR 0019 S3b2）。
///
/// 只描述“哪张表的哪一格正在编辑”：投影据此把整表压成该 cell 的一行源码文本
/// （`Hidden` 前缀 + `Copy` cell + `Hidden` 后缀）。它不产生新文本，也不改源码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectionCellReveal {
    /// 整表源码区间（= 原来 `Object` 段的 source 跨度）。
    pub table: SourceRange,
    /// 正在编辑的 cell 源码区间（**不含**两侧 `|` 与空白）。
    pub cell: SourceRange,
}

/// 一个 GFM 表格块（ADR 0019 S3a：**只读渲染** + 编辑源码切换）。
///
/// 与 [`MarkdownImage`] 一样是**附加元数据**：投影仍通过
/// [`ObjectBlock`]（`SegmentKind::Object`）实现，本类型只提供整块/行/单元格的
/// source 区间、列数与对齐方式，**不产生任何新文本**。
///
/// # block 判定（写死，测试钉住）
///
/// 只有满足下列全部条件才是 Object Block（否则 `block = false`，首版原样显示源码）：
///
/// 1. 表格第一行是 `LineKind::Table`，且不在代码块/未支持块里（解析器保证）；
/// 2. 第一行是**普通顶层行**：不是引用、不在列表项里；
/// 3. 表格源码从该行**内容起点**开始（行首缩进不投影，避免与布局缩进打架）；
/// 4. 表格区间结束在该行内容终点（不含末尾换行），且至少覆盖表头 + 分隔行。
///
/// 引用/列表/缩进里的表格仍然有完整块树（`tables()` 可查），只是不投影成对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownTable {
    span: SourceRange,
    first_line: usize,
    last_line: usize,
    columns: usize,
    alignments: Vec<semantic::TableAlignment>,
    header: MarkdownTableRow,
    delimiter: SourceRange,
    rows: Vec<MarkdownTableRow>,
    block: bool,
}

impl MarkdownTable {
    /// 整表源码区间（表头行起点 .. 最后一行内容终点；**不含**末尾换行）。
    #[must_use]
    pub const fn span(&self) -> SourceRange {
        self.span
    }

    /// 表头行行号（0 起；也是对象行）。
    #[must_use]
    pub const fn line(&self) -> usize {
        self.first_line
    }

    /// 末行行号（0 起，含）。
    #[must_use]
    pub const fn end_line(&self) -> usize {
        self.last_line
    }

    /// 列数（由分隔行决定；恒 ≥ 1）。
    #[must_use]
    pub const fn columns(&self) -> usize {
        self.columns
    }

    /// 每列对齐方式（长度 = [`Self::columns`]）。
    #[must_use]
    pub fn alignments(&self) -> &[semantic::TableAlignment] {
        &self.alignments
    }

    /// 第 `column` 列的对齐方式（越界按默认对齐，不 panic）。
    #[must_use]
    pub fn alignment(&self, column: usize) -> semantic::TableAlignment {
        self.alignments.get(column).copied().unwrap_or_default()
    }

    /// 表头行。
    #[must_use]
    pub const fn header(&self) -> &MarkdownTableRow {
        &self.header
    }

    /// 分隔行整行区间（不含换行）。
    #[must_use]
    pub const fn delimiter(&self) -> SourceRange {
        self.delimiter
    }

    /// 数据行（按源码顺序；不含表头与分隔行）。
    #[must_use]
    pub fn rows(&self) -> &[MarkdownTableRow] {
        &self.rows
    }

    /// 数据行数。
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// 第 `row` 条数据行；越界返回 `None`。
    #[must_use]
    pub fn row(&self, row: usize) -> Option<&MarkdownTableRow> {
        self.rows.get(row)
    }

    /// 第 `row` 行（**0 = 表头**，`1..=n` = 第 `row - 1` 条数据行）第 `column` 列的
    /// cell 源码区间；任一越界返回 `None`。
    ///
    /// 这是单元格就地编辑（ADR 0019 S3b2）对外唯一的行寻址方式：UI 说的“第几行”
    /// 含表头，而 [`Self::rows`] 不含表头，两套下标在 app 里混用必然错位。
    #[must_use]
    pub fn cell(&self, row: usize, column: usize) -> Option<SourceRange> {
        if row == 0 {
            self.header.cell(column)
        } else {
            self.rows.get(row - 1)?.cell(column)
        }
    }

    /// 第 `row` 行（0 = 表头）的整行源码区间；越界返回 `None`。
    #[must_use]
    pub fn row_span(&self, row: usize) -> Option<SourceRange> {
        if row == 0 {
            Some(self.header.span())
        } else {
            self.rows.get(row - 1).map(MarkdownTableRow::span)
        }
    }

    /// 总行数（含表头与分隔行的行号范围）：表头 + 数据行，用于 UI 越界校验。
    #[must_use]
    pub const fn total_rows(&self) -> usize {
        self.rows.len() + 1
    }

    /// 是否整行独占、可投影成 Object Block（见类型文档的判定规则）。
    #[must_use]
    pub const fn is_block(&self) -> bool {
        self.block
    }
}

/// Markdown 扫描 + 显现计算的全部错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownError {
    /// 坐标换算失败（caret/选区超出文本等）。
    Coord(CoordError),
    /// 内部区间构造失败（理论上不可达；不 panic 的防御分支）。
    InternalRange {
        start: usize,
        end: usize,
        len: usize,
    },
}

impl fmt::Display for MarkdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coord(err) => write!(f, "Markdown 坐标换算失败：{err}"),
            Self::InternalRange { start, end, len } => {
                write!(f, "Markdown 内部区间非法：[{start}, {end})，文本长度 {len}")
            }
        }
    }
}

impl Error for MarkdownError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Coord(err) => Some(err),
            Self::InternalRange { .. } => None,
        }
    }
}

impl From<CoordError> for MarkdownError {
    fn from(error: CoordError) -> Self {
        Self::Coord(error)
    }
}

/// 一份源码的 Markdown 构造表 + 显现计算。
///
/// **解析来源（ADR 0019 S1）**：
/// - 块级/行内构造优先由 [`menghunote_markdown`]（pulldown-cmark + span adapter）
///   生成；列表块树、逐行语义（[`Self::line_infos`]）也只来自它；
/// - 旧自研 scanner 保留为 **fallback**：当新解析器的行内构造与旧结果无法逐一对齐
///   （例如 `**a****b**` 这类旧 scanner 已专门处理的相邻定界符、`***` 等定界符
///   嵌套），整篇行内投影退回旧 scanner，保证“定位不确定的构造原样显示”；
/// - 两者都只产出 source 区间，**从不重新序列化 AST**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownDocument {
    source: String,
    constructs: Vec<MarkdownConstruct>,
    /// 图片构造（S2a）：object 投影与 app 的图片加载都从这里取。
    images: Vec<MarkdownImage>,
    /// 表格块（S3a）：object 投影与 app 的只读表格绘制都从这里取。
    tables: Vec<MarkdownTable>,
    /// 全部整块 Object（图片 + 表格，按 `span` 起点排序）：投影/选中/源码切换共用。
    objects: Vec<ObjectBlock>,
    /// wiki 链接（L1）：点击/未解析样式/重命名改写用。
    wiki_links: Vec<MarkdownWikiLink>,
    /// 普通链接（点击打开外链用）。
    links: Vec<MarkdownLink>,
    /// 新解析器的完整语义（列表/行信息），即使行内投影回退到旧 scanner 也保留。
    semantic: ParsedMarkdown,
    /// 行内投影是否退回旧 scanner（只影响 inline 构造，不影响列表/块级）。
    inline_fallback: bool,
}

impl MarkdownDocument {
    /// 解析源码。空文档、纯文本都返回空构造表（不报错）。
    pub fn parse(source: &str) -> Result<Self, MarkdownError> {
        let semantic = semantic::parse(source);
        let images = collect_images(source, &semantic);
        let tables = collect_tables(source, &semantic);
        let objects = collect_objects(&images, &tables);
        let wiki_links = collect_wiki_links(source, &semantic);
        let links = collect_links(source, &semantic);
        let legacy = legacy_constructs(source)?;
        let (mut constructs, inline_fallback) = merge_constructs(source, &semantic, legacy);
        // wiki 构造不依赖旧 scanner 的 inline fallback：即使整篇退回旧 scanner，
        // `[[...]]` 仍要投影，否则 fallback 文档里会突然显示双括号。
        constructs.extend(wiki_constructs(source, &semantic));
        constructs.sort_by_key(|construct| {
            (
                construct.span().start().get(),
                construct.span().end().get(),
                construct.kind(),
            )
        });
        constructs.dedup_by(|a, b| {
            a.kind() == b.kind()
                && a.span() == b.span()
                && a.markers() == b.markers()
                && a.content() == b.content()
        });
        Ok(Self {
            source: source.to_owned(),
            constructs,
            images,
            tables,
            objects,
            wiki_links,
            links,
            semantic,
            inline_fallback,
        })
    }

    /// 全部图片构造（按文档顺序；含无法成为 Object Block 的混排/行内图片）。
    #[must_use]
    pub fn images(&self) -> &[MarkdownImage] {
        &self.images
    }

    /// 全部 wiki 链接（按文档顺序）。
    #[must_use]
    pub fn wiki_links(&self) -> &[MarkdownWikiLink] {
        &self.wiki_links
    }

    /// 全部普通链接（按文档顺序；`dest` 已解析）。
    #[must_use]
    pub fn links(&self) -> &[MarkdownLink] {
        &self.links
    }

    /// 只返回整行独占的图片（可投影成 Object Block）。
    pub fn block_images(&self) -> impl Iterator<Item = &MarkdownImage> + '_ {
        self.images.iter().filter(|image| image.block)
    }

    /// 指定源码行上的整行独占图片；没有则 `None`。
    ///
    /// 同一行不可能有两个 Object Block（要求覆盖整行内容），所以这里返回第一个匹配。
    #[must_use]
    pub fn block_image_at_line(&self, line: usize) -> Option<&MarkdownImage> {
        let object = self.object_at_line(line)?;
        self.object_image(object)
    }

    /// 全部表格块（按文档顺序；含引用/列表/缩进里、不投影成对象的表格）。
    #[must_use]
    pub fn tables(&self) -> &[MarkdownTable] {
        &self.tables
    }

    /// 只返回整行独占、可投影成 Object Block 的表格。
    pub fn block_tables(&self) -> impl Iterator<Item = &MarkdownTable> + '_ {
        self.tables.iter().filter(|table| table.block)
    }

    /// 全部整块 Object（图片 + 表格，按源码顺序）。
    ///
    /// 投影、选中、源码切换、删除**只走这一条路径**；不要再按图片/表格各写一套。
    #[must_use]
    pub fn objects(&self) -> &[ObjectBlock] {
        &self.objects
    }

    /// 指定源码行上的整块 Object（图片所在行 / 表格表头行）；没有则 `None`。
    ///
    /// 同一行不可能有两个 Object Block（要求整行独占），所以返回第一个匹配。
    #[must_use]
    pub fn object_at_line(&self, line: usize) -> Option<&ObjectBlock> {
        self.objects.iter().find(|object| object.line() == line)
    }

    /// Object 对应的图片载荷；不是图片对象时 `None`。
    #[must_use]
    pub fn object_image(&self, object: &ObjectBlock) -> Option<&MarkdownImage> {
        if object.kind() != ObjectKind::Image {
            return None;
        }
        self.images.get(object.index())
    }

    /// Object 对应的表格载荷；不是表格对象时 `None`。
    #[must_use]
    pub fn object_table(&self, object: &ObjectBlock) -> Option<&MarkdownTable> {
        if object.kind() != ObjectKind::Table {
            return None;
        }
        self.tables.get(object.index())
    }

    /// 新解析器的完整语义模型（列表块树 / 行信息）。
    #[must_use]
    pub fn semantic(&self) -> &ParsedMarkdown {
        &self.semantic
    }

    /// 行内投影是否退回了旧 scanner（诊断/测试用）。
    #[must_use]
    pub const fn inline_fallback(&self) -> bool {
        self.inline_fallback
    }

    /// 逐源码行语义（与 [`Self::source`] 的 `\n` 切分一一对应）。
    #[must_use]
    pub fn line_infos(&self) -> &[semantic::LineInfo] {
        &self.semantic.lines
    }

    /// 列表块（按文档顺序）。
    #[must_use]
    pub fn lists(&self) -> &[semantic::ListBlock] {
        &self.semantic.lists
    }

    /// 列表项（按文档顺序）。
    #[must_use]
    pub fn list_items(&self) -> &[semantic::ListItem] {
        &self.semantic.items
    }

    /// 第 `index` 个列表项在**隐藏态**下要显示的 marker 文本（bullet / 右对齐编号）。
    ///
    /// 有序列表示例（最大编号 10）：`" 9. "`、`"10. "` —— 编号右对齐、宽度稳定。
    #[must_use]
    pub fn list_marker_text(&self, index: usize) -> Option<String> {
        let item = self.semantic.items.get(index)?;
        if item.marker.is_empty() {
            return None;
        }
        if item.ordered {
            Some(format!(
                "{:>width$}. ",
                item.number,
                width = item.number_width.max(1)
            ))
        } else {
            Some("• ".to_owned())
        }
    }

    /// 第 `index` 个列表项的**源码** marker（`- ` / `1. `；caret 进入时显示它）。
    #[must_use]
    pub fn list_marker_source(&self, index: usize) -> Option<&str> {
        let item = self.semantic.items.get(index)?;
        self.source.get(item.marker.start..item.marker.end)
    }

    /// caret/选区是否落在该列表 marker 上（Typora 规则：进入 marker 区域才显示源码）。
    ///
    /// 与构造显现规则不同：caret 恰好在 `marker.end`（内容起点）时**不**显现，
    /// 这样按 `End`/点内容时仍显示 bullet。
    ///
    /// ADR 0019 S3 起，任务项的 `[ ]`/`[x]` 也算 marker：caret 进入任务框同样整段
    /// 显现源码（`- [ ] …`），于是点击/键盘进框时看到的是真实字符而不是自绘复选框。
    pub fn list_marker_revealed(
        &self,
        index: usize,
        selection: Selection,
    ) -> Result<bool, MarkdownError> {
        let item = self
            .semantic
            .items
            .get(index)
            .ok_or(MarkdownError::InternalRange {
                start: index,
                end: index,
                len: self.semantic.items.len(),
            })?;
        let (start, end) = self.selection_bytes(selection)?;
        Ok(item_span_revealed(item, start, end))
    }

    /// 一次性算出所有列表 marker 的显现状态（`true` = 显示源码 marker）。
    ///
    /// app 逐行取 marker 状态时不要对每个 item 调 [`Self::list_marker_revealed`]
    /// （每次都会把 selection 换算成 source byte，是 O(n)）；这里只换算一次。
    pub fn list_marker_reveals(&self, selection: Selection) -> Result<Vec<bool>, MarkdownError> {
        let (start, end) = self.selection_bytes(selection)?;
        Ok(self
            .semantic
            .items
            .iter()
            .map(|item| item_span_revealed(item, start, end))
            .collect())
    }

    fn selection_bytes(&self, selection: Selection) -> Result<(usize, usize), MarkdownError> {
        let start = selection.start().to_source_byte(&self.source)?.get();
        let end = selection.end().to_source_byte(&self.source)?.get();
        Ok((start, end))
    }

    /// 生成当前 selection 下的投影片段（列表 marker → bullet/编号）。
    ///
    /// 规则：
    /// - 非列表构造：沿用 [`Self::hidden_ranges_for_selection`] 的显现规则，只是
    ///   **跳过 `ListItem`**（列表 marker 由本方法统一处理）；
    /// - 列表 marker 未显现 → `Replace` 成 `list_marker_text`；
    /// - 列表行前导空白（布局缩进由 app 提供）→ 永远 `Hidden`；
    /// - 返回片段有序、不重叠、非空。
    ///
    /// 图片对象不在这里显现：显式源码模式由
    /// [`Self::projection_pieces_with_objects`] 的 `revealed_object_lines` 控制。
    pub fn projection_pieces(
        &self,
        selection: Selection,
    ) -> Result<Vec<ProjectionPiece>, MarkdownError> {
        self.projection_pieces_with_objects(selection, &[])
    }

    /// 列表 + 图片对象感知的投影片段（ADR 0019 S2a，生产路径）。
    ///
    /// 在 [`Self::projection_pieces`] 的规则之上追加：
    ///
    /// - **整行独占的图片**（[`MarkdownImage::is_block`]）且其源码行**不在**
    ///   `revealed_object_lines` 里 → `ProjectionPiece::Object`（完整 source 跨度，
    ///   display 是占位符）；
    /// - 该行在 `revealed_object_lines` 里（工具条「编辑源码」/`F2` 进入源码模式）→
    ///   不产生 Object 片段，源码原样显示；
    /// - 混排/行内图片永远不产生 Object 片段（首版保持源码，选择写死在这里）；
    /// - 万一对象区间与隐藏/替换片段重叠（防御：不重叠是 block 判定的前提），
    ///   跳过该对象 —— 宁可不投影，也不产生错位。
    pub fn projection_pieces_with_objects(
        &self,
        selection: Selection,
        revealed_object_lines: &[usize],
    ) -> Result<Vec<ProjectionPiece>, MarkdownError> {
        self.projection_pieces_with_objects_and_cell(selection, revealed_object_lines, None)
    }

    /// 在 [`Self::projection_pieces_with_objects`] 之上叠加**单元格就地编辑**的显现
    /// （ADR 0019 S3b2）。
    ///
    /// `cell` 给出“正在编辑的整表 + cell”时：
    ///
    /// - 该表格**不再**产生 `Object` 片段（表格网格让位给一行文本）；
    /// - 表格内除该 cell 之外的源码全部变成 `Hidden`（`|` / 分隔行 / 其它 cell 都不显示）；
    /// - cell 本身保持 `Copy`：display 就是该 cell 的源码原文，caret/选区落在真实
    ///   source 上（因此 IME / 双向映射 / 投影全部沿用现有路径，没有第二套坐标系）。
    ///
    /// 首版显示的就是**源码原文**（`\|` 不会被还原成 `|`，`**` 原样显示）：源码是
    /// 唯一真源，用户没有实际改写这些字节时，保存回去的必须逐字不变。
    pub fn projection_pieces_with_objects_and_cell(
        &self,
        selection: Selection,
        revealed_object_lines: &[usize],
        cell: Option<ProjectionCellReveal>,
    ) -> Result<Vec<ProjectionPiece>, MarkdownError> {
        let mut hidden = self.hidden_ranges_for_selection_filtered(selection, false)?;
        let revealed = self.list_marker_reveals(selection)?;
        let mut replacements: Vec<ProjectionPiece> = Vec::new();
        for index in 0..self.semantic.items.len() {
            let item = &self.semantic.items[index];
            if item.marker.is_empty() || revealed.get(index).copied().unwrap_or(false) {
                continue;
            }
            let Some(text) = self.list_marker_text(index) else {
                continue;
            };
            let marker = self.range(item.marker.start, item.marker.end)?;
            replacements.push(ProjectionPiece::Replace(marker, text));
        }
        // ADR 0019 S3：任务框 `[ ]`/`[x]` → 不可见占位（绘制层在这个 display 区间
        // 上自绘复选框）。marker 与任务框是两段相邻的 Replace，source 仍是逐字真源；
        // caret/选区落在任务框上时（`revealed`）不产生 Replace，显示真实 `[ ]` 供编辑。
        for (index, item) in self.semantic.items.iter().enumerate() {
            if revealed.get(index).copied().unwrap_or(false) {
                continue;
            }
            let Some(box_span) = item.task_marker else {
                continue;
            };
            let box_range = self.range(box_span.start, box_span.end)?;
            replacements.push(ProjectionPiece::Replace(
                box_range,
                TASK_BOX_DISPLAY.to_owned(),
            ));
        }
        for line in &self.semantic.lines {
            let Some(list) = &line.list else {
                continue;
            };
            if !list.hidden_prefix.is_empty() {
                hidden.push(self.range(list.hidden_prefix.start, list.hidden_prefix.end)?);
            }
        }

        // Replace 优先：从 hidden 里扣除替换段（列表 marker 不应被当作隐藏标记）。
        // hidden 已排序合并、replacements 按 marker 顺序（文档顺序）排序；单趟线性扣除，
        // 避免 “每个 marker × 全部 hidden” 的平方级路径。
        hidden = merge_ranges(&self.source, hidden)?;
        hidden.sort_by_key(|range| (range.start().get(), range.end().get()));
        replacements.sort_by_key(|piece| piece.source().start().get());
        let replacement_ranges: Vec<SourceRange> =
            replacements.iter().map(ProjectionPiece::source).collect();
        let mut kept: Vec<SourceRange> = Vec::with_capacity(hidden.len());
        let mut replacement_cursor = 0usize;
        for hidden_range in &hidden {
            let mut cursor = hidden_range.start().get();
            let hidden_end = hidden_range.end().get();
            while replacement_cursor < replacement_ranges.len()
                && replacement_ranges[replacement_cursor].end().get() <= cursor
            {
                replacement_cursor += 1;
            }
            let mut index = replacement_cursor;
            while index < replacement_ranges.len()
                && replacement_ranges[index].start().get() < hidden_end
            {
                let replacement = replacement_ranges[index];
                let replacement_start = replacement.start().get();
                if replacement_start > cursor {
                    kept.push(self.range(cursor, replacement_start.min(hidden_end))?);
                }
                cursor = cursor.max(replacement.end().get().min(hidden_end));
                if cursor >= hidden_end {
                    break;
                }
                index += 1;
            }
            if cursor < hidden_end {
                kept.push(self.range(cursor, hidden_end)?);
            }
        }
        hidden = kept;
        let mut hidden = merge_ranges(&self.source, hidden)?;
        // 整块 Object（图片 / 表格）：不在显式源码模式里 → 原子对象片段。
        //
        // 两种对象走**同一条**路径：block 判定已经把“引用/列表/缩进/表格里的图片”
        // 和“引用/列表/缩进里的表格”排除掉了，所以这里不再按 kind 分支。
        //
        // 单元格就地编辑（ADR 0019 S3b2）：正在编辑的那张表**不**再是原子对象，
        // 表格源码里除该 cell 之外的部分全部隐藏，display 上只剩 cell 文本。
        let mut objects: Vec<SourceRange> = Vec::new();
        for object in &self.objects {
            if revealed_object_lines.contains(&object.line()) {
                continue;
            }
            let span = object.span();
            if let Some(cell) = cell.filter(|cell| cell.table == span) {
                if cell.cell.start() > span.start() {
                    hidden.push(self.range(span.start().get(), cell.cell.start().get())?);
                }
                if cell.cell.end() < span.end() {
                    hidden.push(self.range(cell.cell.end().get(), span.end().get())?);
                }
                continue;
            }
            // 防御：对象不能和隐藏/替换片段重叠（block 判定已排除列表/引用/标题行）。
            let overlaps = hidden.iter().chain(replacement_ranges.iter()).any(|range| {
                range.start().get() < span.end().get() && span.start().get() < range.end().get()
            });
            if !overlaps {
                objects.push(span);
            }
        }
        // cell 编辑追加的隐藏段与已有隐藏段可能相邻（表格前面紧贴一个隐藏构造）→ 再合并一次。
        let hidden = merge_ranges(&self.source, hidden)?;
        let mut pieces: Vec<ProjectionPiece> =
            hidden.into_iter().map(ProjectionPiece::Hidden).collect();
        pieces.extend(replacements);
        pieces.extend(objects.into_iter().map(ProjectionPiece::Object));
        pieces.sort_by_key(|piece| (piece.source().start().get(), piece.source().end().get()));
        // 相邻 hidden 合并（Replace 不合并：文本可能不同）。
        let mut merged: Vec<ProjectionPiece> = Vec::with_capacity(pieces.len());
        for piece in pieces {
            match (&mut merged.last_mut(), &piece) {
                (Some(ProjectionPiece::Hidden(last)), ProjectionPiece::Hidden(next))
                    if next.start().get() <= last.end().get() =>
                {
                    *last =
                        self.range(last.start().get(), last.end().get().max(next.end().get()))?;
                }
                _ => merged.push(piece),
            }
        }
        Ok(merged)
    }

    /// 原始 source。
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// source 字节长度。
    #[must_use]
    pub fn source_len(&self) -> usize {
        self.source.len()
    }

    /// 全部构造（按 `span` 起点升序；块级构造包含其内容里的行内构造）。
    #[must_use]
    pub fn constructs(&self) -> &[MarkdownConstruct] {
        &self.constructs
    }

    /// 当前投影下的语义样式区间（S2-4a）。
    ///
    /// runs 完整覆盖 `[0, display_len)`，无样式处为 `StyleKind::Plain`；
    /// 只覆盖构造内容，不覆盖标记。嵌套规则见 [`crate::model::style`]。
    #[must_use]
    pub fn style_runs(&self, projection: &ProjectionMap) -> Vec<StyleRun> {
        crate::model::style::runs(self, projection)
    }

    /// 块级语义（S2-7a）：每行一个 [`BlockRun`]，只在 source 变化时需要重算。
    ///
    /// 规则见 [`crate::model::block`]（含行首空格不算标题、围栏内部行、
    /// 一行多前缀的优先级）。
    pub fn block_runs(&self) -> Result<Vec<BlockRun>, MarkdownError> {
        crate::model::block::runs(self)
    }

    /// 给定 caret（scalar 坐标），返回当前应隐藏的 source 字节区间。
    pub fn hidden_ranges_for_caret(
        &self,
        caret: UnicodeScalar,
    ) -> Result<Vec<SourceRange>, MarkdownError> {
        let at = caret.to_source_byte(&self.source)?.get();
        self.hidden_ranges_filtered(
            |construct| {
                let span = construct.span();
                span.start().get() <= at && at <= span.end().get()
            },
            true,
        )
    }

    /// 给定选区（scalar 坐标，半开区间），返回当前应隐藏的 source 字节区间。
    ///
    /// 折叠选区等价于 caret；非空选区与构造 span 相交即显现该构造。
    pub fn hidden_ranges_for_selection(
        &self,
        selection: Selection,
    ) -> Result<Vec<SourceRange>, MarkdownError> {
        self.hidden_ranges_for_selection_filtered(selection, true)
    }

    /// 列表感知投影用：跳过 `ListItem` 构造（marker 由 [`Self::projection_pieces`]
    /// 统一替换成 bullet/编号）。
    fn hidden_ranges_for_selection_filtered(
        &self,
        selection: Selection,
        include_list: bool,
    ) -> Result<Vec<SourceRange>, MarkdownError> {
        let start = selection.start().to_source_byte(&self.source)?.get();
        let end = selection.end().to_source_byte(&self.source)?.get();
        if start == end {
            return self.hidden_ranges_filtered(
                |construct| {
                    let span = construct.span();
                    span.start().get() <= start && start <= span.end().get()
                },
                include_list,
            );
        }
        self.hidden_ranges_filtered(
            |construct| {
                let span = construct.span();
                span.start().get() < end && span.end().get() > start
            },
            include_list,
        )
    }

    fn hidden_ranges_filtered(
        &self,
        reveal: impl Fn(&MarkdownConstruct) -> bool,
        include_list: bool,
    ) -> Result<Vec<SourceRange>, MarkdownError> {
        let mut ranges: Vec<SourceRange> = Vec::new();
        for construct in &self.constructs {
            if !include_list && construct.kind() == ConstructKind::ListItem {
                continue;
            }
            if !reveal(construct) {
                ranges.extend(construct.markers.iter().copied());
            }
        }
        merge_ranges(&self.source, ranges)
    }

    fn range(&self, start: usize, end: usize) -> Result<SourceRange, MarkdownError> {
        SourceRange::within(&self.source, SourceByte::new(start), SourceByte::new(end)).map_err(
            |_| MarkdownError::InternalRange {
                start,
                end,
                len: self.source.len(),
            },
        )
    }
}

/// 一个列表 item 的 marker（含任务框）是否被 caret/选区显现。
///
/// 规则与旧实现一致：折叠 caret 严格落在 `[start, end)` 内；非空选区取相交。
/// 任务框与列表 marker 是**同一条**“进入即显现”规则（ADR 0019 S3）：
/// 二者任一命中就整段显示源码。
fn item_span_revealed(item: &semantic::ListItem, start: usize, end: usize) -> bool {
    let hits = |span: &semantic::Span| {
        if start == end {
            span.start <= start && start < span.end
        } else {
            span.start < end && span.end > start
        }
    };
    let marker_hit = !item.marker.is_empty() && hits(&item.marker);
    let box_hit = item.task_marker.is_some_and(|span| hits(&span));
    marker_hit || box_hit
}

/// 合并有序/无序、可能重叠的 source 区间（保证升序、不重叠）。
fn merge_ranges(
    source: &str,
    mut ranges: Vec<SourceRange>,
) -> Result<Vec<SourceRange>, MarkdownError> {
    ranges.sort_by_key(|range| (range.start().get(), range.end().get()));
    let mut merged: Vec<SourceRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last().copied() {
            Some(last) if range.start().get() <= last.end().get() => {
                let combined = span_of(
                    source,
                    last.start().get(),
                    last.end().get().max(range.end().get()),
                )?;
                if let Some(slot) = merged.last_mut() {
                    *slot = combined;
                }
            }
            _ => merged.push(range),
        }
    }
    Ok(merged)
}

fn span_of(source: &str, start: usize, end: usize) -> Result<SourceRange, MarkdownError> {
    SourceRange::within(source, SourceByte::new(start), SourceByte::new(end)).map_err(|_| {
        MarkdownError::InternalRange {
            start,
            end,
            len: source.len(),
        }
    })
}

/// 旧自研 scanner：作为新解析器行内投影的 fallback（见 [`MarkdownDocument`] 文档）。
fn legacy_constructs(source: &str) -> Result<Vec<MarkdownConstruct>, MarkdownError> {
    let mut parser = Parser {
        source,
        len: source.len(),
        constructs: Vec::new(),
    };
    parser.run()?;
    let mut constructs = parser.constructs;
    constructs.sort_by_key(|construct| {
        (
            construct.span.start().get(),
            construct.span.end().get(),
            construct.kind,
        )
    });
    Ok(constructs)
}

/// 新解析器的普通链接元数据 → editor 的 [`MarkdownLink`]。
fn collect_links(source: &str, semantic: &ParsedMarkdown) -> Vec<MarkdownLink> {
    let mut links = Vec::new();
    for link in &semantic.links {
        let (Some(span), Some(content)) = (
            convert_span(source, link.span),
            convert_span(source, link.content),
        ) else {
            continue;
        };
        links.push(MarkdownLink {
            span,
            content,
            dest: link.dest.clone(),
        });
    }
    links
}

/// 新解析器的 wiki 元数据 → editor 的 [`MarkdownWikiLink`]。
fn collect_wiki_links(source: &str, semantic: &ParsedMarkdown) -> Vec<MarkdownWikiLink> {
    let mut links = Vec::new();
    for wiki in &semantic.wiki_links {
        let (Some(span), Some(target), Some(display)) = (
            convert_span(source, wiki.span),
            convert_span(source, wiki.target),
            convert_span(source, wiki.display),
        ) else {
            continue;
        };
        let heading = wiki
            .heading
            .and_then(|heading| convert_span(source, heading));
        let alias = wiki.alias.and_then(|alias| convert_span(source, alias));
        links.push(MarkdownWikiLink {
            span,
            target,
            heading,
            alias,
            display,
        });
    }
    links
}

/// 从解析器的 wiki 构造生成投影构造（含 alias 时的 target/`|` 隐藏）。
fn wiki_constructs(source: &str, semantic: &ParsedMarkdown) -> Vec<MarkdownConstruct> {
    semantic
        .constructs
        .iter()
        .filter(|construct| construct.kind == semantic::ConstructKind::WikiLink)
        .filter_map(|construct| convert_construct(source, construct))
        .collect()
}

/// 新解析器的图片构造 → editor 的 [`MarkdownImage`]（ADR 0019 S2a）。
///
/// 只做**区间换算 + block 判定**，不写任何新文本。定位不合法（坐标越界）的图片
/// 直接跳过（保守：原样显示）。
fn collect_images(source: &str, semantic: &ParsedMarkdown) -> Vec<MarkdownImage> {
    let mut images = Vec::new();
    for image in &semantic.images {
        let (Some(span), Some(alt)) = (
            convert_span(source, image.span),
            convert_span(source, image.content),
        ) else {
            continue;
        };
        let line = source
            .as_bytes()
            .get(..image.span.start.min(source.len()))
            .map_or(0, |prefix| {
                prefix.iter().filter(|byte| **byte == b'\n').count()
            });
        let block = is_block_image(source, semantic, line, &image.span);
        images.push(MarkdownImage {
            span,
            alt,
            dest: image.dest.clone(),
            line,
            block,
        });
    }
    images
}

/// 判定图片是否**整行独占**（可投影成 Object Block）。
///
/// 条件（见 [`MarkdownImage`] 文档）：
/// 1. 不在代码块 / 表格 / 未支持块（HTML）里；
/// 2. 所在行是普通段落（不是标题/引用/列表/围栏/缩进代码/分隔线/表格/HTML）；
/// 3. 图片 span 恰好从行内容起点开始；
/// 4. 图片 span 之后到行内容结尾只有空白（行尾空格/Tab 允许）。
fn is_block_image(
    source: &str,
    semantic: &ParsedMarkdown,
    line: usize,
    image: &SemanticSpan,
) -> bool {
    // 原子块（代码 / 表格 / 未支持块）里的图片一律不是 Object（表格内的图片见 S3a）。
    if inside_any(image.start, image.end, &opaque_spans(semantic)) {
        return false;
    }
    let Some(info) = semantic.lines.get(line) else {
        return false;
    };
    if info.kind != semantic::LineKind::Paragraph || info.list.is_some() || !info.quote.is_empty() {
        return false;
    }
    if image.start != info.content.start || image.end > info.content.end {
        return false;
    }
    source
        .get(image.end..info.content.end)
        .is_some_and(|tail| tail.bytes().all(|byte| byte == b' ' || byte == b'\t'))
}

/// 新解析器的表格块 → editor 的 [`MarkdownTable`]（ADR 0019 S3a）。
///
/// 只做**区间换算 + block 判定**，不写任何新文本；行/格区间全部来自
/// [`menghunote_markdown::TableBlock`] 的源码偏移，坐标越界整表跳过（保守）。
fn collect_tables(source: &str, semantic: &ParsedMarkdown) -> Vec<MarkdownTable> {
    let mut tables = Vec::new();
    for table in &semantic.tables {
        let Some(span) = convert_span(source, table.span) else {
            continue;
        };
        let Some(header) = convert_table_row(source, &table.header) else {
            continue;
        };
        let Some(delimiter) = convert_span(source, table.delimiter) else {
            continue;
        };
        let rows: Vec<MarkdownTableRow> = table
            .rows
            .iter()
            .filter_map(|row| convert_table_row(source, row))
            .collect();
        if rows.len() != table.rows.len() {
            // 任一行坐标不合法就整表不投影（不猜行号，避免页面错位）。
            continue;
        }
        let block = is_block_table(semantic, table);
        tables.push(MarkdownTable {
            span,
            first_line: table.first_line,
            last_line: table.last_line,
            columns: table.columns,
            alignments: table.alignments.clone(),
            header,
            delimiter,
            rows,
            block,
        });
    }
    tables
}

fn convert_table_row(source: &str, row: &semantic::TableRow) -> Option<MarkdownTableRow> {
    let span = convert_span(source, row.span)?;
    let cells: Option<Vec<SourceRange>> = row
        .cells
        .iter()
        .map(|cell| convert_span(source, *cell))
        .collect();
    Some(MarkdownTableRow {
        span,
        line: row.line,
        cells: cells?,
    })
}

/// 判定表格是否**整行独占**（可投影成 Object Block）。
///
/// 条件（见 [`MarkdownTable`] 文档）：
/// 1. 第一行是 `LineKind::Table`（不在代码/未支持块里）；
/// 2. 第一行不是引用、不在列表项里；
/// 3. 表格源码从该行内容起点开始（行首缩进不投影）；
/// 4. 表格区间结束在该行内容终点，且至少覆盖表头 + 分隔行。
fn is_block_table(semantic: &ParsedMarkdown, table: &semantic::TableBlock) -> bool {
    let Some(info) = semantic.lines.get(table.first_line) else {
        return false;
    };
    if info.kind != semantic::LineKind::Table || info.list.is_some() || !info.quote.is_empty() {
        return false;
    }
    if table.span.start != info.content.start {
        return false;
    }
    let Some(last) = semantic.lines.get(table.last_line) else {
        return false;
    };
    table.last_line > table.first_line && table.span.end == last.content.end
}

/// 图片 + 表格 → 统一的 Object 列表（按源码顺序）。
///
/// 只收**整行独占**的块：`block = false` 的图片/表格首版保持源码原样显示。
fn collect_objects(images: &[MarkdownImage], tables: &[MarkdownTable]) -> Vec<ObjectBlock> {
    let mut objects = Vec::new();
    for (index, image) in images.iter().enumerate() {
        if !image.is_block() {
            continue;
        }
        objects.push(ObjectBlock::new(
            ObjectKind::Image,
            image.span(),
            image.line(),
            image.line(),
            index,
        ));
    }
    for (index, table) in tables.iter().enumerate() {
        if !table.is_block() {
            continue;
        }
        objects.push(ObjectBlock::new(
            ObjectKind::Table,
            table.span(),
            table.line(),
            table.end_line(),
            index,
        ));
    }
    objects.sort_by_key(|object| (object.span().start().get(), object.span().end().get()));
    objects
}

/// 新解析器 + 旧 scanner 的构造合并（规则见模块文档与 ADR 0019 S1 实施记录）。
fn merge_constructs(
    source: &str,
    semantic: &ParsedMarkdown,
    mut legacy: Vec<MarkdownConstruct>,
) -> (Vec<MarkdownConstruct>, bool) {
    // 1) 原子块（代码 / 表格 / 未支持块）内部、以及图片（S2 才做 Object）内部：
    //    旧 scanner 的**行内**构造一律丢弃（原样显示，不猜测）。
    let opaque = opaque_spans(semantic);
    let image_spans: Vec<SemanticSpan> = semantic
        .constructs
        .iter()
        .filter(|construct| construct.kind == semantic::ConstructKind::Image)
        .map(|construct| construct.span)
        .collect();
    legacy.retain(|construct| {
        !(construct.kind().is_inline()
            && (inside_any(
                construct.span().start().get(),
                construct.span().end().get(),
                &opaque,
            ) || inside_any(
                construct.span().start().get(),
                construct.span().end().get(),
                &image_spans,
            )))
    });

    // 2) 新解析器构造 → editor 构造（Image 留给 S2；定位不合法的构造直接跳过）。
    let mut semantic_blocks = Vec::new();
    let mut semantic_inlines = Vec::new();
    for construct in &semantic.constructs {
        if construct.kind == semantic::ConstructKind::Image {
            continue;
        }
        if construct.kind.is_inline()
            && (semantic.uncertain_inline
                || inside_any(construct.span.start, construct.span.end, &opaque)
                || inside_any(construct.span.start, construct.span.end, &image_spans))
        {
            continue;
        }
        let Some(converted) = convert_construct(source, construct) else {
            continue;
        };
        if converted.kind().is_block() {
            semantic_blocks.push(converted);
        } else {
            semantic_inlines.push(converted);
        }
    }

    // 3) 行内：旧 scanner 覆盖的每一种构造都必须能在新解析器里找到同 kind/同 span 的
    //    对应项；否则整篇行内退回旧 scanner（保守：宁可少隐藏，也不隐藏错）。
    let legacy_inlines: Vec<MarkdownConstruct> = legacy
        .iter()
        .filter(|construct| construct.kind().is_inline())
        .cloned()
        .collect();
    let inline_fallback = semantic.uncertain_inline
        || legacy_inlines.iter().any(|legacy| {
            !semantic_inlines
                .iter()
                .any(|new| new.kind() == legacy.kind() && new.span() == legacy.span())
        });
    let chosen_inlines = if inline_fallback {
        legacy_inlines
    } else {
        semantic_inlines
    };

    // 4) 块级：**只以新解析器为准**，不再用旧 scanner 补位。
    //    补位会把 `-` / `#`（无空格）重新识别成列表/标题，违背 Markdown 语法；
    //    未支持构造由 `unsupported_spans` 原样显示。旧 scanner 只保留行内 fallback。
    let mut chosen = semantic_blocks;
    chosen.extend(chosen_inlines);
    chosen.sort_by_key(|construct| {
        (
            construct.span.start().get(),
            construct.span.end().get(),
            construct.kind,
        )
    });
    chosen.dedup_by(|a, b| {
        a.kind == b.kind && a.span == b.span && a.markers == b.markers && a.content == b.content
    });
    (chosen, inline_fallback)
}

/// `ParsedMarkdown` 的原子块区间（代码 / 表格 / 未支持块），一次性收集。
fn opaque_spans(semantic: &ParsedMarkdown) -> Vec<SemanticSpan> {
    semantic.opaque_spans().collect()
}

fn inside_any(start: usize, end: usize, spans: &[SemanticSpan]) -> bool {
    spans
        .iter()
        .any(|outer| outer.start <= start && end <= outer.end)
}

fn convert_construct(source: &str, construct: &semantic::Construct) -> Option<MarkdownConstruct> {
    let kind = match construct.kind {
        semantic::ConstructKind::Heading(_) => ConstructKind::Heading,
        semantic::ConstructKind::BlockQuote => ConstructKind::BlockQuote,
        semantic::ConstructKind::ListItem => ConstructKind::ListItem,
        semantic::ConstructKind::FencedCode => ConstructKind::FencedCode,
        semantic::ConstructKind::Bold => ConstructKind::Bold,
        semantic::ConstructKind::Emphasis => ConstructKind::Emphasis,
        semantic::ConstructKind::InlineCode => ConstructKind::InlineCode,
        semantic::ConstructKind::Strike => ConstructKind::Strike,
        semantic::ConstructKind::Link => ConstructKind::Link,
        semantic::ConstructKind::WikiLink => ConstructKind::WikiLink,
        semantic::ConstructKind::Image => return None,
    };
    let span = convert_span(source, construct.span)?;
    let markers: Option<Vec<SourceRange>> = construct
        .markers
        .iter()
        .map(|marker| convert_span(source, *marker))
        .collect();
    let content = convert_span(source, construct.content)?;
    MarkdownConstruct::checked(kind, span, markers?, content)
}

fn convert_span(source: &str, span: SemanticSpan) -> Option<SourceRange> {
    SourceRange::within(
        source,
        SourceByte::new(span.start),
        SourceByte::new(span.end),
    )
    .ok()
}

/// 围栏代码块扫描状态。
#[derive(Debug, Clone, Copy)]
struct FenceState {
    open_start: usize,
    open_end: usize,
    marker: u8,
    marker_len: usize,
    body_start: usize,
}

/// 行内定界符规格（把 `try_delimited` 的参数收敛，避免 8 个参数）。
#[derive(Debug, Clone, Copy)]
struct Delimiter {
    byte: u8,
    len: usize,
    kind: ConstructKind,
    /// 闭合定界符允许比开启更长（`**a****b**` 需要把 4 连星拆成两对）。
    allow_longer_close: bool,
    /// `_` 强调需要词边界，避免把 `snake_case` 当斜体。
    require_word_boundary: bool,
}

const BOLD_STAR: Delimiter = Delimiter {
    byte: b'*',
    len: 2,
    kind: ConstructKind::Bold,
    allow_longer_close: true,
    require_word_boundary: false,
};

const EMPHASIS_STAR: Delimiter = Delimiter {
    byte: b'*',
    len: 1,
    kind: ConstructKind::Emphasis,
    allow_longer_close: false,
    require_word_boundary: false,
};

const EMPHASIS_UNDERSCORE: Delimiter = Delimiter {
    byte: b'_',
    len: 1,
    kind: ConstructKind::Emphasis,
    allow_longer_close: false,
    require_word_boundary: true,
};

const STRIKE: Delimiter = Delimiter {
    byte: b'~',
    len: 2,
    kind: ConstructKind::Strike,
    allow_longer_close: true,
    require_word_boundary: false,
};

/// 源码扫描器。
struct Parser<'a> {
    source: &'a str,
    len: usize,
    constructs: Vec<MarkdownConstruct>,
}

impl Parser<'_> {
    fn run(&mut self) -> Result<(), MarkdownError> {
        let mut line_start = 0usize;
        let mut fence: Option<FenceState> = None;

        while line_start < self.len {
            let rel_end = self.source[line_start..].find('\n');
            let line_end = rel_end.map_or(self.len, |offset| line_start + offset);
            let next_line_start = if line_end < self.len {
                line_end + 1
            } else {
                self.len
            };

            if let Some(state) = fence.take() {
                if self.is_closing_fence(line_start, line_end, &state) {
                    // 内容区间 = 开围栏行的下一行起点 .. 闭围栏行起点（含中间 LF）。
                    self.push(
                        ConstructKind::FencedCode,
                        (state.open_start, line_end),
                        &[(state.open_start, state.open_end), (line_start, line_end)],
                        (state.body_start, line_start),
                    )?;
                } else {
                    fence = Some(state);
                }
            } else if let Some(state) = self.detect_open_fence(line_start, line_end) {
                fence = Some(state);
            } else {
                self.parse_block_line(line_start, line_end)?;
            }

            line_start = next_line_start;
        }

        // 文档结束时仍处于围栏代码块内：未闭合 → 不产生构造（整段保守不隐藏）。
        Ok(())
    }

    fn parse_block_line(
        &mut self,
        line_start: usize,
        line_end: usize,
    ) -> Result<(), MarkdownError> {
        let bytes = self.source.as_bytes();
        let line_len = line_end - line_start;
        let mut pos = 0usize;
        while pos < line_len && pos < 3 && bytes[line_start + pos] == b' ' {
            pos += 1;
        }

        // 1. 引用前缀（可嵌套）：每层 `>` + 可选一个空格。
        let mut quote_markers: Vec<(usize, usize)> = Vec::new();
        let mut cursor = pos;
        while cursor < line_len && bytes[line_start + cursor] == b'>' {
            let marker_start = cursor;
            cursor += 1;
            if cursor < line_len && bytes[line_start + cursor] == b' ' {
                cursor += 1;
            }
            // 若后面还有嵌套层，把层间空格并入当前层前缀。
            let mut probe = cursor;
            while probe < line_len && probe - cursor < 3 && bytes[line_start + probe] == b' ' {
                probe += 1;
            }
            if probe < line_len && bytes[line_start + probe] == b'>' {
                quote_markers.push((line_start + marker_start, line_start + probe));
                cursor = probe;
            } else {
                quote_markers.push((line_start + marker_start, line_start + cursor));
                break;
            }
        }

        if !quote_markers.is_empty() {
            let span_start = quote_markers[0].0;
            let content_start = line_start + cursor;
            self.push(
                ConstructKind::BlockQuote,
                (span_start, line_end),
                &quote_markers,
                (content_start, line_end),
            )?;
        }

        // 2. 列表标记（引用之后、标题之前）。
        if let Some(marker_end) = detect_list_marker(bytes, line_start + cursor, line_end) {
            self.push(
                ConstructKind::ListItem,
                (line_start + cursor, line_end),
                &[(line_start + cursor, marker_end)],
                (marker_end, line_end),
            )?;
            cursor = marker_end - line_start;
        }

        // 3. ATX 标题（可以在引用/列表内容里）。
        if let Some(marker_end) = detect_heading(bytes, line_start + cursor, line_end) {
            self.push(
                ConstructKind::Heading,
                (line_start + cursor, line_end),
                &[(line_start + cursor, marker_end)],
                (marker_end, line_end),
            )?;
            cursor = marker_end - line_start;
        }

        // 4. 行内构造：作用域是这一行去掉所有块级前缀后的内容。
        if cursor < line_len {
            self.parse_inline(line_start + cursor, line_end)?;
        }
        Ok(())
    }

    fn detect_open_fence(&self, line_start: usize, line_end: usize) -> Option<FenceState> {
        let bytes = self.source.as_bytes();
        let line_len = line_end - line_start;
        let mut pos = 0usize;
        while pos < line_len && pos < 3 && bytes[line_start + pos] == b' ' {
            pos += 1;
        }
        if pos >= line_len {
            return None;
        }
        let marker = bytes[line_start + pos];
        if marker != b'`' && marker != b'~' {
            return None;
        }
        let run = count_run(bytes, line_start + pos, line_end, marker);
        if run < 3 {
            return None;
        }
        // 反引号围栏的 info string 里不允许再出现反引号（CommonMark）。
        if marker == b'`' && bytes[line_start + pos + run..line_end].contains(&b'`') {
            return None;
        }
        Some(FenceState {
            open_start: line_start,
            open_end: line_end,
            marker,
            marker_len: run,
            body_start: if line_end < self.len {
                line_end + 1
            } else {
                self.len
            },
        })
    }

    fn is_closing_fence(&self, line_start: usize, line_end: usize, state: &FenceState) -> bool {
        let bytes = self.source.as_bytes();
        let line_len = line_end - line_start;
        let mut pos = 0usize;
        while pos < line_len && pos < 3 && bytes[line_start + pos] == b' ' {
            pos += 1;
        }
        if pos >= line_len || bytes[line_start + pos] != state.marker {
            return false;
        }
        let run = count_run(bytes, line_start + pos, line_end, state.marker);
        if run < state.marker_len {
            return false;
        }
        // 闭围栏后面只允许空格/制表符。
        bytes[line_start + pos + run..line_end]
            .iter()
            .all(|byte| *byte == b' ' || *byte == b'\t')
    }

    fn parse_inline(&mut self, start: usize, end: usize) -> Result<(), MarkdownError> {
        let bytes = self.source.as_bytes();
        let mut index = start;
        while index < end {
            match bytes[index] {
                b'\\' => {
                    // 转义：跳过反斜杠 + 下一个字符（保住 UTF-8 边界）。
                    index = skip_escape(self.source, index, end);
                }
                b'`' => {
                    let next = self.try_code_span(index, end)?;
                    index = next.max(index + 1);
                }
                b'*' => {
                    index = self.try_star(index, end)?;
                }
                b'_' => {
                    index = self.try_underscore_emphasis(index, end)?;
                }
                b'~' => {
                    let run = count_run(bytes, index, end, b'~');
                    if run == 2 {
                        index = self
                            .try_delimited(index, end, &STRIKE)?
                            .unwrap_or(index + run);
                    } else {
                        index += run;
                    }
                }
                b'[' => {
                    index = self.try_link(index, end)?;
                }
                _ => {
                    index = next_char_boundary(self.source, index, end);
                }
            }
        }
        Ok(())
    }

    fn try_star(&mut self, at: usize, end: usize) -> Result<usize, MarkdownError> {
        let run = count_run(self.source.as_bytes(), at, end, b'*');
        match run {
            2 => Ok(self.try_delimited(at, end, &BOLD_STAR)?.unwrap_or(at + 2)),
            1 => Ok(self
                .try_delimited(at, end, &EMPHASIS_STAR)?
                .unwrap_or(at + 1)),
            // `***` 等组合定界符本层不支持：整段跳过，不产生构造。
            _ => Ok(at + run),
        }
    }

    fn try_underscore_emphasis(&mut self, at: usize, end: usize) -> Result<usize, MarkdownError> {
        let run = count_run(self.source.as_bytes(), at, end, b'_');
        if run != 1 {
            return Ok(at + run);
        }
        // `_` 强调要求词边界（避免把 snake_case 当强调）：两侧不能紧邻字母/数字。
        // 边界检查在 `try_delimited` 内部、**写构造之前**完成。
        Ok(self
            .try_delimited(at, end, &EMPHASIS_UNDERSCORE)?
            .unwrap_or(at + 1))
    }

    fn try_delimited(
        &mut self,
        at: usize,
        end: usize,
        spec: &Delimiter,
    ) -> Result<Option<usize>, MarkdownError> {
        let bytes = self.source.as_bytes();
        let delimiter = spec.byte;
        let delimiter_len = spec.len;
        let open_end = at + delimiter_len;
        let mut index = open_end;
        while index < end {
            match bytes[index] {
                b'\\' => index = skip_escape(self.source, index, end),
                byte if byte == delimiter => {
                    let run = count_run(bytes, index, end, delimiter);
                    if run >= delimiter_len && (spec.allow_longer_close || run == delimiter_len) {
                        let content_end = index;
                        let close_end = content_end + delimiter_len;
                        let inner_ok = content_end > open_end
                            && !is_ascii_space(bytes[open_end])
                            && !is_ascii_space(bytes[content_end - 1]);
                        if !inner_ok {
                            return Ok(None);
                        }
                        let boundary_ok = !spec.require_word_boundary
                            || ((at == 0 || !is_word_char(char_before(self.source, at)))
                                && (close_end >= end
                                    || !is_word_char(char_after(self.source, close_end))));
                        if !boundary_ok {
                            return Ok(None);
                        }
                        self.push(
                            spec.kind,
                            (at, close_end),
                            &[(at, open_end), (index, close_end)],
                            (open_end, content_end),
                        )?;
                        // 文本内容递归解析行内构造（代码 / 链接文本等内部规则由各自
                        // 分支决定）。内容严格短于外层，递归必然终止。
                        if content_end > open_end {
                            self.parse_inline(open_end, content_end)?;
                        }
                        return Ok(Some(close_end));
                    }
                    index += run;
                }
                _ => index = next_char_boundary(self.source, index, end),
            }
        }
        Ok(None)
    }

    fn try_code_span(&mut self, at: usize, end: usize) -> Result<usize, MarkdownError> {
        let bytes = self.source.as_bytes();
        let run = count_run(bytes, at, end, b'`');
        let open_end = at + run;
        let mut index = open_end;
        while index < end {
            if bytes[index] == b'`' {
                let close_run = count_run(bytes, index, end, b'`');
                if close_run == run {
                    let close_end = index + close_run;
                    self.push(
                        ConstructKind::InlineCode,
                        (at, close_end),
                        &[(at, open_end), (index, close_end)],
                        (open_end, index),
                    )?;
                    return Ok(close_end);
                }
                index += close_run;
            } else {
                index = next_char_boundary(self.source, index, end);
            }
        }
        // 未闭合：不产生构造，跳过开标记继续找后面的内容。
        Ok(open_end)
    }

    fn try_link(&mut self, at: usize, end: usize) -> Result<usize, MarkdownError> {
        let bytes = self.source.as_bytes();
        // 找匹配的 `]`（尊重反斜杠转义；不支持嵌套方括号）。
        let mut index = at + 1;
        while index < end {
            match bytes[index] {
                b'\\' => index = skip_escape(self.source, index, end),
                b']' => break,
                _ => index = next_char_boundary(self.source, index, end),
            }
        }
        if index >= end || bytes[index] != b']' {
            return Ok(at + 1);
        }
        let text_end = index;
        if index + 1 >= end || bytes[index + 1] != b'(' {
            return Ok(at + 1);
        }
        let mut url = index + 2;
        while url < end {
            match bytes[url] {
                b'\\' => url = skip_escape(self.source, url, end),
                b')' => {
                    let close_end = url + 1;
                    self.push(
                        ConstructKind::Link,
                        (at, close_end),
                        &[(at, at + 1), (text_end, close_end)],
                        (at + 1, text_end),
                    )?;
                    // 链接文本递归解析行内构造；URL 部分保持原文。
                    if text_end > at + 1 {
                        self.parse_inline(at + 1, text_end)?;
                    }
                    return Ok(close_end);
                }
                _ => url = next_char_boundary(self.source, url, end),
            }
        }
        // 未闭合 URL：不产生构造。
        Ok(at + 1)
    }

    fn push(
        &mut self,
        kind: ConstructKind,
        span: (usize, usize),
        markers: &[(usize, usize)],
        content: (usize, usize),
    ) -> Result<(), MarkdownError> {
        if span.1 <= span.0 {
            return Ok(());
        }
        let span = self.range(span.0, span.1)?;
        let mut marker_ranges = Vec::with_capacity(markers.len());
        for &(start, end) in markers {
            if end > start {
                marker_ranges.push(self.range(start, end)?);
            }
        }
        if marker_ranges.is_empty() {
            return Ok(());
        }
        let content = self.range(content.0, content.1)?;
        self.constructs.push(MarkdownConstruct {
            kind,
            span,
            markers: marker_ranges,
            content,
        });
        Ok(())
    }

    fn range(&self, start: usize, end: usize) -> Result<SourceRange, MarkdownError> {
        SourceRange::within(self.source, SourceByte::new(start), SourceByte::new(end)).map_err(
            |_| MarkdownError::InternalRange {
                start,
                end,
                len: self.len,
            },
        )
    }
}

fn count_run(bytes: &[u8], start: usize, end: usize, marker: u8) -> usize {
    let mut run = 0usize;
    while start + run < end && bytes[start + run] == marker {
        run += 1;
    }
    run
}

fn is_ascii_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn is_word_char(ch: Option<char>) -> bool {
    ch.is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
}

fn char_before(source: &str, at: usize) -> Option<char> {
    source.get(..at).and_then(|head| head.chars().next_back())
}

fn char_after(source: &str, at: usize) -> Option<char> {
    source.get(at..).and_then(|tail| tail.chars().next())
}

/// 跳过反斜杠 + 下一个字符；返回新的扫描位置（保持 UTF-8 边界）。
fn skip_escape(source: &str, at: usize, end: usize) -> usize {
    let after_slash = at + 1;
    if after_slash >= end {
        return end;
    }
    let escaped = char_after(source, after_slash).map_or(1, char::len_utf8);
    after_slash + escaped
}

fn next_char_boundary(source: &str, at: usize, end: usize) -> usize {
    let ch = char_after(source, at).map_or(1, char::len_utf8);
    (at + ch).min(end)
}

/// 识别列表标记，返回标记结束位置（不含）。
///
/// 语义：`- ` / `* ` / `+ ` / `1. `（1–9 位数字 + `. `）。标记不在行尾为空。
fn detect_list_marker(bytes: &[u8], at: usize, end: usize) -> Option<usize> {
    if at >= end {
        return None;
    }
    match bytes[at] {
        b'-' | b'*' | b'+' => {
            if at + 1 < end && bytes[at + 1] == b' ' {
                Some(at + 2)
            } else {
                None
            }
        }
        b'0'..=b'9' => {
            let mut digits_end = at;
            while digits_end < end && digits_end - at < 9 && bytes[digits_end].is_ascii_digit() {
                digits_end += 1;
            }
            if digits_end > at
                && digits_end + 1 < end
                && bytes[digits_end] == b'.'
                && bytes[digits_end + 1] == b' '
            {
                Some(digits_end + 2)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// 识别 ATX 标题，返回标记结束位置（不含）；`#` 后必须是空格或行尾。
fn detect_heading(bytes: &[u8], at: usize, end: usize) -> Option<usize> {
    let mut run_end = at;
    while run_end < end && bytes[run_end] == b'#' {
        run_end += 1;
    }
    let run = run_end - at;
    if !(1..=6).contains(&run) {
        return None;
    }
    if run_end == end {
        return Some(run_end);
    }
    if bytes[run_end] == b' ' {
        Some(run_end + 1)
    } else {
        None
    }
}
