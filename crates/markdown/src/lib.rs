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
//! - [`LineInfo`]：逐源码行语义（引用前缀、列表归属、是否在围栏/表格里），
//!   供 app 做 gutter / hanging indent / 命中几何；
//! - [`ParsedMarkdown`]：上述内容的集合，外加 `code_spans` / `unsupported_spans`
//!   （editor 用来抑制“未支持构造”内部的旧 scanner 构造）。
//!
//! # 明确不做
//!
//! 图片 Object、任务框点击、表格单元格交互、脚注/数学/alerts、HTML 执行、
//! 任何源码归一化或重新序列化。这些是 ADR 0019 S2/S3 的范围。
#![forbid(unsafe_code)]

use std::fmt;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

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
    /// 任务框状态（S1 只记录，不交互）。
    pub task: Option<bool>,
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

/// 解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMarkdown {
    /// 逐源码行语义（至少一行；空文档一行空行）。
    pub lines: Vec<LineInfo>,
    /// 列表块（按出现顺序）。
    pub lists: Vec<ListBlock>,
    /// 列表项（按出现顺序）。
    pub items: Vec<ListItem>,
    /// 图片构造（含 inline / 引用式；按 `span` 起点排序）。
    ///
    /// 每个图片同时也出现在 [`Self::constructs`] 里（`ConstructKind::Image`）；
    /// 这里额外给出目标 URL，投影层据此判断能否渲染成 Object Block。
    pub images: Vec<ImageSpan>,
    /// 可投影构造（块级 + 行内；按 span/kind 稳定排序）。
    pub constructs: Vec<Construct>,
    /// 代码块区间（围栏/缩进；editor 用它抑制旧 scanner 在代码内部的构造）。
    pub code_spans: Vec<Span>,
    /// 未支持块区间（表格 / HTML 块；editor 原样显示、不投影内部构造）。
    pub unsupported_spans: Vec<Span>,
    /// 行内构造里出现过“定位不确定”的情况（editor 应整篇退回旧 scanner 的行内投影）。
    pub uncertain_inline: bool,
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
    quote_stack: Vec<Span>,
    table_stack: Vec<Span>,
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
            images: Vec::new(),
            quote_stack: Vec::new(),
            table_stack: Vec::new(),
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
                if let Some(frame) = self.items_stack.last() {
                    if let Some(item) = frame.item.and_then(|index| self.items.get_mut(index)) {
                        item.task = Some(checked);
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
            Tag::Table(_) => self.table_stack.push(range),
            Tag::HtmlBlock => self.html_stack.push(range),
            Tag::Superscript | Tag::Subscript => {}
            Tag::Strong => self.push_inline(InlineKind::Bold, range.start),
            Tag::Emphasis => self.push_inline(InlineKind::Emphasis, range.start),
            Tag::Strikethrough => self.push_inline(InlineKind::Strike, range.start),
            Tag::Link { .. } => self.push_inline(InlineKind::Link, range.start),
            Tag::Image { dest_url, .. } => {
                // `dest_url` 是 pulldown-cmark 解析后的语义目标（引用式图片也在这里
                // 变成真实 URL）。**只读取、不重写正文**：源码逐字节保留。
                self.image_stack.push((range.start, dest_url.to_string()));
                self.push_inline(InlineKind::Image, range.start);
            }
            Tag::Paragraph
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
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
                if let Some(start) = self.table_stack.pop() {
                    let span = Span::new(start.start, range.end.max(start.end));
                    self.table_spans.push(span);
                    self.unsupported_spans.push(span);
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
            TagEnd::Paragraph
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
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
        });
        self.items_stack.push(ItemFrame {
            item: Some(index),
            span_start: range.start,
            marker_end: marker.content_start,
        });
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
        if !frame.fenced {
            return;
        }
        let Some(construct) = self.fenced_construct(first, last, &frame.info) else {
            return;
        };
        self.constructs.push(construct);
    }

    fn fenced_construct(&self, first: usize, last: usize, _info: &str) -> Option<Construct> {
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

    fn finish(self) -> ParsedMarkdown {
        let Builder {
            source,
            lines,
            mut list_blocks,
            mut items,
            mut images,
            constructs,
            headings,
            mut code_spans,
            fence_spans,
            indented_spans,
            mut unsupported_spans,
            table_spans,
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

        // 由列表 item 生成 ListItem 构造（每项首行一段）。
        let mut all = constructs;
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
        unsupported_spans.sort_by_key(|span| (span.start, span.end));
        unsupported_spans.dedup();
        images.sort_by_key(|image| (image.span.start, image.span.end));
        images.dedup();

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

        ParsedMarkdown {
            lines: line_infos,
            lists: list_blocks,
            items,
            images,
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
