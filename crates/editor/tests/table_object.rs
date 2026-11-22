//! ADR 0019 S3a：表格 **Object Block** 的解析 / 投影 / 端点偏置 / 编辑源码切换 /
//! 删除跨度 / 非独占表格保持源码。
//!
//! 只覆盖纯模型（`menghunote-editor`）：源码零改写、display 占位符、双向映射与
//! 一次 `EditRequest` 的删除语义。像素/几何在 app 的 headless 测试里。

mod support;

use menghunote_editor::{
    Bias, DisplayByte, EditorController, MarkdownDocument, ObjectKind, ProjectionMap, Selection,
    SourceByte, SourceRange, UnicodeScalar, OBJECT_PLACEHOLDER,
};

/// 2×2 表格：前文、空行、表格（3 行）、空行、后文。
const TABLE_SOURCE: &str = "前\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n后\n";
/// 表格起始字节（`| a | b |` 的 `|`）。
const TABLE_START: usize = 5;

fn parse(source: &str) -> MarkdownDocument {
    MarkdownDocument::parse(source).expect("Markdown 解析不应失败")
}

fn open(source: &str) -> EditorController {
    EditorController::open(support::snapshot(source)).expect("测试快照必须可打开")
}

fn project(doc: &MarkdownDocument, caret: usize) -> ProjectionMap {
    ProjectionMap::from_markdown_with_lists(doc, Selection::caret(UnicodeScalar::new(caret)))
}

fn range(start: usize, end: usize) -> SourceRange {
    SourceRange::new(SourceByte::new(start), SourceByte::new(end), usize::MAX)
        .expect("测试区间端点必须合法")
}

/// 表格源码末尾（`TABLE_SOURCE` 里表格之后是空行 + `后`）。
fn table_end() -> usize {
    TABLE_SOURCE.find("\n\n后").expect("后文位置")
}

#[test]
fn table_records_rows_cells_alignment_and_block_object() {
    let doc = parse(TABLE_SOURCE);
    assert_eq!(doc.tables().len(), 1);
    let table = &doc.tables()[0];
    assert!(table.is_block(), "顶层 2×2 表格必须是 Object Block");
    assert_eq!(table.line(), 2, "表头行 = 第 3 行（0 起）");
    assert_eq!(table.end_line(), 4);
    assert_eq!(table.columns(), 2);
    assert_eq!(
        (table.span().start().get(), table.span().end().get()),
        (TABLE_START, table_end())
    );
    assert_eq!(
        &TABLE_SOURCE[table.span().start().get()..table.span().end().get()],
        "| a | b |\n|---|---|\n| 1 | 2 |"
    );
    assert_eq!(table.row_count(), 1);
    assert_eq!(table.header().cells().len(), 2);
    assert_eq!(table.rows()[0].cells().len(), 2);

    // 统一对象列表：恰好一个 Table 对象，索引指回 tables()[0]。
    let objects = doc.objects();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].kind(), ObjectKind::Table);
    assert_eq!(objects[0].line(), 2);
    assert_eq!(objects[0].span(), table.span());
    assert_eq!(
        doc.object_table(&objects[0]).map(|t| t.span()),
        Some(table.span())
    );
    assert!(doc.object_image(&objects[0]).is_none(), "表格不是图片对象");
    assert_eq!(
        doc.object_at_line(2).map(|object| object.kind()),
        Some(ObjectKind::Table)
    );
    assert!(doc.object_at_line(3).is_none(), "对象行只有表头行");
    assert!(doc.object_at_line(0).is_none());
}

#[test]
fn alignment_and_escaped_pipe_survive_to_editor_spans() {
    let source = "|a|b|c|\n|:--|:-:|--:|\n| c\\|d | `x` | **y** |\n";
    let doc = parse(source);
    let table = &doc.tables()[0];
    assert_eq!(table.columns(), 3);
    use menghunote_editor::MarkdownTableAlignment as Alignment;
    assert_eq!(
        table.alignments(),
        &[Alignment::Left, Alignment::Center, Alignment::Right]
    );
    assert_eq!(table.alignment(0), Alignment::Left);
    assert_eq!(table.alignment(7), Alignment::None, "越界按默认对齐");
    let row = table.row(0).expect("一行数据");
    assert_eq!(
        source.get(row.cell(0).expect("第 1 格").start().get()..row.cell(0).unwrap().end().get()),
        Some("c\\|d"),
        "escaped `|` 属于 cell 原文"
    );
    // 表格内部行内标记不投影（整表是原子对象）。
    let map = project(&doc, 0);
    let display = map.display_text();
    assert!(
        !display.contains("**y**"),
        "表格内的 `**y**` 不应显示成构造：{display:?}"
    );
}

