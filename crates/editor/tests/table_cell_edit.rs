//! ADR 0019 S3b2：表格**单元格就地编辑**的纯模型契约。
//!
//! 覆盖：进入编辑后 display 只有该 cell 文本（没有 `|`、没有整表源码）、只改该 cell
//! 的源码区间（其余字节逐字不变）、个位 GFM 转义（`|` → `\|`、换行 → 空格）、
//! Enter 提交 = **一条** `ScalarEdit`、Esc 取消不产生请求、Tab 走下一格、
//! undo 组语义（一次提交只有一个请求）、加行/加列后的新 cell 可编辑、
//! 远端 delta / 结构操作在编辑期间的安全边界。
//!
//! 像素级几何与点击命中在 app 的 headless 测试里；这里只钉住模型契约。

mod support;

use menghunote_editor::{
    DisplayByte, EditorController, EditorError, MarkdownDocument, ObjectKind, Selection,
    SourceByte, SourceRange, UnicodeScalar, OBJECT_PLACEHOLDER,
};

/// 2 列 / 1 条数据行 + 两种对齐的表格（与 headless 预测一致）。
const TABLE_SOURCE: &str = "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果 | 3 |\n\n后文\n";
/// 表格表头行号（0 起）。
const TABLE_LINE: usize = 2;

fn open(source: &str) -> EditorController {
    EditorController::open(support::snapshot(source)).expect("测试快照必须可打开")
}

fn parse(source: &str) -> MarkdownDocument {
    MarkdownDocument::parse(source).expect("Markdown 解析不应失败")
}

/// 第 `row` 行（0 = 表头）第 `column` 列的 cell 源码区间。
fn cell(source: &str, row: usize, column: usize) -> SourceRange {
    let doc = parse(source);
    doc.tables()[0]
        .cell(row, column)
        .expect("测试表格必须有这一格")
}

fn cell_text(source: &str, row: usize, column: usize) -> &str {
    let span = cell(source, row, column);
    source
        .get(span.start().get()..span.end().get())
        .expect("cell 区间必须是合法切片")
}

/// 确认一条本地请求（`local_echo = true`：镜像已乐观写入，只推进 live_rev）。
fn confirm(controller: &mut EditorController, command_id: &str, live_rev: u64) {
    let outcome = support::applied(command_id, live_rev, true, support::splice(0, 0, ""));
    controller
        .apply_outcome(outcome)
        .expect("本地回声必须被接受");
}

#[test]
fn entering_a_cell_shows_only_that_cell_text_and_keeps_the_source() {
    let mut controller = open(TABLE_SOURCE);
    assert!(controller
        .begin_table_cell_edit(TABLE_LINE, 1, 0)
        .expect("进入单元格编辑不应失败"));

    // display 只剩该 cell 的文本：没有 `|`、没有分隔行、没有其它 cell。
    let display = controller.display_text().to_string();
    assert_eq!(display, "表格\n\n苹果\n\n后文\n", "只显示 cell 文本");
    assert!(
        !display.contains('|'),
        "display 里不得出现表格管道：{display:?}"
    );
    assert!(
        !display.contains(OBJECT_PLACEHOLDER),
        "编辑中的表格不再是原子对象：{display:?}"
    );

    // 源码一个字节都没动；进入编辑不产生任何请求。
    assert_eq!(controller.source_text(), TABLE_SOURCE);
    assert!(
        controller.pending_command().is_none(),
        "进入编辑不是一次编辑"
    );
    assert!(controller.is_editing_table_cell());
    assert_eq!(
        controller
            .table_cell_edit_target()
            .map(|t| (t.line, t.row, t.column)),
        Some((TABLE_LINE, 1, 0))
    );

    // caret 落在 cell 文本末尾。
    let end = cell(TABLE_SOURCE, 1, 0).end().get();
    let expected = SourceByte::new(end)
        .to_scalar(TABLE_SOURCE)
        .expect("scalar");
    assert_eq!(controller.selection(), Selection::caret(expected));
}

