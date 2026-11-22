//! ADR 0019 S3b7：GFM 表格**单元格行内 Markdown**（纯解析，不产生新文本）。
//!
//! GFM 规定表格 cell 的源码就是普通行内 Markdown（`**粗体**`、`` `代码` ``、
//! `[文本](url)`……），只是**不能**跨行、不能有块级元素。本模块把一段 cell 源码
//! （[`crate::TableRow::cells`] 给出的源码切片，`\|` 转义仍在）解析成：
//!
//! - [`TableCellInline::display`]：**渲染用文本**（标记隐藏、`\|` 等转义还原、
//!   实体解码）。它**不是**源码切片，因此不能反查源码偏移；
//! - [`TableCellInline::spans`]：display 字节区间上的语义样式段（粗体/斜体/删除线/
//!   行内代码/链接），有序、不重叠、**完整覆盖** `[0, display.len())`；
//! - [`TableCellInline::links`]：链接的 display 可见区间 + 目标 URL（点击打开用）。
//!
//! # 为什么不复用 editor 的 `StyleKind`
//!
//! 依赖方向是 `editor → markdown`（ADR 0003），本 crate 不能反向依赖 editor。
//! 因此这里定义**自己的**语义枚举 [`TableCellStyle`]（与 [`crate::TableAlignment`]
//! 同一套做法），app 在边界处映射成 `StyleKind`。
//!
//! # 支持 / 不支持（写死，测试钉住）
//!
//! 支持：`**粗体**`、`*斜体*`、`~~删除线~~`、`` `行内代码` ``、`[文本](url)` /
//! 自动链接（`<https://…>`）。行内代码里的 `|` 与链接里的 `|` 都是**普通字符**
//! （GFM 的列边界在 cell 源码层已由 `\|` 保护，本模块只看到 cell 切片）。
//!
//! 不支持（整格退回**源码原文**，即 [`TableCellInline::is_plain`] 为 `true`）：
//!
//! - 块级元素（标题/引用/列表/围栏代码/分隔线/HTML 块/嵌套表格）——cell 里写
//!   `# 标题` 就显示 `# 标题`，不会被当成标题；
//! - 图片（`![alt](url)`）——按源码原样显示，不解码、不投影成对象；
//! - 换行（GFM 的 cell 不可能跨行；防御性地把 `\n`/`\r` 变成空格，绝不产生换行）。
//!
//! # 嵌套与优先级
//!
//! 与 editor 的 `model::style::runs` **同一套规则**：同一 display 位置被多个构造
//! 覆盖时取**最内层**（display 区间最短），长度相同再按
//! `InlineCode > Bold > Emphasis > Strike > Link` 择优。这样同一个 `**a *b* c**`
//! 在正文与表格 cell 里着色完全一致。

use std::ops::Range;

use pulldown_cmark::{Event, Tag, TagEnd};

use crate::Span;

/// 单元格行内语义种类（**只表示语义**，颜色/字重/字号由 app 决定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TableCellStyle {
    /// 默认段：没有任何行内 Markdown 语义（为覆盖完整性而存在）。
    Plain,
    /// `**bold**`。
    Bold,
    /// `*em*` / `_em_`。
    Emphasis,
    /// `` `code` ``（行内代码）。
    InlineCode,
    /// `~~strike~~`。
    Strike,
    /// `[text](url)` 的**可见文本**（不含方括号/目标）。
    Link,
}

impl TableCellStyle {
    /// 是否为默认（无语义）段。
    #[must_use]
    pub const fn is_plain(self) -> bool {
        matches!(self, Self::Plain)
    }

    /// 叠加优先级（数值越小越优先；仅用于同长度区间择优，与 editor 的
    /// `StyleKind::priority` 保持一致）。
    const fn priority(self) -> u8 {
        match self {
            Self::InlineCode => 0,
            Self::Bold => 1,
            Self::Emphasis => 2,
            Self::Strike => 3,
            Self::Link => 4,
            Self::Plain => 5,
        }
    }
}

/// display 坐标上的一个语义样式区间（半开 `[start, end)`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableCellSpan {
    /// display 字节区间（相对 [`TableCellInline::display`] 起点）。
    pub display: Span,
    /// 语义种类（可能是 [`TableCellStyle::Plain`]）。
    pub kind: TableCellStyle,
}