#[test]
fn block_table_projects_one_object_with_placeholder() {
    let doc = parse(TABLE_SOURCE);
    let map = project(&doc, TABLE_SOURCE.chars().count());
    let expected = format!("前\n\n{OBJECT_PLACEHOLDER}\n\n后\n");
    assert_eq!(map.display_text().as_ref(), expected);
    let objects = map.object_segments();
    assert_eq!(objects.len(), 1, "整表一个 Object 段");
    assert_eq!(objects[0].0, doc.tables()[0].span());
    assert_eq!(
        objects[0].1.end - objects[0].1.start,
        menghunote_editor::OBJECT_PLACEHOLDER_LEN
    );
}

#[test]
fn table_object_endpoint_biases_are_pinned() {
    let doc = parse(TABLE_SOURCE);
    let map = project(&doc, TABLE_SOURCE.chars().count());
    let table = &doc.tables()[0];
    let display_start = 5usize; // "前\n\n" 之后
    let display_end = display_start + OBJECT_PLACEHOLDER.len();

    // source → display：块内部（含起点）收敛到占位符起点；块末尾到占位符末尾。
    for source in [
        table.span().start().get(),
        table.span().start().get() + 1,
        table.span().end().get() - 1,
    ] {
        assert_eq!(
            map.display_byte_at_source(SourceByte::new(source)),
            Some(DisplayByte::new(display_start)),
            "source {source} 在表格内部，应映射到占位符起点"
        );
    }
    assert_eq!(
        map.display_byte_at_source(SourceByte::new(table.span().end().get())),
        Some(DisplayByte::new(display_end))
    );

    // display → source：占位符内部（含起点）只能给出块起点（不落进单元格）。
    for display in [display_start, display_start + 1, display_end - 1] {
        assert_eq!(
            map.source_byte_at_display(DisplayByte::new(display)),
            Some(SourceByte::new(table.span().start().get()))
        );
        assert_eq!(
            map.source_byte_at_display_biased(DisplayByte::new(display), Bias::Left),
            Some(SourceByte::new(table.span().start().get()))
        );
    }
    assert_eq!(
        map.source_byte_at_display(DisplayByte::new(display_end)),
        Some(SourceByte::new(table.span().end().get()))
    );
}

#[test]
fn selection_covering_the_table_copies_complete_markdown() {
    let doc = parse(TABLE_SOURCE);
    let map = project(&doc, TABLE_SOURCE.chars().count());
    let table = &doc.tables()[0];
    let display_start = 5usize;
    let display_end = display_start + OBJECT_PLACEHOLDER.len();
    let copied = map
        .source_range_of_display_range(
            DisplayByte::new(display_start)..DisplayByte::new(display_end),
        )
        .expect("选区必须能反查 source");
    assert_eq!(
        (copied.start().get(), copied.end().get()),
        (table.span().start().get(), table.span().end().get()),
        "复制必须得到完整表格 Markdown（表头/分隔行/所有数据行）"
    );
    // 选区在占位符之前结束 → 不包含表格。
    let before = map
        .source_range_of_display_range(DisplayByte::new(0)..DisplayByte::new(display_start))
        .expect("选区必须能反查 source");
    assert_eq!(before.end().get(), table.span().start().get());
}

#[test]
fn text_selection_intersecting_the_table_does_not_reveal_source() {
    let mut controller = open(TABLE_SOURCE);
    controller
        .set_selection(Selection::new(
            UnicodeScalar::new(0),
            UnicodeScalar::new(TABLE_SOURCE.chars().count() - 1),
        ))
        .expect("选区必须合法");
    let display = controller.display_text();
    assert!(
        display.contains(OBJECT_PLACEHOLDER),
        "表格必须仍是占位符：{display:?}"
    );
    assert!(
        !display.contains("|---|---|") && !display.contains("| a | b |"),
        "文本选区相交不得显示表格源码：{display:?}"
    );
    assert_eq!(controller.object_lines_in_display_selection(), vec![2]);
    // 折叠 caret 不算相交。
    controller
        .set_caret(UnicodeScalar::new(0))
        .expect("caret 合法");
    assert!(controller.object_lines_in_display_selection().is_empty());
}

