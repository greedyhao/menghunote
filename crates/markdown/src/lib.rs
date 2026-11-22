//! Markdown 语义解析（ADR 0019 S1，**纯 crate**）。
//!
//! 本 crate 只做一件事：把 Markdown 源码交给 `pulldown-cmark`，再把事件流翻译成
//! **带 source byte offsets 的块/行内模型 + 列表块树**，供 `menghunote-editor`
//! 生成源码投影（`ProjectionMap`）与列表布局使用。
//!
//! # 为什么需要 span adapter
//!
//! `Parser::into_offset_iter()` 给出的事件范围**不是无损 CST**：
//!
//! - `Event::Text` 的范围可能不含被转义/实体的原始字节（`\*` 的 `\` 不在范围里，
//!   实体 `&amp;` 的范围包含原始字节但文本已解码）；
//! - `Code` 事件的范围包含反引号，文本却是去掉定界符后的内容；
//! - 链接/图片的范围包含 `[text](dest)`/`![alt](dest)`，但文本事件只覆盖 `text`；
//! - 块级事件的范围可能从行内缩进之后开始（嵌套列表的 `Start(Item)` 会落在行首
//!   空格之后），而列表项标记必须在**行文本**里定位。
//!
//! 因此本模块不使用 AST 重新序列化（那会改写正文），只从**原始源码切片**里
//! 定位定界符、转义、实体、引用式链接、表格分隔符，产出 [`Construct`]：
//! `span` + 要隐藏的 `markers` + `content`。所有偏移都是 UTF-8 字节、落在
//! `char` 边界上；源码逐字节保留，本 crate 不拼任何新文本。
//!
//! # 模型
//!
//! - [`Construct`]：可投影的构造（标题/引用/列表项/围栏代码/行内构造）；
//! - [`ListItem`] / [`ListBlock`]：列表块树（嵌套层级、有序起始号、松紧、
//!   续行缩进、子段落、内部围栏）；
//! - [`TableBlock`] / [`TableRow`]：GFM 表格块树（整表跨度、表头行、分隔行、
//!   数据行、每格源码区间与对齐方式；ADR 0019 S3a）；
//! - [`FencedMermaidBlock`]：fenced `mermaid` 代码块的源码元数据（整块/内容跨度、
//!   起止行、info、是否可投影成 Object；ADR 0019 M2）；
//! - [`CodeBlockSpan`]：**任意**代码块（fenced / indented）的源码元数据（整块/
//!   内容/开围栏跨度、info、起止行、是否 fenced）；app 的只读语法高亮按它取内容
//!   与语言，`code_spans` 的既有语义不变；
//! - [`DrawingSpan`]：整行独占的内嵌画布笔记 `![[目标.excalidraw]]`（目标/别名
//!   源码区间、行号；ADR 0025 后续切片）；
//! - [`LineInfo`]：逐源码行语义（引用前缀、列表归属、是否在围栏/表格里），
//!   供 app 做 gutter / hanging indent / 命中几何；
//! - [`ParsedMarkdown`]：上述内容的集合，外加 `code_spans` / `unsupported_spans`
//!   （editor 用来抑制“未支持构造”内部的旧 scanner 构造）。
//!
//! # 明确不做
//!
//! 图片 Object 渲染、任务框点击、**表格单元格就地编辑/加行加列/拖拽**、
//! **画布预览渲染**（本 crate 只给源码区间，scene JSON→PNG 在 app 的 `drawing` 模块）、
//! 脚注/数学/alerts、HTML 执行、任何源码归一化或重新序列化。
//! 表格只提供**只读**块树与源码区间，渲染（Object Block / 编辑源码切换）在
//! editor/app（ADR 0019 S3a/S3b 的范围）。
#![forbid(unsafe_code)]

use std::fmt;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// ADR 0019 S3b7：GFM 表格**单元格行内 Markdown**（纯解析；display 文本 + 样式段 +
/// 链接可见区间）。
///
/// 单独成模块是因为它的输入是**cell 源码切片**（不是整篇文档），输出是 display 坐标
/// （标记隐藏后与源码不再线性对应），与整篇的 [`ParsedMarkdown`] 是两套不同的坐标。
pub mod table_inline;

pub use table_inline::{
    parse_table_cell_inline, TableCellInline, TableCellLink, TableCellSpan, TableCellStyle,
};

/// 源码字节区间（半开 `[start, end)`）。
///
/// 本 crate 不依赖 editor 的坐标强类型：`menghunote-editor` 在边界处把它转成
/// `menghunote_text::SourceRange`，保证“坐标统一 SourceByte”。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Span {
    /// 起始字节（含）。
    pub start: usize,
    /// 结束字节（不含）。
    pub end: usize,
}

impl Span {
    /// 构造半开区间。
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// 空区间（`start == end`）。
    #[must_use]
    pub const fn empty(at: usize) -> Self {
        Self { start: at, end: at }
    }

    /// 字节长度。
    #[must_use]
    pub const fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// 是否为空。
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.end <= self.start
    }

    /// 是否包含字节 `at`（半开区间）。
    #[must_use]
    pub const fn contains(self, at: usize) -> bool {
        self.start <= at && at < self.end
    }

    /// 两个区间是否有交集（空区间永不与任何区间相交）。
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// 是否完整包含另一个区间（空区间包含于任何区间）。
    #[must_use]
    pub const fn contains_span(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    /// 转成 `menghunote_text::SourceRange`（校验 UTF-8 scalar 边界与范围）。
    ///
    /// editor 在边界处用它把本 crate 的裸字节区间收口成强类型 `SourceByte`，
    /// 保证“坐标统一 SourceByte”不是口头约定。
    pub fn to_source_range(
        self,
        source: &str,
    ) -> Result<menghunote_text::SourceRange, menghunote_text::CoordError> {
        menghunote_text::SourceRange::within(
            source,
            menghunote_text::SourceByte::new(self.start),
            menghunote_text::SourceByte::new(self.end),
        )
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}, {})", self.start, self.end)
    }
}

/// 构造种类（只表示语义，不含外观）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConstructKind {
    /// ATX 标题，`level` 为 1–6。
    Heading(u8),
    /// 引用块的一行（`markers` 是每层 `>` 前缀）。
    BlockQuote,
    /// 列表项标记（`markers[0]` 是 marker + 其后的空白）。
    ListItem,
    /// 围栏代码块（开/闭围栏各一段 marker）。
    FencedCode,
    /// `**bold**` / `__bold__`。
    Bold,
    /// `*em*` / `_em_`。
    Emphasis,
    /// `` `code` ``。
    InlineCode,
    /// `~~strike~~`。
    Strike,
    /// `[text](url)` / `[text][label]` / `<autolink>`。
    Link,
    /// `[[Note]]` / `[[Note|alias]]` / `[[Note#Heading]]`（L1 只投影/点击）。
    WikiLink,
    /// `![alt](url)`（S1 只解析，不投影；图片 Object 在 S2）。
    Image,
}

impl ConstructKind {
    /// 是否块级构造。
    #[must_use]
    pub const fn is_block(self) -> bool {
        matches!(
            self,
            Self::Heading(_) | Self::BlockQuote | Self::ListItem | Self::FencedCode
        )
    }

    /// 行内构造。
    #[must_use]
    pub const fn is_inline(self) -> bool {
        matches!(
            self,
            Self::Bold
                | Self::Emphasis
                | Self::InlineCode
                | Self::Strike
                | Self::Link
                | Self::Image
                | Self::WikiLink
        )
    }
}

/// 一个可投影构造：整段 + 要隐藏的标记 + 内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Construct {
    /// 构造类型。
    pub kind: ConstructKind,
    /// 整段 source 区间。
    pub span: Span,
    /// 要隐藏的标记区间（非空、有序、互不重叠、都在 `span` 内）。
    pub markers: Vec<Span>,
    /// 内容区间（不含本层标记；可能为空）。
    pub content: Span,
}

/// 一个普通 Markdown 链接（`[text](url)`）的 source 元数据。
///
/// `span` 是完整链接；`content` 是方括号内文本；`dest` 是 pulldown-cmark 解析后的
/// 目标（引用式链接也已解析）。正文逐字节保留，不重新序列化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    /// 完整链接区间。
    pub span: Span,
    /// 方括号内文本区间。
    pub content: Span,
    /// 解析后的目标 URL。
    pub dest: String,
}

/// 一条 wiki 链接（`[[target#heading|alias]]`）的 source 元数据。
///
/// `display` 是投影后可见的内容：有 alias 时是 alias；否则是 target/heading 原文。
/// `target` 为空表示 `[[#Heading]]`（指向当前笔记）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiLink {
    /// 整条链接（含 `[[`/`]]`）。
    pub span: Span,
    /// 目标笔记（不含 `#heading`；可为空）。
    pub target: Span,
    /// `#` 后的标题（不含 `#`）。
    pub heading: Option<Span>,
    /// `|` 后的别名。
    pub alias: Option<Span>,
    /// 投影可见内容。
    pub display: Span,
}

/// 列表块（同一层级的连续列表项）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListBlock {
    /// 列表块区间。
    pub span: Span,
    /// 是否有序列表。
    pub ordered: bool,
    /// 有序列表起始号（无序列表为 1）。
    pub start: u64,
    /// 是否紧凑（tight）；`false` = 松列表。
    pub tight: bool,
    /// 嵌套层级（最外层 0）。
    pub depth: usize,
    /// 本列表内的 item 下标（全局 `items` 数组）。
    pub items: Vec<usize>,
    /// 编号显示宽度（同一列表里最大编号的位数；无序列表为 1）。
    pub number_width: usize,
}

/// 一个列表项。
///
/// `marker` 是**源码**标记（`- ` / `1. `，含标记后的空白），`content_start` 是
/// 内容起点；续行/子段落/内部围栏的缩进由 `content_indent` 表示（相对引用前缀）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    /// 整个 item 的源码区间（从 marker 到 item 末尾）。
    pub span: Span,
    /// marker + marker 后的空白（源码）。
    pub marker: Span,
    /// 内容区间（`content_start..span.end`）。
    pub content: Span,
    /// 内容起点字节。
    pub content_start: usize,
    /// 嵌套层级（最外层 0）。
    pub depth: usize,
    /// 是否有序。
    pub ordered: bool,
    /// 无序列表的 bullet 字符（`-`/`*`/`+`；有序列表为 `.`/`)`）。
    pub delimiter: char,
    /// 有序编号（无序列表为 0）。
    pub number: u64,
    /// 所属列表块下标。
    pub list_index: usize,
    /// 是否松列表项（列表 tight = false）。
    pub loose: bool,
    /// 首行行号（0 起）。
    pub first_line: usize,
    /// 末行行号（0 起，含）。
    pub last_line: usize,
    /// 首行中 marker 之前、引用前缀之后的空白（布局缩进由 app 提供，需要隐藏）。
    pub hidden_prefix: Span,
    /// 内容缩进（相对引用前缀的字节数；续行要隐藏这么多前导空白）。
    pub content_indent: usize,
    /// 编号显示宽度（右对齐用）。
    pub number_width: usize,
    /// 任务框状态（S1 只记录；S3 起由 app 点击切换，见 [`Self::task_marker`]）。
    pub task: Option<bool>,
    /// 任务框**源码**区间（`[ ]` / `[x]` / `[X]`）。
    ///
    /// 覆盖两个方括号与其内部字符（GFM 允许单个空格/Tab/`x`/`X`），
    /// **不含** `]` 之后的空白或正文内容。`None` 表示该 item 不是任务项。
    ///
    /// 与 `pulldown-cmark` 的 `Event::TaskListMarker` 一一对应：状态看 [`Self::task`]，
    /// 写回时只替换这 3 个字节，缩进/marker/其它正文逐字不变（ADR 0019 S3）。
    pub task_marker: Option<Span>,
}

impl ListItem {
    /// 首行内容结尾（不含换行）。
    #[must_use]
    pub fn first_line_text(&self) -> Span {
        Span::new(self.marker.start, self.content.end.min(self.span.end))
    }
}

/// 列表行的布局信息（app 的 gutter / hanging indent 用它）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListLineInfo {
    /// 所属 item 下标。
    pub item: usize,
    /// 嵌套层级。
    pub depth: usize,
    /// 本行是否是 item 首行。
    pub is_item_start: bool,
    /// item 首行的 marker（其它行为 `None`）。
    pub marker: Option<Span>,
    /// 本行要隐藏的前导空白（布局缩进由 app 提供）。
    pub hidden_prefix: Span,
    /// 内容缩进（相对引用前缀的字节数）。
    pub content_indent: usize,
}