#[test]
fn typing_replaces_only_that_cell_and_commit_is_one_scalar_edit() {
    let mut controller = open(TABLE_SOURCE);
    controller
        .begin_table_cell_edit(TABLE_LINE, 1, 0)
        .expect("进入");
    let request = controller
        .compose_replace_selection("派")
        .expect("cell 草稿编辑不应失败");
    assert!(request.is_none(), "草稿编辑不产生 EditRequest");

    assert_eq!(
        controller.display_text(),
        "表格\n\n苹果派\n\n后文\n",
        "输入立刻可见（只显示该 cell）"
    );
    assert_eq!(
        controller.source_text(),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果派 | 3 |\n\n后文\n",
        "只改该 cell 的源码区间，两侧 `|`/对齐/其它 cell 逐字节不变"
    );

    let request = controller
        .commit_table_cell_edit()
        .expect("提交不应失败")
        .expect("改动过 → 必须产生请求");
    assert_eq!(request.edits.len(), 1, "一次提交只有一条 ScalarEdit");
    let edit = &request.edits[0];
    let span = cell(TABLE_SOURCE, 1, 0);
    assert_eq!(
        (
            edit.start,
            edit.end,
            edit.replacement.as_str(),
            SourceByte::new(span.start().get())
                .to_scalar(TABLE_SOURCE)
                .expect("start"),
            SourceByte::new(span.end().get())
                .to_scalar(TABLE_SOURCE)
                .expect("end")
        ),
        (
            SourceByte::new(span.start().get())
                .to_scalar(TABLE_SOURCE)
                .unwrap(),
            SourceByte::new(span.end().get())
                .to_scalar(TABLE_SOURCE)
                .unwrap(),
            "苹果派",
            SourceByte::new(span.start().get())
                .to_scalar(TABLE_SOURCE)
                .unwrap(),
            SourceByte::new(span.end().get())
                .to_scalar(TABLE_SOURCE)
                .unwrap()
        ),
        "替换区间必须正好是进入编辑前的 cell 区间"
    );
    assert!(!controller.is_editing_table_cell());
    // 提交后表格重新是 Object，caret 落到整表末尾（不留在源码内部）。
    let committed_span = parse(&controller.source_text()).tables()[0].span();
    assert!(controller
        .projection()
        .object_display_range(committed_span)
        .is_some());
    let committed = controller.source_text();
    let committed_end = SourceByte::new(table_end_of(&committed))
        .to_scalar(&committed)
        .expect("表格末尾是 scalar 边界");
    assert_eq!(
        controller.selection(),
        Selection::caret(committed_end),
        "提交后 caret 落在整表末尾"
    );

    // 会话确认：镜像等于源码（提交前后镜像 = 会话文本，不再二次应用）。
    confirm(&mut controller, &request.command_id, 8);
    assert_eq!(
        controller.source_text(),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果派 | 3 |\n\n后文\n"
    );
    assert!(controller.pending_command().is_none());
}

#[test]
fn pipes_are_escaped_and_newlines_never_break_the_row() {
    let mut controller = open(TABLE_SOURCE);
    controller
        .begin_table_cell_edit(TABLE_LINE, 1, 1)
        .expect("进入");
    controller
        .compose_replace_selection("a|b\nc")
        .expect("草稿编辑");

    assert_eq!(
        controller.source_text(),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果 | 3a\\|b c |\n\n后文\n",
        "未转义的 `|` 写成 `\\|`，换行变成空格，表格列数不变"
    );
    let parsed = parse(&controller.source_text());
    assert_eq!(parsed.tables()[0].columns(), 2, "表格结构不能被输入改坏");
    assert_eq!(cell_text(&controller.source_text(), 1, 1), "3a\\|b c");
}

