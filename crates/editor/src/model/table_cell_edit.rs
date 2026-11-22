//! ADR 0019 S3b2：表格**单元格就地编辑**的纯函数（源码转义 / 单元格导航）。
//!
//! 本模块只回答两类无状态问题，不接触 Rope / 会话 / 投影 / UI：
//!
//! 1. **用户输入怎么变成 cell 源码**：GFM 规定 cell 内的字面 `|` 必须写成 `\|`，
//!    换行会直接把表格行切断。因此凡是进入 cell 的替换文本都要先经
//!    [`sanitize_cell_text`]：换行 → 空格、未转义的 `|` → `\|`；
//! 2. **Tab / Shift+Tab 去哪一格**：行末回卷到下一行行首（row 0 = 表头，
//!    1..=n = 数据行），表格最后一格之后返回 `None`（调用方退出表格）。
//!
//! 真正的正文写入由 [`crate::model::controller::EditorController`] 收口：
//! 一次单元格编辑在**提交**时只产生一条 `ScalarEdit`（一个 `EditRequest` =
//! 一个撤销组），Esc 取消不产生任何请求。

/// 表格单元格的行下标：`0` = 表头，`1..=n` = 第 `row - 1` 条数据行。
///
/// 与 [`menghunote_markdown::TableBlock`] 的 `rows` 下标相差 1（解析器的 `rows`
/// **不含**表头）：这里单独包一层是为了让 UI / 控制器都用同一套“人话行号”寻址，
/// 避免 app 自己 `row + 1` 或 `row - 1` 造成错位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableCellAddress {
    /// 行下标（0 = 表头）。
    pub row: usize,
    /// 列下标（0 起）。
    pub column: usize,
}

impl TableCellAddress {
    /// 构造一个单元格地址。
    #[must_use]
    pub const fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }

    /// 是否是表头行。
    #[must_use]
    pub const fn is_header(self) -> bool {
        self.row == 0
    }

    /// 对应的**数据行**下标（表头返回 `None`）。
    #[must_use]
    pub const fn data_row(self) -> Option<usize> {
        if self.row == 0 {
            None
        } else {
            Some(self.row - 1)
        }
    }
}

/// 把一段文本里**未转义**的 `|` 写成 `\|`（GFM cell 边界保护）。
///
/// 判定规则与 CommonMark 一致：前面紧邻的连续反斜杠数为**奇数**时，该 `|` 已经被
/// 转义过，保持原样；偶数（含 0）时补一个 `\`。这样用户粘进来的 `a\|b` 不会被写成
/// `a\\|b`（那会让 `\` 反被转义掉，反而把 cell 断开）。
#[must_use]
pub fn escape_cell_pipes(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    let mut backslashes = 0usize;
    for ch in text.chars() {
        if ch == '|' {
            if backslashes.is_multiple_of(2) {
                escaped.push('\\');
            }
            escaped.push('|');
            backslashes = 0;
        } else {
            if ch == '\\' {
                backslashes += 1;
            } else {
                backslashes = 0;
            }
            escaped.push(ch);
        }
    }
    escaped
}

/// 把用户输入整理成可以放进单元格的**单行**源码文本。
///
/// - `\r\n` / `\n` / `\r` → 一个空格（cell 内不能有行尾，否则表格行被切断）；
/// - 未转义的 `|` → `\|`（见 [`escape_cell_pipes`]）；
/// - 其它字节（含 `**` / 反引号 / 链接标记）**逐字保留**：首版单元格不做富文本，
///   源码是唯一真源，用户没有改写就不应该被解析器重写。
#[must_use]
pub fn sanitize_cell_text(text: &str) -> String {
    let mut single_line = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                single_line.push(' ');
            }
            '\n' => single_line.push(' '),
            _ => single_line.push(ch),
        }
    }
    escape_cell_pipes(&single_line)
}

/// Tab：下一格（行末回卷到下一行行首）。
///
/// `rows` 是**数据行数**（不含表头）；总行数 = `rows + 1`。最后一格之后返回 `None`
/// （调用方退出表格，不再进入新格）。
#[must_use]
pub fn next_cell(
    row: usize,
    column: usize,
    rows: usize,
    columns: usize,
) -> Option<TableCellAddress> {
    if columns == 0 || row > rows || column >= columns {
        return None;
    }
    if column + 1 < columns {
        return Some(TableCellAddress::new(row, column + 1));
    }
    if row < rows {
        return Some(TableCellAddress::new(row + 1, 0));
    }
    None
}