/// 逐行语义种类（app 的块级样式用；与旧 `block_runs` 的分类语义一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineKind {
    /// 普通文本行（含空行）。
    Paragraph,
    /// ATX 标题行。
    Heading(u8),
    /// 引用行。
    BlockQuote,
    /// 列表项首行。
    ListItem,
    /// 围栏代码块的行（开/闭/内部）。
    FencedCode,
    /// 缩进代码块的行。
    IndentedCode,
    /// 表格行。
    Table,
    /// HTML 块的行。
    Html,
    /// 分隔线。
    ThematicBreak,
}

/// GFM 表格单元格的对齐方式（来自分隔行 `:---:` / `---:` / `:---`）。
///
/// 与 `pulldown-cmark` 的 `Alignment` 一一对应，但**本 crate 自己的类型**：
/// 上游枚举不进入公共 API，升级解析器时不强迫下游改签名。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub enum TableAlignment {
    /// 默认（分隔行里没有冒号）。
    #[default]
    None,
    /// 左对齐（`:---`）。
    Left,
    /// 居中（`:---:`）。
    Center,
    /// 右对齐（`---:`）。
    Right,
}

/// 一行表格（表头行或数据行）的源码语义（ADR 0019 S3a）。
///
/// `cells` 与 [`TableBlock::columns`] 等长：解析器给出的列数不足时用空区间补齐，
/// 多出的列（GFM 会丢弃）不存在。cell 区间已去掉 GFM 的单层首尾空白，
/// **不含** `|` 边界与两侧空白。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    /// 整行内容区间（不含 `\n` / `\r\n`）。
    pub span: Span,
    /// 源码行号（0 起）。
    pub line: usize,
    /// 每列 cell 文本的源码区间（长度 = [`TableBlock::columns`]；空 cell 是空区间）。
    pub cells: Vec<Span>,
}

impl TableRow {
    /// 第 `column` 列的 cell 区间；越界返回 `None`（不猜列，也不 panic）。
    #[must_use]
    pub fn cell(&self, column: usize) -> Option<Span> {
        self.cells.get(column).copied()
    }
}

/// 一个 GFM 表格块（ADR 0019 S3a）。
///
/// 这是**只读渲染**需要的最小结构：整表源码跨度 + 表头/分隔行/数据行 + 每行
/// 每个 cell 的源码区间 + 对齐信息。它**不产生任何新文本**，所有区间都能在
/// 源码里逐字节切片回原文；object 投影与绘制都由下游（editor/app）决定。
///
/// # 与 `unsupported_spans` 的关系
///
/// 表格不再是“未支持块”：它有自己的块树，因此**不进入** `unsupported_spans`
/// （否则 editor 的 Object Block 判定会与“未支持块”冲突）。但表格内部仍然是
/// 原子块：内部行内构造不参与源码投影，见 [`ParsedMarkdown::opaque_spans`]。
///
/// # 边界（写死，测试钉住）
///
/// - [`Self::span`] 覆盖整表源码但**不含**表格最后一行之后的换行符：把它换成
///   单行 Object 占位时，后面的换行仍留在正文里，不会与下一段粘成一行；
/// - 行区间同样不含行尾换行；
/// - GFM 规定表格必须由表头行 + 分隔行组成，因此 [`Self::header`] 与
///   [`Self::delimiter`] 恒存在（解析器不给出表头的异常输入不会变成表格）；
/// - 分隔行不产生 `TableRow` 事件（`pulldown-cmark` 只发 `TableHead` / `TableRow`），
///   这里按“表头行的下一行”定位它，只给整行区间（对齐信息在
///   [`Self::alignments`] 里）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableBlock {
    /// 整表源码区间（表头行起点 .. 最后一行的内容终点；不含行尾换行）。
    pub span: Span,
    /// 表头行。
    pub header: TableRow,
    /// 分隔行整行区间（不含行尾换行）。
    pub delimiter: Span,
    /// 数据行（按源码顺序；可能为空）。
    pub rows: Vec<TableRow>,
    /// 每列对齐方式（长度 = [`Self::columns`]）。
    pub alignments: Vec<TableAlignment>,
    /// 列数（由分隔行决定；恒 ≥ 1）。
    pub columns: usize,
    /// 首行行号（0 起；= 表头行行号）。
    pub first_line: usize,
    /// 末行行号（0 起，含）。
    pub last_line: usize,
}

impl TableBlock {
    /// 数据行数（不含表头与分隔行）。
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// 第 `row` 条数据行；越界返回 `None`。
    #[must_use]
    pub fn row(&self, row: usize) -> Option<&TableRow> {
        self.rows.get(row)
    }

    /// 第 `row` 条数据行第 `column` 列的 cell 区间；任一越界返回 `None`。
    #[must_use]
    pub fn cell(&self, row: usize, column: usize) -> Option<Span> {
        self.rows.get(row)?.cell(column)
    }

    /// 第 `column` 列的对齐方式（越界按默认对齐，不 panic）。
    #[must_use]
    pub fn alignment(&self, column: usize) -> TableAlignment {
        self.alignments.get(column).copied().unwrap_or_default()
    }
}

/// 一个 fenced `mermaid` 代码块的源码元数据（ADR 0019 M2）。
///
/// Mermaid 块在编辑器里不是普通代码块，而是 **Object Block**：整块源码被一个
/// display 占位符替换，只有显式「编辑源码」/`F2` 才显示 fenced 源码。本类型只提供
/// **源码事实**（跨度/行号/info），不解析 Mermaid 语法，也不产生任何新文本。
///
/// # 边界（写死，测试钉住）
///
/// - [`Self::span`]：开围栏行行首 .. 闭围栏行内容终点（**不含**闭围栏行之后的
///   换行）；把它换成单行 Object 占位时，后面的换行仍留在正文里；
/// - [`Self::content`]：开围栏行行尾 .. 闭围栏行行首（即两条围栏之间的原始源码，
///   保留内部换行与缩进；行内字符逐字节保留，不做转义/实体还原）；
/// - [`Self::opening`]：开围栏标记本身（跳过行首缩进，到行内容终点；含 info string）；
/// - [`Self::first_line`] / [`Self::last_line`]：开/闭围栏的源码行号（0 起）；
/// - [`Self::info`]：`pulldown-cmark` 给出的 info string（去掉首尾空白）；本块只收
///   **精确等于 `mermaid`** 的 info。
///
/// # `block` 判定（普通段落上下文）
///
/// 只有满足全部条件才 `block = true`（编辑器据此投影成 Object）：
///
/// 1. 围栏块是闭合的（未闭合围栏按普通代码块处理，不产生本类型）；
/// 2. 开/闭围栏所在行都是 `LineKind::FencedCode`；
/// 3. 两行都不是引用、不在列表项里；
/// 4. 开围栏标记从该行**内容起点**开始（行首缩进不投影成对象）；
/// 5. 块区间结束在闭围栏行的内容终点（不含末尾换行）。
///
/// `block = false` 的 Mermaid 块（列表/缩进里）保持普通 fenced code 的编辑
/// 行为：仍然有 `ConstructKind::FencedCode` 构造、可编辑源码。
///
/// 注意：**引用前缀里的围栏**（`> ```mermaid`）沿用 `fenced_construct` 的既有边界
/// （它只跳过空格缩进，不跳过 `>`），因此不产生本类型，仍按普通代码块处理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FencedMermaidBlock {
    /// 整块源码区间（开围栏行行首 .. 闭围栏行内容终点；不含末尾换行）。
    pub span: Span,
    /// 围栏之间的内容区间（不含开/闭围栏行）。
    pub content: Span,
    /// 开围栏标记区间（跳过行首缩进，到行内容终点；含 info string）。
    pub opening: Span,
    /// 开围栏源码行号（0 起）。
    pub first_line: usize,
    /// 闭围栏源码行号（0 起，含）。
    pub last_line: usize,
    /// info string（去掉首尾空白；本类型只出现在精确等于 `mermaid` 时）。
    pub info: String,
    /// 是否可投影成 Object Block（见类型文档的判定规则）。
    pub block: bool,
}

impl FencedMermaidBlock {
    /// 是否整块、普通段落上下文里的 fenced mermaid（可投影成 Object Block）。
    #[must_use]
    pub const fn is_block(&self) -> bool {
        self.block
    }
}

/// 一个代码块（fenced / indented）的**读取用**源码元数据。
///
/// 只记录“源码里这一段是代码块、语言是什么、内容在哪”，**不产生任何新文本**，
/// 也不改变 [`ParsedMarkdown::code_spans`] 的既有语义。`menghunote-app` 的只读语法
/// 高亮用它：
///
/// - [`Self::content`] 切片作为高亮输入（围栏行不算代码内容）；
/// - [`Self::info`] 取语言（app 只取第一个词，如 `rust,no_run` → `rust`）；
/// - [`Self::span`] 用于判定“这一行/这一段在代码块里”，从而只对代码行着色。
///
/// # 边界
///
/// - [`Self::span`]：整块源码区间（首行行首 .. 末行行尾，**含**换行），与
///   [`ParsedMarkdown::code_spans`] 中的对应区间逐字节一致；
/// - [`Self::content`]：
///   - fenced：开围栏行行尾 .. 闭围栏行行首（与 [`FencedMermaidBlock::content`]
///     同一口径；围栏行本身不是代码内容），空块是空区间；
///   - indented：整块区间（缩进代码没有围栏行；保留行首缩进，交给高亮器处理）；
///   - 无法定位闭合围栏的 fenced 块（未闭合/异常围栏）退化为“开围栏行之后到
///     块尾”，且 `opening` 为空区间 —— 宁可少着色，也不把围栏行当代码；
/// - [`Self::opening`]：fenced 的开围栏标记（跳过行首缩进，到行内容终点；含 info），
///   indented 为空区间（`span.start`）；
/// - [`Self::info`]：`pulldown-cmark` 给出的 info string（去掉首尾空白；indented 为空串）；
/// - [`Self::first_line`] / [`Self::last_line`]：首/末源码行号（0 起；fenced 含围栏行）。
///
/// **mermaid 块也记录**（本类型不筛语言）：app 可以按 info 跳过 mermaid；它同时
/// 仍出现在 [`ParsedMarkdown::mermaid_blocks`] 里，两个字段互不影响。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlockSpan {
    /// 整块源码区间（含围栏行与换行），与 `code_spans` 一致。
    pub span: Span,
    /// 代码内容区间（不含围栏行；见类型文档的口径）。
    pub content: Span,
    /// 开围栏标记区间（indented 为空区间）。
    pub opening: Span,
    /// info string（已去首尾空白；indented 为空串）。
    pub info: String,
    /// 首行源码行号（0 起）。
    pub first_line: usize,
    /// 末行源码行号（0 起，含）。
    pub last_line: usize,
    /// 是否 fenced（`false` = 缩进代码块）。
    pub fenced: bool,
}

/// 一行源码的语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineInfo {
    /// 整行区间（含换行符）。
    pub span: Span,
    /// 行内容区间（不含 `\n` / `\r\n`）。
    pub content: Span,
    /// 引用前缀区间（`0..prefix_end`；无引用为空区间在 `span.start`）。
    pub quote: Span,
    /// 语义种类。
    pub kind: LineKind,
    /// 列表归属（非列表行为 `None`）。
    pub list: Option<ListLineInfo>,
}

/// 一个图片构造的**读取用**元数据（ADR 0019 S2a）。
///
/// 只记录“源码里这一段是图片、它的 alt/目标是什么”，**不产生任何新文本**：
/// [`Self::dest`] 来自 `pulldown-cmark` 的 `Tag::Image::dest_url`（已解码的语义
/// 目标，引用式图片也解析成真实 URL），正文仍然逐字节保留在源码里。
///
/// 与 [`Construct`] 的关系：图片仍然是一个 `ConstructKind::Image` 构造（S1 语义
/// 不变），本类型补充“目标 URL”这一投影层之外的信息，供 editor/app 判断能否作为
/// Object Block（是否整行独占由 editor 用 [`LineInfo`] 判断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSpan {
    /// 整段 `![alt](dest)` 的源码区间（含定界符）。
    pub span: Span,
    /// alt 内容区间（`[...]` 内部，不含方括号）。
    pub content: Span,
    /// 解析后的目标 URL（`asset://<hex>` 等原样字符串，不重新序列化）。
    pub dest: String,
}