#[test]
fn escape_cancels_without_any_request() {
    let mut controller = open(TABLE_SOURCE);
    controller
        .begin_table_cell_edit(TABLE_LINE, 1, 0)
        .expect("进入");
    controller
        .compose_replace_selection("毁掉")
        .expect("草稿编辑");
    assert_eq!(
        controller.source_text(),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果毁掉 | 3 |\n\n后文\n"
    );

    assert!(controller.cancel_table_cell_edit(), "取消必须真的取消");
    assert_eq!(controller.source_text(), TABLE_SOURCE, "Esc 恢复进入前文本");
    assert!(controller.pending_command().is_none(), "取消不产生请求");
    assert!(!controller.is_editing_table_cell());
    assert_eq!(
        controller.display_text(),
        format!("表格\n\n{OBJECT_PLACEHOLDER}\n\n后文\n"),
        "取消后表格重新投影成对象"
    );
}

#[test]
fn commit_without_changes_produces_no_request() {
    let mut controller = open(TABLE_SOURCE);
    controller
        .begin_table_cell_edit(TABLE_LINE, 0, 0)
        .expect("进入");
    assert!(
        controller
            .commit_table_cell_edit()
            .expect("提交不应失败")
            .is_none(),
        "没有改动 → 不发空 op 请求（否则单飞行的 pending 会被卡死）"
    );
    assert_eq!(controller.source_text(), TABLE_SOURCE);
    assert!(controller.pending_command().is_none());
    assert!(!controller.is_editing_table_cell());
}

#[test]
fn tab_commits_the_cell_and_moves_to_the_next_one() {
    let mut controller = open(TABLE_SOURCE);
    controller
        .begin_table_cell_edit(TABLE_LINE, 0, 0)
        .expect("进入");
    controller
        .compose_replace_selection("品名")
        .expect("草稿编辑");
    let (request, still_editing) = controller
        .advance_table_cell_edit(true)
        .expect("Tab 不应失败");
    let request = request.expect("Tab 会提交当前格（一次请求）");
    assert_eq!(request.edits.len(), 1);
    assert!(still_editing, "Tab 之后继续编辑下一格");
    assert_eq!(
        controller
            .table_cell_edit_target()
            .map(|t| (t.row, t.column)),
        Some((0, 1)),
        "同一行的下一列"
    );
    confirm(&mut controller, &request.command_id, 8);
    assert_eq!(
        controller.source_text(),
        "# 表格\n\n| 名称品名 | 数量 |\n|:--|--:|\n| 苹果 | 3 |\n\n后文\n"
    );
    // 下一格的 caret 落在该 cell 文本末尾。
    let end = cell(&controller.source_text(), 0, 1).end().get();
    let expected = SourceByte::new(end)
        .to_scalar(&controller.source_text())
        .expect("scalar");
    assert_eq!(controller.selection(), Selection::caret(expected));
}

#[test]
fn tab_after_the_last_cell_leaves_the_table() {
    let mut controller = open(TABLE_SOURCE);
    controller
        .begin_table_cell_edit(TABLE_LINE, 1, 1)
        .expect("进入");
    let (request, still_editing) = controller.advance_table_cell_edit(true).expect("Tab");
    assert!(request.is_none(), "没有改动 → 不产生请求");
    assert!(!still_editing, "最后一格之后退出表格");
    assert!(!controller.is_editing_table_cell());
    let end = table_end_of(TABLE_SOURCE);
    assert_eq!(
        controller.selection(),
        Selection::caret(SourceByte::new(end).to_scalar(TABLE_SOURCE).unwrap()),
        "退出后 caret 在整表末尾（绝不落进表格源码）"
    );
}

