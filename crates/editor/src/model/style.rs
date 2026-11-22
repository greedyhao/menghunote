//! 语义样式区间（S2-4a，**纯模型**）：display 坐标上的 Bold / Emphasis /
//! InlineCode / Strike / Link。
//!
//! 本模块只产出**语义**（哪段文字是粗体、哪段是行内代码……），不产生颜色、字号、
//! 字体或任何 GPUI 类型；外观由 app 的渲染层决定。区间基于当前
//! [`ProjectionMap`] 的 **display 坐标**，因此与“标记是否被 caret 展开”自动一致。
//!
//! # 覆盖与默认段
//!
//! [`runs`] 返回的区间：
//!
//! - **有序、不重叠、完整覆盖 `[0, display_len)`**；
//! - 没有语义样式的文字用 [`StyleKind::Plain`] 填充（这是“默认段”，不属于
//!   Markdown 语义，只用于保证覆盖完整，app 可直接按无语义处理）；
//! - 相邻且同 kind 的区间会合并；空文档返回空 `Vec`；
//! - 同一输入结果确定（边界排序 + 固定择优规则）。
//!
//! # 只覆盖内容，不覆盖标记
//!
//! 样式区间来自每个构造的**内容区间**（[`crate::model::MarkdownConstruct::content`]）
//! 映射到 display；标记区间（`**`、反引号、`[`、`](url)` 等）永远属于 `Plain`。
//! 当 caret 进入构造、标记重新可见时，内容映射随之平移，runs 边界同步更新，
//! 仍不会把标记算进样式。
//!
//! # 嵌套与优先级
//!
//! Markdown 扫描器对行内构造的文本内容做递归解析（见 [`crate::model::markdown`]），
//! 因此可能出现嵌套样式（标题里的粗体、粗体里的行内代码、链接文本里的斜体……）。
//! 同一 display 位置被多个构造覆盖时：
//!
//! 1. **最内层优先**：取 display 区间最短的那个构造；
//! 2. 长度相同再按优先级：
//!    `InlineCode > Bold > Emphasis > Strike > Link`；
//! 3. 都没有则 `Plain`。
//!
//! 同种定界符的嵌套（`**a **b** c**`、`*a *b* c*`）与 `***a***` 仍不支持
//! （S2-1 的扫描器不做同种定界符配对）；这类文本按扫描器实际给出的构造着色，
//! 不会猜测。
//!
//! # 与投影/控制器的关系
//!
//! - [`crate::model::MarkdownDocument::style_runs`]：doc + 任意 projection → runs；
//! - `EditorController::style_runs`：控制器缓存的“当前投影下的 runs”，
//!   在 source / selection 变化、投影重建时同步重算，app 每帧直接借用。

use std::ops::Range;

use menghunote_text::DisplayByte;

use crate::model::markdown::{ConstructKind, MarkdownDocument};
use crate::model::projection::ProjectionMap;

/// 语义样式种类。
///
/// **只表示语义**，不包含任何外观信息（颜色/字重/字号由 app 决定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StyleKind {
    /// 默认段：没有任何 Markdown 语义样式（为覆盖完整性而存在）。
    Plain,
    /// `**bold**`。
    Bold,
    /// `*em*` / `_em_`。
    Emphasis,
    /// `` `code` ``（行内代码）。
    InlineCode,
    /// `~~strike~~`。
    Strike,
    /// `[text](url)` 的文本部分。
    Link,
}

impl StyleKind {
    /// 是否为默认（无语义）段。
    #[must_use]
    pub const fn is_plain(self) -> bool {
        matches!(self, Self::Plain)
    }

    /// 叠加优先级（数值越小越优先；仅用于同长度区间择优）。
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleRun {
    display: Range<DisplayByte>,
    kind: StyleKind,
}

impl StyleRun {
    /// display 半开区间。
    #[must_use]
    pub fn display(&self) -> &Range<DisplayByte> {
        &self.display
    }