/// Shift+Tab：上一格（行首回卷到上一行末格）；表头第一格之前返回 `None`。
#[must_use]
pub fn previous_cell(
    row: usize,
    column: usize,
    rows: usize,
    columns: usize,
) -> Option<TableCellAddress> {
    if columns == 0 || row > rows || column >= columns {
        return None;
    }
    if column > 0 {
        return Some(TableCellAddress::new(row, column - 1));
    }
    if row > 0 {
        return Some(TableCellAddress::new(row - 1, columns - 1));
    }
    None
}

/// 删除第 `row` 行（0 = 表头）之后，原本选中的 `(row, column)` 应落到哪一格。
///
/// 契约（写死，测试钉住）：
/// - 同列**上一行**优先（删中间/末尾数据行时视觉位置不动）；删的是第一条数据行、
///   上面只剩表头时退到**下一行**（同列，原第 2 条数据行上移成第 1 条）；
/// - 删完一条数据行都不剩、或 `row` 越界 / 表头行（首版禁止删表头）→ `None`
///   （调用方清空单元格选中态，不猜一个不存在的格子）；
/// - `rows_after` 是**删除后**的数据行数（不含表头）；列数不变，`column` 越界时收敛到
///   最后一列（防御陈旧坐标，不返回不存在的列）。
#[must_use]
pub fn cell_after_row_deletion(
    row: usize,
    column: usize,
    rows_after: usize,
    columns: usize,
) -> Option<TableCellAddress> {
    if row == 0 || columns == 0 || row > rows_after + 1 {
        return None;
    }
    let current = if row > 1 {
        row - 1
    } else if rows_after >= 1 {
        1
    } else {
        return None;
    };
    if current > rows_after {
        return None;
    }
    Some(TableCellAddress::new(current, column.min(columns - 1)))
}

/// 删除第 `column` 列之后，原本选中的 `(row, column)` 应落到哪一格。
///
/// 契约（写死，测试钉住）：
/// - 同行的**右侧格**优先（原第 `column + 1` 列左移补位，视觉位置不动）；删的是最后一列
///   时退到**左侧格**；
/// - `columns_after == 0`（不允许出现）或 `row` 越界 / `column` 越界 → `None`；
/// - `rows` 是数据行数（不含表头），删列不改行数。
#[must_use]
pub fn cell_after_column_deletion(
    row: usize,
    column: usize,
    rows: usize,
    columns_after: usize,
) -> Option<TableCellAddress> {
    if columns_after == 0 || row > rows || column > columns_after {
        return None;
    }
    Some(TableCellAddress::new(row, column.min(columns_after - 1)))
}

/// 重排第 `from` 条数据行到第 `to` 条（0 起、不含表头）之后，原本选中的
/// `(row, column)` 应落到哪一格。
///
/// 契约（写死，测试钉住）：
///
/// - **选中的内容跟着走**：选中的是 `from` 这一行 → 落到 `to`；原来夹在
///   `from` 与 `to` 之间的行整体让位一格（往后拖时上游行前移、往前拖时下游行后移）；
/// - 表头行（`row == 0`）不参与重排 → 原样留在表头；
/// - `rows` 是**数据行数**（不含表头），`columns` 是列数；表头之外的越界行 / 0 列 →
///   `None`（调用方清空选中态，不猜一个不存在的格子）；
/// - `column` 越界时收敛到最后一列（防御陈旧坐标，不返回不存在的列）。
#[must_use]
pub fn cell_after_row_move(
    row: usize,
    column: usize,
    from: usize,
    to: usize,
    rows: usize,
    columns: usize,
) -> Option<TableCellAddress> {
    if columns == 0 || row > rows || from >= rows || to >= rows {
        return None;
    }
    let column = column.min(columns - 1);
    if row == 0 {
        return Some(TableCellAddress::new(0, column));
    }
    let current = row - 1;
    let next = if current == from {
        to
    } else if from < current && current <= to {
        current - 1
    } else if to <= current && current < from {
        current + 1
    } else {
        current
    };
    if next >= rows {
        return None;
    }
    Some(TableCellAddress::new(next + 1, column))
}