/// 一整个**内嵌画布笔记**的源码元数据（ADR 0025 后续切片）。
///
/// 普通 Markdown 笔记里写一行
///
/// ```md
/// ![[流程图.excalidraw]]
/// ```
///
/// 就得到一条本记录：整行源码投影成一个图片式 Object Block，预览图由 app 通过
/// 宿主注入的资产 provider 生成（本 crate 只给源码事实，不渲染、不读库）。
///
/// # 边界（写死，测试钉住）
///
/// - [`Self::span`]：整行内容区间（行内容起点 .. 行内容终点），**不含**行尾
///   空白与换行；把它换成单行 Object 占位时，行尾换行仍留在正文里；
/// - [`Self::target`]：`|` **之前**的源码区间（已去掉首尾空白），大小写不敏感地
///   以 `.excalidraw` 结尾；目标原文从源码切片，不重新序列化；
/// - [`Self::alias`]：`|` **之后**的源码区间（已去掉首尾空白）；`![[A|]]` 这类
///   空白别名与没有别名等价；
/// - [`Self::line`]：源码行号（0 起）；
/// - [`Self::block`]：是否可投影成 Object Block（当前解析器只收录整行独占的行，
///   因此恒为 `true`；保留字段与其它 Object 块保持一致，便于将来扩展）；
/// - 不满足整行独占 / 普通段落 / 非空 `.excalidraw` 目标时**不产生记录**：该行
///   继续按普通文本显示，绝不吞掉半行内容（与图片 `block` 判定同一策略）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawingSpan {
    /// 整行内容区间（不含行尾空白与换行）。
    pub span: Span,
    /// 目标笔记区间（`|` 前，已去首尾空白；大小写不敏感地以 `.excalidraw` 结尾）。
    pub target: Span,
    /// 可选别名的源码区间（`|` 后，已去首尾空白）。
    pub alias: Option<Span>,
    /// 源码行号（0 起）。
    pub line: usize,
    /// 是否可投影成 Object Block（当前恒为 `true`）。
    pub block: bool,
}

impl DrawingSpan {
    /// 是否整行独占、可投影成 Object Block。
    #[must_use]
    pub const fn is_block(&self) -> bool {
        self.block
    }
}

/// 解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMarkdown {
    /// 逐源码行语义（至少一行；空文档一行空行）。
    pub lines: Vec<LineInfo>,
    /// 列表块（按出现顺序）。
    pub lists: Vec<ListBlock>,
    /// 列表项（按出现顺序）。
    pub items: Vec<ListItem>,
    /// wiki 链接（按 `span` 起点排序；不含代码块/行内代码/转义）。
    pub wiki_links: Vec<WikiLink>,
    /// 普通链接（含 inline/reference/autolink；`dest` 已解析）。
    pub links: Vec<LinkSpan>,
    /// 图片构造（含 inline / 引用式；按 `span` 起点排序）。
    ///
    /// 每个图片同时也出现在 [`Self::constructs`] 里（`ConstructKind::Image`）；
    /// 这里额外给出目标 URL，投影层据此判断能否渲染成 Object Block。
    pub images: Vec<ImageSpan>,
    /// 整行独占的内嵌画布笔记（`![[目标.excalidraw]]`，ADR 0025 后续切片）。
    ///
    /// 只收录**可投影成 Object Block** 的整行独占写法（按 `span` 起点排序）；
    /// 混排/行内/代码块/引用/列表/表格里的 `![[...]]` 不在此列表里，继续按普通
    /// 文本显示。目标不以 `.excalidraw` 结尾（大小写不敏感）也不算。
    pub drawings: Vec<DrawingSpan>,
    /// GFM 表格块（ADR 0019 S3a；按 `span` 起点排序）。
    ///
    /// 表格是**原子块**：内部行内构造不会出现在 [`Self::constructs`] 里，
    /// editor 把它投影成 Object Block 或（不满足整行独占时）原样显示源码。
    pub tables: Vec<TableBlock>,
    /// fenced `mermaid` 块（按 `span` 起点排序）。
    ///
    /// `block = true` 的块**不产生** `ConstructKind::FencedCode`（它投影成 Object
    /// Block，不能同时被当成隐藏围栏的普通代码块）；`block = false` 的块保持普通
    /// 代码块行为（构造仍在 [`Self::constructs`] 里）。两种块都在 `code_spans` 里，
    /// 内部行内构造一律不投影。引用前缀里的围栏不产生本类型（见
    /// [`FencedMermaidBlock`] 的说明）。
    pub mermaid_blocks: Vec<FencedMermaidBlock>,
    /// 全部代码块（fenced / indented；按 `span` 起点排序）。
    ///
    /// 与 [`Self::mermaid_blocks`] 不同，这里**不筛语言**：mermaid 块也在。给 app
    /// 的只读语法高亮提供语言与内容区间；不改变 [`Self::code_spans`] 的语义。
    pub code_blocks: Vec<CodeBlockSpan>,
    /// 可投影构造（块级 + 行内；按 span/kind 稳定排序）。
    pub constructs: Vec<Construct>,
    /// 代码块区间（围栏/缩进；editor 用它抑制旧 scanner 在代码内部的构造）。
    pub code_spans: Vec<Span>,
    /// 未支持块区间（HTML 块 / Setext / 无空格列表 marker 等；editor 原样显示、
    /// 不投影内部构造）。
    ///
    /// **表格不在这里**（ADR 0019 S3a）：表格有 [`Self::tables`] 块树，属于已支持
    /// 原子块；把整表再塞进来会与 Object Block 判定冲突。需要“不投影内部构造”的
    /// 调用方请用 [`Self::opaque_spans`]。
    pub unsupported_spans: Vec<Span>,
    /// 行内构造里出现过“定位不确定”的情况（editor 应整篇退回旧 scanner 的行内投影）。
    pub uncertain_inline: bool,
}

impl ParsedMarkdown {
    /// 不投影内部行内构造的**原子块**区间（代码块 + 表格 + 未支持块）。
    ///
    /// editor 用它回答“这个构造是不是在原子块内部”：在内部的构造一律按源码原样
    /// 显示（代码/表格/HTML 里的 `**`、`![...]` 都不是可投影语法）。返回顺序按区间
    /// 起点近似有序，调用方按需自行排序/合并，不假设互不重叠。
    pub fn opaque_spans(&self) -> impl Iterator<Item = Span> + '_ {
        self.code_spans
            .iter()
            .copied()
            .chain(self.tables.iter().map(|table| table.span))
            .chain(self.unsupported_spans.iter().copied())
    }
}

/// ADR 0019 §1 规定的 Options：显式开启表格/任务列表/删除线。
///
/// `ENABLE_GFM` **不包含**这三者，不能拿它代替。
#[must_use]
pub fn options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options
}

/// 用 ADR 规定的 Options 解析。
#[must_use]
pub fn parse(source: &str) -> ParsedMarkdown {
    parse_with_options(source, options())
}

/// 用显式 Options 解析（测试/未来扩展用；生产请用 [`parse`]）。
#[must_use]
pub fn parse_with_options(source: &str, options: Options) -> ParsedMarkdown {
    let mut builder = Builder::new(source);
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        builder.handle(event, Span::new(range.start, range.end));
    }
    builder.finish()
}

// ---------------------------------------------------------------------------
// 实现
// ---------------------------------------------------------------------------

/// 一行源码的原始几何。
#[derive(Debug, Clone, Copy)]
struct Line {
    /// 行起点（含）。
    start: usize,
    /// 行内容终点（不含 `\n` / `\r\n`）。
    content_end: usize,
    /// 行终点（含换行符）。
    end: usize,
    /// 引用前缀终点（无引用时为 `start`）。
    quote_end: usize,
}

/// 列表块构建状态。
struct ListFrame {
    span_start: usize,
    ordered: bool,
    start: u64,
    items: Vec<usize>,
    depth: usize,
}

/// 列表项构建状态。
struct ItemFrame {
    item: Option<usize>,
    span_start: usize,
    marker_end: usize,
}

/// 围栏/缩进代码块构建状态。
struct CodeFrame {
    span_start: usize,
    fenced: bool,
    info: String,
}

/// 表格块构建状态。
struct TableFrame {
    span_start: usize,
    alignments: Vec<TableAlignment>,
    header: Option<TableFrameRow>,
    rows: Vec<TableFrameRow>,
    /// 正在收集的当前行（表头或数据行）。
    current: Option<TableFrameRow>,
}

/// 一行表格构建中的原始区间（cell 尚未裁掉首尾空白）。
struct TableFrameRow {
    span: Span,
    cells: Vec<Span>,
}

/// 行内构造的开始位置。
struct InlineFrame {
    kind: InlineKind,
    start: usize,
}

#[derive(Clone, Copy)]
enum InlineKind {
    Bold,
    Emphasis,
    Strike,
    Link,
    Image,
}

struct Builder<'a> {
    source: &'a str,
    lines: Vec<Line>,
    lists: Vec<ListFrame>,
    list_blocks: Vec<ListBlock>,
    items: Vec<ListItem>,
    items_stack: Vec<ItemFrame>,
    code_stack: Vec<CodeFrame>,
    inline_stack: Vec<InlineFrame>,
    /// 当前打开的图片事件 `(起点, 目标 URL)`（`TagEnd::Image` 时与 `finish_inline` 配对）。
    image_stack: Vec<(usize, String)>,
    /// 解析出的图片元数据（按 `span` 起点排序后写进 [`ParsedMarkdown::images`]）。
    images: Vec<ImageSpan>,
    /// 普通链接的 `(start, dest)` 栈，`TagEnd::Link` 时配对。
    link_stack: Vec<(usize, String)>,
    /// 解析出的普通链接元数据。
    links: Vec<LinkSpan>,
    quote_stack: Vec<Span>,
    table_stack: Vec<TableFrame>,
    /// 正在收集的 cell 起点（`TagEnd::TableCell` 时配对成区间）。
    table_cell_start: Option<usize>,
    /// 解析出的表格块（按 `span` 起点排序后写进 [`ParsedMarkdown::tables`]）。
    tables: Vec<TableBlock>,
    /// 解析出的 fenced `mermaid` 块（`block` 在 finish 阶段按行语义计算）。
    mermaid_blocks: Vec<FencedMermaidBlock>,
    /// 解析出的全部代码块（性质见 [`CodeBlockSpan`]；mermaid 也在内）。
    code_blocks: Vec<CodeBlockSpan>,
    html_stack: Vec<Span>,
    constructs: Vec<Construct>,
    headings: Vec<(Span, u8)>,
    code_spans: Vec<Span>,
    fence_spans: Vec<Span>,
    indented_spans: Vec<Span>,
    unsupported_spans: Vec<Span>,
    table_spans: Vec<Span>,
    html_spans: Vec<Span>,
    rule_lines: Vec<usize>,
    uncertain_inline: bool,
}