#[test]
fn shift_tab_walks_backwards() {
    let mut controller = open(TABLE_SOURCE);
    controller
        .begin_table_cell_edit(TABLE_LINE, 1, 1)
        .expect("进入");
    let (_, still_editing) = controller
        .advance_table_cell_edit(false)
        .expect("Shift+Tab");
    assert!(still_editing);
    assert_eq!(
        controller
            .table_cell_edit_target()
            .map(|t| (t.row, t.column)),
        Some((1, 0))
    );
    let (_, still_editing) = controller
        .advance_table_cell_edit(false)
        .expect("Shift+Tab");
    assert!(still_editing);
    assert_eq!(
        controller
            .table_cell_edit_target()
            .map(|t| (t.row, t.column)),
        Some((0, 1)),
        "行首回卷到上一行末列"
    );
    let (_, still_editing) = controller
        .advance_table_cell_edit(false)
        .expect("Shift+Tab");
    assert!(still_editing);
    assert_eq!(
        controller
            .table_cell_edit_target()
            .map(|t| (t.row, t.column)),
        Some((0, 0))
    );
    let (_, still_editing) = controller
        .advance_table_cell_edit(false)
        .expect("Shift+Tab");
    assert!(!still_editing, "表头第一格之前退出表格");
    assert!(!controller.is_editing_table_cell());
}

#[test]
fn caret_and_selection_are_clamped_inside_the_cell() {
    let mut controller = open(TABLE_SOURCE);
    controller
        .begin_table_cell_edit(TABLE_LINE, 1, 0)
        .expect("进入");
    let span = cell(TABLE_SOURCE, 1, 0);
    let start = SourceByte::new(span.start().get())
        .to_scalar(TABLE_SOURCE)
        .unwrap();
    let end = SourceByte::new(span.end().get())
        .to_scalar(TABLE_SOURCE)
        .unwrap();

    // 想跳出 cell（例如按到行首/文档头）→ 夹在 cell 内。
    controller
        .set_selection(Selection::caret(UnicodeScalar::new(0)))
        .expect("设置选区");
    assert_eq!(controller.selection(), Selection::caret(start));
    controller
        .set_selection(Selection::caret(UnicodeScalar::new(
            controller.scalar_len(),
        )))
        .expect("设置选区");
    assert_eq!(controller.selection(), Selection::caret(end));

    // cell 起点按 Backspace：目标 grapheme 在 cell 之外 → no-op（不产生请求）。
    controller
        .set_selection(Selection::caret(start))
        .expect("设置选区");
    assert!(
        controller
            .delete_backward()
            .expect("删除不应报错")
            .is_none(),
        "cell 之前的字节不属于这次编辑，不能删"
    );
    assert_eq!(controller.source_text(), TABLE_SOURCE);
}

#[test]
fn added_row_and_column_cells_are_editable() {
    let mut controller = open(TABLE_SOURCE);
    let request = controller
        .append_table_row(TABLE_LINE)
        .expect("加行不应失败")
        .expect("加行必须产生请求");
    confirm(&mut controller, &request.command_id, 8);
    let source_after_row = controller.source_text();
    assert_eq!(
        source_after_row,
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果 | 3 |\n|  |  |\n\n后文\n"
    );

    // 新数据行（row 2）第一格：可进入、可编辑、提交后只改这一格。
    assert!(
        controller
            .begin_table_cell_edit(TABLE_LINE, 2, 0)
            .expect("进入新行"),
        "加行后的新 cell 必须可编辑"
    );
    controller
        .compose_replace_selection("西瓜")
        .expect("草稿编辑");
    let request = controller
        .commit_table_cell_edit()
        .expect("提交")
        .expect("有改动 → 请求");
    confirm(&mut controller, &request.command_id, 9);
    assert_eq!(
        controller.source_text(),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果 | 3 |\n|  西瓜|  |\n\n后文\n"
    );

    // 加列后新列的空 cell 也能编辑（空 cell 的落点在行内 `|` 边界内）。
    let request = controller
        .append_table_column(TABLE_LINE)
        .expect("加列")
        .expect("加列必须产生请求");
    confirm(&mut controller, &request.command_id, 10);
    let source_after_column = controller.source_text();
    assert!(source_after_column.contains("| 名称 | 数量 | |"));
    assert!(
        controller
            .begin_table_cell_edit(TABLE_LINE, 0, 2)
            .expect("进入新列"),
        "加列后的新空 cell 必须可编辑"
    );
    controller
        .compose_replace_selection("备注")
        .expect("草稿编辑");
    let request = controller
        .commit_table_cell_edit()
        .expect("提交")
        .expect("有改动 → 请求");
    confirm(&mut controller, &request.command_id, 11);
    assert_eq!(
        controller.source_text(),
        "# 表格\n\n| 名称 | 数量 | 备注|\n|:--|--:| ---|\n| 苹果 | 3 | |\n|  西瓜|  | |\n\n后文\n"
    );
}

