//! ADR 0019 S3b：表格 Object「加行 / 加列 / 删除行 / 删除列 / 拖拽重排行列」的
//! **源码位置构造**（纯函数）。
//!
//! 这里只回答一个问题：**在哪个源码字节处插入 / 删除哪段文本**。真正的正文修改一律由
//! [`crate::model::controller::EditorController`] 走 `compose_edits`（一次点击一个
//! `EditRequest` = 一个撤销组），本模块不接触 Rope、会话、投影，也不认识 UI。
//!
//! # 不变量（测试钉住）
//!
//! 1. **插入只插入、删除只删除**：[`TableInsertion`] 恒是空区间（`at..at`）；
//!    [`TableDeletion`] 恒是删除区间（替换成空串），其余字节**逐字节保留**——行首 `|`、
//!    其它 cell 的空白、分隔行的冒号与对齐标记都不会被重写；
//! 2. 插入点只落在**行内容末尾**（不含行尾换行），新行 / 新列不会吃掉原换行；
//!    CRLF 文档插入 CRLF，LF 文档插入 LF（不混用行尾）；
//! 3. 删除**至少保留 1 列**（GFM 分隔行至少要有 1 个分隔 cell，否则表格不再成立）；
//!    表头行永不删除（删掉表头 GFM 表格会整体消失），删除数据行时连行尾换行一起删，
//!    不留空行（末行没有行尾换行时改删**前一行**的行尾）。
//!
//! # 与 GFM 语法的关系
//!
//! GFM 的行列数由**分隔行**决定：分隔行多一列，后面所有行就按多一列解析（缺列补空、
//! 多列丢弃）。所以加列必须同时给表头 / 分隔行 / **每一条数据行**追加，缺一列都会
//! 让表格列数回到旧值；反过来，加行只是多一条数据行，不影响分隔行与列数。删列同理：
//! 必须把表头、分隔行、所有数据行里的同一列都删掉，否则分隔行会把列数“补回来”。

use crate::model::markdown::MarkdownTable;
use crate::{CoordError, SourceByte, SourceRange};

/// 一条「在字节 `at` 处插入 `text`」的源码编辑（空区间，纯插入）。
///
/// `at` 恒是 UTF-8 字节位置且落在行内容末尾；由控制器换算成 `UnicodeScalar`
/// 之后再变成 `ScalarEdit`，本模块不产生任何替换/删除。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInsertion {
    at: usize,
    text: String,
}

impl TableInsertion {
    /// 插入位置（source UTF-8 字节；恒是 scalar 边界）。
    #[must_use]
    pub const fn at(&self) -> usize {
        self.at
    }

    /// 要插入的源码文本：加列时是行尾的一小段（不含换行），加行时是「行尾换行 +
    /// 整条空数据行」。
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// 一条**空数据行**的源码文本（列数与表格一致，所有 cell 为空）。
///
/// 形如 `|  |  |  |`（每个 cell 两个空格）：GFM 会把这些 cell 裁成空串，
/// 但源码里保留可读的间距（与手写表格一致，且不会把相邻 `|` 挤成 `||`）。
#[must_use]
pub fn empty_row_text(columns: usize) -> String {
    let columns = columns.max(1);
    let mut text = String::with_capacity(columns * 3 + 1);
    text.push('|');
    for _ in 0..columns {
        text.push_str("  |");
    }
    text
}

/// 「加行」的插入：在**最后一条数据行之后**（没有数据行时在分隔行之后）插入一条
/// 空数据行。
///
/// 插入点取整表区间末尾（= 最后一行的内容终点，不含换行），因此：
/// - 原表各行（含分隔行）逐字节不变，也不会复制上一行的内容；
/// - 新行是表格的**最后一行**，不会插到表格中间；
/// - 表格如果在文档末尾且没有换行，会先补一个本文件所用的行尾再写新行。
#[must_use]
pub fn append_row_insertions(source: &str, table: &MarkdownTable) -> Vec<TableInsertion> {
    let at = table.span().end().get();
    let text = format!(
        "{}{}",
        line_ending_at(source, at),
        empty_row_text(table.columns())
    );
    vec![TableInsertion { at, text }]
}

/// 「加列」的插入：给表头、分隔行与**每一条数据行**的末尾各追加一列。
///
/// 具体追加什么（写死，测试钉住）：
/// - 普通行（表头/数据行）：行尾是 `|` 时追加 `" |"`，否则追加 `" | |"`
///   （两种写法都让 GFM 多解析出一个**空 cell**，且不动已有字节）；
/// - 分隔行：追加一个**默认对齐**（无冒号）的分隔 cell —— 行尾是 `|` 时 `" ---|"`，
///   否则 `"| ---"`。分隔行的 cell 必须是 `-+` 形状，不能用空 cell；
/// - 有/无对齐的表格都保留原有冒号与对齐（只动行尾）；
/// - 没有数据行的表格也能加列（表头 + 分隔行照样扩列）。
#[must_use]
pub fn append_column_insertions(source: &str, table: &MarkdownTable) -> Vec<TableInsertion> {
    let mut insertions = Vec::with_capacity(table.row_count() + 2);
    insertions.push(column_insertion(source, table.header().span(), true));
    insertions.push(column_insertion(source, table.delimiter(), false));
    for row in table.rows() {
        insertions.push(column_insertion(source, row.span(), true));
    }
    insertions.sort_by_key(TableInsertion::at);
    insertions
}

/// 单行「加列」的插入（`cell_row = true` 是表头/数据行，`false` 是分隔行）。
fn column_insertion(source: &str, row: crate::SourceRange, cell_row: bool) -> TableInsertion {
    let at = row.end().get();
    let line = source
        .get(row.start().get()..at)
        .unwrap_or_default()
        .trim_end_matches([' ', '\t']);
    // 行尾是否已经有 `|`：有就只补一个 cell 的内容 + 收尾 `|`，没有就连 cell 边界一起补。
    let closed = line.ends_with('|');
    let text = match (cell_row, closed) {
        (true, true) => " |".to_owned(),
        (true, false) => " | |".to_owned(),
        (false, true) => " ---|".to_owned(),
        (false, false) => "| ---".to_owned(),
    };
    TableInsertion { at, text }
}

/// 插入点之后的行尾：优先用**文档里已有的**那一个，避免 CRLF 文档混进 LF。
///
/// 插入点在行内容末尾，所以后面紧跟本行自己的 `\r\n` / `\n`；表格在文档末尾
/// （没有换行）时退回“文档里是否出现过 CRLF”。没有换行的空文档用 LF。
fn line_ending_at(source: &str, at: usize) -> &'static str {
    let rest = source.get(at..).unwrap_or_default();
    if rest.starts_with("\r\n") {
        "\r\n"
    } else if rest.starts_with('\n') {
        "\n"
    } else if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// 一条「删除源码区间」的编辑：把 [`Self::span`] 内的字节整段删掉（替换成空串）。
///
/// 区间两端恒是 UTF-8 scalar 边界，且**互不重叠**；由控制器换算成
/// `UnicodeScalar` 之后再变成 `ScalarEdit`，本模块不产生任何插入/替换。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableDeletion {
    span: SourceRange,
}