impl<'a> Builder<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            lines: split_lines(source),
            lists: Vec::new(),
            list_blocks: Vec::new(),
            items: Vec::new(),
            items_stack: Vec::new(),
            code_stack: Vec::new(),
            inline_stack: Vec::new(),
            image_stack: Vec::new(),
            link_stack: Vec::new(),
            images: Vec::new(),
            links: Vec::new(),
            quote_stack: Vec::new(),
            table_stack: Vec::new(),
            table_cell_start: None,
            tables: Vec::new(),
            mermaid_blocks: Vec::new(),
            code_blocks: Vec::new(),
            html_stack: Vec::new(),
            constructs: Vec::new(),
            headings: Vec::new(),
            code_spans: Vec::new(),
            fence_spans: Vec::new(),
            indented_spans: Vec::new(),
            unsupported_spans: Vec::new(),
            table_spans: Vec::new(),
            html_spans: Vec::new(),
            rule_lines: Vec::new(),
            uncertain_inline: false,
        }
    }

    fn handle(&mut self, event: Event<'_>, range: Span) {
        match event {
            Event::Start(tag) => self.start(tag, range),
            Event::End(tag) => self.end(tag, range),
            Event::Code(_) => self.inline_code(range),
            Event::TaskListMarker(checked) => {
                // 事件顺序是 Start(Item) → TaskListMarker → Text；必须挂到当前打开的 item，
                // 不能留到下一个 Start(Item)（否则会整体错位一个 item）。
                //
                // 区间先按当前 item 算好再拿可变借用：事件 range 就是源码里 `[ ]`/`[x]`
                // 的 3 个字节（已用 golden 测试钉住），这里再做一次形状校验，形状不符时
                // 宁可只记录状态、不暴露可点区间（不猜 offset）。
                let marker = Self::task_marker_span(self.source, range);
                if let Some(frame) = self.items_stack.last() {
                    if let Some(item) = frame.item.and_then(|index| self.items.get_mut(index)) {
                        item.task = Some(checked);
                        item.task_marker = marker;
                    }
                }
            }
            Event::Rule => {
                if let Some(line) = self.line_index_at(range.start) {
                    self.rule_lines.push(line);
                }
            }
            Event::InlineHtml(_) | Event::Html(_) => {
                // raw HTML 不执行、不改写正文；块级 HTML 在 start/end 里登记。
            }
            Event::Text(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::FootnoteReference(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>, range: Span) {
        match tag {
            Tag::Heading { level, .. } => {
                // 构造在 Start 时即可完整确定（ATX 标题是单行）。
                let _ = self.heading_construct(range, level as u8);
            }
            Tag::BlockQuote(_) => self.quote_stack.push(range),
            Tag::List(start) => self.lists.push(ListFrame {
                span_start: range.start,
                ordered: start.is_some(),
                start: start.unwrap_or(1),
                items: Vec::new(),
                depth: self.lists.len(),
            }),
            Tag::Item => self.start_item(range),
            Tag::CodeBlock(kind) => {
                let (fenced, info) = match kind {
                    CodeBlockKind::Fenced(info) => (true, info.to_string()),
                    CodeBlockKind::Indented => (false, String::new()),
                };
                self.code_stack.push(CodeFrame {
                    span_start: range.start,
                    fenced,
                    info,
                });
            }
            Tag::Table(alignments) => self.table_stack.push(TableFrame {
                span_start: range.start,
                alignments: alignments
                    .iter()
                    .map(|alignment| convert_alignment(*alignment))
                    .collect(),
                header: None,
                rows: Vec::new(),
                current: None,
            }),
            Tag::TableHead => self.start_table_row(range, true),
            Tag::TableRow => self.start_table_row(range, false),
            Tag::TableCell => self.table_cell_start = Some(range.start),
            Tag::HtmlBlock => self.html_stack.push(range),
            Tag::Superscript | Tag::Subscript => {}
            Tag::Strong => self.push_inline(InlineKind::Bold, range.start),
            Tag::Emphasis => self.push_inline(InlineKind::Emphasis, range.start),
            Tag::Strikethrough => self.push_inline(InlineKind::Strike, range.start),
            Tag::Link { dest_url, .. } => {
                self.link_stack.push((range.start, dest_url.to_string()));
                self.push_inline(InlineKind::Link, range.start);
            }
            Tag::Image { dest_url, .. } => {
                // `dest_url` 是 pulldown-cmark 解析后的语义目标（引用式图片也在这里
                // 变成真实 URL）。**只读取、不重写正文**：源码逐字节保留。
                self.image_stack.push((range.start, dest_url.to_string()));
                self.push_inline(InlineKind::Image, range.start);
            }
            Tag::Paragraph
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd, range: Span) {
        match tag {
            TagEnd::Heading(_) => {}
            TagEnd::BlockQuote(_) => {
                if let Some(span) = self.quote_stack.pop() {
                    self.quote_constructs(span);
                }
            }
            TagEnd::List(_) => {
                if let Some(frame) = self.lists.pop() {
                    self.finish_list(frame, range.end);
                }
            }
            TagEnd::Item => self.finish_item(range.end),
            TagEnd::CodeBlock => {
                if let Some(frame) = self.code_stack.pop() {
                    self.finish_code(frame, range.end);
                }
            }
            TagEnd::Table => {
                if let Some(frame) = self.table_stack.pop() {
                    // 表格是已支持原子块：**不进** `unsupported_spans`（ADR 0019 S3a），
                    // 但整表区间仍然登记为表格行，供 `LineKind::Table` / 不投影内部构造。
                    if let Some(table) = self.build_table(frame, range.end) {
                        self.table_spans.push(table.span);
                        self.tables.push(table);
                    }
                }
            }
            TagEnd::HtmlBlock => {
                if let Some(start) = self.html_stack.pop() {
                    let span = Span::new(start.start, range.end.max(start.end));
                    self.html_spans.push(span);
                    self.unsupported_spans.push(span);
                }
            }
            TagEnd::Superscript | TagEnd::Subscript => {}
            TagEnd::Strong => self.finish_inline(InlineKind::Bold, range.end),
            TagEnd::Emphasis => self.finish_inline(InlineKind::Emphasis, range.end),
            TagEnd::Strikethrough => self.finish_inline(InlineKind::Strike, range.end),
            TagEnd::Link => self.finish_inline(InlineKind::Link, range.end),
            TagEnd::Image => self.finish_inline(InlineKind::Image, range.end),
            TagEnd::TableHead => self.finish_table_row(true),
            TagEnd::TableRow => self.finish_table_row(false),
            TagEnd::TableCell => self.finish_table_cell(range.end),
            TagEnd::Paragraph
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn push_inline(&mut self, kind: InlineKind, start: usize) {
        self.inline_stack.push(InlineFrame { kind, start });
    }

    fn finish_inline(&mut self, kind: InlineKind, end: usize) {
        // 事件栈是严格嵌套的；只接受与栈顶同类的结束事件（防御：不匹配就忽略）。
        let Some(frame) = self.inline_stack.pop() else {
            return;
        };
        if !same_inline(frame.kind, kind) {
            return;
        }
        let span = Span::new(frame.start, end);
        // 图片的事件开始处记录了目标 URL；配对检查起点，防止不平衡的事件流错位。
        let image_dest = if matches!(kind, InlineKind::Image) {
            self.image_stack
                .pop()
                .filter(|(start, _)| *start == frame.start)
                .map(|(_, dest)| dest)
        } else {
            None
        };
        let link_dest = if matches!(kind, InlineKind::Link) {
            self.link_stack
                .pop()
                .filter(|(start, _)| *start == frame.start)
                .map(|(_, dest)| dest)
        } else {
            None
        };
        let construct = match kind {
            InlineKind::Bold => self
                .analyze_delimited(span, b'*', 2, ConstructKind::Bold)
                .or_else(|| self.analyze_delimited(span, b'_', 2, ConstructKind::Bold)),
            InlineKind::Emphasis => self
                .analyze_delimited(span, b'*', 1, ConstructKind::Emphasis)
                .or_else(|| self.analyze_delimited(span, b'_', 1, ConstructKind::Emphasis)),
            InlineKind::Strike => self.analyze_delimited(span, b'~', 2, ConstructKind::Strike),
            InlineKind::Link => self.analyze_link(span, false),
            InlineKind::Image => self.analyze_link(span, true),
        };
        match construct {
            Some(construct) => {
                if let Some(dest) = image_dest {
                    self.images.push(ImageSpan {
                        span: construct.span,
                        content: construct.content,
                        dest,
                    });
                }
                if let Some(dest) = link_dest {
                    self.links.push(LinkSpan {
                        span: construct.span,
                        content: construct.content,
                        dest,
                    });
                }
                self.constructs.push(construct);
            }
            None => self.uncertain_inline = true,
        }
    }

    fn inline_code(&mut self, range: Span) {
        let source = self.source.as_bytes();
        if range.is_empty() || range.end > source.len() {
            self.uncertain_inline = true;
            return;
        }
        let run = count_run(source, range.start, range.end, b'`');
        if run == 0 || range.start + run > range.end {
            self.uncertain_inline = true;
            return;
        }
        let mut close_run = 0usize;
        let mut index = range.end;
        while index > range.start + run && source[index - 1] == b'`' {
            close_run += 1;
            index -= 1;
        }
        if close_run != run {
            self.uncertain_inline = true;
            return;
        }
        let content = Span::new(range.start + run, range.end - run);
        self.constructs.push(Construct {
            kind: ConstructKind::InlineCode,
            span: range,
            markers: vec![
                Span::new(range.start, content.start),
                Span::new(content.end, range.end),
            ],
            content,
        });
    }

    fn analyze_delimited(
        &self,
        span: Span,
        marker: u8,
        expected: usize,
        kind: ConstructKind,
    ) -> Option<Construct> {
        let source = self.source.as_bytes();
        if span.end > source.len() || span.end <= span.start + expected {
            return None;
        }
        if source[span.start] != marker {
            return None;
        }
        let open_run = count_run(source, span.start, span.end, marker);
        if open_run != expected {
            return None;
        }
        let mut close_run = 0usize;
        let mut index = span.end;
        while index > span.start + expected && source[index - 1] == marker {
            close_run += 1;
            index -= 1;
        }
        if close_run != expected {
            return None;
        }
        let content = Span::new(span.start + expected, span.end - expected);
        if content.is_empty() {
            return None;
        }
        Some(Construct {
            kind,
            span,
            markers: vec![
                Span::new(span.start, span.start + expected),
                Span::new(span.end - expected, span.end),
            ],
            content,
        })
    }

    fn analyze_link(&self, span: Span, image: bool) -> Option<Construct> {
        let bytes = self.source.as_bytes();
        if span.end > bytes.len() || span.end <= span.start + 1 {
            return None;
        }
        let kind = if image {
            ConstructKind::Image
        } else {
            ConstructKind::Link
        };
        let open_len = if image { 2 } else { 1 };
        if image && bytes.get(span.start..span.start + 2) != Some(b"![") {
            return None;
        }
        if !image && bytes[span.start] != b'[' && bytes[span.start] != b'<' {
            return None;
        }

        // 自动链接：`<https://…>` / `<a@b.c>`。
        if !image && bytes[span.start] == b'<' && bytes[span.end - 1] == b'>' {
            let content = Span::new(span.start + 1, span.end - 1);
            if content.is_empty() {
                return None;
            }
            return Some(Construct {
                kind,
                span,
                markers: vec![
                    Span::new(span.start, span.start + 1),
                    Span::new(span.end - 1, span.end),
                ],
                content,
            });
        }
        let bracket = if image { span.start + 1 } else { span.start };
        if bytes.get(bracket) != Some(&b'[') {
            return None;
        }
        let close = find_matching_bracket(self.source, bracket, span.end)?;
        let content = Span::new(bracket + 1, close);
        if content.is_empty() {
            return None;
        }
        // `[text]` / `[text][]` / `[text][label]` / `[text](dest)`：从 `]` 到 span 末尾
        // 全部是“目标/标签”部分，属于隐藏标记（旧 scanner 对 inline 链接也是这么切）。
        let marker_end = Span::new(close, span.end);
        if marker_end.is_empty() {
            return None;
        }
        Some(Construct {
            kind,
            span,
            markers: vec![Span::new(span.start, span.start + open_len), marker_end],
            content,
        })
    }

    fn heading_construct(&mut self, range: Span, level: u8) -> Option<Construct> {
        let bytes = self.source.as_bytes();
        let line = self.line_index_at(range.start)?;
        let line_info = self.lines[line];
        let start = range.start;
        let run = count_run(bytes, start, line_info.content_end, b'#');
        if run == 0 || run > 6 {
            // Setext 标题（`Title\n=====`）没有 ATX 前缀：S1 原样显示。
            self.unsupported_spans
                .push(Span::new(start, line_info.end.max(range.end)));
            return None;
        }
        let mut marker_end = start + run;
        // 标题 marker 后必须有空格/Tab；`#`/`###` 单独一行不算标题（Typora 体验）。
        if marker_end >= line_info.content_end
            || (bytes[marker_end] != b' ' && bytes[marker_end] != b'\t')
        {
            return None;
        }
        marker_end += 1;
        let span = Span::new(start, line_info.content_end);
        if span.is_empty() {
            return None;
        }
        let content = Span::new(marker_end.min(span.end), span.end);
        let construct = Construct {
            kind: ConstructKind::Heading(level),
            span,
            markers: vec![Span::new(start, marker_end.min(span.end))],
            content,
        };
        if !construct.markers[0].is_empty() {
            self.headings.push((span, level));
            self.constructs.push(construct.clone());
            return Some(construct);
        }
        None
    }

    fn quote_constructs(&mut self, span: Span) {
        let Some(first) = self.line_index_at(span.start) else {
            return;
        };
        let Some(last) = self.line_index_at(span.end.saturating_sub(1)) else {
            return;
        };
        for line in first..=last {
            let info = self.lines[line];
            let markers = scan_quote_markers(self.source, info.start, info.content_end);
            if markers.is_empty() {
                continue;
            }
            let start = markers[0].start;
            let content_start = markers[markers.len() - 1].end;
            let construct_span = Span::new(start, info.content_end);
            if construct_span.is_empty() {
                continue;
            }
            self.constructs.push(Construct {
                kind: ConstructKind::BlockQuote,
                span: construct_span,
                markers,
                content: Span::new(content_start.min(construct_span.end), construct_span.end),
            });
        }
    }

    fn start_item(&mut self, range: Span) {
        let Some(line) = self.line_index_at(range.start) else {
            return;
        };
        let info = self.lines[line];
        let marker_start = skip_spaces(self.source.as_bytes(), range.start, info.content_end);
        let (ordered, list_start, depth) = self.lists.last().map_or((false, 1, 0), |frame| {
            (frame.ordered, frame.start, frame.depth)
        });
        let number = if ordered {
            list_start.saturating_add(
                self.lists
                    .last()
                    .map_or(0, |frame| u64::try_from(frame.items.len()).unwrap_or(0)),
            )
        } else {
            0
        };

        let Some(marker) = scan_list_marker(self.source, marker_start, info.content_end) else {
            // marker 后没有空格/Tab：这不是列表项，原样显示；不要创建 ListItem，
            // 否则 editor 的列表布局会把 `-` 立刻画成 bullet。
            self.unsupported_spans
                .push(Span::new(marker_start, info.content_end));
            self.items_stack.push(ItemFrame {
                item: None,
                span_start: range.start,
                marker_end: marker_start,
            });
            return;
        };

        let content_indent = marker.content_start.saturating_sub(info.quote_end);
        let hidden_prefix = Span::new(info.quote_end.min(marker_start), marker_start);
        let index = self.items.len();
        self.items.push(ListItem {
            span: Span::new(marker_start, marker.content_start),
            marker: marker.span,
            content: Span::empty(marker.content_start),
            content_start: marker.content_start,
            depth,
            ordered,
            delimiter: marker.delimiter,
            number,
            list_index: 0,
            loose: false,
            first_line: line,
            last_line: line,
            hidden_prefix,
            content_indent,
            number_width: 1,
            task: None,
            task_marker: None,
        });
        self.items_stack.push(ItemFrame {
            item: Some(index),
            span_start: range.start,
            marker_end: marker.content_start,
        });
    }

    /// 把 `TaskListMarker` 事件的区间校验成任务框源码区间。
    ///
    /// `pulldown-cmark` 给出的 range 已经是源码里 `[`…`]` 的精确区间（golden 测试
    /// `task_list_marker_range_covers_brackets_only` 钉住）；这里只接受
    /// “`[` + 单个空格/Tab/x/X + `]`” 的形状，任何意外都返回 `None`，不把不确定的
    /// offset 暴露给投影/点击（宁可少一个复选框，也不点错字节）。
    fn task_marker_span(source: &str, range: Span) -> Option<Span> {
        if range.end <= range.start || range.end > source.len() {
            return None;
        }
        let bytes = source.as_bytes().get(range.start..range.end)?;
        if bytes.len() != 3 || bytes[0] != b'[' || bytes[2] != b']' {
            return None;
        }
        if !matches!(bytes[1], b' ' | b'\t' | b'x' | b'X') {
            return None;
        }
        Some(Span::new(range.start, range.end))
    }

    fn finish_item(&mut self, end: usize) {
        let Some(frame) = self.items_stack.pop() else {
            return;
        };
        let last_line = self
            .line_index_at(end.saturating_sub(1))
            .unwrap_or_else(|| self.lines.len().saturating_sub(1));
        if let Some(index) = frame.item {
            if let Some(item) = self.items.get_mut(index) {
                let item_end = end.max(frame.marker_end).max(frame.span_start);
                item.span.end = item_end;
                item.content = Span::new(item.content_start.min(item_end), item_end);
                item.last_line = last_line.max(item.first_line);
            }
            if let Some(list) = self.lists.last_mut() {
                list.items.push(index);
            }
        }
    }

    fn finish_list(&mut self, frame: ListFrame, end: usize) {
        // 所有 item 都因为没有 marker 后空格被拒绝：整个列表就是普通文本，不要产生空表。
        if frame.items.is_empty() {
            return;
        }
        // `TagEnd::List(bool)` 的 bool 是 "ordered"，**不是** tight；松紧必须自己从
        // 源码里的空行判断（CommonMark：item 之间或 item 内部有空白行即 loose）。
        let span = Span::new(frame.span_start.min(end), end.max(frame.span_start));
        let loose = list_is_loose(self.source, &self.lines, span);
        let mut width = 1usize;
        for &index in &frame.items {
            if let Some(item) = self.items.get(index) {
                width = width.max(decimal_digits(item.number));
            }
        }
        let list_index = self.list_blocks.len();
        for &index in &frame.items {
            if let Some(item) = self.items.get_mut(index) {
                item.list_index = list_index;
                item.loose = loose;
                item.number_width = width;
            }
        }
        self.list_blocks.push(ListBlock {
            span,
            ordered: frame.ordered,
            start: frame.start,
            tight: !loose,
            depth: frame.depth,
            items: frame.items,
            number_width: width,
        });
    }

    fn finish_code(&mut self, frame: CodeFrame, end: usize) {
        let block = Span::new(frame.span_start.min(end), end.max(frame.span_start));
        let Some(first) = self.line_index_at(block.start) else {
            return;
        };
        let Some(last) = self
            .line_index_at(block.end.saturating_sub(1))
            .or_else(|| self.line_index_at(block.start))
        else {
            return;
        };
        let full = Span::new(self.lines[first].start, self.lines[last].end);
        self.code_spans.push(full);
        if frame.fenced {
            self.fence_spans.push(full);
        } else {
            self.indented_spans.push(full);
        }
        // 只读高亮的块元数据：fenced 的围栏识别与 `Construct` 同源（内容不含围栏行），
        // indented 没有围栏行，内容就是整块。识别不出来的 fenced（未闭合/引用前缀）
        // 退化为“开围栏行之后到块尾”，宁可少高亮也不把围栏行当代码。
        let construct = if frame.fenced {
            self.fenced_construct(first, last)
        } else {
            None
        };
        let content = match (&construct, frame.fenced) {
            (Some(construct), true) => construct.content,
            (None, true) => Span::new(self.lines[first].end.min(full.end), full.end),
            (_, false) => full,
        };
        self.code_blocks.push(CodeBlockSpan {
            span: full,
            content,
            opening: construct
                .as_ref()
                .map_or(Span::empty(self.lines[first].start), |construct| {
                    construct.markers[0]
                }),
            info: frame.info.trim().to_owned(),
            first_line: first,
            last_line: last,
            fenced: frame.fenced,
        });
        let Some(construct) = construct else {
            return;
        };
        if is_mermaid_info(&frame.info) {
            // 普通 fenced code 的构造照旧先登记；`finish` 阶段按行语义决定是否
            // 把可投影成 Object 的 Mermaid 块从构造表里摘掉（引用/列表/缩进里的
            // Mermaid 仍是普通代码块，保留该构造）。
            self.mermaid_blocks.push(FencedMermaidBlock {
                span: construct.span,
                content: construct.content,
                opening: construct.markers[0],
                first_line: first,
                last_line: last,
                info: frame.info.trim().to_owned(),
                block: false,
            });
        }
        self.constructs.push(construct);
    }

    fn fenced_construct(&self, first: usize, last: usize) -> Option<Construct> {
        let bytes = self.source.as_bytes();
        let open = self.lines[first];
        let open_at = skip_spaces(bytes, open.start, open.content_end);
        let open_marker = bytes.get(open_at)?;
        if *open_marker != b'`' && *open_marker != b'~' {
            return None;
        }
        let open_run = count_run(bytes, open_at, open.content_end, *open_marker);
        if open_run < 3 {
            return None;
        }
        if open_marker == &b'`' && bytes[open_at + open_run..open.content_end].contains(&b'`') {
            return None;
        }
        // 闭围栏：从末行往前找“只由同种 marker 组成、长度 >= open_run”的行。
        let mut close = None;
        for index in (first + 1..=last).rev() {
            let line = self.lines[index];
            let at = skip_spaces(bytes, line.start, line.content_end);
            let run = count_run(bytes, at, line.content_end, *open_marker);
            if run < open_run {
                continue;
            }
            let rest = &bytes[at + run..line.content_end];
            if rest.iter().all(|byte| *byte == b' ' || *byte == b'\t') {
                close = Some((index, at));
                break;
            }
        }
        let (close, close_at) = close?;
        let close_line = self.lines[close];
        let span = Span::new(open.start, close_line.content_end);
        if span.is_empty() {
            return None;
        }
        Some(Construct {
            kind: ConstructKind::FencedCode,
            span,
            // marker 不包含行首缩进：列表/引用缩进由布局提供（hidden_prefix 永远隐藏），
            // 否则 caret 显现时会出现“布局缩进 + 源码缩进”双份。
            markers: vec![
                Span::new(open_at, open.content_end),
                Span::new(close_at, close_line.content_end),
            ],
            content: Span::new(open.end, close_line.start),
        })
    }

    /// 开始收集一行表格（表头或数据行）。
    ///
    /// `pulldown-cmark` 在 `TableHead` / `TableRow` 开始时给出整行区间（含行尾换行），
    /// 这里先记下区间，cell 由后续 `TableCell` 事件补齐。
    fn start_table_row(&mut self, range: Span, _header: bool) {
        let Some(frame) = self.table_stack.last_mut() else {
            return;
        };
        frame.current = Some(TableFrameRow {
            span: range,
            cells: Vec::new(),
        });
    }

    /// 结束一行表格：表头行存进 `header`，其余按源码顺序进 `rows`。
    fn finish_table_row(&mut self, header: bool) {
        let Some(frame) = self.table_stack.last_mut() else {
            return;
        };
        let Some(row) = frame.current.take() else {
            return;
        };
        if header {
            // GFM 表格恒有表头；重复的 TableHead 只保留第一个（防御：不覆盖已收集的行）。
            if frame.header.is_none() {
                frame.header = Some(row);
            }
        } else {
            frame.rows.push(row);
        }
    }

    /// 结束一个 cell：`Start(TableCell)` 的起点 + `End(TableCell)` 的终点。
    fn finish_table_cell(&mut self, end: usize) {
        let Some(start) = self.table_cell_start.take() else {
            return;
        };
        let Some(frame) = self.table_stack.last_mut() else {
            return;
        };
        let Some(row) = frame.current.as_mut() else {
            return;
        };
        row.cells.push(Span::new(start.min(end), end.max(start)));
    }

    /// 把事件收集到的表格帧变成 [`TableBlock`]（区间按源码行裁齐，cell 去掉首尾空白）。
    ///
    /// 返回 `None` 的情况（宁可不产生表格块，也不猜偏移）：
    /// - 没有表头行（GFM 不可能，防御异常事件流）；
    /// - 表头行之后没有分隔行（区间越界）；
    /// - 事件区间整体为空/倒序。
    fn build_table(&self, frame: TableFrame, end: usize) -> Option<TableBlock> {
        let header = frame.header?;
        let source_len = self.source.len();
        let start = frame.span_start.min(source_len);
        let end = end.min(source_len).max(start);
        if end <= start {
            return None;
        }
        let first_line = self.line_index_at(start)?;
        let last_line = self.line_index_at(end.saturating_sub(1))?;
        if last_line < first_line {
            return None;
        }
        // 整表区间不含最后一行之后的换行：Object 占位不能吞掉换行，否则会和下一段粘行。
        let last = self.lines.get(last_line)?;
        let span = Span::new(start, last.content_end.max(start));
        let columns = frame.alignments.len().max(1);
        let header = self.finish_table_source_row(header, columns)?;
        // 分隔行紧跟表头行（GFM 语法要求如此，pulldown 也为它不产生 TableRow 事件）。
        let delimiter_line = header.line.checked_add(1)?;
        if delimiter_line > last_line {
            return None;
        }
        let delimiter_info = self.lines.get(delimiter_line)?;
        let delimiter = Span::new(delimiter_info.start, delimiter_info.content_end);
        let mut alignments = frame.alignments;
        alignments.resize(columns, TableAlignment::None);
        let rows = frame
            .rows
            .into_iter()
            .filter_map(|row| self.finish_table_source_row(row, columns))
            .collect();
        Some(TableBlock {
            span,
            header,
            delimiter,
            rows,
            alignments,
            columns,
            first_line,
            last_line,
        })
    }

    /// 一行表格的原始区间 → [`TableRow`]：整行裁到源码行内容，cell 去首尾空白并补列。
    fn finish_table_source_row(&self, row: TableFrameRow, columns: usize) -> Option<TableRow> {
        let source_len = self.source.len();
        let start = row.span.start.min(source_len);
        let line = self.line_index_at(start)?;
        let info = self.lines.get(line)?;
        let content_end = info.content_end.max(info.start);
        let row_start = start.min(content_end);
        let mut cells = Vec::with_capacity(columns);
        for raw in row.cells.into_iter().take(columns) {
            let cell_start = raw.start.clamp(row_start, content_end);
            let cell_end = raw.end.clamp(cell_start, content_end);
            cells.push(trim_span(self.source, Span::new(cell_start, cell_end)));
        }
        // 列数不足（GFM 允许缺列）用空 cell 补齐，下游可以按列索引，不必判断长度。
        while cells.len() < columns {
            cells.push(Span::empty(content_end));
        }
        Some(TableRow {
            span: Span::new(row_start, content_end),
            line,
            cells,
        })
    }

    fn finish(self) -> ParsedMarkdown {
        let Builder {
            source,
            lines,
            mut list_blocks,
            mut items,
            mut images,
            mut links,
            constructs,
            headings,
            mut code_spans,
            fence_spans,
            indented_spans,
            mut unsupported_spans,
            mut tables,
            table_spans,
            mut mermaid_blocks,
            mut code_blocks,
            html_spans,
            rule_lines,
            uncertain_inline,
            ..
        } = self;

        // 列表 item 的 number_width / loose 以所属 list block 为准（列表结束时才知道 tight）。
        for block in &list_blocks {
            for &index in &block.items {
                if let Some(item) = items.get_mut(index) {
                    item.number_width = block.number_width;
                    item.loose = !block.tight;
                }
            }
        }

        let mut line_infos = build_line_infos(
            &lines,
            &fence_spans,
            &indented_spans,
            &table_spans,
            &html_spans,
            &headings,
            &rule_lines,
        );

        // 列表行归属：外层先写、内层覆盖；首行总是覆盖（子 item 的 marker 属于子 item）。
        let mut order: Vec<usize> = (0..items.len()).collect();
        order.sort_by_key(|&index| (items[index].first_line, items[index].depth));
        for index in order {
            let item = &items[index];
            if item.marker.is_empty() {
                continue;
            }
            let first = item.first_line;
            let last = item.last_line.max(first).min(lines.len().saturating_sub(1));
            for line_index in first..=last {
                let is_start = line_index == first;
                let occupied = line_infos[line_index]
                    .list
                    .as_ref()
                    .is_some_and(|existing| existing.depth >= item.depth);
                if occupied && !is_start {
                    continue;
                }
                let line = lines[line_index];
                let (hidden_prefix, marker) = if is_start {
                    (item.hidden_prefix, Some(item.marker))
                } else {
                    (
                        continuation_prefix(source, &line, item.content_indent),
                        None,
                    )
                };
                line_infos[line_index].list = Some(ListLineInfo {
                    item: index,
                    depth: item.depth,
                    is_item_start: is_start,
                    marker,
                    hidden_prefix,
                    content_indent: item.content_indent,
                });
            }
        }

        // fenced mermaid：`block` 依赖逐行语义（引用/列表归属在上一段才补齐）。
        // 只有可投影成 Object 的块才摘掉 `FencedCode` 构造：它不再是一个可编辑的
        // 代码块，而是原子的 Object（源码模式由对象行控制，不靠围栏构造显现）。
        for block in &mut mermaid_blocks {
            block.block = mermaid_block_is_object(&line_infos, block);
        }
        mermaid_blocks.sort_by_key(|block| (block.span.start, block.span.end));
        mermaid_blocks.dedup();
        let object_mermaid_spans: Vec<Span> = mermaid_blocks
            .iter()
            .filter(|block| block.block)
            .map(|block| block.span)
            .collect();

        // 由列表 item 生成 ListItem 构造（每项首行一段）。
        let mut all = constructs;
        all.retain(|construct| {
            !(construct.kind == ConstructKind::FencedCode
                && object_mermaid_spans.contains(&construct.span))
        });
        for item in &items {
            if item.marker.is_empty() {
                continue;
            }
            let Some(line) = line_infos.get(item.first_line) else {
                continue;
            };
            let span = Span::new(item.marker.start, line.content.end.max(item.marker.end));
            if span.is_empty() {
                continue;
            }
            all.push(Construct {
                kind: ConstructKind::ListItem,
                span,
                markers: vec![item.marker],
                content: Span::new(item.content_start.min(span.end), span.end),
            });
        }

        // 稳定排序 + 去重（嵌套引用会为同一行生成同样的引用构造）。
        all.sort_by_key(|construct| (construct.span.start, construct.span.end, construct.kind));
        all.dedup_by(|a, b| {
            a.kind == b.kind && a.span == b.span && a.markers == b.markers && a.content == b.content
        });
        code_spans.sort_by_key(|span| (span.start, span.end));
        code_spans.dedup();
        code_blocks.sort_by_key(|block| (block.span.start, block.span.end));
        code_blocks.dedup();
        unsupported_spans.sort_by_key(|span| (span.start, span.end));
        unsupported_spans.dedup();
        images.sort_by_key(|image| (image.span.start, image.span.end));
        images.dedup();
        links.sort_by_key(|link| (link.span.start, link.span.end));
        links.dedup();
        tables.sort_by_key(|table| (table.span.start, table.span.end));
        // wiki 链接：自定义 inline pass；排除代码块/行内代码/表格/转义（L1）。
        // 表格是原子块：单元格里的 `[[...]]` 不参与源码投影（否则会与 Object 段重叠）。
        let mut excluded = code_spans.clone();
        excluded.extend(tables.iter().map(|table| table.span));
        excluded.extend(
            all.iter()
                .filter(|construct| matches!(construct.kind, ConstructKind::InlineCode))
                .map(|construct| construct.span),
        );
        let (wiki_links, wiki_constructs) = scan_wiki_links(source, &excluded);
        all.extend(wiki_constructs);
        // 表格内部的行内构造不投影：整表要么是 Object 占位，要么原样显示源码。
        all.retain(|construct| {
            !tables.iter().any(|table| {
                table.span.start <= construct.span.start && construct.span.end <= table.span.end
            })
        });
        all.sort_by_key(|construct| (construct.span.start, construct.span.end, construct.kind));

        // list block 按文档顺序（嵌套 list 的 End 是内层先到），排序后要重映射 list_index。
        let mut order: Vec<usize> = (0..list_blocks.len()).collect();
        order.sort_by_key(|&index| (list_blocks[index].span.start, list_blocks[index].span.end));
        let mut new_position = vec![0usize; list_blocks.len()];
        for (position, &old) in order.iter().enumerate() {
            new_position[old] = position;
        }
        let sorted_blocks: Vec<ListBlock> =
            order.iter().map(|&old| list_blocks[old].clone()).collect();
        list_blocks = sorted_blocks;
        for item in &mut items {
            item.list_index = new_position
                .get(item.list_index)
                .copied()
                .unwrap_or_default();
        }
        for line in &mut line_infos {
            if line.list.as_ref().is_some_and(|list| list.is_item_start)
                && line_kind_rank(LineKind::ListItem) > line_kind_rank(line.kind)
            {
                line.kind = LineKind::ListItem;
            }
        }

        // 内嵌画布（ADR 0025 后续切片）：整行独占的 `![[目标.excalidraw]]`。
        // 依赖逐行语义（引用/列表/代码/表格归属在上面才算齐），所以放在最后扫；
        // 原子块来源单独传进去做防御（不依赖 LineKind 一处判定）。
        let mut drawing_excluded = code_spans.clone();
        drawing_excluded.extend(tables.iter().map(|table| table.span));
        drawing_excluded.extend(unsupported_spans.iter().copied());
        let mut drawings = scan_drawings(source, &line_infos, &drawing_excluded);
        drawings.sort_by_key(|drawing| (drawing.span.start, drawing.span.end));
        drawings.dedup();

        ParsedMarkdown {
            lines: line_infos,
            lists: list_blocks,
            items,
            images,
            drawings,
            tables,
            links,
            wiki_links,
            mermaid_blocks,
            code_blocks,
            constructs: all,
            code_spans,
            unsupported_spans,
            uncertain_inline,
        }
    }

    fn line_index_at(&self, byte: usize) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        if byte >= self.source.len() {
            return Some(self.lines.len() - 1);
        }
        let index = self
            .lines
            .partition_point(|line| line.start <= byte)
            .saturating_sub(1);
        Some(index)
    }
}

/// CommonMark 松紧：列表区间内部出现空白行即 loose（item 之间或 item 内子块之间）。
fn list_is_loose(source: &str, lines: &[Line], span: Span) -> bool {
    let Some(first) = line_index(lines, span.start) else {
        return false;
    };
    let Some(last) = line_index(lines, span.end.saturating_sub(1)) else {
        return false;
    };
    let bytes = source.as_bytes();
    for line in lines.iter().take(last + 1).skip(first) {
        // 首行是列表 marker，行尾的空行不可能在这里；只检查真正空白的内容行。
        if line.content_end == line.start {
            return true;
        }
        // 仅空白字符的行也算空行。
        if bytes[line.start..line.content_end]
            .iter()
            .all(|byte| *byte == b' ' || *byte == b'\t')
        {
            return true;
        }
    }
    false
}

/// 构造逐行语义（块类型 + 行区间）。
#[allow(clippy::too_many_arguments)]
fn build_line_infos(
    lines: &[Line],
    fence_spans: &[Span],
    indented_spans: &[Span],
    table_spans: &[Span],
    html_spans: &[Span],
    headings: &[(Span, u8)],
    rule_lines: &[usize],
) -> Vec<LineInfo> {
    let mut infos: Vec<LineInfo> = lines
        .iter()
        .map(|line| LineInfo {
            span: Span::new(line.start, line.end),
            content: Span::new(line.start, line.content_end),
            quote: Span::new(line.start, line.quote_end),
            kind: LineKind::Paragraph,
            list: None,
        })
        .collect();

    let mut consider = |line: usize, kind: LineKind| {
        let Some(info) = infos.get_mut(line) else {
            return;
        };
        if line_kind_rank(kind) > line_kind_rank(info.kind) {
            info.kind = kind;
        }
    };

    for span in fence_spans {
        mark_lines(lines, *span, |line| consider(line, LineKind::FencedCode));
    }
    for span in indented_spans {
        mark_lines(lines, *span, |line| consider(line, LineKind::IndentedCode));
    }
    for span in table_spans {
        mark_lines(lines, *span, |line| consider(line, LineKind::Table));
    }
    for span in html_spans {
        mark_lines(lines, *span, |line| consider(line, LineKind::Html));
    }
    for (span, level) in headings {
        if let Some(line) = line_index(lines, span.start) {
            consider(line, LineKind::Heading(*level));
        }
    }
    for &line in rule_lines {
        consider(line, LineKind::ThematicBreak);
    }
    for (index, line) in lines.iter().enumerate() {
        if line.quote_end > line.start {
            consider(index, LineKind::BlockQuote);
        }
    }
    infos
}

fn line_kind_rank(kind: LineKind) -> u8 {
    match kind {
        LineKind::Paragraph => 0,
        LineKind::BlockQuote => 1,
        LineKind::ThematicBreak => 2,
        LineKind::ListItem => 3,
        LineKind::Heading(_) => 4,
        LineKind::Table => 5,
        LineKind::Html => 5,
        LineKind::FencedCode => 6,
        LineKind::IndentedCode => 6,
    }
}

fn mark_lines(lines: &[Line], span: Span, mut f: impl FnMut(usize)) {
    if span.is_empty() {
        return;
    }
    for (index, line) in lines.iter().enumerate() {
        if line.start < span.end && line.end > span.start {
            f(index);
        }
    }
}

fn line_index(lines: &[Line], byte: usize) -> Option<usize> {
    if lines.is_empty() {
        return None;
    }
    Some(
        lines
            .partition_point(|line| line.start <= byte)
            .saturating_sub(1)
            .min(lines.len() - 1),
    )
}

/// 续行要隐藏的前导空白：引用前缀之后，最多隐藏 `content_indent` 字节。
fn continuation_prefix(source: &str, line: &Line, content_indent: usize) -> Span {
    let bytes = source.as_bytes();
    let base = line.quote_end.min(line.content_end);
    let mut end = base;
    while end < line.content_end && (bytes[end] == b' ' || bytes[end] == b'\t') {
        end += 1;
    }
    let limit = base.saturating_add(content_indent);
    Span::new(base, end.min(limit).max(base))
}

fn same_inline(a: InlineKind, b: InlineKind) -> bool {
    matches!(
        (a, b),
        (InlineKind::Bold, InlineKind::Bold)
            | (InlineKind::Emphasis, InlineKind::Emphasis)
            | (InlineKind::Strike, InlineKind::Strike)
            | (InlineKind::Link, InlineKind::Link)
            | (InlineKind::Image, InlineKind::Image)
    )
}

/// 把源码按 `\n` 切成行；空文档也有一行空行。
fn split_lines(source: &str) -> Vec<Line> {
    let bytes = source.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    while start <= source.len() {
        let newline = source[start..].find('\n').map(|offset| start + offset);
        let (content_end, end) = match newline {
            Some(at) => {
                let content_end = if at > start && bytes[at - 1] == b'\r' {
                    at - 1
                } else {
                    at
                };
                (content_end, at + 1)
            }
            None => (source.len(), source.len()),
        };
        let quote_end = scan_quote_prefix(source, start, content_end);
        lines.push(Line {
            start,
            content_end,
            end,
            quote_end,
        });
        if newline.is_none() {
            break;
        }
        start = end;
    }
    if lines.is_empty() {
        lines.push(Line {
            start: 0,
            content_end: 0,
            end: 0,
            quote_end: 0,
        });
    }
    lines
}

/// 引用前缀终点：每层允许 ≤3 空格 + `>` + 可选一个空格。没有 `>` 时返回行首。
fn scan_quote_prefix(source: &str, start: usize, end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut index = start;
    let mut prefix_end = start;
    let mut seen = false;
    loop {
        let mut probe = index;
        let mut spaces = 0usize;
        while probe < end && spaces < 3 && bytes[probe] == b' ' {
            probe += 1;
            spaces += 1;
        }
        if probe >= end || bytes[probe] != b'>' {
            break;
        }
        seen = true;
        probe += 1;
        if probe < end && bytes[probe] == b' ' {
            probe += 1;
        }
        prefix_end = probe;
        index = probe;
    }
    if seen {
        prefix_end
    } else {
        start
    }
}

fn scan_quote_markers(source: &str, start: usize, end: usize) -> Vec<Span> {
    let bytes = source.as_bytes();
    let mut markers = Vec::new();
    let mut index = start;
    while index < end {
        let mut spaces = 0usize;
        while index < end && spaces < 3 && bytes[index] == b' ' {
            index += 1;
            spaces += 1;
        }
        if index >= end || bytes[index] != b'>' {
            break;
        }
        let level_start = index;
        index += 1;
        if index < end && bytes[index] == b' ' {
            index += 1;
        }
        markers.push(Span::new(level_start, index));
    }
    markers
}

/// 列表 marker 扫描结果。
struct MarkerScan {
    span: Span,
    content_start: usize,
    delimiter: char,
}

fn scan_list_marker(source: &str, start: usize, end: usize) -> Option<MarkerScan> {
    let bytes = source.as_bytes();
    if start >= end {
        return None;
    }
    let first = bytes[start];
    let (delimiter, mut index) = if first.is_ascii_digit() {
        let mut digits_end = start;
        while digits_end < end && digits_end - start < 9 && bytes[digits_end].is_ascii_digit() {
            digits_end += 1;
        }
        if digits_end >= end {
            return None;
        }
        let delimiter = match bytes[digits_end] {
            b'.' => '.',
            b')' => ')',
            _ => return None,
        };
        (delimiter, digits_end + 1)
    } else if matches!(first, b'-' | b'*' | b'+') {
        (char::from(first), start + 1)
    } else {
        return None;
    };
    // marker 后必须是空格/Tab；`-`/`1.` 单独一行不算列表（输入空格后才渲染）。
    if index >= end || (bytes[index] != b' ' && bytes[index] != b'\t') {
        return None;
    }
    while index < end && (bytes[index] == b' ' || bytes[index] == b'\t') {
        index += 1;
    }
    Some(MarkerScan {
        span: Span::new(start, index),
        content_start: index,
        delimiter,
    })
}

/// info string 是否是 Mermaid 语言标记。
///
/// 只接受**去掉首尾空白后精确等于 `mermaid`**（小写）：Mermaid CLI / GitHub /
/// Obsidian 的 fenced mermaid 都是这个写法。`Mermaid`、`mermaid title="x"` 等
/// 变体不识别（保守：不猜用户意图，按普通代码块处理）。
fn is_mermaid_info(info: &str) -> bool {
    info.trim() == "mermaid"
}

/// fenced mermaid 是否可投影成 Object Block（普通段落上下文，见类型文档）。
fn mermaid_block_is_object(line_infos: &[LineInfo], block: &FencedMermaidBlock) -> bool {
    if block.last_line <= block.first_line {
        return false;
    }
    let (Some(first), Some(last)) = (
        line_infos.get(block.first_line),
        line_infos.get(block.last_line),
    ) else {
        return false;
    };
    // 开/闭围栏行都必须是围栏代码，且不在引用/列表项里。
    if first.kind != LineKind::FencedCode || last.kind != LineKind::FencedCode {
        return false;
    }
    if !first.quote.is_empty() || !last.quote.is_empty() {
        return false;
    }
    if first.list.is_some() || last.list.is_some() {
        return false;
    }
    // 行首缩进不投影（否则布局缩进与源码缩进会叠加）；块必须结束在闭围栏行内容终点。
    block.opening.start == first.content.start && block.span.end == last.content.end
}

/// 扫描整行独占的内嵌画布笔记 `![[目标.excalidraw]]`（ADR 0025 后续切片）。
///
/// # 判定（写死，测试钉住）
///
/// 1. **整行独占**：去掉行尾空白后，行内容必须以 `![[` 开始、以 `]]` 结束；
///    行首不允许缩进（与图片/表格/围栏一致：缩进留在源码里，该行按普通段落显示）；
/// 2. **普通段落上下文**：该行是 [`LineKind::Paragraph`]，不是引用、不在列表项里，
///    也不在代码块/表格/未支持块（HTML）区间内；
/// 3. **目标是画布笔记**：`|` 之前的 target 去首尾空白后非空，且**大小写不敏感**
///    地以 `.excalidraw` 结尾；内层不允许再次出现 `[[` / `]]`（避免嵌套误判）；
/// 4. **可选别名**：`|` 之后是别名（显示文本，去首尾空白）；空别名等价于没有别名。
///
/// 任一条不满足就**不产生记录**：这一行继续按普通文本显示，绝不吞掉半行内容。
/// `span` 只覆盖 trimmed 行内容（不含行尾空白/换行），因此 Object 占位不会吞掉
/// 行尾换行；target/alias 区间同样已 trim，端点都在 UTF-8 char 边界上。
fn scan_drawings(source: &str, line_infos: &[LineInfo], excluded: &[Span]) -> Vec<DrawingSpan> {
    /// 画布笔记标题后缀（大小写不敏感；`is_drawing_title` 的解析侧副本）。
    const DRAWING_SUFFIX: &[u8] = b".excalidraw";
    let bytes = source.as_bytes();
    let mut drawings = Vec::new();
    for (line, info) in line_infos.iter().enumerate() {
        // 普通段落：标题/引用/列表/围栏/缩进代码/表格/HTML/分隔线一律不算。
        if info.kind != LineKind::Paragraph || info.list.is_some() || !info.quote.is_empty() {
            continue;
        }
        let content_start = info.content.start.min(bytes.len());
        let content_end = info.content.end.min(bytes.len()).max(content_start);
        let mut start = content_start;
        let mut end = content_end;
        while start < end && matches!(bytes[start], b' ' | b'\t') {
            start += 1;
        }
        while end > start && matches!(bytes[end - 1], b' ' | b'\t') {
            end -= 1;
        }
        // 行首缩进不投影成 Object；空行/纯空白行直接跳过。
        if start != content_start || end <= start {
            continue;
        }
        // `![[` .. `]]`（最短 `![[]]` 也放进来；空 target 在下面被拒绝）。
        if end < start + 5 || &bytes[start..start + 3] != b"![[" || &bytes[end - 2..end] != b"]]" {
            continue;
        }
        // 原子块（代码 / 表格 / 未支持块）内部不投影（逐行语义已经是兜底，这里是
        // 防御：任一来源只要把它算成原子块，就不该再生成画布对象）。
        if excluded
            .iter()
            .any(|span| span.start <= start && end <= span.end)
        {
            continue;
        }
        let inner_start = start + 3;
        let inner_end = end - 2;
        if inner_start >= inner_end {
            continue;
        }
        // 内层不允许嵌套 `[[` / `]]`（与 wiki 链接的保守规则一致）。
        if bytes[inner_start..inner_end]
            .windows(2)
            .any(|window| window == b"[[" || window == b"]]")
        {
            continue;
        }
        let pipe = bytes[inner_start..inner_end]
            .iter()
            .position(|byte| *byte == b'|')
            .map(|offset| inner_start + offset);
        let target = trim_span(source, Span::new(inner_start, pipe.unwrap_or(inner_end)));
        if target.is_empty() {
            continue;
        }
        let Some(target_text) = source.get(target.start..target.end) else {
            continue;
        };
        if target_text.len() < DRAWING_SUFFIX.len()
            || !target_text.as_bytes()[target_text.len() - DRAWING_SUFFIX.len()..]
                .eq_ignore_ascii_case(DRAWING_SUFFIX)
        {
            continue;
        }
        let alias = pipe.and_then(|pipe| {
            let alias = trim_span(source, Span::new(pipe + 1, inner_end));
            (!alias.is_empty()).then_some(alias)
        });
        drawings.push(DrawingSpan {
            span: Span::new(start, end),
            target,
            alias,
            line,
            block: true,
        });
    }
    drawings
}

fn count_run(bytes: &[u8], start: usize, end: usize, marker: u8) -> usize {
    let mut run = 0usize;
    while start + run < end && bytes[start + run] == marker {
        run += 1;
    }
    run
}

fn skip_spaces(bytes: &[u8], start: usize, end: usize) -> usize {
    let mut index = start;
    while index < end && (bytes[index] == b' ' || bytes[index] == b'\t') {
        index += 1;
    }
    index
}

fn decimal_digits(value: u64) -> usize {
    if value == 0 {
        1
    } else {
        let mut digits = 0usize;
        let mut value = value;
        while value > 0 {
            digits += 1;
            value /= 10;
        }
        digits
    }
}

/// 从 `start`（指向 `[`）找匹配的 `]`，尊重转义与嵌套方括号。
fn find_matching_bracket(source: &str, start: usize, end: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut index = start;
    while index < end {
        match bytes[index] {
            b'\\' => {
                index = skip_escape(source, index, end);
                continue;
            }
            b'[' => depth += 1,
            b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {
                index = next_char_boundary(source, index, end);
                continue;
            }
        }
        index += 1;
    }
    None
}

fn skip_escape(source: &str, at: usize, end: usize) -> usize {
    let after_slash = at + 1;
    if after_slash >= end {
        return end;
    }
    let size = source[after_slash..]
        .chars()
        .next()
        .map_or(1, char::len_utf8);
    after_slash + size
}

fn next_char_boundary(source: &str, at: usize, end: usize) -> usize {
    let size = source[at..].chars().next().map_or(1, char::len_utf8);
    (at + size).min(end)
}

/// 扫描 `[[...]]` wiki 链接（L1）。
///
/// 只接受单行、非代码、非转义的写法；`![[...]]` 暂不支持（当普通文本）。
fn scan_wiki_links(source: &str, excluded: &[Span]) -> (Vec<WikiLink>, Vec<Construct>) {
    let bytes = source.as_bytes();
    let mut links = Vec::new();
    let mut wiki_constructs = Vec::new();
    let mut index = 0usize;
    while index + 1 < bytes.len() {
        if bytes[index] != b'[' || bytes[index + 1] != b'[' {
            index += 1;
            continue;
        }
        if is_escaped(bytes, index) || (index > 0 && bytes[index - 1] == b'!') {
            index += 2;
            continue;
        }
        if excluded
            .iter()
            .any(|span| span.start <= index && index < span.end)
        {
            index += 2;
            continue;
        }
        let Some(close) = find_wiki_close(bytes, index + 2) else {
            index += 1;
            continue;
        };
        let inner_start = index + 2;
        let inner_end = close;
        if inner_start >= inner_end {
            index = close + 2;
            continue;
        }
        // 内层不允许再出现 `[[`（避免嵌套误判）。
        if bytes[inner_start..inner_end]
            .windows(2)
            .any(|window| window == b"[[")
        {
            index = close + 2;
            continue;
        }
        let Some((target, heading, alias)) = split_wiki_inner(inner_start, inner_end, bytes) else {
            index = close + 2;
            continue;
        };
        let display = alias.unwrap_or(Span::new(inner_start, inner_end));
        let mut markers = vec![Span::new(index, inner_start)];
        if let Some(alias_span) = alias {
            markers.push(Span::new(inner_start, alias_span.start));
        }
        markers.push(Span::new(close, close + 2));
        let span = Span::new(index, close + 2);
        wiki_constructs.push(Construct {
            kind: ConstructKind::WikiLink,
            span,
            markers,
            content: display,
        });
        links.push(WikiLink {
            span,
            target,
            heading,
            alias,
            display,
        });
        index = close + 2;
    }
    links.sort_by_key(|link| (link.span.start, link.span.end));
    links.dedup();
    (links, wiki_constructs)
}

fn find_wiki_close(bytes: &[u8], mut index: usize) -> Option<usize> {
    while index + 1 < bytes.len() {
        match bytes[index] {
            b'\n' | b'\r' => return None,
            b']' if bytes[index + 1] == b']' => return Some(index),
            b'[' if bytes.get(index + 1) == Some(&b'[') => return None,
            _ => {}
        }
        index += 1;
    }
    None
}

fn split_wiki_inner(
    start: usize,
    end: usize,
    bytes: &[u8],
) -> Option<(Span, Option<Span>, Option<Span>)> {
    // 第一个未转义的 `|` 分别名；之前的部分再按第一个 `#` 分 heading。
    let mut pipe = None;
    let mut index = start;
    while index < end {
        if bytes[index] == b'\\' {
            index += 2;
            continue;
        }
        if bytes[index] == b'|' {
            pipe = Some(index);
            break;
        }
        index += 1;
    }
    let target_end = pipe.unwrap_or(end);
    let alias = pipe.map(|at| {
        let alias_start = at + 1;
        Span::new(alias_start, end)
    });
    if alias.is_some_and(|span| span.is_empty()) {
        return None;
    }
    let mut hash = None;
    let mut index = start;
    while index < target_end {
        if bytes[index] == b'\\' {
            index += 2;
            continue;
        }
        if bytes[index] == b'#' {
            hash = Some(index);
            break;
        }
        index += 1;
    }
    let target = Span::new(start, hash.unwrap_or(target_end));
    let heading = hash.map(|at| Span::new(at + 1, target_end));
    if target.is_empty() && heading.as_ref().is_none_or(|span| span.is_empty()) {
        return None;
    }
    Some((target, heading, alias))
}

/// `pulldown-cmark` 的对齐枚举 → 本 crate 的 [`TableAlignment`]。
fn convert_alignment(alignment: Alignment) -> TableAlignment {
    match alignment {
        Alignment::None => TableAlignment::None,
        Alignment::Left => TableAlignment::Left,
        Alignment::Center => TableAlignment::Center,
        Alignment::Right => TableAlignment::Right,
    }
}

/// 去掉区间首尾的 ASCII 空白（GFM 规定 cell 内容去首尾空白）。
///
/// 只裁 ASCII 空白字节，因此端点仍落在 UTF-8 char 边界上。
fn trim_span(source: &str, span: Span) -> Span {
    let bytes = source.as_bytes();
    let mut start = span.start.min(bytes.len());
    let mut end = span.end.min(bytes.len()).max(start);
    while start < end && matches!(bytes[start], b' ' | b'\t' | b'\r' | b'\n') {
        start += 1;
    }
    while end > start && matches!(bytes[end - 1], b' ' | b'\t' | b'\r' | b'\n') {
        end -= 1;
    }
    Span::new(start, end)
}

fn is_escaped(bytes: &[u8], at: usize) -> bool {
    let mut count = 0usize;
    let mut index = at;
    while index > 0 && bytes[index - 1] == b'\\' {
        count += 1;
        index -= 1;
    }
    count % 2 == 1
}

// ---------------------------------------------------------------------------
// 单元测试（ADR 0025 后续切片：`![[目标.excalidraw]]` 解析）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod drawing_tests {
    use super::{parse, DrawingSpan, Span};

    fn slice(source: &str, span: Span) -> &str {
        &source[span.start..span.end]
    }

    fn drawings(source: &str) -> Vec<DrawingSpan> {
        parse(source).drawings
    }

    #[test]
    fn whole_line_drawing_records_span_target_line_and_block() {
        let source = "前言\n\n![[流程图.excalidraw]]\n\n后文\n";
        let found = drawings(source);
        assert_eq!(found.len(), 1, "整行独占的画布嵌入必须解析：{source:?}");
        let drawing = &found[0];
        assert!(drawing.is_block());
        assert_eq!(drawing.line, 2);
        assert_eq!(slice(source, drawing.span), "![[流程图.excalidraw]]");
        assert_eq!(slice(source, drawing.target), "流程图.excalidraw");
        assert!(drawing.alias.is_none());
        assert!(source.is_char_boundary(drawing.span.start));
        assert!(source.is_char_boundary(drawing.span.end));
    }

    #[test]
    fn suffix_matches_case_insensitively_and_multiple_lines_parse() {
        for target in ["a.excalidraw", "a.EXCALIDRAW", "a.Excalidraw"] {
            let source = format!("![[{target}]]\n");
            let found = drawings(&source);
            assert_eq!(found.len(), 1, "后缀必须大小写不敏感：{source:?}");
            assert_eq!(slice(&source, found[0].target), target);
        }
        let source = "![[a.excalidraw]]\n![[b.excalidraw]]\n";
        let found = drawings(source);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].line, 0);
        assert_eq!(found[1].line, 1);
        assert_eq!(slice(source, found[1].target), "b.excalidraw");
    }

    #[test]
    fn alias_is_recorded_and_target_is_before_pipe() {
        let source = "![[流程图.excalidraw|示意图]]\n";
        let found = drawings(source);
        assert_eq!(found.len(), 1);
        let drawing = &found[0];
        assert_eq!(slice(source, drawing.target), "流程图.excalidraw");
        let alias = drawing.alias.expect("alias 必须记录");
        assert_eq!(slice(source, alias), "示意图");
        assert_eq!(slice(source, drawing.span), "![[流程图.excalidraw|示意图]]");

        // 空白别名等价于没有别名；target 两侧空白被裁掉。
        let source = "![[ a.excalidraw |  ]]\n";
        let found = drawings(source);
        assert_eq!(found.len(), 1);
        assert_eq!(slice(source, found[0].target), "a.excalidraw");
        assert!(found[0].alias.is_none(), "空白别名必须视为没有别名");
    }

    #[test]
    fn mixed_line_and_non_drawing_targets_stay_plain_text() {
        let cases = [
            "前 ![[a.excalidraw]] 后\n",
            "![[a.png]]\n",
            "![[a.excalidraw.md]]\n",
            "![[a.excalidraw\n",
            "a.excalidraw]]\n",
            "![[a.excalidraw]] 后\n",
            "![[a.excalidraw]]]]\n",
            "![[]]\n",
            "![[|alias]]\n",
            "![](asset://aa)\n",
        ];
        for source in cases {
            assert!(
                drawings(source).is_empty(),
                "不应解析为画布对象：{source:?}"
            );
        }
    }

    #[test]
    fn leading_indent_and_trailing_whitespace() {
        // 行首缩进不投影（与图片/表格/围栏一致）。
        assert!(drawings(" ![[a.excalidraw]]\n").is_empty());
        assert!(drawings("  ![[a.excalidraw]]\n").is_empty());
        assert!(drawings("\t![[a.excalidraw]]\n").is_empty());
        // 行尾空白允许，但 span 不含行尾空白（不能吞掉换行/空格）。
        let source = "![[a.excalidraw]]   \n";
        let found = drawings(source);
        assert_eq!(found.len(), 1);
        assert_eq!(slice(source, found[0].span), "![[a.excalidraw]]");
    }

    #[test]
    fn drawing_lines_inside_code_quote_list_table_and_html_are_not_parsed() {
        let cases = [
            "```\n![[a.excalidraw]]\n```\n",
            "    ![[a.excalidraw]]\n",
            "> ![[a.excalidraw]]\n",
            "- ![[a.excalidraw]]\n",
            "1. ![[a.excalidraw]]\n",
            "| ![[a.excalidraw]] |\n| --- |\n| x |\n",
            "<div>\n![[a.excalidraw]]\n</div>\n",
            "# ![[a.excalidraw]]\n",
        ];
        for source in cases {
            assert!(
                drawings(source).is_empty(),
                "非普通段落上下文不得解析：{source:?}"
            );
        }
    }

    #[test]
    fn crlf_line_endings_are_supported() {
        let source = "前言\r\n\r\n![[a.excalidraw]]\r\n";
        let found = drawings(source);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, 2);
        assert_eq!(slice(source, found[0].span), "![[a.excalidraw]]");
    }

    #[test]
    fn drawing_lines_do_not_become_images_or_wiki_links() {
        // `![[...]]` 既不是普通图片（没有目标 URL），也不是 wiki 链接。
        let source = "![[a.excalidraw]]\n";
        let parsed = parse(source);
        assert_eq!(parsed.drawings.len(), 1);
        assert!(parsed.images.is_empty(), "画布嵌入不能当成图片：{parsed:?}");
        assert!(
            parsed.wiki_links.is_empty(),
            "画布嵌入不能当成 wiki 链接：{parsed:?}"
        );
        assert!(
            !parsed
                .constructs
                .iter()
                .any(|construct| matches!(construct.kind, super::ConstructKind::Image)),
            "画布嵌入不能产生 Image 构造：{parsed:?}"
        );
    }
}

