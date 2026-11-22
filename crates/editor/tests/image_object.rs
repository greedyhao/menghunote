//! ADR 0019 S2a：图片 **Object Block** 的解析 / 投影 / 端点偏置 / 编辑切换 /
//! 删除跨度 / 复制源文。
//!
//! 只覆盖纯模型（`menghunote-editor`）：源码零改写、display 占位符、双向映射与
//! 一次 `EditRequest` 的删除语义。像素/几何在 app 的 headless 测试里。

mod support;

use menghunote_editor::{
    DisplayByte, EditorController, MarkdownDocument, ProjectionMap, Selection, SourceByte,
    SourceRange, UnicodeScalar, OBJECT_PLACEHOLDER,
};

/// 一张整行独占的图片：行 0 前文、行 1 空行、行 2 图片、行 3 空行、行 4 后文。
const BLOCK_SOURCE: &str = "前\n\n![图](asset://aaaa)\n\n后\n";
/// 图片 span 在 `BLOCK_SOURCE` 里的字节区间。
const IMAGE_SPAN: (usize, usize) = (5, 25);
/// 图片行号。
const IMAGE_LINE: usize = 2;

fn parse(source: &str) -> MarkdownDocument {
    MarkdownDocument::parse(source).expect("Markdown 解析不应失败")
}

fn project(doc: &MarkdownDocument, caret: usize) -> ProjectionMap {
    ProjectionMap::from_markdown_with_lists(doc, Selection::caret(UnicodeScalar::new(caret)))
}

fn open(source: &str) -> EditorController {
    EditorController::open(support::snapshot(source)).expect("测试快照必须可打开")
}

fn range(start: usize, end: usize) -> SourceRange {
    SourceRange::new(SourceByte::new(start), SourceByte::new(end), usize::MAX)
        .expect("测试区间端点必须合法")
}

#[test]
fn block_image_records_span_alt_dest_and_line() {
    let doc = parse(BLOCK_SOURCE);
    assert_eq!(doc.images().len(), 1);
    let image = &doc.images()[0];
    assert!(image.is_block(), "整行独占的图片必须是 Object Block 候选");
    assert_eq!(image.line(), IMAGE_LINE);
    assert_eq!(
        (image.span().start().get(), image.span().end().get()),
        IMAGE_SPAN
    );
    assert_eq!(
        &BLOCK_SOURCE[image.alt().start().get()..image.alt().end().get()],
        "图"
    );
    assert_eq!(image.dest(), "asset://aaaa");
    assert_eq!(
        doc.block_image_at_line(IMAGE_LINE)
            .map(|image| image.span()),
        Some(range(IMAGE_SPAN.0, IMAGE_SPAN.1))
    );
    assert!(doc.block_image_at_line(0).is_none());
}

#[test]
fn trailing_spaces_after_the_image_still_allow_a_block_object() {
    // 行尾空格是合法的硬换行残留：允许，且不产生错位（display 里空格留在占位符之后）。
    let source = "![a](asset://b)  \n";
    let doc = parse(source);
    assert!(doc.images()[0].is_block());
}

#[test]
fn inline_and_mixed_images_stay_source_only() {
    // 首版明确选择：混排/行内图片**保持源码原样显示**，绝不原子替换半行内容。
    for (source, reason) in [
        ("前 ![a](asset://b) 后\n", "同一行有其它文字"),
        (" ![a](asset://b)\n", "行首缩进"),
        ("- ![a](asset://b)\n", "列表项里"),
        ("> ![a](asset://b)\n", "引用里"),
        ("# ![a](asset://b)\n", "标题里"),
        ("| ![a](asset://b) |\n| --- |\n", "表格里"),
    ] {
        let doc = parse(source);
        assert_eq!(
            doc.images().len(),
            1,
            "{reason}：必须仍解析出图片 {source:?}"
        );
        assert!(!doc.images()[0].is_block(), "{reason}：不能当 Object Block");
        let map = project(&doc, source.chars().count());
        let display = map.display_text();
        assert!(
            display.contains("![a](asset://b)"),
            "{reason}：图片源码必须原样可见：{display:?}"
        );
        assert!(
            !display.contains(OBJECT_PLACEHOLDER),
            "{reason}：混排/行内图片不得投影成对象：{display:?}"
        );
        assert!(
            map.object_segments().is_empty(),
            "{reason}：不得产生 Object 段"
        );
    }

    // 围栏代码里的 `![...]` 是字面文本，pulldown 根本不发图片事件：源码原样显示。
    let fenced = "```\n![a](asset://b)\n```\n";
    let doc = parse(fenced);
    assert!(doc.images().is_empty(), "围栏代码内不得解析出图片");
    assert_eq!(project(&doc, 0).display_text().as_ref(), fenced);
}