#[test]
fn table_source_edit_toggles_raw_source_and_restores_the_object() {
    let mut controller = open(TABLE_SOURCE);
    assert_eq!(controller.object_source_edit_line(), None);
    assert!(controller.display_text().contains(OBJECT_PLACEHOLDER));

    assert!(
        controller
            .begin_object_source_edit(2)
            .expect("进入源码模式不应失败"),
        "整块表格必须能进入源码模式"
    );
    assert_eq!(controller.object_source_edit_line(), Some(2));
    let display = controller.display_text();
    assert!(
        display.contains("| a | b |\n|---|---|\n| 1 | 2 |"),
        "源码模式必须显示整张表：{display:?}"
    );
    assert!(
        !display.contains(OBJECT_PLACEHOLDER),
        "源码模式下不再有 Object 占位符：{display:?}"
    );
    assert_eq!(
        controller.selection().head(),
        SourceByte::new(TABLE_START)
            .to_scalar(&controller.source_text())
            .expect("起点是 scalar 边界"),
        "caret 落在表格源码起点"
    );
    assert!(controller.projection().object_segments().is_empty());

    assert!(
        controller.end_object_source_edit(),
        "退出源码模式必须返回 true"
    );
    assert_eq!(controller.object_source_edit_line(), None);
    assert!(controller.display_text().contains(OBJECT_PLACEHOLDER));
    assert_eq!(
        controller.selection().head(),
        SourceByte::new(table_end())
            .to_scalar(&controller.source_text())
            .expect("末尾是 scalar 边界"),
        "退出后 caret 落在表格之后（不是表格源码中间）"
    );
    assert!(!controller.end_object_source_edit(), "再次退出是 no-op");
}

#[test]
fn clicking_inside_a_table_cannot_place_a_text_caret_inside_cells() {
    // 对象内部没有文本位置：display 上占位符内部的任何点都映射回块起点，
    // app 的单击路径因此只能“选中整块”，不可能把 caret 放进某个单元格。
    let doc = parse(TABLE_SOURCE);
    let map = project(&doc, TABLE_SOURCE.chars().count());
    let display_start = 5usize;
    for offset in 0..OBJECT_PLACEHOLDER.len() {
        let source = map
            .source_byte_at_display(DisplayByte::new(display_start + offset))
            .expect("display 必须可映射");
        assert_eq!(source.get(), TABLE_START);
    }
}

#[test]
fn deleting_the_table_removes_the_whole_source_span_in_one_request() {
    let mut controller = open(TABLE_SOURCE);
    let request = controller
        .delete_object(2)
        .expect("删除对象不应失败")
        .expect("删除表格必须产生一次 EditRequest");
    assert_eq!(request.edits.len(), 1, "整块删除必须是单个 ScalarEdit");
    let edit = &request.edits[0];
    assert_eq!(edit.replacement, "");
    assert_eq!(
        edit.start,
        SourceByte::new(TABLE_START)
            .to_scalar(TABLE_SOURCE)
            .expect("起点是 scalar 边界")
    );
    assert_eq!(
        edit.end,
        SourceByte::new(table_end())
            .to_scalar(TABLE_SOURCE)
            .expect("末尾是 scalar 边界")
    );
    assert_eq!(controller.source_text(), "前\n\n\n\n后\n");
    assert!(controller.tables().is_empty(), "删除后不应再解析出表格");
}

#[test]
fn table_source_edit_exits_when_the_table_syntax_is_destroyed() {
    let mut controller = open(TABLE_SOURCE);
    assert!(controller
        .begin_object_source_edit(2)
        .expect("进入源码模式"));
    // 把分隔行改成普通文本：表格消失，源码模式必须自动退出（不再有对象）。
    let text = controller.source_text();
    let delimiter = TABLE_SOURCE.find("|---|---|").expect("分隔行位置");
    let start = SourceByte::new(delimiter)
        .to_scalar(&text)
        .expect("起点是 scalar 边界");
    let end = SourceByte::new(delimiter + 9)
        .to_scalar(&text)
        .expect("分隔行是 ASCII");
    controller
        .set_selection(Selection::new(start, end))
        .expect("选区合法");
    controller
        .compose_replace_selection("not a table")
        .expect("替换分隔行必须成功");
    assert_eq!(controller.object_source_edit_line(), None);
    assert!(controller.tables().is_empty());
    assert!(!controller.display_text().contains(OBJECT_PLACEHOLDER));
}