impl TableCellSpan {
    /// 构造一个样式段。
    #[must_use]
    pub const fn new(display: Span, kind: TableCellStyle) -> Self {
        Self { display, kind }
    }

    /// 段长（字节）。
    #[must_use]
    pub const fn len(&self) -> usize {
        self.display.len()
    }

    /// 是否是空段（正常不会出现；空段不参与绘制）。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.display.is_empty()
    }
}

/// 单元格里的一条链接（display 可见区间 + 目标 URL）。
///
/// `display` 是**可见文本**区间（`[文本](url)` 的 `文本`），点击命中用它；
/// `dest` 是解析后的目标（自动链接/引用式链接都已展开），原样字符串，不重新序列化。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableCellLink {
    /// display 可见文本区间（相对 [`TableCellInline::display`] 起点）。
    pub display: Span,
    /// 目标 URL（`http`/`https`/`mailto` 之外的 scheme 由宿主拒绝）。
    pub dest: String,
}

impl TableCellLink {
    /// 构造一条链接。
    #[must_use]
    pub fn new(display: Span, dest: impl Into<String>) -> Self {
        Self {
            display,
            dest: dest.into(),
        }
    }
}

/// 一段 cell 源码的行内 Markdown 解析结果。
///
/// 所有 display 偏移都是 [`Self::display`] 的 UTF-8 字节下标；源码偏移**不**保留
/// （标记被隐藏后与源码不再线性对应）。空 cell 的 `display` 为空、`spans`/`links`
/// 为空 `Vec`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableCellInline {
    display: String,
    spans: Vec<TableCellSpan>,
    links: Vec<TableCellLink>,
    plain: bool,
}

impl TableCellInline {
    /// 渲染用文本（标记隐藏、转义还原、实体解码；**不含**换行）。
    #[must_use]
    pub fn display(&self) -> &str {
        &self.display
    }

    /// display 字节区间上的样式段（有序、不重叠、完整覆盖 `[0, display.len())`）。
    #[must_use]
    pub fn spans(&self) -> &[TableCellSpan] {
        &self.spans
    }

    /// 链接（按 display 起点排序；可见文本为空的不收录）。
    #[must_use]
    pub fn links(&self) -> &[TableCellLink] {
        &self.links
    }

    /// 是否是**源码原样显示**（cell 里有不支持的构造，或根本没有行内语法）。
    ///
    /// `true` 时 `display` 是源码原文（只还原 `\|` 转义），`spans` 至多一个
    /// [`TableCellStyle::Plain`] 段。
    #[must_use]
    pub const fn is_plain(&self) -> bool {
        self.plain
    }
}

/// 把一段 cell 源码解析成行内 Markdown（display 文本 + 样式段 + 链接）。
///
/// 输入是**cell 源码切片**（[`crate::TableRow::cells`] 的区间，`\|` 转义仍在，
/// 已去掉首尾空白）；输出**不产生任何新语义**：display 全部来自解析器给出的事件文本，
/// 标记（`**`、反引号、`[`、`](url)`）不进入 display。
#[must_use]
pub fn parse_table_cell_inline(cell_source: &str) -> TableCellInline {
    // GFM 的表格规则：cell 里的 `\|` 是**列边界保护**，在行内解析**之前**就要还原成
    // 字面 `|`（包括行内代码内部 —— `` `a\|b` `` 渲染成 `a|b`，不是 `a\|b`）。
    // 因此这里先做一次 `\|` → `|` 的预处理，再交给行内解析器；行内代码不处理反斜杠
    // 转义，不预处理的话代码里的 `\|` 会原样显示成反斜杠。
    let unescaped = unescape_cell_pipes(cell_source);
    let mut builder = CellInlineBuilder::default();
    for (event, range) in super::Parser::new_ext(&unescaped, super::options()).into_offset_iter() {
        if !builder.handle(event, range, &unescaped) {
            return TableCellInline::plain(cell_source);
        }
    }
    builder.finish()
}

