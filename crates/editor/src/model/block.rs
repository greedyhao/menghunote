//! 块级语义区间（S2-7a，**纯模型**）：每行一个 [`BlockRun`]，供 app 做标题放大 /
//! 块级样式。只出语义（块类型 + 标题级别），**不出字号/颜色**，也不依赖 GPUI。
//!
//! # 行切分与覆盖
//!
//! - 按 `\n` 切行，每行一个 run；run 的 `source` 区间**包含行尾的 `\n`**（最后一行
//!   没有 `\n` 时到 `source_len` 为止），因此 runs 有序、不重叠、完整覆盖
//!   `[0, source_len)`；
//! - 空行（连续的 `\n\n`、文首 `\n` 等）也是独立的一行，`kind = Paragraph`；
//! - 文档以 `\n` 结尾时，**不会**再额外产生一个末尾空行 run（`\n` 属于上一行）；
//! - 空文档返回空 `Vec`（覆盖 `[0, 0)`）；
//! - 每行只给一个 `kind`；标题行 `level = Some(1..=6)`，其它行 `level = None`。
//!
//! # 行分类规则
//!
//! 分类复用 [`MarkdownDocument`] 的构造（与 S2-1 扫描器同一套语义），不重复解析：
//!
//! | kind | 判定 |
//! | --- | --- |
//! | `FencedCode` | 行起点落在任一围栏代码块构造的 `span` 内（开围栏行、内部行、闭围栏行都算；**内部行永远不会被误判成标题**） |
//! | `Heading` | 该行有 ATX 标题构造，且 `#` 之前没有“纯空格缩进”；`level` = `#` 的个数（1–6） |
//! | `ListItem` | 该行有列表项构造 |
//! | `BlockQuote` | 该行有引用构造 |
//! | `Paragraph` | 其余所有行（含空行、普通文本、未闭合围栏等） |
//!
//! ## 行首空格与标题
//!
//! S2-1 扫描器按 CommonMark 允许 `#` 前有 ≤3 个空格并识别为标题标记。但块级样式
//! 层**不把这种行当标题**：如果 `#` 之前只有空格（没有 `>` / 列表标记等块前缀），
//! 这一行退化为 `Paragraph`——即“行首空格不构成标题”。这样缩进写出的 `#` 不会
//! 被放大成标题。
//!
//! ## 一行多个块前缀时的取舍（自行定义）
//!
//! 同一行可能同时命中多个块级构造（如 `> - item`、`- # h`、`> # h`）。本层每行
//! 只输出一个 kind，优先级固定为：
//!
//! `FencedCode > Heading > ListItem > BlockQuote > Paragraph`
//!
//! 理由：标题级别是块级样式最强的信号（字号/间距）；列表项比引用更“逐行具体”
//! （缩进/圆点）；引用作为容器样式可在后续 app 层结合 `MarkdownDocument` 的构造
//! 另行处理。例：
//!
//! - `> # h` → `Heading(1)`（标题优先）；
//! - `> - item` → `ListItem`（列表项优先于引用）；
//! - `> quote` → `BlockQuote`；
//! - 围栏代码块内的 `# heading` → `FencedCode`（绝不误判标题）。
//!
//! 未闭合围栏块在 S2-1 中不产生构造（保守不隐藏），因此其行按普通行分类；
//! 由于扫描器也不会在其内部识别标题，不会误判成 `Heading`。

use menghunote_text::{SourceByte, SourceRange};

use crate::model::markdown::{ConstructKind, MarkdownDocument, MarkdownError};

/// 块级语义类型（只表示结构，不表示外观）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BlockKind {
    /// ATX 标题（`#` … `######`），级别见 [`BlockRun::level`]。
    Heading,
    /// 引用行（`> …`）。
    BlockQuote,
    /// 列表项行（`- ` / `* ` / `+ ` / `1. `）。
    ListItem,
    /// 围栏代码块的行（开围栏、内部、闭围栏）。
    FencedCode,
    /// 普通文本行（含空行、未识别行）。
    Paragraph,
}

/// 一行的块级语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRun {
    source: SourceRange,
    kind: BlockKind,
    level: Option<u8>,
}

impl BlockRun {
    /// 该行在 source 中的区间（**含行尾 `\n`**，最后一行到文本尾）。
    #[must_use]
    pub const fn source(&self) -> SourceRange {
        self.source
    }

    /// 块级语义类型。
    #[must_use]
    pub const fn kind(&self) -> BlockKind {
        self.kind
    }