#[test]
fn empty_cell_can_be_edited_in_place() {
    let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
    let mut controller = open(source);
    // 先把第 2 列数据格清空（模拟用户删空），确认空 cell 可再次进入编辑。
    controller.begin_table_cell_edit(0, 1, 1).expect("进入");
    controller
        .set_selection(Selection::new(
            SourceByte::new(cell(source, 1, 1).start().get())
                .to_scalar(source)
                .unwrap(),
            SourceByte::new(cell(source, 1, 1).end().get())
                .to_scalar(source)
                .unwrap(),
        ))
        .expect("全选 cell 文本");
    controller.compose_replace_selection("").expect("删空");
    let request = controller
        .commit_table_cell_edit()
        .expect("提交")
        .expect("有改动 → 请求");
    confirm(&mut controller, &request.command_id, 8);
    assert_eq!(controller.source_text(), "| a | b |\n|---|---|\n| 1 |  |\n");

    // 空 cell：display 宽度为 0，但仍可进入/输入/提交。
    assert!(controller
        .begin_table_cell_edit(0, 1, 1)
        .expect("进入空 cell"));
    assert_eq!(
        controller.display_text(),
        "\n",
        "空 cell 的显示宽度为 0：整表只剩表格之后的那个换行"
    );
    controller.compose_replace_selection("2").expect("输入");
    let request = controller
        .commit_table_cell_edit()
        .expect("提交")
        .expect("有改动 → 请求");
    confirm(&mut controller, &request.command_id, 9);
    assert_eq!(
        controller.source_text(),
        "| a | b |\n|---|---|\n| 1 |  2|\n",
        "空 cell 里插字落在 `|` 边界内，原本的空白逐字保留"
    );
}

#[test]
fn inline_markdown_markers_stay_plain_text_and_keep_their_bytes() {
    // 单元格里的 `**` / 反引号 / 链接标记首版按**纯文本**显示；用户没有实际改写这些
    // 字节时，提交回去必须逐字不变（ADR 0019 S3b2 §4）。
    let source = "| a | b |\n|---|---|\n| **粗** \u{0060}x\u{0060} | [t](u) |\n";
    let mut controller = open(source);
    assert!(controller.begin_table_cell_edit(0, 1, 0).expect("进入"));
    assert_eq!(
        controller.display_text(),
        "**粗** `x`\n",
        "display 只有该 cell 的源码原文（不解析成富文本）"
    );
    controller
        .compose_replace_selection("!")
        .expect("在格尾追加");
    let request = controller
        .commit_table_cell_edit()
        .expect("提交")
        .expect("有改动 → 请求");
    assert_eq!(request.edits.len(), 1);
    assert_eq!(request.edits[0].replacement, "**粗** `x`!");
    confirm(&mut controller, &request.command_id, 8);
    // 同一行的另一个 cell（链接）一个字节都没动。
    assert_eq!(
        controller.source_text(),
        "| a | b |\n|---|---|\n| **粗** `x`! | [t](u) |\n"
    );
}