/// 把 cell 源码里**未转义**的 `|` 前面的反斜杠去掉（GFM 的 `\|` → `|`）。
///
/// 判定规则与 CommonMark 一致：前面紧邻的连续反斜杠数为**奇数**时该 `|` 是转义的，
/// 去掉一个反斜杠；偶数（含 0）时 `|` 是列边界（不会出现在 cell 源码里），保持原样。
fn unescape_cell_pipes(cell_source: &str) -> String {
    if !cell_source.contains('|') {
        return cell_source.to_owned();
    }
    let mut unescaped = String::with_capacity(cell_source.len());
    let mut backslashes = 0usize;
    for ch in cell_source.chars() {
        if ch == '|' {
            if backslashes % 2 == 1 {
                // 去掉紧邻的一个反斜杠：`\` + `|` → `|`。
                unescaped.pop();
            }
            unescaped.push('|');
            backslashes = 0;
        } else {
            if ch == '\\' {
                backslashes += 1;
            } else {
                backslashes = 0;
            }
            unescaped.push(ch);
        }
    }
    unescaped
}

/// 源码原样显示（不支持的构造 / 解析器拒绝）：只还原 `\|` 转义，换行变空格。
fn plain_display(cell_source: &str) -> String {
    // GFM 的 cell 不可能跨行；防御性地收敛成空格，绝不让换行进 display
    // （`shape_line` 对含换行的文本会断言）。
    unescape_cell_pipes(cell_source)
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect()
}

impl TableCellInline {
    /// 整格退回源码原文（块级元素/图片等不支持的构造）。
    fn plain(cell_source: &str) -> Self {
        let display = plain_display(cell_source);
        let spans = if display.is_empty() {
            Vec::new()
        } else {
            vec![TableCellSpan::new(
                Span::new(0, display.len()),
                TableCellStyle::Plain,
            )]
        };
        Self {
            display,
            spans,
            links: Vec::new(),
            plain: true,
        }
    }
}

/// 一次解析的中间状态（只在本模块内使用）。
#[derive(Default)]
struct CellInlineBuilder {
    /// 已产出的渲染文本。
    display: String,
    /// 带样式的构造候选：`(display_start, display_end, kind)`。
    ///
    /// 只记录**内容区间**（标记不进入候选），与 editor 的 `style::runs` 同源。
    candidates: Vec<(usize, usize, TableCellStyle)>,
    /// 正在收集的链接：`(display_start, dest)`。
    link_stack: Vec<(usize, String)>,
    /// 已闭合的链接（按闭合顺序；display 起点天然有序）。
    links: Vec<TableCellLink>,
    /// 正在跳过的图片内部事件深度（图片按源码原样显示）。
    skip_image_depth: usize,
}