#[test]
fn quoted_listed_and_indented_tables_stay_source_only() {
    for (source, reason) in [
        ("> | a | b |\n> |---|---|\n> | 1 | 2 |\n", "引用里"),
        ("- | a | b |\n  |---|---|\n  | 1 | 2 |\n", "列表项里"),
        ("  | a | b |\n  |---|---|\n  | 1 | 2 |\n", "行首缩进"),
    ] {
        let doc = parse(source);
        assert_eq!(doc.tables().len(), 1, "{reason}：仍然必须解析出表格");
        assert!(!doc.tables()[0].is_block(), "{reason}：不能当 Object Block");
        assert!(doc.objects().is_empty(), "{reason}：不得产生 Object");
        let map = project(&doc, source.chars().count());
        let display = map.display_text();
        assert!(
            !display.contains(OBJECT_PLACEHOLDER),
            "{reason}：不得投影成对象：{display:?}"
        );
        assert!(
            display.contains("| 1 | 2 |") || display.contains("| a | b |"),
            "{reason}：表格源码必须原样可见：{display:?}"
        );
        assert!(map.object_segments().is_empty());
    }
}

#[test]
fn table_object_coexists_with_lists_images_and_task_boxes() {
    // 表格不得破坏其它块：列表 bullet 仍是 Replace、任务框仍在、图片仍是独立对象。
    let source = "- [ ] 任务\n\n![图](asset://aaaa)\n\n| a |\n|---|\n| b |\n\n- 普通\n";
    let doc = parse(source);
    assert_eq!(doc.tables().len(), 1);
    assert_eq!(doc.images().len(), 1);
    let objects = doc.objects();
    assert_eq!(objects.len(), 2, "图片 + 表格：{objects:?}");
    assert_eq!(objects[0].kind(), ObjectKind::Image);
    assert_eq!(objects[1].kind(), ObjectKind::Table);
    let map = project(&doc, source.chars().count());
    let display = map.display_text();
    assert_eq!(
        display.matches(OBJECT_PLACEHOLDER).count(),
        2,
        "两个对象各一个占位符：{display:?}"
    );
    assert!(
        display.contains("• "),
        "列表 bullet 仍被 Replace：{display:?}"
    );
    assert!(
        !display.contains("- [ ]"),
        "caret 不在任务框上时 marker/任务框都隐藏：{display:?}"
    );
    // 表格内部的 `a`/`b` 不应作为正文出现（整表被占位）。
    assert!(!display.contains("| a |"), "{display:?}");
}

#[test]
fn object_ranges_are_sorted_and_do_not_overlap() {
    let source = "| a |\n|---|\n| b |\n\n![x](asset://y)\n\n| c |\n|---|\n";
    let doc = parse(source);
    assert_eq!(doc.objects().len(), 3);
    let mut previous_end = 0usize;
    for object in doc.objects() {
        assert!(
            object.span().start().get() >= previous_end,
            "对象区间必须有序且不重叠：{object:?}"
        );
        assert_eq!(
            object.span(),
            range(object.span().start().get(), object.span().end().get())
        );
        previous_end = object.span().end().get();
    }
    assert_eq!(
        doc.objects()
            .iter()
            .map(|object| object.kind())
            .collect::<Vec<_>>(),
        vec![ObjectKind::Table, ObjectKind::Image, ObjectKind::Table]
    );
}

#[test]
fn table_row_exposes_parsed_inline_markdown_per_cell() {
    // ADR 0019 S3b7：cell 源码在解析文档时**一次性**变成行内 Markdown
    // （display 文本 + 样式段 + 链接），绘制/命中只借用，不每帧重解析。
    let source = "| **粗** | `a\\|b` |\n|---|---|\n| [文档](https://x.y) | 平 |\n";
    let doc = parse(source);
    let table = &doc.tables()[0];
    let header = table.header();
    assert_eq!(header.cell_inline(0).expect("表头第 1 格").display(), "粗");
    assert_eq!(
        header.cell_inline(0).expect("表头第 1 格").spans()[0].kind,
        menghunote_editor::TableCellStyle::Bold
    );
    assert_eq!(
        header.cell_inline(1).expect("表头第 2 格").display(),
        "a|b",
        "行内代码里的 `\\|` 还原成字面 `|`"
    );
    let row = &table.rows()[0];
    let link = &row.cell_inline(0).expect("数据行第 1 格").links()[0];
    assert_eq!(link.dest, "https://x.y");
    assert_eq!(
        &row.cell_inline(0).expect("数据行第 1 格").display()[link.display.start..link.display.end],
        "文档"
    );
    assert_eq!(row.cell_inline(1).expect("数据行第 2 格").display(), "平");
    // 越界不 panic。
    assert!(row.cell_inline(9).is_none());
    // 源码零改写：cell 区间仍是原始切片（`\|` 与标记都在）。
    assert_eq!(
        &source[row.cells()[0].start().get()..row.cells()[0].end().get()],
        "[文档](https://x.y)"
    );
}