    /// display 起始 byte（含）。
    #[must_use]
    pub const fn start(&self) -> DisplayByte {
        self.display.start
    }

    /// display 结束 byte（不含）。
    #[must_use]
    pub const fn end(&self) -> DisplayByte {
        self.display.end
    }

    /// 语义种类（可能是 [`StyleKind::Plain`]）。
    #[must_use]
    pub const fn kind(&self) -> StyleKind {
        self.kind
    }
}

/// 计算当前投影下的语义样式区间。
///
/// 返回 runs 有序、不重叠、完整覆盖 `[0, projection.display_len())`；无样式处为
/// [`StyleKind::Plain`]。
#[must_use]
pub fn runs(doc: &MarkdownDocument, projection: &ProjectionMap) -> Vec<StyleRun> {
    let display_len = projection.display_len().get();
    if display_len == 0 {
        return Vec::new();
    }

    let spans = candidate_spans(doc, projection);

    // 所有候选边界 + 文档两端；相邻边界之间选一个样式（或 Plain）。
    let mut boundaries: Vec<usize> = Vec::with_capacity(spans.len() * 2 + 2);
    boundaries.push(0);
    boundaries.push(display_len);
    for &(start, end, _) in &spans {
        boundaries.push(start);
        boundaries.push(end);
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut merged: Vec<(usize, usize, StyleKind)> = Vec::new();
    for window in boundaries.windows(2) {
        let start = window[0];
        let end = window[1];
        if start >= end {
            continue;
        }
        // 只考虑完整覆盖当前基础区间的候选：
        // 先比长度（最内层优先），长度相同再比语义优先级。
        let best = spans
            .iter()
            .filter(|(span_start, span_end, _)| *span_start <= start && *span_end >= end)
            .min_by_key(|(span_start, span_end, kind)| (span_end - span_start, kind.priority()))
            .map_or(StyleKind::Plain, |(_, _, kind)| *kind);

        match merged.last_mut() {
            Some((_, last_end, last_kind)) if *last_kind == best && *last_end == start => {
                *last_end = end;
            }
            _ => merged.push((start, end, best)),
        }
    }

    merged
        .into_iter()
        .map(|(start, end, kind)| StyleRun {
            display: DisplayByte::new(start)..DisplayByte::new(end),
            kind,
        })
        .collect()
}

/// 每个带样式的构造：`(display_start, display_end, kind)`。
///
/// 只映射**内容区间**：标记不进入候选，因此样式绝不会覆盖标记。
fn candidate_spans(
    doc: &MarkdownDocument,
    projection: &ProjectionMap,
) -> Vec<(usize, usize, StyleKind)> {
    let mut spans = Vec::new();
    for construct in doc.constructs() {
        let Some(kind) = semantic_kind(construct.kind()) else {
            continue;
        };
        let content = construct.content();
        if content.is_empty() {
            continue;
        }
        // 内容端点一定落在可见段的边界上：投影只会省略“标记”，不会省略内容。
        let Some(start) = projection.display_byte_at_source(content.start()) else {
            continue;
        };
        let Some(end) = projection.display_byte_at_source(content.end()) else {
            continue;
        };
        if end.get() > start.get() {
            spans.push((start.get(), end.get(), kind));
        }
    }
    spans
}

fn semantic_kind(kind: ConstructKind) -> Option<StyleKind> {
    match kind {
        ConstructKind::Bold => Some(StyleKind::Bold),
        ConstructKind::Emphasis => Some(StyleKind::Emphasis),
        ConstructKind::InlineCode => Some(StyleKind::InlineCode),
        ConstructKind::Strike => Some(StyleKind::Strike),
        ConstructKind::Link => Some(StyleKind::Link),
        ConstructKind::Heading
        | ConstructKind::BlockQuote
        | ConstructKind::ListItem
        | ConstructKind::FencedCode => None,
    }
}