impl CellInlineBuilder {
    /// 处理一个解析器事件；返回 `false` 表示遇到**不支持**的构造（整格退回源码）。
    fn handle(&mut self, event: Event<'_>, range: Range<usize>, cell_source: &str) -> bool {
        match event {
            Event::Start(tag) => self.start(tag, range, cell_source),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if self.skip_image_depth == 0 {
                    self.push_text(&text);
                }
                true
            }
            Event::Code(code) => {
                if self.skip_image_depth > 0 {
                    return true;
                }
                let start = self.display.len();
                self.display.push_str(&code);
                let end = self.display.len();
                if end > start {
                    self.candidates
                        .push((start, end, TableCellStyle::InlineCode));
                }
                true
            }
            // cell 不能跨行（GFM）；真出现时按一个空格处理，绝不产生换行。
            Event::SoftBreak | Event::HardBreak => {
                if self.skip_image_depth == 0 {
                    self.push_text(" ");
                }
                true
            }
            // 行内 HTML：按源码原样显示（不执行、不解析）。
            Event::InlineHtml(_) => {
                if self.skip_image_depth == 0 {
                    self.push_raw(range, cell_source);
                }
                true
            }
            // 脚注引用：按源码原样显示（脚注本身不是本切片支持的构造）。
            Event::FootnoteReference(_) => {
                if self.skip_image_depth == 0 {
                    self.push_raw(range, cell_source);
                }
                true
            }
            // 块级 HTML / 分隔线 / 任务框标记 / 数学：cell 里不支持，整格退回源码。
            Event::Html(_)
            | Event::Rule
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => false,
        }
    }

    fn start(&mut self, tag: Tag<'_>, range: Range<usize>, cell_source: &str) -> bool {
        match tag {
            // 普通段落：cell 的正常上下文。
            Tag::Paragraph => true,
            Tag::Strong => {
                self.open(TableCellStyle::Bold);
                true
            }
            Tag::Emphasis => {
                self.open(TableCellStyle::Emphasis);
                true
            }
            Tag::Strikethrough => {
                self.open(TableCellStyle::Strike);
                true
            }
            Tag::Link { dest_url, .. } => {
                self.open(TableCellStyle::Link);
                self.link_stack
                    .push((self.display.len(), dest_url.to_string()));
                true
            }
            // 图片不支持：整段 `![alt](dest)` 按源码原样显示，内部事件全部跳过。
            Tag::Image { .. } => {
                self.push_raw(range, cell_source);
                self.skip_image_depth += 1;
                true
            }
            // 其余（标题/引用/列表/围栏代码/表格/HTML 块/脚注定义……）都是块级构造：
            // cell 里不支持，整格退回源码原文。
            _ => false,
        }
    }

    fn end(&mut self, tag: TagEnd) -> bool {
        match tag {
            TagEnd::Paragraph => true,
            TagEnd::Strong => {
                self.close(TableCellStyle::Bold);
                true
            }
            TagEnd::Emphasis => {
                self.close(TableCellStyle::Emphasis);
                true
            }
            TagEnd::Strikethrough => {
                self.close(TableCellStyle::Strike);
                true
            }
            TagEnd::Link => {
                self.close(TableCellStyle::Link);
                if let Some((start, dest)) = self.link_stack.pop() {
                    let end = self.display.len();
                    if end > start {
                        self.links
                            .push(TableCellLink::new(Span::new(start, end), dest));
                    }
                }
                true
            }
            TagEnd::Image => {
                self.skip_image_depth = self.skip_image_depth.saturating_sub(1);
                true
            }
            _ => false,
        }
    }

    /// 打开一个样式构造：记下 display 起点，等闭合时产出候选。
    fn open(&mut self, kind: TableCellStyle) {
        self.candidates.push((self.display.len(), usize::MAX, kind));
    }

    /// 闭合样式构造：把**最近一个**同 kind 的未闭合候选补上终点。
    fn close(&mut self, kind: TableCellStyle) {
        if let Some(slot) = self
            .candidates
            .iter_mut()
            .rev()
            .find(|(_, end, candidate)| *candidate == kind && *end == usize::MAX)
        {
            slot.1 = self.display.len();
        }
    }

    /// 追加普通文本（样式取当前最内层构造，闭合时由候选区间覆盖）。
    fn push_text(&mut self, text: &str) {
        self.display.push_str(text);
    }

    /// 追加**源码原文**（不支持的构造按原样显示）。
    fn push_raw(&mut self, range: Range<usize>, cell_source: &str) {
        let raw = cell_source.get(range).unwrap_or_default();
        // 源码里可能有 `\n`（防御性），统一收敛成空格：display 绝不能含换行。
        let text: String = raw
            .chars()
            .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
            .collect();
        self.display.push_str(&text);
    }

    /// 完成解析：把候选区间变成**有序、不重叠、完整覆盖**的样式段。
    fn finish(mut self) -> TableCellInline {
        let display_len = self.display.len();
        // 未闭合的候选（理论上不会发生）直接丢弃：只有闭合的构造才有终点。
        self.candidates
            .retain(|(start, end, _)| *end != usize::MAX && end > start);
        self.candidates.sort_unstable();
        let spans = merge_runs(display_len, &self.candidates);
        TableCellInline {
            display: self.display,
            spans,
            links: self.links,
            plain: false,
        }
    }
}