#[test]
fn padded_missing_cells_cannot_be_edited() {
    // 分隔行 2 列，但表头行只有一个 cell → 第 2 列是“缺列补出来的空区间”，
    // 插字会写到行内容之外（写坏表格），必须拒绝进入。
    let source = "| a |\n|---|---|\n| 1 |\n";
    let mut controller = open(source);
    assert!(
        !controller.begin_table_cell_edit(0, 0, 1).expect("不应报错"),
        "缺列补出来的 cell 没有 `|` 边界，不能就地编辑"
    );
    assert!(!controller.is_editing_table_cell());
}

#[test]
fn ime_commit_goes_through_the_normal_path_into_the_cell() {
    let mut controller = open(TABLE_SOURCE);
    controller
        .begin_table_cell_edit(TABLE_LINE, 0, 1)
        .expect("进入");
    controller.set_preedit("shuliang").expect("开始组合");
    assert!(
        controller.commit_preedit("数量").expect("上屏").is_none(),
        "IME 上屏在 cell 编辑期间也只写镜像（不产生请求）"
    );
    assert_eq!(
        controller.display_text(),
        "表格\n\n数量数量\n\n后文\n",
        "组合上屏只改该 cell 的显示文本"
    );
    let request = controller
        .commit_table_cell_edit()
        .expect("提交")
        .expect("有改动 → 请求");
    assert_eq!(request.edits.len(), 1);
    assert_eq!(request.edits[0].replacement, "数量数量");
}

#[test]
fn structural_operations_are_refused_while_editing_a_cell() {
    let mut controller = open(TABLE_SOURCE);
    controller
        .begin_table_cell_edit(TABLE_LINE, 1, 0)
        .expect("进入");
    assert_eq!(
        controller.append_table_row(TABLE_LINE),
        Err(EditorError::CellEditActive)
    );
    assert_eq!(
        controller.append_table_column(TABLE_LINE),
        Err(EditorError::CellEditActive)
    );
    assert_eq!(
        controller.toggle_task_marker(0),
        Err(EditorError::CellEditActive)
    );
    assert_eq!(
        controller.delete_object(TABLE_LINE),
        Err(EditorError::CellEditActive)
    );
    assert_eq!(controller.list_enter(), Err(EditorError::CellEditActive));
    assert_eq!(
        controller.begin_object_source_edit(TABLE_LINE),
        Err(EditorError::CellEditActive)
    );
    // 被拒的操作一个字节都不能改。
    assert_eq!(controller.source_text(), TABLE_SOURCE);
    assert!(controller.is_editing_table_cell(), "被拒不等于退出编辑");
}

#[test]
fn remote_delta_discards_the_uncommitted_draft() {
    let mut controller = open(TABLE_SOURCE);
    controller
        .begin_table_cell_edit(TABLE_LINE, 1, 0)
        .expect("进入");
    controller
        .compose_replace_selection("毁掉")
        .expect("草稿编辑");
    assert!(controller.source_text().contains("苹果毁掉"));

    let delta = support::live_delta(7, 8, support::splice(0, 0, "前"));
    controller
        .apply_live_delta(delta)
        .expect("远端 delta 必须被应用");
    assert!(
        !controller.is_editing_table_cell(),
        "远端变更会放弃未提交的草稿（镜像必须先回到会话坐标）"
    );
    assert_eq!(
        controller.source_text(),
        format!("前{TABLE_SOURCE}"),
        "草稿被丢弃，镜像与会话一致后再应用 splice"
    );
}

