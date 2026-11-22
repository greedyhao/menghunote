//! ADR 0019 S3b：表格 Object「加行 / 加列」的纯模型测试。
//!
//! 只覆盖 `menghunote-editor`：源码逐字节结果、行列数/空 cell、对齐保留、
//! “一次点击一个 `EditRequest`（一个撤销组）”、caret/选区不变、表格仍是 Object。
//! 点击按钮与持久化在 app 的 headless 测试里（`crates/app/tests/headless_table_edit.rs`）。

mod support;

use menghunote_editor::{EditorController, ObjectKind, Selection, SourceByte, UnicodeScalar};
use menghunote_storage::session::EditRequest;

/// 2×2 表格（无对齐）+ 前后正文。
const TABLE_SOURCE: &str = "前\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n后\n";
/// 表格起始字节（`| a | b |` 的 `|`）。
const TABLE_START: usize = 5;

/// 表格末尾字节（最后一条数据行内容终点；不含空行）。
fn table_end() -> usize {
    TABLE_SOURCE.find("\n\n后").expect("后文位置")
}

/// 带三种对齐、三列的表格（中文 cell）。
const ALIGNED_SOURCE: &str =
    "| 名称 | 数量 | 备注 |\n|:--|:-:|--:|\n| 苹果 | 3 | 红 |\n| 香蕉 | 5 | 黄 |\n";

fn open(source: &str) -> EditorController {
    EditorController::open(support::snapshot(source)).expect("测试快照必须可打开")
}

/// 消费本地回声（单飞行：下一个 `EditRequest` 前必须确认掉 `pending`）。
fn confirm(controller: &mut EditorController, request: &EditRequest, live_rev: u64) {
    controller
        .apply_outcome(support::applied(
            &request.command_id,
            live_rev,
            true,
            support::splice(0, 0, ""),
        ))
        .expect("本地回声必须被接受");
}