#[cfg(test)]
mod code_block_tests {
    use super::{parse, Span};

    fn slice(source: &str, span: Span) -> &str {
        &source[span.start..span.end]
    }

    #[test]
    fn fenced_rust_records_info_content_and_lines() {
        let source = "```rust\nfn main() {}\n```\n";
        let parsed = parse(source);
        assert_eq!(parsed.code_blocks.len(), 1, "一个 fenced 块：{parsed:?}");
        let block = &parsed.code_blocks[0];
        assert!(block.fenced);
        assert_eq!(block.info, "rust");
        assert_eq!(slice(source, block.content), "fn main() {}\n");
        assert_eq!(slice(source, block.opening), "```rust");
        assert_eq!(slice(source, block.span), source);
        assert_eq!(block.first_line, 0);
        assert_eq!(block.last_line, 2);
        // `code_spans` 的既有语义不变：与块元数据同源同区间。
        assert_eq!(parsed.code_spans, vec![block.span]);
        assert!(source.is_char_boundary(block.content.start));
        assert!(source.is_char_boundary(block.content.end));
    }

    #[test]
    fn fence_info_keeps_raw_words_and_attributes() {
        let source = "```rust,no_run\nlet x = 1;\n```\n";
        let block = &parse(source).code_blocks[0];
        assert_eq!(
            block.info, "rust,no_run",
            "info 原样保留，切片语言由 app 负责"
        );
        let source = "~~~{rust}\nlet x = 1;\n~~~\n";
        let block = &parse(source).code_blocks[0];
        assert_eq!(block.info, "{rust}");
        assert_eq!(slice(source, block.opening), "~~~{rust}");
    }