    /// 标题级别（`Some(1..=6)`）；非标题行恒为 `None`。
    #[must_use]
    pub const fn level(&self) -> Option<u8> {
        self.level
    }
}

/// 计算整篇文档的块级语义（每行一个 run）。
///
/// 仅在 source 变化时需要重算；与 selection / caret 无关。
pub fn runs(doc: &MarkdownDocument) -> Result<Vec<BlockRun>, MarkdownError> {
    let source = doc.source();
    if source.is_empty() {
        return Ok(Vec::new());
    }

    // 行表：line_starts[i] 行首；line_ends[i] 行尾（含 `\n`，不含则到 len）。
    let mut line_starts: Vec<usize> = Vec::new();
    let mut line_ends: Vec<usize> = Vec::new();
    let mut start = 0usize;
    while start < source.len() {
        line_starts.push(start);
        let end = source[start..]
            .find('\n')
            .map_or(source.len(), |offset| start + offset + 1);
        line_ends.push(end);
        start = end;
    }

    let line_count = line_starts.len();
    // 每行先按优先级收集一个候选；None 最终落为 Paragraph。
    let mut best: Vec<Option<(BlockKind, Option<u8>)>> = vec![None; line_count];

    let line_of = |byte: usize| -> usize {
        line_starts
            .partition_point(|line_start| *line_start <= byte)
            .saturating_sub(1)
    };

    for construct in doc.constructs() {
        let span = construct.span();
        let construct_start = span.start().get();
        match construct.kind() {
            // 围栏代码块覆盖开围栏、内部行、闭围栏。
            ConstructKind::FencedCode => {
                let first_line = line_of(construct_start);
                let last_line = line_of(span.end().get().saturating_sub(1));
                for slot in &mut best[first_line..=last_line] {
                    *slot = Some((BlockKind::FencedCode, None));
                }
            }
            ConstructKind::Heading => {
                let line = line_of(construct_start);
                let line_start = line_starts[line];
                let prefix = &source[line_start..construct_start];
                // 只有空格的缩进不构成标题（任务约定）；有 `>` / 列表标记等块前缀时可以。
                if construct_start > line_start && prefix.bytes().all(|byte| byte == b' ') {
                    continue;
                }
                let hashes = source[construct_start..]
                    .bytes()
                    .take_while(|byte| *byte == b'#')
                    .count();
                let level = u8::try_from(hashes).map_err(|_| MarkdownError::InternalRange {
                    start: construct_start,
                    end: construct_start + hashes,
                    len: source.len(),
                })?;
                consider(&mut best[line], (BlockKind::Heading, Some(level)));
            }
            ConstructKind::ListItem => {
                let line = line_of(construct_start);
                consider(&mut best[line], (BlockKind::ListItem, None));
            }
            ConstructKind::BlockQuote => {
                let line = line_of(construct_start);
                consider(&mut best[line], (BlockKind::BlockQuote, None));
            }
            // 行内构造不影响块级分类。
            ConstructKind::Bold
            | ConstructKind::Emphasis
            | ConstructKind::InlineCode
            | ConstructKind::Strike
            | ConstructKind::Link
            | ConstructKind::WikiLink => {}
        }
    }

    let mut runs = Vec::with_capacity(line_count);
    for (index, &line_start) in line_starts.iter().enumerate() {
        let line_end = line_ends[index];
        let (kind, level) = best[index].unwrap_or((BlockKind::Paragraph, None));
        runs.push(BlockRun {
            source: source_range(source, line_start, line_end)?,
            kind,
            level,
        });
    }
    Ok(runs)
}

fn source_range(source: &str, start: usize, end: usize) -> Result<SourceRange, MarkdownError> {
    SourceRange::within(source, SourceByte::new(start), SourceByte::new(end)).map_err(|_| {
        MarkdownError::InternalRange {
            start,
            end,
            len: source.len(),
        }
    })
}

/// 按优先级保留更强的块级语义。
fn consider(best: &mut Option<(BlockKind, Option<u8>)>, candidate: (BlockKind, Option<u8>)) {
    let replace = match best {
        None => true,
        Some((kind, _)) => rank(candidate.0) < rank(*kind),
    };
    if replace {
        *best = Some(candidate);
    }
}

const fn rank(kind: BlockKind) -> u8 {
    match kind {
        BlockKind::FencedCode => 0,
        BlockKind::Heading => 1,
        BlockKind::ListItem => 2,
        BlockKind::BlockQuote => 3,
        BlockKind::Paragraph => 4,
    }
}