/// source 里表格的行/列/对齐/每格文本（断言用；不经过 app）。
fn table_snapshot(controller: &EditorController) -> TableFacts {
    let source = controller.source_text();
    let table = controller.tables().first().expect("必须解析出一张表格");
    let rows = std::iter::once(table.header())
        .chain(table.rows().iter())
        .map(|row| {
            row.cells()
                .iter()
                .map(|cell| source[cell.start().get()..cell.end().get()].to_owned())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    TableFacts {
        columns: table.columns(),
        alignments: table.alignments().to_vec(),
        header: source[table.header().span().start().get()..table.header().span().end().get()]
            .to_owned(),
        delimiter: source[table.delimiter().start().get()..table.delimiter().end().get()]
            .to_owned(),
        rows,
        object_line: controller.objects().first().map(|object| object.line()),
        kind: controller.objects().first().map(|object| object.kind()),
        end_line: controller.objects().first().map(|object| object.end_line()),
    }
}

#[derive(Debug)]
struct TableFacts {
    columns: usize,
    alignments: Vec<menghunote_editor::MarkdownTableAlignment>,
    header: String,
    delimiter: String,
    /// 第 0 行是表头，其余是数据行。
    rows: Vec<Vec<String>>,
    object_line: Option<usize>,
    kind: Option<ObjectKind>,
    end_line: Option<usize>,
}

/// 把 selection 设到给定字节（测试夹具）。
fn set_caret(controller: &mut EditorController, byte: usize) {
    let scalar = SourceByte::new(byte)
        .to_scalar(&controller.source_text())
        .expect("测试用字节必须是 scalar 边界");
    controller
        .set_selection(Selection::caret(scalar))
        .expect("caret 必须合法");
}

#[test]
fn append_row_inserts_one_empty_row_after_the_last_data_row() {
    let mut controller = open(TABLE_SOURCE);
    set_caret(&mut controller, table_end());

    let request = controller
        .append_table_row(2)
        .expect("加行不应失败")
        .expect("整块表格必须产生一次 EditRequest");
    assert_eq!(request.edits.len(), 1, "加行必须是单个 ScalarEdit");
    let edit = &request.edits[0];
    assert_eq!(edit.start, edit.end, "加行只能是插入（不替换任何字节）");
    assert_eq!(edit.replacement, "\n|  |  |", "新行必须是空 cell 的 2 列行");
    assert_eq!(
        edit.start,
        SourceByte::new(table_end())
            .to_scalar(TABLE_SOURCE)
            .expect("表格末尾是 scalar 边界")
    );

    // 源码逐字节：只在最后一条数据行之后多了新行，其余字节一个不变。
    assert_eq!(
        controller.source_text(),
        "前\n\n| a | b |\n|---|---|\n| 1 | 2 |\n|  |  |\n\n后\n"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(facts.columns, 2, "加行不改列数");
    assert_eq!(facts.rows.len(), 3, "表头 + 2 条数据行");
    assert_eq!(facts.header, "| a | b |", "表头逐字节不变");
    assert_eq!(facts.delimiter, "|---|---|", "对齐分隔行逐字节不变");
    assert_eq!(facts.rows[1], vec!["1", "2"], "原数据行不变");
    assert_eq!(
        facts.rows[2],
        vec!["", ""],
        "新行 cell 必须为空（不复制上一行）"
    );
    assert_eq!(
        facts.alignments,
        vec![
            menghunote_editor::MarkdownTableAlignment::None,
            menghunote_editor::MarkdownTableAlignment::None,
        ],
        "对齐不变"
    );
    // 表格仍是 Object（不进入源码模式、仍投影成一个占位符）。
    assert_eq!(facts.kind, Some(ObjectKind::Table));
    assert_eq!(facts.object_line, Some(2));
    assert_eq!(facts.end_line, Some(5), "表格多占一行源码");
    assert_eq!(
        controller.object_source_edit_line(),
        None,
        "不得进入源码模式"
    );
    assert!(controller
        .display_text()
        .contains(menghunote_editor::OBJECT_PLACEHOLDER));
    assert!(controller.tables()[0].is_block());
    // caret 在表格末尾（= 插入点）时按**文本位置**映射：仍停在新表格末尾，
    // 也就是“既不落进新行内部，也不挪到表格前面”。
    let table = controller.tables()[0].clone();
    assert_eq!(
        controller.selection().head(),
        SourceByte::new(table.span().end().get())
            .to_scalar(&controller.source_text())
            .expect("新表格末尾是 scalar 边界"),
        "caret 必须停在加行后的表格末尾（不能落进表格内部）"
    );
    assert!(controller.selection().is_empty(), "选区仍是折叠光标");
}

#[test]
fn append_row_goes_after_the_last_of_several_data_rows() {
    let mut controller = open("| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n");
    controller
        .append_table_row(0)
        .expect("加行不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n|  |  |\n",
        "新行必须在**最后一条**数据行之后（不能插到表格中间）"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(facts.rows.len(), 4, "表头 + 3 条数据行");
    assert_eq!(facts.rows[1], vec!["1", "2"]);
    assert_eq!(facts.rows[2], vec!["3", "4"]);
    assert_eq!(facts.rows[3], vec!["", ""], "只有最后一行是新的空行");
    assert_eq!(facts.end_line, Some(4));
}

#[test]
fn append_row_works_when_the_table_has_no_data_rows() {
    let source = "| a | b |\n|---|---|\n\n后\n";
    let mut controller = open(source);
    let request = controller
        .append_table_row(0)
        .expect("空表格加行不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(request.edits.len(), 1);
    assert_eq!(
        controller.source_text(),
        "| a | b |\n|---|---|\n|  |  |\n\n后\n"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(facts.rows.len(), 2, "新增的那条数据行成为第一行");
    assert_eq!(facts.rows[1], vec!["", ""]);
    assert_eq!(facts.header, "| a | b |");
}

#[test]
fn append_column_extends_every_line_and_keeps_alignment() {
    let mut controller = open(ALIGNED_SOURCE);
    set_caret(&mut controller, ALIGNED_SOURCE.len());
    let before = ALIGNED_SOURCE.chars().count();

    let request = controller
        .append_table_column(0)
        .expect("加列不应失败")
        .expect("整块表格必须产生一次 EditRequest");
    // 表头 + 分隔行 + 2 条数据行 = 4 条插入，全部在同一个 EditRequest（一个撤销组）里。
    assert_eq!(request.edits.len(), 4, "每行末尾一条插入");
    assert!(request.edits.iter().all(|edit| edit.start == edit.end));
    assert!(
        request
            .edits
            .windows(2)
            .all(|pair| pair[0].start <= pair[1].start),
        "编辑必须按位置升序（compose_edits 的前置条件）"
    );

    assert_eq!(
        controller.source_text(),
        "| 名称 | 数量 | 备注 | |\n|:--|:-:|--:| ---|\n| 苹果 | 3 | 红 | |\n| 香蕉 | 5 | 黄 | |\n"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(facts.columns, 4);
    assert_eq!(
        facts.alignments,
        vec![
            menghunote_editor::MarkdownTableAlignment::Left,
            menghunote_editor::MarkdownTableAlignment::Center,
            menghunote_editor::MarkdownTableAlignment::Right,
            menghunote_editor::MarkdownTableAlignment::None,
        ],
        "原三列对齐保留，新列是默认对齐（无冒号）"
    );
    assert_eq!(
        facts.rows[0],
        vec!["名称", "数量", "备注", ""],
        "表头多出一列空 cell"
    );
    assert_eq!(facts.rows[1], vec!["苹果", "3", "红", ""]);
    assert_eq!(facts.rows[2], vec!["香蕉", "5", "黄", ""]);
    assert_eq!(facts.header, "| 名称 | 数量 | 备注 | |");
    assert_eq!(
        facts.delimiter, "|:--|:-:|--:| ---|",
        "分隔行只追加默认对齐的 ---，原冒号不动"
    );
    assert!(controller.tables()[0].is_block());
    assert_eq!(controller.object_source_edit_line(), None);
    // caret 在文档末尾（所有插入点之后）→ 按插入量平移，文本位置不变。
    let inserted = " |".chars().count() * 3 + " ---|".chars().count();
    assert_eq!(
        controller.selection().head(),
        UnicodeScalar::new(before + inserted),
        "插入点之前的 caret 必须随文本平移"
    );
}

#[test]
fn append_column_works_for_tables_without_trailing_pipes() {
    let mut controller = open("a | b\n---|---\n1 | 2\n");
    controller
        .append_table_column(0)
        .expect("加列不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "a | b | |\n---|---| ---\n1 | 2 | |\n"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(facts.columns, 3, "无尾管道也必须真的多一列");
    assert_eq!(facts.rows[0], vec!["a", "b", ""]);
    assert_eq!(facts.rows[1], vec!["1", "2", ""]);
}

#[test]
fn append_column_works_when_the_table_has_no_data_rows() {
    let mut controller = open("| a | b |\n|---|---|\n\n后\n");
    controller
        .append_table_column(0)
        .expect("空表格加列不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| a | b | |\n|---|---| ---|\n\n后\n"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(facts.columns, 3);
    assert_eq!(facts.rows, vec![vec!["a", "b", ""]], "只有表头，没有数据行");
    assert_eq!(facts.delimiter, "|---|---| ---|");
    assert!(controller.tables()[0].is_block());
}

#[test]
fn append_column_does_not_touch_existing_cell_bytes_or_delimiter_colons() {
    // 原表行的空白形状（`|a|b|` 紧凑写法）必须逐字节保留，只在行尾追加。
    let mut controller = open("|a|b|\n|:-|-:|\n|c|d|\n");
    controller
        .append_table_column(0)
        .expect("加列不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(controller.source_text(), "|a|b| |\n|:-|-:| ---|\n|c|d| |\n");
    let facts = table_snapshot(&controller);
    assert_eq!(facts.columns, 3);
    assert_eq!(facts.rows[0], vec!["a", "b", ""]);
    assert_eq!(facts.rows[1], vec!["c", "d", ""]);
}

#[test]
fn crlf_tables_keep_their_line_endings() {
    let mut controller = open("| a | b |\r\n|---|---|\r\n| 1 | 2 |\r\n");
    controller
        .append_table_row(0)
        .expect("CRLF 表格加行不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| a | b |\r\n|---|---|\r\n| 1 | 2 |\r\n|  |  |\r\n",
        "新行必须用 CRLF，不能混进 LF"
    );

    let mut controller = open("| a | b |\r\n|---|---|\r\n| 1 | 2 |\r\n");
    controller
        .append_table_column(0)
        .expect("CRLF 表格加列不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| a | b | |\r\n|---|---| ---|\r\n| 1 | 2 | |\r\n"
    );
    assert_eq!(table_snapshot(&controller).columns, 3);
}

#[test]
fn table_structure_edits_only_apply_to_block_tables() {
    // 图片对象 / 普通段落 / 越界行 / 引用里的表格（不是 Object）都不能加行加列。
    let mut controller = open("![图](asset://abc)\n\n段落\n\n> | a | b |\n> |---|---|\n");
    for line in 0..6 {
        assert!(
            controller
                .append_table_row(line)
                .expect("拒绝必须不是错误")
                .is_none(),
            "行 {line} 不是整块表格，加行必须是 no-op"
        );
        assert!(
            controller
                .append_table_column(line)
                .expect("拒绝必须不是错误")
                .is_none(),
            "行 {line} 不是整块表格，加列必须是 no-op"
        );
    }
    assert_eq!(
        controller.source_text(),
        "![图](asset://abc)\n\n段落\n\n> | a | b |\n> |---|---|\n"
    );
    // 行号越界同样只是 no-op，不 panic。
    assert!(controller
        .append_table_row(99)
        .expect("越界不是错误")
        .is_none());
}

#[test]
fn appended_rows_and_columns_stay_object_mode_without_source_edit() {
    let mut controller = open(ALIGNED_SOURCE);
    assert_eq!(controller.object_source_edit_line(), None);
    let display_before = controller.display_text().to_string();
    assert!(display_before.contains(menghunote_editor::OBJECT_PLACEHOLDER));

    let row = controller
        .append_table_row(0)
        .expect("加行不应失败")
        .expect("必须产生请求");
    confirm(&mut controller, &row, 8);
    let column = controller
        .append_table_column(0)
        .expect("加列不应失败")
        .expect("必须产生请求");
    confirm(&mut controller, &column, 9);

    assert_eq!(
        controller.object_source_edit_line(),
        None,
        "结构编辑不得把表格切到源码模式"
    );
    let display = controller.display_text().to_string();
    assert!(
        display.contains(menghunote_editor::OBJECT_PLACEHOLDER),
        "表格仍是 Object 占位：{display:?}"
    );
    assert!(
        !display.contains("| 名称 |"),
        "不得显示表格源码：{display:?}"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(facts.columns, 4);
    assert_eq!(facts.rows.len(), 4, "表头 + 原 2 行 + 新 1 行");
    assert_eq!(facts.kind, Some(ObjectKind::Table));
}

#[test]
fn append_row_does_not_move_a_caret_before_the_table() {
    // caret 在表格之前：插入点都在它之后 → 完全不动（含选区）。
    let mut controller = open(TABLE_SOURCE);
    set_caret(&mut controller, TABLE_START - 1);
    let before = controller.selection();
    controller
        .append_table_row(2)
        .expect("加行不应失败")
        .expect("必须产生请求");
    assert_eq!(controller.selection(), before, "表格之前的 caret 不得移动");
}

// ---- ADR 0019 S3b3：删除行 / 删除列 ----

/// 3 条数据行 + 前后正文（删中间行用）。
const THREE_ROWS: &str = "前\n\n| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n| 5 | 6 |\n\n后\n";

/// 表格在文档末尾、最后一行没有行尾换行（删末行不能留下空行）。
const TABLE_AT_EOF: &str = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";

/// 把 caret 放到整表末尾（删除前的典型 caret 位置：单元格编辑提交后就在这里）。
fn set_caret_at_table_end(controller: &mut EditorController) {
    let source = controller.source_text();
    let table = controller.tables().first().expect("必须解析出一张表格");
    let end = SourceByte::new(table.span().end().get())
        .to_scalar(&source)
        .expect("表格末尾是 scalar 边界");
    controller
        .set_selection(Selection::caret(end))
        .expect("caret 必须合法");
}

/// 新表格末尾的 scalar（断言 caret 用）。
fn table_end_scalar(controller: &EditorController) -> UnicodeScalar {
    let source = controller.source_text();
    let table = controller.tables().first().expect("必须解析出一张表格");
    SourceByte::new(table.span().end().get())
        .to_scalar(&source)
        .expect("表格末尾是 scalar 边界")
}

#[test]
fn delete_middle_data_row_removes_exactly_that_row_in_one_request() {
    let mut controller = open(THREE_ROWS);
    set_caret_at_table_end(&mut controller);

    let request = controller
        .delete_table_row(2, 2)
        .expect("删行不应失败")
        .expect("整块表格必须产生一次 EditRequest");
    assert_eq!(request.edits.len(), 1, "删行必须是单个 ScalarEdit");
    let edit = &request.edits[0];
    assert!(edit.start < edit.end, "删行必须是删除（不能是空区间插入）");
    assert_eq!(edit.replacement, "", "删行必须是纯删除");

    assert_eq!(
        controller.source_text(),
        "前\n\n| a | b |\n|---|---|\n| 1 | 2 |\n| 5 | 6 |\n\n后\n",
        "只删第 2 条数据行（整行含行尾换行），其余字节逐字不变"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(facts.rows.len(), 3, "表头 + 2 条数据行");
    assert_eq!(facts.rows[0], vec!["a", "b"], "表头不变");
    assert_eq!(facts.rows[1], vec!["1", "2"]);
    assert_eq!(facts.rows[2], vec!["5", "6"], "被删行之后的数据行上移");
    assert_eq!(facts.header, "| a | b |");
    assert_eq!(facts.delimiter, "|---|---|");
    assert_eq!(facts.kind, Some(ObjectKind::Table), "表格仍是 Object");
    assert_eq!(facts.object_line, Some(2));
    assert_eq!(facts.end_line, Some(5), "表格少占一行源码");
    assert_eq!(
        controller.object_source_edit_line(),
        None,
        "不得进入源码模式"
    );
    assert!(controller
        .display_text()
        .contains(menghunote_editor::OBJECT_PLACEHOLDER));
    // caret：表格末尾 → 新表格末尾（不落进表格源码内部，也不跳到删除区间起点）。
    assert_eq!(
        controller.selection(),
        Selection::caret(table_end_scalar(&controller))
    );
}

#[test]
fn delete_first_and_last_data_rows_are_byte_exact() {
    let mut controller = open(THREE_ROWS);
    controller
        .delete_table_row(2, 1)
        .expect("删行不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "前\n\n| a | b |\n|---|---|\n| 3 | 4 |\n| 5 | 6 |\n\n后\n",
        "删第一条数据行：后面的行上移，不留空行"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(facts.rows.len(), 3);
    assert_eq!(facts.rows[1], vec!["3", "4"]);

    // 文档末尾的表格、最后一行没有行尾换行：连前一行的行尾一起删，不留空行。
    let mut controller = open(TABLE_AT_EOF);
    set_caret_at_table_end(&mut controller);
    let request = controller
        .delete_table_row(0, 2)
        .expect("删行不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(request.edits.len(), 1);
    assert_eq!(
        controller.source_text(),
        "| a | b |\n|---|---|\n| 1 | 2 |",
        "表格末尾没有换行时必须连前一行的行尾一起删（不能留下空行）"
    );
    assert_eq!(table_snapshot(&controller).rows.len(), 2);
    assert_eq!(
        controller.selection(),
        Selection::caret(table_end_scalar(&controller)),
        "caret 落在新表格末尾"
    );
}

#[test]
fn header_row_deletion_is_a_noop() {
    let mut controller = open(THREE_ROWS);
    assert!(
        controller
            .delete_table_row(2, 0)
            .expect("拒绝必须不是错误")
            .is_none(),
        "首版明确不删表头（删掉 GFM 表格会整体消失）"
    );
    assert!(
        controller
            .delete_table_row(2, 4)
            .expect("越界行不是错误")
            .is_none(),
        "越界数据行"
    );
    assert_eq!(controller.source_text(), THREE_ROWS);
}

#[test]
fn delete_middle_column_removes_it_from_header_delimiter_and_all_rows() {
    let mut controller = open(ALIGNED_SOURCE);
    set_caret_at_table_end(&mut controller);

    let request = controller
        .delete_table_column(0, 1)
        .expect("删列不应失败")
        .expect("整块表格必须产生一次 EditRequest");
    assert_eq!(
        request.edits.len(),
        4,
        "表头 + 分隔行 + 2 条数据行各一条删除"
    );
    assert!(
        request
            .edits
            .iter()
            .all(|edit| edit.start < edit.end && edit.replacement.is_empty()),
        "删列必须是纯删除"
    );
    assert!(
        request
            .edits
            .windows(2)
            .all(|pair| pair[0].start < pair[1].start),
        "编辑必须按位置升序、互不重叠"
    );
    assert_eq!(
        controller.source_text(),
        "| 名称 | 备注 |\n|:--|--:|\n| 苹果 | 红 |\n| 香蕉 | 黄 |\n",
        "中间列从表头/分隔行/所有数据行删除；其余 cell 字节与对齐冒号逐字保留"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(facts.columns, 2);
    assert_eq!(
        facts.alignments,
        vec![
            menghunote_editor::MarkdownTableAlignment::Left,
            menghunote_editor::MarkdownTableAlignment::Right,
        ],
        "删中间列后，左边（左对齐）与右边（右对齐）的对齐保留"
    );
    assert_eq!(facts.rows[1], vec!["苹果", "红"]);
    assert_eq!(facts.kind, Some(ObjectKind::Table));
    assert_eq!(facts.object_line, Some(0));
    assert_eq!(facts.end_line, Some(3), "删列不改行数");
    assert!(controller.tables()[0].is_block());
    assert_eq!(
        controller.selection(),
        Selection::caret(table_end_scalar(&controller)),
        "caret 在表格末尾 → 新表格末尾（不落进表格源码内部）"
    );
}

#[test]
fn delete_first_and_last_columns_are_byte_exact() {
    let mut controller = open(ALIGNED_SOURCE);
    controller
        .delete_table_column(0, 0)
        .expect("删列不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| 数量 | 备注 |\n|:-:|--:|\n| 3 | 红 |\n| 5 | 黄 |\n",
        "删首列：保留行首 `|` 与其余 cell 的空白"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(
        facts.alignments,
        vec![
            menghunote_editor::MarkdownTableAlignment::Center,
            menghunote_editor::MarkdownTableAlignment::Right,
        ]
    );
    assert_eq!(facts.rows[0], vec!["数量", "备注"]);

    let mut controller = open(ALIGNED_SOURCE);
    controller
        .delete_table_column(0, 2)
        .expect("删列不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| 名称 | 数量 |\n|:--|:-:|\n| 苹果 | 3 |\n| 香蕉 | 5 |\n",
        "删末列：保留收尾 `|`，不留下多余空白"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(facts.columns, 2);
    assert_eq!(
        facts.alignments,
        vec![
            menghunote_editor::MarkdownTableAlignment::Left,
            menghunote_editor::MarkdownTableAlignment::Center,
        ]
    );
    assert_eq!(facts.rows[1], vec!["苹果", "3"]);
}

#[test]
fn delete_last_remaining_column_is_rejected_and_source_is_untouched() {
    let source = "| a |\n|---|\n| 1 |\n";
    let mut controller = open(source);
    assert_eq!(controller.tables()[0].columns(), 1);
    assert!(
        controller
            .delete_table_column(0, 0)
            .expect("拒绝必须不是错误")
            .is_none(),
        "至少保留 1 列：删最后一列会把表格改成非法结构"
    );
    assert!(controller
        .delete_table_column(0, 1)
        .expect("越界列不是错误")
        .is_none());
    assert_eq!(controller.source_text(), source);
    assert_eq!(controller.tables()[0].columns(), 1);
    assert_eq!(controller.object_source_edit_line(), None);
}

#[test]
fn delete_column_works_for_tables_without_data_rows() {
    let mut controller = open("| a | b |\n|---|---|\n\n后\n");
    let request = controller
        .delete_table_column(0, 0)
        .expect("删列不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(request.edits.len(), 2, "只有表头 + 分隔行");
    assert_eq!(controller.source_text(), "| b |\n|---|\n\n后\n");
    let facts = table_snapshot(&controller);
    assert_eq!(facts.columns, 1);
    assert_eq!(facts.rows, vec![vec!["b".to_owned()]], "只剩表头");
    assert!(controller.tables()[0].is_block());
}

#[test]
fn delete_actions_are_noops_on_non_table_lines_and_cell_editing() {
    let source = "![图](asset://abc)\n\n段落\n\n> | a | b |\n> |---|---|\n";
    let mut controller = open(source);
    for line in 0..6 {
        assert!(
            controller
                .delete_table_row(line, 1)
                .expect("拒绝必须不是错误")
                .is_none(),
            "行 {line} 不是整块表格，删行必须是 no-op"
        );
        assert!(
            controller
                .delete_table_column(line, 0)
                .expect("拒绝必须不是错误")
                .is_none(),
            "行 {line} 不是整块表格，删列必须是 no-op"
        );
    }
    assert_eq!(controller.source_text(), source);

    // 单元格编辑期间镜像领先于会话：结构删除必须被拒绝（与加行加列同一闸门）。
    let mut controller = open(ALIGNED_SOURCE);
    assert!(controller
        .begin_table_cell_edit(0, 1, 0)
        .expect("进入单元格编辑不应失败"));
    assert!(matches!(
        controller.delete_table_row(0, 1),
        Err(menghunote_editor::EditorError::CellEditActive)
    ));
    assert!(matches!(
        controller.delete_table_column(0, 1),
        Err(menghunote_editor::EditorError::CellEditActive)
    ));
    assert_eq!(controller.source_text(), ALIGNED_SOURCE);
}

#[test]
fn deleting_rows_and_columns_keeps_carets_outside_the_table() {
    // caret 在表格之前：删除都在它之后 → 完全不动（含选区）。
    let mut controller = open(THREE_ROWS);
    let before = controller.selection();
    controller
        .delete_table_row(2, 2)
        .expect("删行不应失败")
        .expect("必须产生请求");
    assert_eq!(controller.selection(), before, "表格之前的 caret 不得移动");

    // caret 在表格之后（文档末尾）：随删除量左移，仍在表格之外。
    let source = "| a | b |\n|---|---|\n| 1 | 2 |\n\n后文\n";
    let mut controller = open(source);
    let tail = SourceByte::new(source.len())
        .to_scalar(source)
        .expect("文档末尾是 scalar 边界");
    controller
        .set_selection(Selection::caret(tail))
        .expect("caret 必须合法");
    controller
        .delete_table_row(0, 1)
        .expect("删行不应失败")
        .expect("必须产生请求");
    assert_eq!(controller.source_text(), "| a | b |\n|---|---|\n\n后文\n");
    let expected = SourceByte::new(controller.source_text().len())
        .to_scalar(&controller.source_text())
        .expect("文档末尾是 scalar 边界");
    assert_eq!(
        controller.selection(),
        Selection::caret(expected),
        "表格之后的 caret 按删除量左移（文本位置不变）"
    );
}

#[test]
fn deleting_columns_and_rows_handles_crlf_tables() {
    let source = "| a | b |\r\n|---|---|\r\n| 1 | 2 |\r\n| 3 | 4 |\r\n";
    let mut controller = open(source);
    controller
        .delete_table_row(0, 1)
        .expect("CRLF 删行不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| a | b |\r\n|---|---|\r\n| 3 | 4 |\r\n",
        "CRLF 删行必须连 \\r\\n 一起删"
    );
    assert_eq!(table_snapshot(&controller).rows.len(), 2);

    let mut controller = open(source);
    controller
        .delete_table_column(0, 0)
        .expect("CRLF 删列不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| b |\r\n|---|\r\n| 2 |\r\n| 4 |\r\n",
        "CRLF 删列保留行尾，只删该列"
    );
    assert_eq!(table_snapshot(&controller).columns, 1);
}

// ---- ADR 0019 S3b4：右键「插入 → 表格」 ----

/// 默认 GFM 表格（2 列 × 1 条数据行，cell 全空）。
const DEFAULT_TABLE: &str = "|  |  |\n| --- | --- |\n|  |  |\n";

#[test]
fn insert_table_at_caret_is_one_request_and_becomes_an_object() {
    let mut controller = open("前\n\n后\n");
    let end = SourceByte::new(controller.source_text().len())
        .to_scalar(&controller.source_text())
        .expect("文档末尾是 scalar 边界");
    controller
        .set_selection(Selection::caret(end))
        .expect("caret 必须合法");

    let request = controller.insert_table().expect("插入表格不应失败");
    assert_eq!(request.edits.len(), 1, "插入表格必须是一条 ScalarEdit");
    assert_eq!(
        request.edits[0].start, request.edits[0].end,
        "空选区插入 = 纯插入（不替换任何字节）"
    );
    assert_eq!(
        controller.source_text(),
        format!("前\n\n后\n\n{DEFAULT_TABLE}")
    );
    let facts = table_snapshot(&controller);
    assert_eq!(facts.columns, 2);
    assert_eq!(facts.rows.len(), 2, "表头 + 1 条数据行");
    assert_eq!(facts.header, "|  |  |");
    assert_eq!(facts.delimiter, "| --- | --- |");
    assert_eq!(facts.rows[1], vec!["", ""], "数据行两个空 cell");
    assert_eq!(facts.kind, Some(ObjectKind::Table), "表格必须投影成 Object");
    assert_eq!(facts.object_line, Some(4));
    assert_eq!(facts.end_line, Some(6));
    assert_eq!(
        controller.object_source_edit_line(),
        None,
        "不得进入源码模式"
    );
    assert!(controller
        .display_text()
        .contains(menghunote_editor::OBJECT_PLACEHOLDER));
    // caret 落在表格之后（= 插入文本末尾 = 文档末尾），不落进表格源码内部。
    let new_end = SourceByte::new(controller.source_text().len())
        .to_scalar(&controller.source_text())
        .expect("文档末尾是 scalar 边界");
    assert_eq!(controller.selection(), Selection::caret(new_end));
}

#[test]
fn insert_table_into_empty_document_creates_an_object_on_line_zero() {
    let mut controller = open("");
    let request = controller.insert_table().expect("插入表格不应失败");
    assert_eq!(request.edits.len(), 1);
    assert_eq!(controller.source_text(), DEFAULT_TABLE);
    let facts = table_snapshot(&controller);
    assert_eq!((facts.object_line, facts.columns), (Some(0), 2));
    assert_eq!(facts.rows.len(), 2, "表头 + 1 条空数据行");
    assert_eq!(facts.rows[0], vec!["", ""]);
    assert_eq!(facts.rows[1], vec!["", ""]);
    assert_eq!(facts.kind, Some(ObjectKind::Table));
    assert!(controller.tables()[0].is_block());
}

#[test]
fn insert_table_replaces_the_selection_in_one_edit() {
    let mut controller = open("abcdef");
    controller
        .set_selection(Selection::new(UnicodeScalar::new(2), UnicodeScalar::new(4)))
        .expect("选区必须合法");
    let request = controller.insert_table().expect("插入表格不应失败");
    assert_eq!(request.edits.len(), 1);
    assert_eq!(request.edits[0].start, UnicodeScalar::new(2));
    assert_eq!(request.edits[0].end, UnicodeScalar::new(4));
    assert_eq!(
        controller.source_text(),
        format!("ab\n\n{DEFAULT_TABLE}\nef"),
        "选区被替换成表格；前后仍保证表格独占行块"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(facts.object_line, Some(2));
    assert_eq!(facts.kind, Some(ObjectKind::Table));
}

#[test]
fn insert_table_is_rejected_while_a_cell_is_being_edited() {
    let mut controller = open(ALIGNED_SOURCE);
    assert!(controller
        .begin_table_cell_edit(0, 1, 0)
        .expect("进入单元格编辑不应失败"));
    assert!(matches!(
        controller.insert_table(),
        Err(menghunote_editor::EditorError::CellEditActive)
    ));
    assert_eq!(controller.source_text(), ALIGNED_SOURCE);
}

// ---- ADR 0019 S3b5：拖拽重排行 / 列 ----

/// 3 条数据行 / 2 列（前后没有正文：重排逐字节断言更直观）。
const BARE_THREE_ROWS: &str = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n| 5 | 6 |\n";

#[test]
fn move_row_forward_is_one_edit_and_byte_exact() {
    let mut controller = open(BARE_THREE_ROWS);
    let before_selection = controller.selection();
    let request = controller
        .move_table_row(0, 0, 2)
        .expect("行重排不应失败")
        .expect("整块表格必须产生一次 EditRequest");
    assert_eq!(request.edits.len(), 1, "行重排是单条等长替换");
    let edit = &request.edits[0];
    assert!(edit.start < edit.end, "重排是替换（不是纯插入）");
    assert_eq!(
        menghunote_editor::units::scalar_len(&edit.replacement),
        edit.end.get() - edit.start.get(),
        "替换必须等长（纯顺序变化，caret 坐标因此不变）"
    );
    assert_eq!(
        controller.source_text(),
        "| a | b |\n|---|---|\n| 3 | 4 |\n| 5 | 6 |\n| 1 | 2 |\n",
        "首行移到末尾：表头/分隔行/其余行逐字节不变"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(facts.header, "| a | b |", "表头不动");
    assert_eq!(facts.delimiter, "|---|---|", "分隔行不动");
    assert_eq!(facts.rows.len(), 4, "表头 + 3 条数据行");
    assert_eq!(facts.rows[1], vec!["3".to_owned(), "4".to_owned()]);
    assert_eq!(facts.rows[3], vec!["1".to_owned(), "2".to_owned()]);
    assert_eq!(facts.kind, Some(ObjectKind::Table), "表格仍是 Object");
    assert_eq!(
        controller.object_source_edit_line(),
        None,
        "不得进入源码模式"
    );
    assert!(controller
        .display_text()
        .contains(menghunote_editor::OBJECT_PLACEHOLDER));
    assert_eq!(
        controller.selection(),
        before_selection,
        "表格之前的 caret 一动不动"
    );
}

#[test]
fn move_row_backward_and_middle_are_byte_exact() {
    let mut controller = open(BARE_THREE_ROWS);
    controller
        .move_table_row(0, 2, 0)
        .expect("行重排不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| a | b |\n|---|---|\n| 5 | 6 |\n| 1 | 2 |\n| 3 | 4 |\n",
        "末行移到最前"
    );

    let mut controller = open(BARE_THREE_ROWS);
    controller
        .move_table_row(0, 1, 2)
        .expect("行重排不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| a | b |\n|---|---|\n| 1 | 2 |\n| 5 | 6 |\n| 3 | 4 |\n",
        "中间行移到末尾"
    );
    assert_eq!(
        controller.selection(),
        Selection::caret(UnicodeScalar::new(0)),
        "caret 仍在文档开头（等长重排不移动任何坐标）"
    );
}

#[test]
fn move_row_at_document_end_does_not_add_a_line_ending() {
    let source = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
    let mut controller = open(source);
    controller
        .move_table_row(0, 0, 1)
        .expect("行重排不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| a | b |\n|---|---|\n| 3 | 4 |\n| 1 | 2 |",
        "末行没有行尾换行时不能凭空补一个"
    );
    assert_eq!(controller.source_text().len(), source.len());
}

#[test]
fn move_row_keeps_caret_after_the_table_in_place() {
    let source = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\n后文\n";
    let mut controller = open(source);
    let tail = source.find("\n\n后文").expect("后文位置");
    set_caret(&mut controller, tail);
    let before = controller.selection();
    controller
        .move_table_row(0, 0, 1)
        .expect("行重排不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.selection(),
        before,
        "表格之后的 caret 不得因为重排移动（等长置换）"
    );
    assert_eq!(
        controller.source_text(),
        "| a | b |\n|---|---|\n| 3 | 4 |\n| 1 | 2 |\n\n后文\n"
    );

    // 表格末尾（对象内部没有落点，这里是最靠近对象的合法位置）：同样不动。
    let mut controller = open(source);
    set_caret(&mut controller, tail);
    let before = controller.selection();
    controller
        .move_table_row(0, 1, 0)
        .expect("行重排不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(controller.selection(), before);
}

#[test]
fn move_column_is_one_request_with_one_edit_per_row() {
    let mut controller = open(ALIGNED_SOURCE);
    let before_selection = controller.selection();
    let request = controller
        .move_table_column(0, 1, 0)
        .expect("列重排不应失败")
        .expect("整块表格必须产生一次 EditRequest");
    assert_eq!(request.edits.len(), 4, "表头 + 分隔行 + 2 条数据行各一条");
    assert!(
        request
            .edits
            .windows(2)
            .all(|pair| pair[0].end.get() <= pair[1].start.get()),
        "替换必须按位置升序、互不重叠（整批一个撤销组）"
    );
    for edit in &request.edits {
        assert_eq!(
            menghunote_editor::units::scalar_len(&edit.replacement),
            edit.end.get() - edit.start.get(),
            "每条替换都等长"
        );
    }
    assert_eq!(
        controller.source_text(),
        "| 数量 | 名称 | 备注 |\n|:-:|:--|--:|\n| 3 | 苹果 | 红 |\n| 5 | 香蕉 | 黄 |\n",
        "cell 文本与对齐标记一起移动；其它字节逐字保留"
    );
    let facts = table_snapshot(&controller);
    assert_eq!(
        facts.alignments,
        vec![
            menghunote_editor::MarkdownTableAlignment::Center,
            menghunote_editor::MarkdownTableAlignment::Left,
            menghunote_editor::MarkdownTableAlignment::Right,
        ],
        "对齐跟着列走"
    );
    assert_eq!(facts.rows[1], vec!["3", "苹果", "红"], "数据行同步重排");
    assert_eq!(facts.kind, Some(ObjectKind::Table));
    assert_eq!(
        controller.object_source_edit_line(),
        None,
        "不得进入源码模式"
    );
    assert_eq!(controller.selection(), before_selection, "caret 不变");
}

#[test]
fn move_column_first_and_last_are_byte_exact() {
    let mut controller = open(ALIGNED_SOURCE);
    controller
        .move_table_column(0, 0, 2)
        .expect("列重排不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| 数量 | 备注 | 名称 |\n|:-:|--:|:--|\n| 3 | 红 | 苹果 |\n| 5 | 黄 | 香蕉 |\n",
        "首列移到末列"
    );

    let mut controller = open(ALIGNED_SOURCE);
    controller
        .move_table_column(0, 2, 0)
        .expect("列重排不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| 备注 | 名称 | 数量 |\n|--:|:--|:-:|\n| 红 | 苹果 | 3 |\n| 黄 | 香蕉 | 5 |\n",
        "末列移到首列"
    );
}

#[test]
fn move_row_and_column_handle_crlf_tables() {
    let source = "| 名称 | 数量 |\r\n|:--|--:|\r\n| 苹果 | 3 |\r\n| 香蕉 | 5 |\r\n";
    let mut controller = open(source);
    controller
        .move_table_row(0, 0, 1)
        .expect("CRLF 行重排不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| 名称 | 数量 |\r\n|:--|--:|\r\n| 香蕉 | 5 |\r\n| 苹果 | 3 |\r\n",
        "CRLF 重排行：行尾留在槽位"
    );

    let mut controller = open(source);
    controller
        .move_table_column(0, 0, 1)
        .expect("CRLF 列重排不应失败")
        .expect("必须产生一次 EditRequest");
    assert_eq!(
        controller.source_text(),
        "| 数量 | 名称 |\r\n|--:|:--|\r\n| 3 | 苹果 |\r\n| 5 | 香蕉 |\r\n",
        "CRLF 重排列：行尾 \\r\\n 逐字节保留"
    );
    let after = controller.source_text();
    assert!(
        !after.replace("\r\n", "").contains('\n'),
        "CRLF 文档里不得出现孤立 LF"
    );
}

#[test]
fn move_actions_are_noops_on_non_table_lines_out_of_range_and_in_place() {
    let source = "段落\n\n> | a | b |\n> |---|---|\n";
    let mut controller = open(source);
    for line in 0..4 {
        assert!(
            controller
                .move_table_row(line, 0, 1)
                .expect("拒绝必须不是错误")
                .is_none(),
            "行 {line} 不是整块表格，重排行必须是 no-op"
        );
        assert!(controller
            .move_table_column(line, 0, 1)
            .expect("拒绝必须不是错误")
            .is_none());
    }
    assert_eq!(controller.source_text(), source);

    let mut controller = open(BARE_THREE_ROWS);
    assert!(controller
        .move_table_row(0, 1, 1)
        .expect("原地不是错误")
        .is_none());
    assert!(controller
        .move_table_row(0, 0, 3)
        .expect("越界不是错误")
        .is_none());
    assert!(controller
        .move_table_column(0, 0, 0)
        .expect("原地不是错误")
        .is_none());
    assert_eq!(controller.source_text(), BARE_THREE_ROWS);
    assert_eq!(controller.object_source_edit_line(), None);

    // 只有 1 条数据行 / 1 列：没有可换的位置。
    let mut controller = open("| a | b |\n|---|---|\n| 1 | 2 |\n");
    assert!(controller
        .move_table_row(0, 0, 0)
        .expect("不是错误")
        .is_none());
    let mut controller = open("| a |\n|---|\n| 1 |\n");
    assert!(controller
        .move_table_column(0, 0, 0)
        .expect("不是错误")
        .is_none());
}

#[test]
fn move_actions_are_rejected_while_a_cell_is_being_edited() {
    let mut controller = open(ALIGNED_SOURCE);
    assert!(controller
        .begin_table_cell_edit(0, 1, 0)
        .expect("进入单元格编辑不应失败"));
    assert!(matches!(
        controller.move_table_row(0, 0, 1),
        Err(menghunote_editor::EditorError::CellEditActive)
    ));
    assert!(matches!(
        controller.move_table_column(0, 0, 1),
        Err(menghunote_editor::EditorError::CellEditActive)
    ));
    assert_eq!(controller.source_text(), ALIGNED_SOURCE);
}

#[test]
fn two_moves_in_a_row_are_two_independent_requests() {
    let mut controller = open(BARE_THREE_ROWS);
    let first = controller
        .move_table_row(0, 0, 2)
        .expect("行重排不应失败")
        .expect("必须产生一次 EditRequest");
    confirm(&mut controller, &first, 8);
    assert_eq!(
        controller.source_text(),
        "| a | b |\n|---|---|\n| 3 | 4 |\n| 5 | 6 |\n| 1 | 2 |\n"
    );
    // 第二次移动基于新顺序：把中间行移到最前。
    let second = controller
        .move_table_row(0, 1, 0)
        .expect("行重排不应失败")
        .expect("必须产生一次 EditRequest");
    assert_ne!(first.command_id, second.command_id, "两次移动是两个撤销组");
    assert_eq!(
        controller.source_text(),
        "| a | b |\n|---|---|\n| 5 | 6 |\n| 3 | 4 |\n| 1 | 2 |\n"
    );
}