    #[test]
    fn fenced_without_info_has_empty_info() {
        let source = "```\ncode\n```\n";
        let block = &parse(source).code_blocks[0];
        assert_eq!(block.info, "");
        assert_eq!(slice(source, block.content), "code\n");
    }

    #[test]
    fn empty_fenced_block_has_empty_content() {
        let source = "```rust\n```\n";
        let block = &parse(source).code_blocks[0];
        assert_eq!(block.info, "rust");
        assert_eq!(slice(source, block.content), "");
        assert_eq!(block.first_line, 0);
        assert_eq!(block.last_line, 1);
    }

    #[test]
    fn indented_block_is_recorded_without_info() {
        let source = "前言\n\n    let x = 1;\n";
        let parsed = parse(source);
        assert_eq!(parsed.code_blocks.len(), 1, "缩进代码块：{parsed:?}");
        let block = &parsed.code_blocks[0];
        assert!(!block.fenced);
        assert_eq!(block.info, "");
        assert_eq!(slice(source, block.content), "    let x = 1;\n");
        assert_eq!(block.span, block.content);
        assert!(block.opening.is_empty());
        assert_eq!(block.first_line, 2);
        assert_eq!(block.last_line, 2);
    }

    #[test]
    fn mermaid_blocks_are_recorded_but_keep_their_object_metadata() {
        let source = "```mermaid\ngraph TD;\n```\n";
        let parsed = parse(source);
        assert_eq!(parsed.code_blocks.len(), 1);
        assert_eq!(parsed.code_blocks[0].info, "mermaid");
        assert_eq!(slice(source, parsed.code_blocks[0].content), "graph TD;\n");
        assert_eq!(parsed.mermaid_blocks.len(), 1, "既有 mermaid 元数据不变");
        assert!(
            parsed.mermaid_blocks[0].block,
            "整块 mermaid 仍投影成 Object"
        );
    }