#[test]
fn block_image_projects_an_object_segment_with_placeholder() {
    let doc = parse(BLOCK_SOURCE);
    let map = project(&doc, BLOCK_SOURCE.chars().count());
    // 前两行 + 占位符 + 后续空行/后文。
    let expected = format!("前\n\n{OBJECT_PLACEHOLDER}\n\n后\n");
    assert_eq!(map.display_text().as_ref(), expected);
    assert_eq!(map.source_len(), BLOCK_SOURCE.len());
    assert_eq!(map.display_len().get(), expected.len());

    // 段表里恰好有一段 Object，source 是完整图片跨度。
    let objects = map.object_segments();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].0, range(IMAGE_SPAN.0, IMAGE_SPAN.1));
    assert_eq!(
        objects[0].1.end - objects[0].1.start,
        menghunote_editor::OBJECT_PLACEHOLDER_LEN
    );
    assert!(map
        .segments()
        .iter()
        .any(|segment| segment.kind() == menghunote_editor::SegmentKind::Object));
}

#[test]
fn object_endpoint_biases_are_pinned() {
    let doc = parse(BLOCK_SOURCE);
    let map = project(&doc, BLOCK_SOURCE.chars().count());
    let display_start = 5usize; // "前\n\n" 之后
    let display_end = display_start + OBJECT_PLACEHOLDER.len();

    // source → display：对象内部（含起点）收敛到占位符起点；对象末尾到占位符末尾。
    for source in [IMAGE_SPAN.0, IMAGE_SPAN.0 + 1, IMAGE_SPAN.1 - 1] {
        assert_eq!(
            map.display_byte_at_source(SourceByte::new(source)),
            Some(DisplayByte::new(display_start)),
            "source {source} 在对象内部，应映射到占位符起点"
        );
    }
    assert_eq!(
        map.display_byte_at_source(SourceByte::new(IMAGE_SPAN.1)),
        Some(DisplayByte::new(display_end)),
        "对象末尾（source end）应映射到占位符末尾"
    );

    // display → source：占位符内部（含起点）都给出对象起点；占位符末尾给出对象末尾。
    for display in [display_start, display_start + 1, display_end - 1] {
        assert_eq!(
            map.source_byte_at_display(DisplayByte::new(display)),
            Some(SourceByte::new(IMAGE_SPAN.0)),
            "display {display} 在占位符内部，应映射到对象起点"
        );
        assert_eq!(
            map.source_byte_at_display_biased(
                DisplayByte::new(display),
                menghunote_editor::Bias::Left
            ),
            Some(SourceByte::new(IMAGE_SPAN.0)),
            "Left 偏置同样不能落进对象源码中间（display {display}）"
        );
    }
    assert_eq!(
        map.source_byte_at_display(DisplayByte::new(display_end)),
        Some(SourceByte::new(IMAGE_SPAN.1)),
        "占位符末尾必须映射到对象末尾"
    );
    // 文档尾 / 越界：不 panic、明确 None。
    assert_eq!(
        map.source_byte_at_display(DisplayByte::new(map.display_len().get())),
        Some(SourceByte::new(map.source_len()))
    );
    assert_eq!(
        map.source_byte_at_display(DisplayByte::new(map.display_len().get() + 1)),
        None
    );
    assert_eq!(
        map.display_byte_at_source(SourceByte::new(map.source_len() + 1)),
        None
    );
}

#[test]
fn display_selection_covering_the_object_copies_complete_markdown() {
    let doc = parse(BLOCK_SOURCE);
    let map = project(&doc, BLOCK_SOURCE.chars().count());
    let display_start = 5usize;
    let display_end = display_start + OBJECT_PLACEHOLDER.len();

    // 完整覆盖占位符 → source 是完整 `![图](asset://aaaa)`。
    let copied = map
        .source_range_of_display_range(
            DisplayByte::new(display_start)..DisplayByte::new(display_end),
        )
        .expect("选区必须能反查 source");
    assert_eq!(
        (copied.start().get(), copied.end().get()),
        IMAGE_SPAN,
        "复制必须得到完整 Markdown（不得漏掉 `![...](...)`）"
    );
    assert_eq!(
        &BLOCK_SOURCE[copied.start().get()..copied.end().get()],
        "![图](asset://aaaa)"
    );

    // 选区在占位符之前结束 → 不包含对象。
    let before = map
        .source_range_of_display_range(DisplayByte::new(0)..DisplayByte::new(display_start))
        .expect("选区必须能反查 source");
    assert_eq!(
        (before.start().get(), before.end().get()),
        (0, IMAGE_SPAN.0)
    );

    // 选区从占位符末尾开始 → 不含对象。
    let after = map
        .source_range_of_display_range(
            DisplayByte::new(display_end)..DisplayByte::new(map.display_len().get()),
        )
        .expect("选区必须能反查 source");
    assert_eq!(after.start().get(), IMAGE_SPAN.1);

    // 端点倒序 / 越界：None（不 panic）。
    assert!(map
        .source_range_of_display_range(
            DisplayByte::new(display_end)..DisplayByte::new(display_start)
        )
        .is_none());
    assert!(map
        .source_range_of_display_range(
            DisplayByte::new(0)..DisplayByte::new(map.display_len().get() + 1)
        )
        .is_none());
}

