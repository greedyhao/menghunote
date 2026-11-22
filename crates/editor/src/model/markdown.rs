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
use crate::model::projection::{ProjectionMap, ProjectionPiece};
use crate::model::selection::Selection;
use crate::model::style::StyleRun;

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
}

impl ConstructKind {
    /// 是否属于行内构造。
    #[must_use]
    pub const fn is_inline(self) -> bool {
        matches!(
            self,
            Self::Bold | Self::Emphasis | Self::InlineCode | Self::Strike | Self::Link
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
    /// 新解析器的完整语义（列表/行信息），即使行内投影回退到旧 scanner 也保留。
    semantic: ParsedMarkdown,
    /// 行内投影是否退回旧 scanner（只影响 inline 构造，不影响列表/块级）。
    inline_fallback: bool,
}

impl MarkdownDocument {
    /// 解析源码。空文档、纯文本都返回空构造表（不报错）。
    pub fn parse(source: &str) -> Result<Self, MarkdownError> {
        let semantic = semantic::parse(source);
        let legacy = legacy_constructs(source)?;
        let (constructs, inline_fallback) = merge_constructs(source, &semantic, legacy);
        Ok(Self {
            source: source.to_owned(),
            constructs,
            semantic,
            inline_fallback,
        })
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
        if item.marker.is_empty() {
            return Ok(false);
        }
        let marker = self.range(item.marker.start, item.marker.end)?;
        let start = selection.start().to_source_byte(&self.source)?.get();
        let end = selection.end().to_source_byte(&self.source)?.get();
        if start == end {
            Ok(marker.start().get() <= start && start < marker.end().get())
        } else {
            Ok(marker.start().get() < end && marker.end().get() > start)
        }
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
            .map(|item| {
                if item.marker.is_empty() {
                    return false;
                }
                if start == end {
                    item.marker.start <= start && start < item.marker.end
                } else {
                    item.marker.start < end && item.marker.end > start
                }
            })
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
    pub fn projection_pieces(
        &self,
        selection: Selection,
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
        let hidden = merge_ranges(&self.source, hidden)?;
        let mut pieces: Vec<ProjectionPiece> =
            hidden.into_iter().map(ProjectionPiece::Hidden).collect();
        pieces.extend(replacements);
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

/// 新解析器 + 旧 scanner 的构造合并（规则见模块文档与 ADR 0019 S1 实施记录）。
fn merge_constructs(
    source: &str,
    semantic: &ParsedMarkdown,
    mut legacy: Vec<MarkdownConstruct>,
) -> (Vec<MarkdownConstruct>, bool) {
    // 1) 未支持块（表格 / HTML / 缩进代码 / Setext）、围栏代码内部、以及图片（S2 才做
    //    Object）：旧 scanner 的**行内**构造一律丢弃（原样显示，不猜测）。
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
                &semantic.code_spans,
                &semantic.unsupported_spans,
            ) || inside_any(
                construct.span().start().get(),
                construct.span().end().get(),
                &image_spans,
                &[],
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
                || inside_any(
                    construct.span.start,
                    construct.span.end,
                    &semantic.code_spans,
                    &semantic.unsupported_spans,
                )
                || inside_any(construct.span.start, construct.span.end, &image_spans, &[]))
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

fn inside_any(
    start: usize,
    end: usize,
    code: &[SemanticSpan],
    unsupported: &[SemanticSpan],
) -> bool {
    code.iter()
        .chain(unsupported.iter())
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