/// 重排第 `from` 列到第 `to` 列之后，原本选中的 `(row, column)` 应落到哪一格。
///
/// 契约与 [`cell_after_row_move`] 同构（选中的内容跟着走、让位的列整体挪一格）：
///
/// - `columns` 是**当前**列数（重排不改列数）；0 列 / 越界行 / 越界列 → `None`；
/// - 行号不变（重排只改列顺序）。
#[must_use]
pub fn cell_after_column_move(
    row: usize,
    column: usize,
    from: usize,
    to: usize,
    rows: usize,
    columns: usize,
) -> Option<TableCellAddress> {
    if columns == 0 || row > rows || column >= columns || from >= columns || to >= columns {
        return None;
    }
    let next = if column == from {
        to
    } else if from < column && column <= to {
        column - 1
    } else if to <= column && column < from {
        column + 1
    } else {
        column
    };
    Some(TableCellAddress::new(row, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_pipe_is_escaped_but_existing_escape_is_kept() {
        assert_eq!(escape_cell_pipes("a|b"), "a\\|b");
        assert_eq!(escape_cell_pipes("a\\|b"), "a\\|b");
        assert_eq!(escape_cell_pipes("a\\\\|b"), "a\\\\\\|b");
        assert_eq!(escape_cell_pipes("|"), "\\|");
        assert_eq!(escape_cell_pipes(""), "");
        assert_eq!(escape_cell_pipes("**x** `y`"), "**x** `y`");
        // 反斜杠在 pipe 之后重新计数（不与前一个 pipe 的转义混在一起）。
        assert_eq!(escape_cell_pipes("|\\|"), "\\|\\|");
    }

    #[test]
    fn sanitize_strips_line_breaks_and_escapes_pipes() {
        assert_eq!(sanitize_cell_text("a\nb"), "a b");
        assert_eq!(sanitize_cell_text("a\r\nb"), "a b");
        assert_eq!(sanitize_cell_text("a\rb"), "a b");
        assert_eq!(sanitize_cell_text("x|y"), "x\\|y");
        assert_eq!(sanitize_cell_text("**粗**"), "**粗**");
        assert_eq!(sanitize_cell_text(""), "");
    }

    #[test]
    fn tab_walks_columns_then_rows_and_stops_at_the_end() {
        // 2 列 / 1 条数据行（总 2 行）。
        assert_eq!(next_cell(0, 0, 1, 2), Some(TableCellAddress::new(0, 1)));
        assert_eq!(next_cell(0, 1, 1, 2), Some(TableCellAddress::new(1, 0)));
        assert_eq!(next_cell(1, 0, 1, 2), Some(TableCellAddress::new(1, 1)));
        assert_eq!(next_cell(1, 1, 1, 2), None, "最后一格之后退出表格");
        assert_eq!(next_cell(2, 0, 1, 2), None, "越界行不再回卷");
        assert_eq!(next_cell(0, 0, 0, 0), None, "0 列是异常输入：不猜");
    }

    #[test]
    fn shift_tab_walks_backwards_and_stops_at_the_header() {
        assert_eq!(previous_cell(0, 1, 1, 2), Some(TableCellAddress::new(0, 0)));
        assert_eq!(previous_cell(1, 0, 1, 2), Some(TableCellAddress::new(0, 1)));
        assert_eq!(previous_cell(1, 1, 1, 2), Some(TableCellAddress::new(1, 0)));
        assert_eq!(previous_cell(0, 0, 1, 2), None);
        assert_eq!(previous_cell(0, 0, 0, 0), None);
    }

    #[test]
    fn address_maps_header_and_data_rows() {
        assert!(TableCellAddress::new(0, 0).is_header());
        assert_eq!(TableCellAddress::new(0, 0).data_row(), None);
        assert_eq!(TableCellAddress::new(1, 2).data_row(), Some(0));
        assert!(!TableCellAddress::new(1, 2).is_header());
    }

    #[test]
    fn cell_after_row_deletion_prefers_the_row_above_then_below() {
        // 3 条数据行（总 4 行），删第 2 条数据行 → 上一行同列。
        assert_eq!(
            cell_after_row_deletion(2, 1, 2, 3),
            Some(TableCellAddress::new(1, 1))
        );
        // 删最后一条数据行 → 上一行同列。
        assert_eq!(
            cell_after_row_deletion(3, 2, 2, 3),
            Some(TableCellAddress::new(2, 2))
        );
        // 删第一条数据行 → 下面那条上移，仍选同列（原第 2 条）。
        assert_eq!(
            cell_after_row_deletion(1, 0, 2, 3),
            Some(TableCellAddress::new(1, 0))
        );
        // 只有一条数据行：删完没有数据行可落 → 清空选中。
        assert_eq!(cell_after_row_deletion(1, 0, 0, 3), None);
        // 表头（首版不删）/ 越界行 / 0 列 → 不猜。
        assert_eq!(cell_after_row_deletion(0, 0, 3, 3), None);
        assert_eq!(cell_after_row_deletion(4, 0, 2, 3), None);
        assert_eq!(cell_after_row_deletion(1, 0, 2, 0), None);
        // 陈旧列号收敛到最后一列（不返回不存在的列）。
        assert_eq!(
            cell_after_row_deletion(2, 9, 2, 3),
            Some(TableCellAddress::new(1, 2))
        );
    }

    #[test]
    fn cell_after_column_deletion_prefers_the_right_then_the_left() {
        // 3 列删中间列：原右邻左移补位，仍选同一列号。
        assert_eq!(
            cell_after_column_deletion(1, 1, 2, 2),
            Some(TableCellAddress::new(1, 1))
        );
        // 删首列：原第 2 列左移成第 0 列。
        assert_eq!(
            cell_after_column_deletion(0, 0, 2, 2),
            Some(TableCellAddress::new(0, 0))
        );
        // 删末列：退到左侧列。
        assert_eq!(
            cell_after_column_deletion(2, 2, 2, 2),
            Some(TableCellAddress::new(2, 1))
        );
        // 至少保留 1 列：0 列是异常输入 → 不猜。
        assert_eq!(cell_after_column_deletion(1, 0, 2, 0), None);
        // 越界行 / 越界列 → 不猜。
        assert_eq!(cell_after_column_deletion(3, 0, 2, 2), None);
        assert_eq!(cell_after_column_deletion(1, 3, 2, 2), None);
    }

    #[test]
    fn cell_after_row_move_follows_the_moved_content() {
        // 3 条数据行（总 4 行）、3 列：首行拖到末尾。
        // 选中的是首行 → 跟着到末尾（数据行下标 2 → 人话行号 3）。
        assert_eq!(
            cell_after_row_move(1, 1, 0, 2, 3, 3),
            Some(TableCellAddress::new(3, 1))
        );
        // 选中的是被让位的行（原第 2 行）→ 前移一行。
        assert_eq!(
            cell_after_row_move(2, 0, 0, 2, 3, 3),
            Some(TableCellAddress::new(1, 0))
        );
        // 末行拖到最前：被让位的行整体后移一格。
        assert_eq!(
            cell_after_row_move(1, 2, 2, 0, 3, 3),
            Some(TableCellAddress::new(2, 2))
        );
        // 选中的行就是被拖动的末行本身 → 落到最前。
        assert_eq!(
            cell_after_row_move(3, 0, 2, 0, 3, 3),
            Some(TableCellAddress::new(1, 0))
        );
        // 表头不参与行重排：原样留在表头（列号照样夹到有效范围）。
        assert_eq!(
            cell_after_row_move(0, 9, 1, 2, 3, 3),
            Some(TableCellAddress::new(0, 2))
        );
        // 越界行 / 0 列 → 不猜。
        assert_eq!(cell_after_row_move(4, 0, 0, 1, 3, 3), None);
        assert_eq!(cell_after_row_move(1, 0, 0, 3, 3, 3), None);
        assert_eq!(cell_after_row_move(1, 0, 0, 1, 3, 0), None);
    }

    #[test]
    fn cell_after_column_move_follows_the_moved_content() {
        // 3 列：首列拖到末列。
        assert_eq!(
            cell_after_column_move(1, 0, 0, 2, 2, 3),
            Some(TableCellAddress::new(1, 2))
        );
        // 被让位的列（原第 2 列）左移一格。
        assert_eq!(
            cell_after_column_move(1, 1, 0, 2, 2, 3),
            Some(TableCellAddress::new(1, 0))
        );
        // 末列拖到最前：被让位的列右移一格。
        assert_eq!(
            cell_after_column_move(0, 0, 2, 0, 2, 3),
            Some(TableCellAddress::new(0, 1))
        );
        // 不相干的行/列不受影响。
        assert_eq!(
            cell_after_column_move(2, 2, 0, 1, 3, 3),
            Some(TableCellAddress::new(2, 2))
        );
        // 越界 / 0 列 → 不猜。
        assert_eq!(cell_after_column_move(3, 0, 0, 1, 2, 3), None);
        assert_eq!(cell_after_column_move(1, 3, 0, 1, 2, 3), None);
        assert_eq!(cell_after_column_move(1, 0, 3, 0, 2, 3), None);
        assert_eq!(cell_after_column_move(1, 0, 0, 1, 2, 0), None);
        // 原地移动（from == to）不是错误：选中格留在原处。
        assert_eq!(
            cell_after_column_move(1, 0, 2, 2, 2, 3),
            Some(TableCellAddress::new(1, 0))
        );
    }
}