impl TableDeletion {
    /// 要删除的源码区间（source UTF-8 字节）。
    #[must_use]
    pub const fn span(self) -> SourceRange {
        self.span
    }
}

/// 「删除行」的删除区间：删掉第 `row` 行（0 = 表头）**整行含行尾换行**。
///
/// 契约（写死，测试钉住）：
/// - `row == 0`（表头）或越界 → `None`：首版明确不删表头（删了 GFM 表格会整体消失）；
/// - 删除区间从行内容起点开始，**连本行行尾换行一起删**，因此不会留下空行；
/// - 表格最后一行且文档没有行尾换行（表格就是文档末尾）→ 改从**前一行行尾**开始删，
///   否则会残留一个空行；
/// - 其它行（含表头/分隔行/其它数据行）逐字节不变。
#[must_use]
pub fn delete_row_deletion(
    source: &str,
    table: &MarkdownTable,
    row: usize,
) -> Option<TableDeletion> {
    if row == 0 || row >= table.total_rows() {
        return None;
    }
    let span = table.row_span(row)?;
    let start = span.start().get();
    let end = span.end().get();
    let rest = source.get(end..).unwrap_or_default();
    let after_end = if rest.starts_with("\r\n") {
        end.checked_add(2)?
    } else if rest.starts_with('\n') {
        end.checked_add(1)?
    } else {
        end
    };
    if after_end > end {
        // 常规情形：整行内容 + 本行行尾换行。
        return source_range(source, start, after_end).map(|span| TableDeletion { span });
    }
    // 文档末尾没有换行：连**前一行**的行尾一起删，避免留下一个空行。
    let newline = start.checked_sub(1)?;
    if source.as_bytes().get(newline) != Some(&b'\n') {
        return None;
    }
    let previous_start = if newline > 0 && source.as_bytes().get(newline - 1) == Some(&b'\r') {
        newline - 1
    } else {
        newline
    };
    source_range(source, previous_start, end).map(|span| TableDeletion { span })
}

/// 「删除列」的删除区间：从**表头、分隔行与每一条数据行**里删掉第 `column` 列。
///
/// 契约（写死，测试钉住）：
/// - `columns() <= 1`（只剩最后一列）或 `column` 越界 → 空 `Vec`：GFM 分隔行至少要
///   有一个分隔 cell，删到 0 列会把表格改成非法结构（表格会整体消失）；
/// - 每行只删**本列自己的文本 + 一侧分隔符与相邻空白**：其它 cell 的源码字节、
///   行首 `|`、分隔行的冒号（对齐）**逐字节保留**；
/// - 表头/数据行用解析器给的 cell 区间（已去首尾空白）；分隔行没有 cell 事件，
///   按 `|` 切分并去掉首尾空白得到每列区间（GFM 分隔 cell 只含 `-`/`:`/空白，
///   因此 `|` 恒是列边界）；分隔行切不出 `columns` 个 cell 时**不动手**（返回空）；
/// - 缺列补出来的空区间（该行本来就没有这一列）跳过：GFM 会自动补空 cell；
/// - 返回的区间按位置升序、互不重叠（控制器整批进一个 `EditRequest`）。
#[must_use]
pub fn delete_column_deletions(
    source: &str,
    table: &MarkdownTable,
    column: usize,
) -> Vec<TableDeletion> {
    let columns = table.columns();
    if columns <= 1 || column >= columns {
        return Vec::new();
    }
    let delimiter = delimiter_cells(source, table.delimiter());
    if delimiter.len() != columns {
        // 分隔行形状与解析结果对不上（非规范表格）：宁可不删，也不猜列边界。
        return Vec::new();
    }
    let mut deletions = Vec::with_capacity(table.row_count() + 2);
    for cells in std::iter::once(table.header().cells())
        .chain(std::iter::once(delimiter.as_slice()))
        .chain(table.rows().iter().map(|row| row.cells()))
    {
        if let Some(span) = column_deletion_in_row(source, cells, column) {
            deletions.push(TableDeletion { span });
        }
    }
    deletions.sort_by_key(|deletion| deletion.span.start().get());
    deletions
}

/// 单行「删除第 `column` 列」的删除区间。
///
/// 取区间的方式保证**其它 cell 的文本逐字节保留**：
/// - 不是最后一列：删 `[本列文本起点, 下一列文本起点)` —— 本列文本、本列尾部空白、
///   分隔 `|`、下一列前导空白一起删，剩下的两段拼起来正好是“跳过了这一列”的行；
/// - 最后一列：删 `[上一列文本终点, 本列文本终点)` —— 保留本列后侧的空白与收尾 `|`，
///   于是 `| a | b |` → `| a |`（不会把行尾 `|` 也吃掉，也不留多余空格）。
///
/// 空区间（缺列补出来的空 cell）返回 `None`：这一行本来就没有该列，不必删。
fn column_deletion_in_row(
    source: &str,
    cells: &[SourceRange],
    column: usize,
) -> Option<SourceRange> {
    let cell = *cells.get(column)?;
    let (start, end) = if column + 1 < cells.len() {
        let next = *cells.get(column + 1)?;
        (cell.start().get(), next.start().get())
    } else {
        let previous = cells.get(column.checked_sub(1)?)?;
        (previous.end().get(), cell.end().get())
    };
    if start >= end {
        return None;
    }
    source_range(source, start, end)
}

/// 分隔行的逐列区间：按 `|` 切段、裁掉首尾空白，跳过行首/行尾管道产生的空段。
///
/// GFM 分隔行的 cell 只允许 `-` / `:` / 空白，因此行内每个 `|` 都是列边界，
/// 不需要考虑转义。
fn delimiter_cells(source: &str, delimiter: SourceRange) -> Vec<SourceRange> {
    let Some(text) = source.get(delimiter.start().get()..delimiter.end().get()) else {
        return Vec::new();
    };
    let base = delimiter.start().get();
    let mut cells = Vec::new();
    let mut segment_start = 0usize;
    for (index, ch) in text.char_indices() {
        if ch != '|' {
            continue;
        }
        push_delimiter_segment(source, base, text, segment_start, index, &mut cells);
        segment_start = index + 1;
    }
    push_delimiter_segment(source, base, text, segment_start, text.len(), &mut cells);
    cells
}

/// 把分隔行 `[start, end)` 这一段裁掉首尾空白后收进 `cells`（空段直接跳过）。
fn push_delimiter_segment(
    source: &str,
    base: usize,
    text: &str,
    start: usize,
    end: usize,
    cells: &mut Vec<SourceRange>,
) {
    let Some(segment) = text.get(start..end) else {
        return;
    };
    let trimmed = segment.trim();
    if trimmed.is_empty() {
        return;
    }
    let leading = segment.len() - segment.trim_start().len();
    let abs_start = base + start + leading;
    let abs_end = abs_start + trimmed.len();
    if let Some(span) = source_range(source, abs_start, abs_end) {
        cells.push(span);
    }
}

/// 在 `source` 上构造一个合法区间（两端越界 / 非 scalar 边界 → `None`，不 panic）。
fn source_range(source: &str, start: usize, end: usize) -> Option<SourceRange> {
    SourceRange::within(source, SourceByte::new(start), SourceByte::new(end)).ok()
}