    #[test]
    fn blocks_are_sorted_by_span_and_match_code_spans() {
        let source = "```rust\na\n```\n\n文本\n\n```python\nb\n```\n";
        let parsed = parse(source);
        assert_eq!(parsed.code_blocks.len(), 2);
        assert_eq!(parsed.code_blocks[0].info, "rust");
        assert_eq!(parsed.code_blocks[1].info, "python");
        assert!(
            parsed.code_blocks[0].span.start < parsed.code_blocks[1].span.start,
            "按 span 起点排序"
        );
        let mut expected: Vec<Span> = parsed.code_blocks.iter().map(|block| block.span).collect();
        expected.sort_by_key(|span| (span.start, span.end));
        assert_eq!(expected, parsed.code_spans, "code_spans 与块元数据一一对应");

        // 嵌套在列表里的围栏也是代码块（缩进不改变块识别）。
        let source = "- 列表\n\n  ```js\n  let a = 1;\n  ```\n";
        let parsed = parse(source);
        assert_eq!(parsed.code_blocks.len(), 1, "列表内围栏：{parsed:?}");
        assert_eq!(parsed.code_blocks[0].info, "js");
    }

    #[test]
    fn code_blocks_inside_quote_are_recorded_best_effort() {
        // 引用前缀里的围栏沿用 `fenced_construct` 的既有边界（不跳过 `>`）：
        // 没有 construct 时内容退化为开围栏行之后，不 panic、不把围栏行当内容。
        let source = "> ```rust\n> let x = 1;\n> ```\n";
        let parsed = parse(source);
        assert_eq!(parsed.code_blocks.len(), 1, "引用内代码块：{parsed:?}");
        let block = &parsed.code_blocks[0];
        assert_eq!(block.info, "rust");
        assert_eq!(block.span, parsed.code_spans[0]);
        assert!(block.opening.is_empty(), "无法识别开围栏时为空区间");
        // 引用前缀让围栏识别失败，内容从开围栏**行尾**开始；闭围栏行可能落在里面
        // （宁可少高亮，不能把开围栏行当代码）。
        assert!(
            !slice(source, block.content).contains("```rust"),
            "开围栏行不能算代码内容"
        );
    }
}