#[test]
fn text_selection_intersecting_the_object_does_not_reveal_source() {
    let mut controller = open(BLOCK_SOURCE);
    // 选中从"前"到"后"（跨过图片）：对象仍显示为占位符，不展开源码。
    let start = UnicodeScalar::new(0);
    let end = UnicodeScalar::new(BLOCK_SOURCE.chars().count() - 1);
    controller
        .set_selection(Selection::new(start, end))
        .expect("选区必须合法");
    let display = controller.display_text();
    assert!(
        display.contains(OBJECT_PLACEHOLDER),
        "对象必须仍是占位符：{display:?}"
    );
    assert!(
        !display.contains("![图]"),
        "文本选区相交不得显示源码：{display:?}"
    );
    assert_eq!(
        controller.object_lines_in_display_selection(),
        vec![IMAGE_LINE]
    );
    // 折叠 caret 不算相交。
    controller
        .set_caret(UnicodeScalar::new(0))
        .expect("caret 合法");
    assert!(controller.object_lines_in_display_selection().is_empty());
}

#[test]
fn object_source_edit_toggles_raw_source_and_restores_the_object() {
    let mut controller = open(BLOCK_SOURCE);
    assert_eq!(controller.object_source_edit_line(), None);
    assert!(controller.display_text().contains(OBJECT_PLACEHOLDER));

    assert!(
        controller
            .begin_object_source_edit(IMAGE_LINE)
            .expect("进入源码模式不应失败"),
        "整行独占图片必须能进入源码模式"
    );
    assert_eq!(controller.object_source_edit_line(), Some(IMAGE_LINE));
    let display = controller.display_text();
    assert!(
        display.contains("![图](asset://aaaa)"),
        "源码模式必须显示完整源码：{display:?}"
    );
    assert_eq!(
        controller.selection().head(),
        SourceByte::new(IMAGE_SPAN.0)
            .to_scalar(&controller.source_text())
            .expect("起点是 scalar 边界"),
        "caret 落在图片源码起点"
    );
    assert!(
        controller.projection().object_segments().is_empty(),
        "源码模式下不再有 Object 段"
    );

    assert!(
        controller.end_object_source_edit(),
        "退出源码模式必须返回 true"
    );
    assert_eq!(controller.object_source_edit_line(), None);
    assert!(controller.display_text().contains(OBJECT_PLACEHOLDER));
    assert_eq!(
        controller.selection().head(),
        SourceByte::new(IMAGE_SPAN.1)
            .to_scalar(&controller.source_text())
            .expect("末尾是 scalar 边界"),
        "退出后 caret 落在对象之后（不是对象源码中间）"
    );
    // 再次退出是 no-op。
    assert!(!controller.end_object_source_edit());
}

#[test]
fn entering_source_edit_on_a_non_object_line_is_a_no_op() {
    let mut controller = open(BLOCK_SOURCE);
    assert!(!controller
        .begin_object_source_edit(0)
        .expect("普通行不应报错"));
    assert_eq!(controller.object_source_edit_line(), None);
    assert_eq!(controller.source_text(), BLOCK_SOURCE);
}

#[test]
fn deleting_the_object_removes_the_whole_source_span_in_one_request() {
    let mut controller = open(BLOCK_SOURCE);
    let request = controller
        .delete_object(IMAGE_LINE)
        .expect("删除对象不应失败")
        .expect("删除对象必须产生一次 EditRequest");
    assert_eq!(request.edits.len(), 1, "整块删除必须是单个 ScalarEdit");
    let edit = &request.edits[0];
    assert_eq!(edit.replacement, "");
    assert_eq!(
        edit.start,
        SourceByte::new(IMAGE_SPAN.0)
            .to_scalar(BLOCK_SOURCE)
            .expect("起点是 scalar 边界")
    );
    assert_eq!(
        edit.end,
        SourceByte::new(IMAGE_SPAN.1)
            .to_scalar(BLOCK_SOURCE)
            .expect("末尾是 scalar 边界")
    );
    assert_eq!(controller.source_text(), "前\n\n\n\n后\n");
    assert!(controller.images().is_empty(), "删除后不应再解析出图片");
}

#[test]
fn object_source_edit_exits_when_the_image_syntax_is_destroyed() {
    let mut controller = open(BLOCK_SOURCE);
    assert!(controller
        .begin_object_source_edit(IMAGE_LINE)
        .expect("进入源码模式"));
    // 把 `![` 改成普通文本：图片消失，源码模式必须自动退出（不再有对象）。
    let text = controller.source_text();
    let start = SourceByte::new(IMAGE_SPAN.0)
        .to_scalar(&text)
        .expect("起点是 scalar 边界");
    let end = SourceByte::new(IMAGE_SPAN.0 + 2)
        .to_scalar(&text)
        .expect("两个字节都是 ASCII");
    controller
        .set_selection(Selection::new(start, end))
        .expect("选区合法");
    controller
        .compose_replace_selection("xx")
        .expect("替换 `![` 必须成功");
    assert_eq!(controller.object_source_edit_line(), None);
    assert_eq!(
        controller.source_text(),
        "前\n\nxx图](asset://aaaa)\n\n后\n"
    );
    assert!(controller.images().is_empty());
}