/// 候选区间 → 完整覆盖的样式段（与 editor 的 `style::runs` 同一套算法）。
///
/// 所有候选边界 + 文档两端切成基础窗口；每个窗口取**完整覆盖**它的候选里
/// 最短的那个（长度相同按 [`TableCellStyle::priority`]），都没有则 `Plain`。
fn merge_runs(
    display_len: usize,
    candidates: &[(usize, usize, TableCellStyle)],
) -> Vec<TableCellSpan> {
    if display_len == 0 {
        return Vec::new();
    }
    let mut boundaries: Vec<usize> = Vec::with_capacity(candidates.len() * 2 + 2);
    boundaries.push(0);
    boundaries.push(display_len);
    for &(start, end, _) in candidates {
        boundaries.push(start);
        boundaries.push(end);
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut merged: Vec<(usize, usize, TableCellStyle)> = Vec::new();
    for window in boundaries.windows(2) {
        let (start, end) = (window[0], window[1]);
        if start >= end {
            continue;
        }
        let best = candidates
            .iter()
            .filter(|(candidate_start, candidate_end, _)| {
                *candidate_start <= start && *candidate_end >= end
            })
            .min_by_key(|(candidate_start, candidate_end, kind)| {
                (candidate_end - candidate_start, kind.priority())
            })
            .map_or(TableCellStyle::Plain, |(_, _, kind)| *kind);
        match merged.last_mut() {
            Some((_, last_end, last_kind)) if *last_kind == best && *last_end == start => {
                *last_end = end;
            }
            _ => merged.push((start, end, best)),
        }
    }

    merged
        .into_iter()
        .map(|(start, end, kind)| TableCellSpan::new(Span::new(start, end), kind))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{parse_table_cell_inline, TableCellStyle};
    use crate::Span;

    /// 把样式段变成 `(display 切片, kind)`，便于断言。
    fn styled(inline: &super::TableCellInline) -> Vec<(&str, TableCellStyle)> {
        inline
            .spans()
            .iter()
            .map(|span| {
                (
                    &inline.display()[span.display.start..span.display.end],
                    span.kind,
                )
            })
            .collect()
    }

    /// 覆盖完整性：段长之和 = display 长度，且首尾相接。
    fn assert_covers(inline: &super::TableCellInline) {
        let mut cursor = 0usize;
        for span in inline.spans() {
            assert_eq!(span.display.start, cursor, "样式段必须首尾相接：{inline:?}");
            assert!(
                span.display.end > span.display.start,
                "不允许空段：{inline:?}"
            );
            cursor = span.display.end;
        }
        assert_eq!(
            cursor,
            inline.display().len(),
            "样式段必须完整覆盖：{inline:?}"
        );
    }

    #[test]
    fn plain_text_has_no_markers_and_one_plain_run() {
        let inline = parse_table_cell_inline("苹果");
        assert_eq!(inline.display(), "苹果");
        assert_eq!(styled(&inline), vec![("苹果", TableCellStyle::Plain)]);
        assert!(inline.links().is_empty());
        assert!(!inline.is_plain());
        assert_covers(&inline);
    }

    #[test]
    fn bold_emphasis_strike_and_code_hide_their_markers() {
        let inline = parse_table_cell_inline("**粗** *斜* ~~删~~ `码` 平");
        assert_eq!(inline.display(), "粗 斜 删 码 平");
        assert_eq!(
            styled(&inline),
            vec![
                ("粗", TableCellStyle::Bold),
                (" ", TableCellStyle::Plain),
                ("斜", TableCellStyle::Emphasis),
                (" ", TableCellStyle::Plain),
                ("删", TableCellStyle::Strike),
                (" ", TableCellStyle::Plain),
                ("码", TableCellStyle::InlineCode),
                (" 平", TableCellStyle::Plain),
            ]
        );
        assert_covers(&inline);
    }

    #[test]
    fn nested_constructs_take_the_innermost_run() {
        let inline = parse_table_cell_inline("**a *b* c**");
        assert_eq!(inline.display(), "a b c");
        assert_eq!(
            styled(&inline),
            vec![
                ("a ", TableCellStyle::Bold),
                ("b", TableCellStyle::Emphasis),
                (" c", TableCellStyle::Bold),
            ]
        );
        assert_covers(&inline);

        // 行内代码里的 `*` 不是斜体定界符。
        let code = parse_table_cell_inline("`a*b*c`");
        assert_eq!(code.display(), "a*b*c");
        assert_eq!(styled(&code), vec![("a*b*c", TableCellStyle::InlineCode)]);
        assert_covers(&code);
    }

    #[test]
    fn escaped_pipes_and_backslashes_are_unescaped_in_display() {
        // `\|` 是 GFM 的列边界保护：display 里必须是一个字面 `|`。
        let inline = parse_table_cell_inline("c\\|d");
        assert_eq!(inline.display(), "c|d");
        assert_eq!(styled(&inline), vec![("c|d", TableCellStyle::Plain)]);
        assert_covers(&inline);

        // 行内代码里的 `\|` 同样还原成 `|`（cell 源码层已保证不切列）。
        let code = parse_table_cell_inline("`a\\|b`");
        assert_eq!(code.display(), "a|b");
        assert_eq!(styled(&code), vec![("a|b", TableCellStyle::InlineCode)]);
        assert_covers(&code);

        // `\*` 不是斜体定界符，display 里是字面 `*`。
        let star = parse_table_cell_inline("\\*not em\\*");
        assert_eq!(star.display(), "*not em*");
        assert_eq!(styled(&star), vec![("*not em*", TableCellStyle::Plain)]);
        assert_covers(&star);
    }

    #[test]
    fn links_expose_display_range_and_destination() {
        let inline = parse_table_cell_inline("[文档](https://example.com/a?b=1) 尾巴");
        assert_eq!(inline.display(), "文档 尾巴");
        assert_eq!(
            inline.links(),
            &[super::TableCellLink::new(
                Span::new(0, "文档".len()),
                "https://example.com/a?b=1"
            )]
        );
        assert_eq!(
            styled(&inline),
            vec![
                ("文档", TableCellStyle::Link),
                (" 尾巴", TableCellStyle::Plain)
            ]
        );
        assert_covers(&inline);

        // 自动链接同样算普通链接（display 就是 URL 本身）。
        let auto = parse_table_cell_inline("<mailto:a@b.c>");
        assert_eq!(auto.display(), "mailto:a@b.c");
        assert_eq!(
            auto.links(),
            &[super::TableCellLink::new(
                Span::new(0, "mailto:a@b.c".len()),
                "mailto:a@b.c"
            )]
        );
        assert_covers(&auto);
    }

    #[test]
    fn link_inside_bold_keeps_both_candidates_and_link_wins_inside() {
        let inline = parse_table_cell_inline("**[a](https://x.y)**");
        assert_eq!(inline.display(), "a");
        // 同长度时按优先级：Link(4) 输给 Bold(1)？——不，候选里 Bold 覆盖整个 `a`，
        // Link 也覆盖整个 `a`；长度相同 → 取优先级更高的 Bold（与 editor 同规则）。
        assert_eq!(styled(&inline), vec![("a", TableCellStyle::Bold)]);
        assert_eq!(inline.links().len(), 1, "链接仍然可点击（样式不影响命中）");
        assert_covers(&inline);
    }

    #[test]
    fn empty_cell_has_no_runs_and_no_links() {
        let inline = parse_table_cell_inline("");
        assert_eq!(inline.display(), "");
        assert!(inline.spans().is_empty());
        assert!(inline.links().is_empty());
        assert!(!inline.is_plain());
    }

    #[test]
    fn block_level_constructs_fall_back_to_source_text() {
        for source in ["# 标题", "> 引用", "- 列表", "1. 有序", "---", "```"] {
            let inline = parse_table_cell_inline(source);
            assert!(
                inline.is_plain(),
                "{source:?} 是块级构造，cell 里必须按源码原样显示：{inline:?}"
            );
            assert_eq!(inline.display(), source, "{source:?} 必须逐字保留");
            assert_eq!(
                styled(&inline),
                vec![(source, TableCellStyle::Plain)],
                "{source:?} 只有一个 Plain 段"
            );
            assert!(inline.links().is_empty());
        }
    }

    #[test]
    fn images_fall_back_to_source_text_but_keep_supported_neighbours() {
        let inline = parse_table_cell_inline("**a** ![x](asset://y) b");
        assert_eq!(inline.display(), "a ![x](asset://y) b");
        assert_eq!(
            styled(&inline),
            vec![
                ("a", TableCellStyle::Bold),
                (" ![x](asset://y) b", TableCellStyle::Plain),
            ]
        );
        assert!(inline.links().is_empty(), "图片不是链接");
        assert_covers(&inline);
    }

    #[test]
    fn newlines_never_reach_the_display_text() {
        // GFM 的 cell 不可能跨行；防御性输入也必须收敛成单行（shaping 会断言）。
        let inline = parse_table_cell_inline("a\nb\r\nc");
        assert!(!inline.display().contains('\n'));
        assert!(!inline.display().contains('\r'));
        assert_eq!(inline.display(), "a b c");
        assert_covers(&inline);
    }

    #[test]
    fn unmatched_markers_stay_literal_text() {
        let inline = parse_table_cell_inline("**未闭合");
        assert_eq!(inline.display(), "**未闭合");
        assert_eq!(
            styled(&inline),
            vec![("**未闭合", TableCellStyle::Plain)],
            "未闭合定界符是字面文本"
        );
        assert_covers(&inline);
    }
}