#[test]
fn undo_of_the_commit_restores_the_original_cell() {
    // 一次提交 = 一条 ScalarEdit = 一个撤销组：调用方把同一条请求交给会话，
    // 一次撤销就能回到进入编辑前的整篇原文。
    let mut controller = open(TABLE_SOURCE);
    controller
        .begin_table_cell_edit(TABLE_LINE, 0, 0)
        .expect("进入");
    controller
        .compose_replace_selection("-改")
        .expect("草稿编辑");
    let request = controller
        .commit_table_cell_edit()
        .expect("提交")
        .expect("请求");
    assert_eq!(request.edits.len(), 1, "只产生一个撤销单元");
    confirm(&mut controller, &request.command_id, 8);
    assert!(controller.source_text().contains("名称-改"));

    // 用同一请求的逆操作验证“其它字节不变”（会话层 undo 由 storage 测试覆盖）。
    let edit = &request.edits[0];
    let inverse = menghunote_storage::session::ScalarEdit {
        start: edit.start,
        end: UnicodeScalar::new(edit.start.get() + edit.replacement.chars().count()),
        replacement: cell_text(TABLE_SOURCE, 0, 0).to_owned(),
    };
    let request = controller
        .compose_edits(vec![inverse])
        .expect("逆向编辑应被接受");
    confirm(&mut controller, &request.command_id, 9);
    assert_eq!(controller.source_text(), TABLE_SOURCE);
}

#[test]
fn aligned_delimiters_and_trailing_newline_survive_a_cell_commit() {
    let mut controller = open(TABLE_SOURCE);
    // 表头第一格 + 数据行最后一格各改一次，分隔行的 `:--` / `--:` 一个字节不能动。
    for (row, column, text) in [(0, 1, "个"), (1, 1, "4")] {
        assert!(controller
            .begin_table_cell_edit(TABLE_LINE, row, column)
            .expect("进入"));
        controller
            .compose_replace_selection(text)
            .expect("草稿编辑");
        let request = controller
            .commit_table_cell_edit()
            .expect("提交")
            .expect("请求");
        confirm(
            &mut controller,
            &request.command_id,
            8 + row as u64 * 10 + column as u64,
        );
    }
    assert_eq!(
        controller.source_text(),
        "# 表格\n\n| 名称 | 数量个 |\n|:--|--:|\n| 苹果 | 34 |\n\n后文\n",
        "对齐标记/分隔行/行尾换行逐字不变"
    );
}

/// `TABLE_SOURCE` 里表格末行的内容终点（字节）。
fn table_end_of(source: &str) -> usize {
    parse(source).tables()[0].span().end().get()
}

#[test]
fn editing_cell_of_a_table_keeps_other_objects_intact() {
    // 后方还有一张表：编辑前一张表时，后一张表仍是 Object，display 位置随之移动。
    let source = "| a | b |\n|---|---|\n| 1 | 2 |\n\n| c | d |\n|---|---|\n| 3 | 4 |\n";
    let mut controller = open(source);
    let objects = controller.objects().to_vec();
    assert_eq!(objects.len(), 2);
    controller.begin_table_cell_edit(0, 1, 0).expect("进入");
    let display = controller.display_text().to_string();
    assert!(
        display.contains("1") && display.contains(OBJECT_PLACEHOLDER),
        "正在编辑的表显示 cell 文本，另一张表仍是占位符：{display:?}"
    );
    let second = controller
        .objects()
        .iter()
        .find(|object| object.kind() == ObjectKind::Table && object.line() != 0)
        .expect("第二张表");
    assert!(
        controller
            .projection()
            .object_display_range(second.span())
            .is_some(),
        "未编辑的表格必须继续投影成 Object"
    );
    assert!(controller.cancel_table_cell_edit());
    assert_eq!(controller.source_text(), source);
}

#[test]
fn display_anchor_exists_even_for_an_empty_cell() {
    let source = "| a | b |\n|---|---|\n|  |  |\n";
    let mut controller = open(source);
    assert!(controller
        .begin_table_cell_edit(0, 1, 0)
        .expect("进入空 cell"));
    let anchor = controller
        .table_cell_edit_display_anchor()
        .expect("空 cell 也有 display 落点");
    assert!(anchor <= controller.display_len().get());
    assert!(
        controller.table_cell_edit_display_range().is_none(),
        "空 cell 的显示宽度是 0"
    );
    // display 上表格不再是对象：表格区间没有 Object 段。
    let span = parse(source).tables()[0].span();
    assert!(controller.projection().object_display_range(span).is_none());
    let _ = DisplayByte::new(anchor);
}