/// 默认新表格的源码（2 列 × 1 条数据行；表头与数据 cell 都是空）。
///
/// 形如（LF 文档）：
///
/// ```text
/// |  |  |
/// | --- | --- |
/// |  |  |
/// ```
///
/// 分隔行用 `---`（默认对齐）；文本**恒以行尾结束**，因此表格 span 之后一定还有
/// 文本位置（caret 不会落在表格源码内部）。前后空行由 [`new_table_insertion`] 补。
#[must_use]
pub fn new_table_text(eol: &str) -> String {
    format!("|  |  |{eol}| --- | --- |{eol}|  |  |{eol}")
}

/// 「插入表格」的插入文本（ADR 0019 S3b4）：在 `[start, end)` 处插一张默认 GFM 表格，
/// 并保证表格**独占一行块**（前面是空行或文档开头，后面是空行或文档末尾）。
///
/// 契约（写死，测试钉住）：
///
/// - `start == end` 是纯插入；`start < end` 时替换选区（与打字语义一致）；
/// - 前面只**按需要**补空行：插入点在文档开头 / 已经紧跟空行 → 不补；在行首（前一个
///   字节是行尾）→ 补一个行尾（形成空行）；在行中间 → 补两个行尾（结束当前行 + 空行）；
/// - 后面只按需要补：原文为空、或紧随其后的是行尾（已经有空行）→ 不补；否则补一个行尾；
///   行尾跟随文档（CRLF 文档用 CRLF），不与 LF 混用；
/// - `start`/`end` 越界或不在 scalar 边界 → [`CoordError`]（调用方用 `SourceByte`
///   换算过，正常路径不会触发）。
pub fn new_table_insertion(source: &str, start: usize, end: usize) -> Result<String, CoordError> {
    if start > end {
        return Err(CoordError::ReversedRange { start, end });
    }
    check_boundary(source, start)?;
    check_boundary(source, end)?;
    let prefix = source.get(..start).unwrap_or_default();
    let suffix = source.get(end..).unwrap_or_default();
    let eol = line_ending_at(source, start.min(source.len()));
    // 前缀边界处的行尾（插入点在行中间时用文档行尾）。
    let boundary_eol = if prefix.ends_with("\r\n") {
        "\r\n"
    } else if prefix.ends_with('\n') {
        "\n"
    } else {
        eol
    };
    let mut text = String::new();
    if !prefix.is_empty() && !ends_with_blank_line(prefix) {
        text.push_str(boundary_eol);
        if !prefix.ends_with('\n') {
            // 插入点在行中间：先结束当前行，再留一个空行。
            text.push_str(boundary_eol);
        }
    }
    text.push_str(&new_table_text(eol));
    let suffix_starts_line_break = suffix.starts_with('\n') || suffix.starts_with("\r\n");
    if !suffix.is_empty() && !suffix_starts_line_break {
        text.push_str(eol);
    }
    Ok(text)
}

/// 前缀是否已经以“空行”结束（最后一行没有任何内容）。
fn ends_with_blank_line(prefix: &str) -> bool {
    let Some(without_last) = prefix
        .strip_suffix("\r\n")
        .or_else(|| prefix.strip_suffix('\n'))
    else {
        return false;
    };
    without_last.is_empty() || without_last.ends_with('\n')
}

/// 校验字节偏移是合法 scalar 边界（不钳制、不猜）。
fn check_boundary(source: &str, offset: usize) -> Result<(), CoordError> {
    let len = source.len();
    if offset > len {
        return Err(CoordError::ByteOutOfBounds { offset, len });
    }
    if !source.is_char_boundary(offset) {
        return Err(CoordError::NotScalarBoundary { offset, len });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ADR 0019 S3b5：表格「拖拽重排行 / 列」的源码重排（等长置换）
// ---------------------------------------------------------------------------
//
// 重排**不重写任何 cell 内容**，只把整块内容搬到另一个槽位：
//
// - 行：一条数据行的**整行内容**（不含行尾换行）搬到另一条数据行的位置；槽位之间的
//   行尾换行原样留在原位（因此末行没有换行时不会凭空新增一个换行）；
// - 列：表头 / 分隔行 / 每条数据行的**第 `from` 列 cell 文本**搬到第 `to` 列，槽位之间
//   的 `|` 与空白原样留在原位（分隔行的 `:-:` 等对齐标记跟着列一起走）。
//
// 用「整段替换」表达（而不是插入 + 删除），因此每条编辑都**等长**：表格区间、表格之外
// 的字节、caret 的 scalar 坐标、行号/列数全部不变；一次重排只产生一批互不重叠的编辑，
// 控制器整批进一个 `EditRequest` = 一个撤销组。

/// 一条「整段替换」的重排编辑：把 [`Self::span`] 内的字节换成 [`Self::text`]。
///
/// 与 [`TableInsertion`] / [`TableDeletion`] 的区别：重排同时包含“搬走”和“搬来”，
/// 用替换表达才能保证一批编辑互不重叠、按位置升序（控制器整批 `compose_edits`）。
///
/// # 不变量（测试钉住）
///
/// - [`Self::span`] 恒在表格内部，两端是 scalar 边界；
/// - `text` 与 `span` **等长**（同一批整块内容 + 槽位分隔符的置换），因此重排是
///   纯粹的“顺序变化”：表格总字节数、行号、列数、表格之外的字节都不变。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableReplacement {
    span: SourceRange,
    text: String,
}

impl TableReplacement {
    /// 被替换的源码区间（source UTF-8 字节；恒在表格内部）。
    #[must_use]
    pub const fn span(&self) -> SourceRange {
        self.span
    }

    /// 重排后的文本（与 [`Self::span`] 等长）。
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// 拖放**槽位**（插入缝隙）→ 移动后的最终下标。
///
/// `slot` 是“缝隙”下标：`0` = 最前、`count` = 最后；`from` 是被拖动的项。
/// 契约（写死，测试钉住）：
///
/// - 缝隙落在自身两侧（删掉自己之后插入回原位）→ `None`：不产生任何编辑；
/// - `from` / `slot` 越界、或少于 2 项（没有可换的位置）→ `None`；
/// - 返回值恒 `< count`，且与 [`move_row_replacements`] / [`move_column_replacements`]
///   的 `to` 参数**同一口径**（“移动后位于第 `to` 项”）。
#[must_use]
pub fn drop_target(from: usize, slot: usize, count: usize) -> Option<usize> {
    if count < 2 || from >= count || slot > count {
        return None;
    }
    // 往后拖时，“从槽位里抽走自己”会让后面的项整体前移一格。
    let to = if slot > from { slot - 1 } else { slot };
    if to >= count || to == from {
        return None;
    }
    Some(to)
}

/// 「重排行」的替换：把第 `from` 条**数据行**的整行内容移动到第 `to` 条数据行的位置。
///
/// 契约（写死，测试钉住）：
///
/// - `from` / `to` 是**数据行下标**（0 = 第一条数据行；表头与分隔行不参与，恒不动）；
/// - 返回**一条**替换：区间 = 第一条数据行内容起点 .. 最后一条数据行内容终点；
/// - 行内容按 `to` 重排，**槽位之间的行尾换行原样留在原位**：因此 LF 文档恒是 LF、
///   CRLF 文档恒是 CRLF，表格在文档末尾且最后一行没有换行时也不会凭空补一个换行；
/// - 表头 / 分隔行、表格前后的正文、未参与重排的字节**逐字节不变**；表格总字节数不变；
/// - `from == to` / 越界 / 少于 2 条数据行 / 重排后文本与原样相同（各行内容完全一样且
///   行尾也相同）→ 空 `Vec`（不产生任何编辑）。
#[must_use]
pub fn move_row_replacements(
    source: &str,
    table: &MarkdownTable,
    from: usize,
    to: usize,
) -> Vec<TableReplacement> {
    let count = table.row_count();
    if count < 2 || from >= count || to >= count || from == to {
        return Vec::new();
    }
    let rows = table.rows();
    let units: Vec<SourceRange> = rows.iter().map(|row| row.span()).collect();
    let order = reorder_order(count, from, to);
    let Some((start, end, text)) = reorder_units(source, &units, &order) else {
        return Vec::new();
    };
    if text == source.get(start..end).unwrap_or_default() {
        return Vec::new();
    }
    match source_range(source, start, end) {
        Some(span) => vec![TableReplacement { span, text }],
        None => Vec::new(),
    }
}

/// 「重排列」的替换：把第 `from` 列移动到第 `to` 列（表头 / 分隔行 / 每条数据行各一条）。
///
/// 契约（写死，测试钉住）：
///
/// - `from` / `to` 是**列下标**（0 起）；`from == to` / 越界 / 少于 2 列 → 空 `Vec`；
/// - 每行返回**一条**替换：区间 = 该行第 0 列 cell 文本起点 .. 第 `columns - 1` 列
///   cell 文本终点，因此行首/行尾 `|` 与未参与重排的字节**逐字节保留**；
/// - cell 文本按 `to` 重排，**槽位之间的 `|` 与空白原样留在原位**：分隔行的对齐标记
///   （`:-:` 等）跟着列一起移动，其它列的对齐不变；
/// - 缺列的行（GFM 补出来的空 cell 落在行内容末尾）**整次放弃**（空 `Vec`）：这种行
///   的真实 cell 数少于列数，重排会把“隐式补在末尾的空列”搬到错误位置，宁可不猜
///   （与 [`delete_column_deletions`] 的保守口径一致）；
/// - 多出列数之外的 cell 不参与重排（GFM 本来就忽略它们），其字节原样留在行尾；
/// - 返回的替换按行顺序（表头 → 分隔行 → 数据行），互不重叠；每行一条。
#[must_use]
pub fn move_column_replacements(
    source: &str,
    table: &MarkdownTable,
    from: usize,
    to: usize,
) -> Vec<TableReplacement> {
    let columns = table.columns();
    if columns < 2 || from >= columns || to >= columns || from == to {
        return Vec::new();
    }
    // 分隔行没有 cell 事件：按 `|` 切段 + 裁首尾空白得到每列区间；形状对不上就不动手。
    let delimiter = delimiter_cells(source, table.delimiter());
    if delimiter.len() != columns {
        return Vec::new();
    }
    let order = reorder_order(columns, from, to);
    let header_cells = table.header().cells();
    if !column_row_is_complete(table.header().span(), header_cells, columns) {
        return Vec::new();
    }
    let mut replacements = Vec::with_capacity(table.row_count() + 2);
    let Some(header) = column_replacement(source, header_cells, &order) else {
        return Vec::new();
    };
    replacements.push(header);
    // 分隔行：区间是“逐列裁好的 cell”，`delimiter[..columns]` 与 columns 相同。
    let Some(delimiter) = column_replacement(source, &delimiter, &order) else {
        return Vec::new();
    };
    replacements.push(delimiter);
    for row in table.rows() {
        let cells = row.cells();
        if !column_row_is_complete(row.span(), cells, columns) {
            // 有缺列的行：整次放弃（GFM 会把它补成末尾空列，重排位置就错了）。
            return Vec::new();
        }
        let Some(replacement) = column_replacement(source, cells, &order) else {
            return Vec::new();
        };
        replacements.push(replacement);
    }
    // 内容完全相同的列（尤其是分隔行两侧对齐一致时）重排后文本不变：不为它生成
    // 一条空替换（所有行都不变 → 整个移动就是 no-op）。
    replacements.retain(|replacement| {
        let start = replacement.span.start().get();
        let end = replacement.span.end().get();
        source.get(start..end) != Some(replacement.text.as_str())
    });
    replacements.sort_by_key(|replacement| replacement.span.start().get());
    replacements
}

/// `count` 项、把第 `from` 项移动到第 `to` 项之后的下标顺序（`to` 是最终下标）。
fn reorder_order(count: usize, from: usize, to: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..count).collect();
    if from < count && to < count && from != to {
        let moved = order.remove(from);
        order.insert(to, moved);
    }
    order
}

/// 一行的「重排列」替换：区间 = 第 0 列 cell 起点 .. 最后一列 cell 终点。
///
/// 只处理**真实 cell 数 ≥ 列数**的行（调用方已校验）；多出列数之外的 cell 不参与，
/// 其字节留在区间之外原样不动。
fn column_replacement(
    source: &str,
    cells: &[SourceRange],
    order: &[usize],
) -> Option<TableReplacement> {
    let columns = order.len();
    if columns == 0 || cells.len() < columns {
        return None;
    }
    let units = cells.get(..columns)?;
    let (start, end, text) = reorder_units(source, units, order)?;
    Some(TableReplacement {
        span: source_range(source, start, end)?,
        text,
    })
}

/// 一行的真实 cell 数是否等于列数（缺列行返回 `false`）。
///
/// 解析器把缺的列补成**行内容末尾的空区间**（`start == end == row.span().end()`）；
/// 真实空 cell 也会是空区间，但它的位置在收尾 `|` 之前，不会正好落在行内容终点。
fn column_row_is_complete(row: SourceRange, cells: &[SourceRange], columns: usize) -> bool {
    if cells.len() < columns {
        return false;
    }
    let content_end = row.end().get();
    cells
        .iter()
        .take(columns)
        .all(|cell| !(cell.start().get() == content_end && cell.end().get() == content_end))
}

/// 把一组**相邻**的源码单元（行内容 / cell 内容）按 `order` 重排，分隔符留在槽位。
///
/// 返回 `(区间起点, 区间终点, 新文本)`：区间 = `[units[0].start, units[n-1].end)`，
/// 新文本 = 按 `order` 取出的单元内容，**槽位之间的分隔字节原样插回**（因此 `\r\n`
/// 行尾、cell 之间的 `|` 与空白、以及未参与重排的字节都逐字节保留）。
fn reorder_units(
    source: &str,
    units: &[SourceRange],
    order: &[usize],
) -> Option<(usize, usize, String)> {
    let count = units.len();
    if count == 0 || order.len() != count || order.iter().any(|index| *index >= count) {
        return None;
    }
    let start = units.first()?.start().get();
    let end = units.last()?.end().get();
    let mut text = String::with_capacity(end.saturating_sub(start));
    for (slot, index) in order.iter().enumerate() {
        let unit = units.get(*index)?;
        text.push_str(source.get(unit.start().get()..unit.end().get())?);
        if slot + 1 < count {
            let next_slot = units.get(slot + 1)?;
            let slot_unit = units.get(slot)?;
            // 分隔符属于**槽位**：第 slot 个与第 slot + 1 个单元之间的字节留在原位。
            text.push_str(source.get(slot_unit.end().get()..next_slot.start().get())?);
        }
    }
    Some((start, end, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::markdown::MarkdownDocument;

    /// 解析源码里的第一张表（测试夹具：解析失败直接失败，避免假通过）。
    fn parse_table(source: &str) -> MarkdownTable {
        MarkdownDocument::parse(source)
            .expect("测试源码必须能解析")
            .tables()
            .first()
            .expect("必须解析出一张表格")
            .clone()
    }

    /// 按区间升序把删除应用到源码（控制器就是把这个区间变成 `ScalarEdit`）。
    fn apply(source: &str, deletions: &[TableDeletion]) -> String {
        let mut text = source.to_owned();
        for deletion in deletions.iter().rev() {
            let start = deletion.span().start().get();
            let end = deletion.span().end().get();
            text.replace_range(start..end, "");
        }
        text
    }

    #[test]
    fn empty_row_text_has_one_empty_cell_per_column() {
        assert_eq!(empty_row_text(1), "|  |");
        assert_eq!(empty_row_text(2), "|  |  |");
        assert_eq!(empty_row_text(3), "|  |  |  |");
        // 列数 0 是异常输入：按 1 列收口，绝不产生 `|` 这种空行。
        assert_eq!(empty_row_text(0), "|  |");
    }

    #[test]
    fn delete_middle_first_and_last_data_rows() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n| 5 | 6 |\n\n后\n";
        let table = parse_table(source);
        let deletions = delete_row_deletion(source, &table, 2).expect("中间数据行可删");
        assert_eq!(
            apply(source, &[deletions]),
            "| a | b |\n|---|---|\n| 1 | 2 |\n| 5 | 6 |\n\n后\n",
            "删除第 2 条数据行：整行含行尾换行，其余字节逐字不变"
        );

        let first = delete_row_deletion(source, &table, 1).expect("第一条数据行可删");
        assert_eq!(
            apply(source, &[first]),
            "| a | b |\n|---|---|\n| 3 | 4 |\n| 5 | 6 |\n\n后\n"
        );

        let last = delete_row_deletion(source, &table, 3).expect("最后一条数据行可删");
        assert_eq!(
            apply(source, &[last]),
            "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\n后\n",
            "删除最后一条数据行时不能把后面的空行一起吃掉"
        );
    }

    #[test]
    fn delete_last_row_at_end_of_document_leaves_no_blank_line() {
        // 表格就是文档末尾、最后一行没有换行：必须连前一行的行尾一起删。
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
        let table = parse_table(source);
        let deletion = delete_row_deletion(source, &table, 2).expect("最后一行可删");
        assert_eq!(
            apply(source, &[deletion]),
            "| a | b |\n|---|---|\n| 1 | 2 |",
            "不能留下空行（前一行行尾一起删）"
        );
        // 删完仍是一张合法表格（表头 + 分隔行 + 1 条数据行）。
        let after = apply(source, &[deletion]);
        assert_eq!(parse_table(&after).row_count(), 1);
    }

    #[test]
    fn delete_row_handles_crlf_documents() {
        let source = "| a | b |\r\n|---|---|\r\n| 1 | 2 |\r\n| 3 | 4 |\r\n";
        let table = parse_table(source);
        let deletion = delete_row_deletion(source, &table, 1).expect("数据行可删");
        assert_eq!(
            apply(source, &[deletion]),
            "| a | b |\r\n|---|---|\r\n| 3 | 4 |\r\n",
            "CRLF 表格删行必须连 \\r\\n 一起删（不留下孤立的 \\r 或空行）"
        );
    }

    #[test]
    fn header_and_out_of_range_rows_are_never_deleted() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let table = parse_table(source);
        assert!(
            delete_row_deletion(source, &table, 0).is_none(),
            "首版明确不删表头（删掉 GFM 表格会整体消失）"
        );
        assert!(delete_row_deletion(source, &table, 2).is_none(), "越界行");
        assert!(delete_row_deletion(source, &table, 99).is_none(), "越界行");
    }

    #[test]
    fn delete_middle_column_from_header_delimiter_and_all_data_rows() {
        let source =
            "| 名称 | 数量 | 备注 |\n|:--|:-:|--:|\n| 苹果 | 3 | 红 |\n| 香蕉 | 5 | 黄 |\n";
        let table = parse_table(source);
        let deletions = delete_column_deletions(source, &table, 1);
        assert_eq!(deletions.len(), 4, "表头 + 分隔行 + 2 条数据行各一条删除");
        assert!(
            deletions
                .windows(2)
                .all(|pair| pair[0].span().start().get() < pair[1].span().start().get()),
            "删除区间必须按位置升序（控制器整批进一个 EditRequest）"
        );
        let after = apply(source, &deletions);
        assert_eq!(
            after, "| 名称 | 备注 |\n|:--|--:|\n| 苹果 | 红 |\n| 香蕉 | 黄 |\n",
            "中间列从四处删除：其余 cell 字节与对齐冒号逐字保留"
        );
        let parsed = parse_table(&after);
        assert_eq!(parsed.columns(), 2);
        assert_eq!(
            parsed.alignments(),
            &[
                crate::MarkdownTableAlignment::Left,
                crate::MarkdownTableAlignment::Right,
            ],
            "删中间列后，左右两列的对齐（左对齐/右对齐）保留"
        );
    }

    #[test]
    fn delete_first_and_last_columns_keep_the_remaining_bytes() {
        let source = "| a | b | c |\n|---|:-:|--:|\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |\n";

        let table = parse_table(source);
        let first = delete_column_deletions(source, &table, 0);
        assert_eq!(
            apply(source, &first),
            "| b | c |\n|:-:|--:|\n| 2 | 3 |\n| 5 | 6 |\n",
            "删除首列：保留行首 `|` 与其余 cell 的空白"
        );
        let parsed = parse_table(&apply(source, &first));
        assert_eq!(
            parsed.alignments(),
            &[
                crate::MarkdownTableAlignment::Center,
                crate::MarkdownTableAlignment::Right,
            ]
        );

        let table = parse_table(source);
        let last = delete_column_deletions(source, &table, 2);
        assert_eq!(
            apply(source, &last),
            "| a | b |\n|---|:-:|\n| 1 | 2 |\n| 4 | 5 |\n",
            "删除末列：保留收尾 `|`，不留下多余空白"
        );
        assert_eq!(parse_table(&apply(source, &last)).columns(), 2);
    }

    #[test]
    fn delete_column_works_without_trailing_pipes_and_keeps_alignment() {
        let source = "a | b | c\n---|:-:|--:\n1 | 2 | 3\n";
        let table = parse_table(source);
        let middle = delete_column_deletions(source, &table, 1);
        assert_eq!(apply(source, &middle), "a | c\n---|--:\n1 | 3\n");
        assert_eq!(parse_table(&apply(source, &middle)).columns(), 2);

        let table = parse_table(source);
        let last = delete_column_deletions(source, &table, 2);
        assert_eq!(apply(source, &last), "a | b\n---|:-:\n1 | 2\n");
        assert_eq!(parse_table(&apply(source, &last)).columns(), 2);

        let table = parse_table(source);
        let first = delete_column_deletions(source, &table, 0);
        assert_eq!(apply(source, &first), "b | c\n:-:|--:\n2 | 3\n");
        assert_eq!(parse_table(&apply(source, &first)).columns(), 2);
    }

    #[test]
    fn delete_column_works_for_tables_without_data_rows() {
        let source = "| a | b | c |\n|---|---|---|\n\n后\n";
        let table = parse_table(source);
        assert_eq!(table.row_count(), 0);
        let deletions = delete_column_deletions(source, &table, 1);
        assert_eq!(deletions.len(), 2, "只有表头 + 分隔行两条删除");
        assert_eq!(apply(source, &deletions), "| a | c |\n|---|---|\n\n后\n");
        assert_eq!(parse_table(&apply(source, &deletions)).columns(), 2);
    }

    #[test]
    fn deleting_the_last_remaining_column_is_rejected() {
        let source = "| a |\n|---|\n| 1 |\n";
        let table = parse_table(source);
        assert_eq!(table.columns(), 1);
        assert!(
            delete_column_deletions(source, &table, 0).is_empty(),
            "至少保留 1 列：删最后一列会把表格改成非法结构"
        );
        assert!(
            delete_column_deletions(source, &table, 1).is_empty(),
            "越界列"
        );
    }

    #[test]
    fn column_deletion_skips_rows_that_are_missing_that_column() {
        // 数据行只有 1 个真实 cell（GFM 会补空列）：删中间列不能凭空删到别的字节。
        let source = "| a | b | c |\n|---|---|---|\n| 1 |\n";
        let table = parse_table(source);
        let deletions = delete_column_deletions(source, &table, 1);
        assert_eq!(deletions.len(), 2, "表头 + 分隔行两条；缺列的数据行跳过");
        let after = apply(source, &deletions);
        assert_eq!(after, "| a | c |\n|---|---|\n| 1 |\n");
        let parsed = parse_table(&after);
        assert_eq!(parsed.columns(), 2);
        assert_eq!(
            parsed.rows()[0]
                .cells()
                .iter()
                .map(|cell| after[cell.start().get()..cell.end().get()].to_owned())
                .collect::<Vec<_>>(),
            vec!["1".to_owned(), String::new()],
            "缺列的旧行在删列后按新列数补空 cell（源码不重写）"
        );
    }

    #[test]
    fn column_deletion_covers_header_delimiter_and_every_data_row() {
        // 表头 + 分隔行 + 1 条数据行 = 3 条删除（每行一条，缺列的行跳过）。
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let table = parse_table(source);
        assert_eq!(delete_column_deletions(source, &table, 0).len(), 3);
        assert_eq!(
            apply(source, &delete_column_deletions(source, &table, 1)),
            "| a |\n|---|\n| 1 |\n"
        );
    }

    #[test]
    fn new_table_text_is_two_empty_columns_with_one_data_row() {
        assert_eq!(new_table_text("\n"), "|  |  |\n| --- | --- |\n|  |  |\n");
        assert_eq!(
            new_table_text("\r\n"),
            "|  |  |\r\n| --- | --- |\r\n|  |  |\r\n"
        );
        let parsed = parse_table(&new_table_text("\n"));
        assert_eq!(parsed.columns(), 2);
        assert_eq!(parsed.row_count(), 1);
        assert_eq!(
            parsed.alignments(),
            &[
                crate::MarkdownTableAlignment::None,
                crate::MarkdownTableAlignment::None,
            ],
            "默认表格两列都是默认对齐"
        );
    }

    #[test]
    fn insert_table_into_empty_document_is_exactly_the_default_table() {
        assert_eq!(
            new_table_insertion("", 0, 0).expect("空文档可插入"),
            new_table_text("\n")
        );
    }

    /// 把插入文本替换进 `[start, end)`（字节；调用方保证是 scalar 边界）。
    fn insert_text(source: &str, start: usize, end: usize) -> String {
        let insertion = new_table_insertion(source, start, end).expect("合法偏移");
        let mut text = source.to_owned();
        text.replace_range(start..end, &insertion);
        text
    }

    #[test]
    fn insert_table_at_paragraph_end_adds_the_needed_blank_lines() {
        // 段落末尾、文档没有末尾换行：先结束段落，再留一个空行；插入文本只含这些分隔。
        assert_eq!(
            new_table_insertion("abc", 3, 3).expect("合法偏移"),
            format!("\n\n{}", new_table_text("\n"))
        );
        assert_eq!(
            insert_text("abc", 3, 3),
            format!("abc\n\n{}", new_table_text("\n")),
            "正文其它字节不变（只在 caret 之后补空行与表格）"
        );
        // 段落末尾、后面原本就有一个换行（它成为表格后的空行）：插入文本不再补尾。
        assert_eq!(
            new_table_insertion("abc\n", 3, 3).expect("合法偏移"),
            format!("\n\n{}", new_table_text("\n"))
        );
        assert_eq!(
            insert_text("abc\n", 3, 3),
            format!("abc\n\n{}\n", new_table_text("\n"))
        );
        // 插入点在行首：前一行与表格之间补一个空行，后续原文留在表格后的空行之后。
        assert_eq!(
            new_table_insertion("abc\nnext", 4, 4).expect("合法偏移"),
            format!("\n{}\n", new_table_text("\n"))
        );
        assert_eq!(
            insert_text("abc\nnext", 4, 4),
            format!("abc\n\n{}\nnext", new_table_text("\n")),
            "插入点之后的原文逐字保留"
        );
        // 插入点已经紧跟空行：前面不再补。
        assert_eq!(
            new_table_insertion("abc\n\nnext", 5, 5).expect("合法偏移"),
            format!("{}\n", new_table_text("\n"))
        );
        assert_eq!(
            insert_text("abc\n\nnext", 5, 5),
            format!("abc\n\n{}\nnext", new_table_text("\n"))
        );
        // 插出来的表确实是整块 Object 表格（表头 + 分隔行 + 1 条数据行）。
        let parsed = parse_table(&insert_text("abc", 3, 3));
        assert_eq!(parsed.columns(), 2);
        assert_eq!(parsed.row_count(), 1);
        assert!(parsed.is_block(), "插入的表格必须可投影成 Object Block");
    }

    #[test]
    fn insert_table_mid_line_splits_the_line_and_ends_with_a_blank_line() {
        assert_eq!(
            new_table_insertion("abcdef", 3, 3).expect("合法偏移"),
            format!("\n\n{}\n", new_table_text("\n"))
        );
        assert_eq!(
            insert_text("abcdef", 3, 3),
            format!("abc\n\n{}\ndef", new_table_text("\n")),
            "行中间插入：结束当前行 + 空行 + 表格 + 空行 + 原后半行"
        );
        // 选区（替换 `cd`）：前缀/后缀各自独立判断，前后仍然独占行。
        assert_eq!(
            new_table_insertion("abcdef", 2, 4).expect("合法偏移"),
            format!("\n\n{}\n", new_table_text("\n"))
        );
        assert_eq!(
            insert_text("abcdef", 2, 4),
            format!("ab\n\n{}\nef", new_table_text("\n"))
        );
    }

    #[test]
    fn insert_table_keeps_crlf_line_endings() {
        assert_eq!(
            new_table_insertion("abc\r\ndef", 5, 5).expect("合法偏移"),
            format!("\r\n{}\r\n", new_table_text("\r\n"))
        );
        assert_eq!(
            insert_text("abc\r\ndef", 5, 5),
            format!("abc\r\n\r\n{}\r\ndef", new_table_text("\r\n")),
            "CRLF 文档插入的表格与空行都用 CRLF"
        );
        // 插入点在 CRLF 行尾之前：前面补两行尾，后面的 `\r\n` 就是表格后的空行。
        assert_eq!(
            new_table_insertion("abc\r\n", 3, 3).expect("合法偏移"),
            format!("\r\n\r\n{}", new_table_text("\r\n"))
        );
        assert_eq!(
            insert_text("abc\r\n", 3, 3),
            format!("abc\r\n\r\n{}\r\n", new_table_text("\r\n"))
        );
    }

    #[test]
    fn insert_table_rejects_invalid_offsets() {
        assert!(matches!(
            new_table_insertion("中", 1, 1),
            Err(CoordError::NotScalarBoundary { offset: 1, len: 3 })
        ));
        assert!(matches!(
            new_table_insertion("ab", 3, 3),
            Err(CoordError::ByteOutOfBounds { offset: 3, len: 2 })
        ));
        assert!(matches!(
            new_table_insertion("ab", 2, 1),
            Err(CoordError::ReversedRange { start: 2, end: 1 })
        ));
    }

    // ---- ADR 0019 S3b5：拖拽重排行 / 列 ----

    /// 按区间升序把替换应用到源码（控制器就是把它们变成 `ScalarEdit`）。
    fn apply_replacements(source: &str, replacements: &[TableReplacement]) -> String {
        let mut text = source.to_owned();
        for replacement in replacements.iter().rev() {
            let start = replacement.span().start().get();
            let end = replacement.span().end().get();
            text.replace_range(start..end, replacement.text());
        }
        text
    }

    /// 一次重排的替换必须**互不重叠、按位置升序、每条等长**（等长 = 纯重排）。
    fn assert_well_formed(source: &str, replacements: &[TableReplacement]) {
        assert!(
            replacements
                .windows(2)
                .all(|pair| pair[0].span().end().get() <= pair[1].span().start().get()),
            "替换必须按位置升序且互不重叠：{replacements:?}"
        );
        for replacement in replacements {
            let start = replacement.span().start().get();
            let end = replacement.span().end().get();
            assert_eq!(
                replacement.text().len(),
                end - start,
                "重排必须是等长替换（纯顺序变化）：{replacement:?}"
            );
            assert!(
                source.get(start..end).is_some(),
                "替换区间必须落在源码里：{replacement:?}"
            );
        }
    }

    #[test]
    fn move_row_reorders_whole_lines_byte_exactly() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n";
        let table = parse_table(source);
        let replacements = move_row_replacements(source, &table, 0, 1);
        assert_eq!(replacements.len(), 1, "行重排是单条整段替换");
        assert_well_formed(source, &replacements);
        assert_eq!(
            apply_replacements(source, &replacements),
            "| a | b |\n|---|---|\n| 3 | 4 |\n| 1 | 2 |\n",
            "整行（含行尾换行）换位，表头/分隔行逐字节不变"
        );
        // 表格区间与总长度都不变（caret 的 scalar 坐标因此天然保持）。
        let after = apply_replacements(source, &replacements);
        assert_eq!(after.len(), source.len(), "重排不改表格总字节数");
        assert_eq!(
            parse_table(&after).span(),
            table.span(),
            "重排不改表格源码区间"
        );
    }

    #[test]
    fn move_row_covers_first_middle_and_last_rows_with_surrounding_text() {
        let source = "前\n\n| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n| 5 | 6 |\n\n后\n";
        let table = parse_table(source);

        // 第 1 条 → 最后一条。
        let first_to_last = move_row_replacements(source, &table, 0, 2);
        assert_eq!(
            apply_replacements(source, &first_to_last),
            "前\n\n| a | b |\n|---|---|\n| 3 | 4 |\n| 5 | 6 |\n| 1 | 2 |\n\n后\n",
            "首行移到末尾：表头/分隔行/其余行与前后正文逐字节不变"
        );
        // 最后一条 → 第一条。
        let last_to_first = move_row_replacements(source, &table, 2, 0);
        assert_eq!(
            apply_replacements(source, &last_to_first),
            "前\n\n| a | b |\n|---|---|\n| 5 | 6 |\n| 1 | 2 |\n| 3 | 4 |\n\n后\n"
        );
        // 中间 → 第一条。
        let middle_to_first = move_row_replacements(source, &table, 1, 0);
        assert_eq!(
            apply_replacements(source, &middle_to_first),
            "前\n\n| a | b |\n|---|---|\n| 3 | 4 |\n| 1 | 2 |\n| 5 | 6 |\n\n后\n"
        );
        // 全部替换都在表格内部（表格之外的字节一个不碰）。
        for replacements in [&first_to_last, &last_to_first, &middle_to_first] {
            assert_well_formed(source, replacements);
            for replacement in replacements {
                assert!(
                    replacement.span().start().get() >= table.span().start().get()
                        && replacement.span().end().get() <= table.span().end().get(),
                    "重排区间必须在表格内部：{replacement:?}"
                );
            }
        }
    }

    #[test]
    fn move_row_never_touches_the_header_or_delimiter() {
        let source = "| a | b |\n|:--|--:|\n| 1 | 2 |\n| 3 | 4 |\n";
        let table = parse_table(source);
        let replacements = move_row_replacements(source, &table, 1, 0);
        let after = apply_replacements(source, &replacements);
        assert!(
            after.starts_with("| a | b |\n|:--|--:|\n"),
            "表头与分隔行（含对齐冒号）恒不变：{after:?}"
        );
        let parsed = parse_table(&after);
        assert_eq!(
            parsed.alignments(),
            &[
                crate::MarkdownTableAlignment::Left,
                crate::MarkdownTableAlignment::Right,
            ]
        );
    }

    #[test]
    fn move_column_moves_cell_text_and_alignment_with_the_column() {
        let source =
            "| 名称 | 数量 | 备注 |\n|:--|:-:|--:|\n| 苹果 | 3 | 红 |\n| 香蕉 | 5 | 黄 |\n";
        let table = parse_table(source);

        // 中间列 → 首列：内容与对齐标记一起左移，槽位的 `|` 与空白不动。
        let middle_to_first = move_column_replacements(source, &table, 1, 0);
        assert_eq!(middle_to_first.len(), 4, "表头 + 分隔行 + 2 条数据行各一条");
        assert_well_formed(source, &middle_to_first);
        let after = apply_replacements(source, &middle_to_first);
        assert_eq!(
            after, "| 数量 | 名称 | 备注 |\n|:-:|:--|--:|\n| 3 | 苹果 | 红 |\n| 5 | 香蕉 | 黄 |\n",
            "cell 文本与分隔行对齐标记一起移动，其它字节逐字保留"
        );
        let parsed = parse_table(&after);
        assert_eq!(
            parsed.alignments(),
            &[
                crate::MarkdownTableAlignment::Center,
                crate::MarkdownTableAlignment::Left,
                crate::MarkdownTableAlignment::Right,
            ],
            "对齐跟着列走"
        );

        // 首列 → 末列。
        let first_to_last = move_column_replacements(source, &table, 0, 2);
        assert_eq!(
            apply_replacements(source, &first_to_last),
            "| 数量 | 备注 | 名称 |\n|:-:|--:|:--|\n| 3 | 红 | 苹果 |\n| 5 | 黄 | 香蕉 |\n"
        );

        // 末列 → 首列。
        let last_to_first = move_column_replacements(source, &table, 2, 0);
        assert_eq!(
            apply_replacements(source, &last_to_first),
            "| 备注 | 名称 | 数量 |\n|--:|:--|:-:|\n| 红 | 苹果 | 3 |\n| 黄 | 香蕉 | 5 |\n"
        );
    }

    #[test]
    fn move_row_and_column_handle_crlf_tables() {
        let source = "| a | b |\r\n|---|---|\r\n| 1 | 2 |\r\n| 3 | 4 |\r\n";
        let table = parse_table(source);
        let rows = move_row_replacements(source, &table, 0, 1);
        assert_eq!(
            apply_replacements(source, &rows),
            "| a | b |\r\n|---|---|\r\n| 3 | 4 |\r\n| 1 | 2 |\r\n",
            "CRLF 重排行：行尾 \\r\\n 留在槽位，不混用 LF"
        );
        let columns = move_column_replacements(source, &table, 0, 1);
        assert_eq!(
            apply_replacements(source, &columns),
            "| b | a |\r\n|---|---|\r\n| 2 | 1 |\r\n| 4 | 3 |\r\n",
            "CRLF 重排列：行尾 \\r\\n 逐字节保留"
        );
        assert!(
            !apply_replacements(source, &rows)
                .replace("\r\n", "")
                .contains('\n'),
            "CRLF 文档里不得出现孤立 LF"
        );
    }

    #[test]
    fn move_row_at_document_end_does_not_add_a_line_ending() {
        // 表格就是文档末尾、最后一行没有行尾换行：重排不能凭空补一个换行。
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
        let table = parse_table(source);
        let after = apply_replacements(source, &move_row_replacements(source, &table, 0, 1));
        assert_eq!(
            after, "| a | b |\n|---|---|\n| 3 | 4 |\n| 1 | 2 |",
            "末行没有换行时，换到末尾的行也不带换行（行尾换行属于槽位）"
        );
        assert_eq!(after.len(), source.len());
        assert_eq!(parse_table(&after).row_count(), 2);
    }

    #[test]
    fn move_row_noops_are_empty() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n";
        let table = parse_table(source);
        assert!(
            move_row_replacements(source, &table, 1, 1).is_empty(),
            "原地"
        );
        assert!(
            move_row_replacements(source, &table, 0, 2).is_empty(),
            "越界行"
        );
        assert!(
            move_row_replacements(source, &table, 2, 0).is_empty(),
            "越界行"
        );
        let single = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let single_table = parse_table(single);
        assert!(
            move_row_replacements(single, &single_table, 0, 0).is_empty(),
            "只有一条数据行时没有可换的位置"
        );
    }

    #[test]
    fn move_column_noops_are_empty() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let table = parse_table(source);
        assert!(
            move_column_replacements(source, &table, 0, 0).is_empty(),
            "原地"
        );
        assert!(
            move_column_replacements(source, &table, 0, 2).is_empty(),
            "越界列"
        );
        let single = "| a |\n|---|\n| 1 |\n";
        let single_table = parse_table(single);
        assert!(
            move_column_replacements(single, &single_table, 0, 0).is_empty(),
            "只有一列时没有可换的位置"
        );
    }

    #[test]
    fn move_column_refuses_rows_with_missing_cells() {
        // 数据行只有 1 个真实 cell（GFM 把缺的列补在行尾）：重排会把隐式空列搬到错位置，
        // 因此整次放弃。
        let source = "| a | b | c |\n|---|---|---|\n| 1 |\n";
        let table = parse_table(source);
        assert!(
            move_column_replacements(source, &table, 0, 1).is_empty(),
            "缺列的行不猜：整次移动放弃"
        );
        assert!(
            move_column_replacements(source, &table, 1, 2).is_empty(),
            "缺列的行不猜（任意列）"
        );
    }

    #[test]
    fn move_column_leaves_cells_beyond_the_column_count_untouched() {
        // 2 列表格里多写了一个 cell（GFM 忽略它）：只重排前 2 列，多余字节原样保留。
        let source = "| a | b |\n|---|---|\n| 1 | 2 | 3 |\n";
        let table = parse_table(source);
        assert_eq!(table.columns(), 2);
        let after = apply_replacements(source, &move_column_replacements(source, &table, 0, 1));
        assert_eq!(
            after, "| b | a |\n|---|---|\n| 2 | 1 | 3 |\n",
            "多出来的第 3 个 cell 不参与重排"
        );
    }

    #[test]
    fn move_column_works_for_tables_without_data_rows() {
        let source = "| a | b | c |\n|:--|:-:|--:|\n\n后\n";
        let table = parse_table(source);
        let replacements = move_column_replacements(source, &table, 2, 0);
        assert_eq!(replacements.len(), 2, "只有表头 + 分隔行两条替换");
        assert_eq!(
            apply_replacements(source, &replacements),
            "| c | a | b |\n|--:|:--|:-:|\n\n后\n"
        );
    }

    #[test]
    fn drop_target_maps_slots_to_final_indices() {
        // 3 项：缝隙 0..=3。
        assert_eq!(drop_target(0, 0, 3), None, "放回原位（前）");
        assert_eq!(drop_target(0, 1, 3), None, "放回原位（后）");
        assert_eq!(drop_target(0, 2, 3), Some(1), "首项拖到中间");
        assert_eq!(drop_target(0, 3, 3), Some(2), "首项拖到末尾");
        assert_eq!(drop_target(2, 0, 3), Some(0), "末项拖到最前");
        assert_eq!(drop_target(2, 1, 3), Some(1));
        assert_eq!(drop_target(2, 2, 3), None, "放回原位（前）");
        assert_eq!(drop_target(2, 3, 3), None, "放回原位（后）");
        assert_eq!(drop_target(1, 3, 3), Some(2));
        assert_eq!(drop_target(1, 0, 3), Some(0));
        // 越界 / 少于 2 项。
        assert_eq!(drop_target(0, 4, 3), None);
        assert_eq!(drop_target(3, 0, 3), None);
        assert_eq!(drop_target(0, 1, 1), None);
        assert_eq!(drop_target(0, 2, 1), None);
    }
}
