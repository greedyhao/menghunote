//! ADR 0019 M2：fenced `mermaid` **Object Block** 的解析 / 投影 / 端点偏置 /
//! 编辑源码切换 / 删除跨度 / 非独占 mermaid 保持普通代码。
//!
//! 只覆盖纯模型（`menghunote-editor`）：源码零改写、display 占位符、双向映射与一次
//! `EditRequest` 的删除语义。后台渲染/光栅化/缓存与像素几何在 app 侧测试。

mod support;

use menghunote_editor::{
    Bias, DisplayByte, EditorController, MarkdownDocument, ObjectKind, ProjectionMap, Selection,
    SourceByte, UnicodeScalar, OBJECT_PLACEHOLDER,
};

/// 前文、空行、3 行 mermaid（开围栏 + 1 行图源码 + 闭围栏）、空行、后文。
const MERMAID_SOURCE: &str = "前\n\n```mermaid\nflowchart TD\nA-->B\n```\n\n后\n";

fn mermaid_start() -> usize {
    MERMAID_SOURCE.find("```mermaid").expect("开围栏")
}

fn mermaid_end() -> usize {
    MERMAID_SOURCE.find("\n\n后").expect("后文位置")
}

fn parse(source: &str) -> MarkdownDocument {
    MarkdownDocument::parse(source).expect("Markdown 解析不应失败")
}

fn open(source: &str) -> EditorController {
    EditorController::open(support::snapshot(source)).expect("测试快照必须可打开")
}

fn project(doc: &MarkdownDocument, caret: usize) -> ProjectionMap {
    ProjectionMap::from_markdown_with_lists(doc, Selection::caret(UnicodeScalar::new(caret)))
}

#[test]
fn fenced_mermaid_is_one_object_with_block_and_content_spans() {
    let doc = parse(MERMAID_SOURCE);
    assert_eq!(doc.mermaid_blocks().len(), 1);
    let mermaid = &doc.mermaid_blocks()[0];
    assert!(mermaid.is_block(), "顶层 mermaid 必须是 Object Block");
    assert_eq!(mermaid.line(), 2, "开围栏行 = 第 3 行（0 起）");
    assert_eq!(mermaid.end_line(), 5);
    assert_eq!(mermaid.info(), "mermaid");
    assert_eq!(
        (mermaid.span().start().get(), mermaid.span().end().get()),
        (mermaid_start(), mermaid_end())
    );
    assert_eq!(
        &MERMAID_SOURCE[mermaid.content().start().get()..mermaid.content().end().get()],
        "flowchart TD\nA-->B\n",
        "content 是围栏之间的原始源码"
    );

    // 统一对象列表：恰好一个 Mermaid 对象，索引指回 mermaid_blocks()[0]。
    let objects = doc.objects();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].kind(), ObjectKind::Mermaid);
    assert_eq!(objects[0].line(), 2);
    assert_eq!(objects[0].span(), mermaid.span());
    assert_eq!(
        doc.object_mermaid(&objects[0])
            .map(|mermaid| mermaid.span()),
        Some(mermaid.span())
    );
    assert!(doc.object_image(&objects[0]).is_none());
    assert!(doc.object_table(&objects[0]).is_none());
    assert_eq!(
        doc.object_at_line(2).map(|object| object.kind()),
        Some(ObjectKind::Mermaid)
    );
    assert!(doc.object_at_line(3).is_none(), "对象行只有开围栏行");
}

#[test]
fn block_mermaid_projects_one_placeholder_and_hides_fences() {
    let doc = parse(MERMAID_SOURCE);
    let map = project(&doc, MERMAID_SOURCE.chars().count());
    let expected = format!("前\n\n{OBJECT_PLACEHOLDER}\n\n后\n");
    assert_eq!(map.display_text().as_ref(), expected);
    let objects = map.object_segments();
    assert_eq!(objects.len(), 1, "整块 mermaid 一个 Object 段");
    assert_eq!(objects[0].0, doc.mermaid_blocks()[0].span());
    assert_eq!(
        objects[0].1.end - objects[0].1.start,
        menghunote_editor::OBJECT_PLACEHOLDER_LEN
    );
}

#[test]
fn mermaid_object_endpoint_biases_are_pinned() {
    let doc = parse(MERMAID_SOURCE);
    let map = project(&doc, MERMAID_SOURCE.chars().count());
    let mermaid = &doc.mermaid_blocks()[0];
    let display_start = 5usize; // "前\n\n" 之后
    let display_end = display_start + OBJECT_PLACEHOLDER.len();

    // source → display：块内部（含起点）收敛到占位符起点；块末尾到占位符末尾。
    for source in [
        mermaid.span().start().get(),
        mermaid.span().start().get() + 1,
        mermaid.span().end().get() - 1,
    ] {
        assert_eq!(
            map.display_byte_at_source(SourceByte::new(source)),
            Some(DisplayByte::new(display_start)),
            "source {source} 在块内部，应映射到占位符起点"
        );
    }
    assert_eq!(
        map.display_byte_at_source(SourceByte::new(mermaid.span().end().get())),
        Some(DisplayByte::new(display_end))
    );

    // display → source：占位符内部（含起点/左偏置）只给出块起点；末尾给出块末尾。
    for display in [display_start, display_start + 1, display_end - 1] {
        assert_eq!(
            map.source_byte_at_display(DisplayByte::new(display)),
            Some(SourceByte::new(mermaid.span().start().get()))
        );
        assert_eq!(
            map.source_byte_at_display_biased(DisplayByte::new(display), Bias::Left),
            Some(SourceByte::new(mermaid.span().start().get()))
        );
    }
    assert_eq!(
        map.source_byte_at_display(DisplayByte::new(display_end)),
        Some(SourceByte::new(mermaid.span().end().get()))
    );
}

#[test]
fn selection_covering_the_mermaid_copies_complete_fenced_source() {
    let doc = parse(MERMAID_SOURCE);
    let map = project(&doc, MERMAID_SOURCE.chars().count());
    let mermaid = &doc.mermaid_blocks()[0];
    let display_start = 5usize;
    let display_end = display_start + OBJECT_PLACEHOLDER.len();
    let copied = map
        .source_range_of_display_range(
            DisplayByte::new(display_start)..DisplayByte::new(display_end),
        )
        .expect("选区必须能反查 source");
    assert_eq!(
        (copied.start().get(), copied.end().get()),
        (mermaid.span().start().get(), mermaid.span().end().get()),
        "复制必须得到完整 fenced 源码"
    );
    assert_eq!(
        &MERMAID_SOURCE[copied.start().get()..copied.end().get()],
        "```mermaid\nflowchart TD\nA-->B\n```"
    );
    // 选区在占位符之前结束 → 不包含块。
    let before = map
        .source_range_of_display_range(DisplayByte::new(0)..DisplayByte::new(display_start))
        .expect("选区必须能反查 source");
    assert_eq!(before.end().get(), mermaid.span().start().get());
}

#[test]
fn non_empty_selection_keeps_the_mermaid_object() {
    let mut controller = open(MERMAID_SOURCE);
    controller
        .set_selection(Selection::new(
            UnicodeScalar::new(0),
            UnicodeScalar::new(MERMAID_SOURCE.chars().count() - 1),
        ))
        .expect("选区必须合法");
    let display = controller.display_text();
    assert!(
        display.contains(OBJECT_PLACEHOLDER),
        "非空选区跨过 Mermaid 时必须保持 Object，不能突然切源码：{display:?}"
    );
    assert!(
        !display.contains("```mermaid"),
        "非空选区不得显示 fenced 源码：{display:?}"
    );
    assert_eq!(
        controller.projection().object_segments().len(),
        1,
        "非空选区跨过时仍应有一个 Object 段"
    );
}

#[test]
fn caret_inside_the_mermaid_reveals_source_without_object() {
    let mut controller = open(MERMAID_SOURCE);
    // 折叠 caret：起点算块内；**终点不算**（点块末尾/文档尾不应突然切源码）。
    let start_scalar = SourceByte::new(mermaid_start())
        .to_scalar(MERMAID_SOURCE)
        .expect("起点");
    let end_scalar = SourceByte::new(mermaid_end())
        .to_scalar(MERMAID_SOURCE)
        .expect("末尾");
    controller.set_caret(start_scalar).expect("caret 合法");
    assert!(
        controller.display_text().contains("```mermaid"),
        "caret 在 block 起点必须显示源码"
    );
    assert!(!controller.display_text().contains(OBJECT_PLACEHOLDER));

    controller.set_caret(end_scalar).expect("caret 合法");
    assert!(
        controller.display_text().contains(OBJECT_PLACEHOLDER),
        "caret 在 block 终点不得切回源码：{}",
        controller.display_text()
    );
    assert!(!controller.display_text().contains("```mermaid"));

    // 移出 block 一个字节（块尾换行之后）→ Object 仍是 Object。
    let outside = SourceByte::new(mermaid_end() + 1)
        .to_scalar(MERMAID_SOURCE)
        .expect("块外");
    controller.set_caret(outside).expect("caret 合法");
    assert!(controller.display_text().contains(OBJECT_PLACEHOLDER));
    assert_eq!(controller.projection().object_segments().len(), 1);
}

#[test]
fn caret_at_block_end_of_file_keeps_the_mermaid_object() {
    let source = "```mermaid\nflowchart TD\nA-->B\n```";
    let mut controller = open(source);
    let end = UnicodeScalar::new(source.chars().count());
    controller.set_caret(end).expect("文档尾 caret 合法");
    let display = controller.display_text();
    assert!(
        display.contains(OBJECT_PLACEHOLDER),
        "文档尾点块末尾不得自动切源码：{display:?}"
    );
    assert!(!display.contains("```mermaid"));
}

#[test]
fn mermaid_source_edit_shows_complete_fence_and_restores_object() {
    let mut controller = open(MERMAID_SOURCE);
    assert_eq!(controller.object_source_edit_line(), None);
    assert!(controller.display_text().contains(OBJECT_PLACEHOLDER));

    assert!(
        controller
            .begin_object_source_edit(2)
            .expect("进入源码模式不应失败"),
        "整块 mermaid 必须能进入源码模式"
    );
    assert_eq!(controller.object_source_edit_line(), Some(2));
    let display = controller.display_text();
    assert!(
        display.contains("```mermaid\nflowchart TD\nA-->B\n```"),
        "源码模式必须显示完整 fenced 源码：{display:?}"
    );
    assert!(
        !display.contains(OBJECT_PLACEHOLDER),
        "源码模式下不再有 Object 占位符：{display:?}"
    );
    assert_eq!(
        controller.selection().head(),
        SourceByte::new(mermaid_start())
            .to_scalar(&controller.source_text())
            .expect("起点是 scalar 边界"),
        "caret 落在开围栏行首"
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
        SourceByte::new(mermaid_end() + 1)
            .to_scalar(&controller.source_text())
            .expect("末尾是 scalar 边界"),
        "退出后 caret 落在块尾换行之后（块外，Object 才能重新出现）"
    );
    assert!(!controller.end_object_source_edit(), "再次退出是 no-op");
}

#[test]
fn deleting_the_mermaid_removes_the_whole_source_span_in_one_request() {
    let mut controller = open(MERMAID_SOURCE);
    let request = controller
        .delete_object(2)
        .expect("删除对象不应失败")
        .expect("删除 mermaid 必须产生一次 EditRequest");
    assert_eq!(request.edits.len(), 1, "整块删除必须是单个 ScalarEdit");
    let edit = &request.edits[0];
    assert_eq!(edit.replacement, "");
    assert_eq!(
        edit.start,
        SourceByte::new(mermaid_start())
            .to_scalar(MERMAID_SOURCE)
            .expect("起点是 scalar 边界")
    );
    assert_eq!(
        edit.end,
        SourceByte::new(mermaid_end())
            .to_scalar(MERMAID_SOURCE)
            .expect("末尾是 scalar 边界")
    );
    assert_eq!(controller.source_text(), "前\n\n\n\n后\n");
    assert!(
        controller.mermaid_blocks().is_empty(),
        "删除后不再解析出 mermaid 对象"
    );
}

#[test]
fn nested_or_indented_mermaid_stays_ordinary_code() {
    for source in [
        "- item\n\n  ```mermaid\n  flowchart TD\n  A-->B\n  ```\n",
        "   ```mermaid\n   flowchart TD\n   A-->B\n   ```\n",
    ] {
        let doc = parse(source);
        assert!(
            doc.mermaid_blocks().is_empty(),
            "非普通段落上下文不进入对象列表：{source:?}"
        );
        assert!(doc.objects().is_empty(), "不得产生 Object：{source:?}");
        // 仍是普通 fenced code：围栏构造存在，源码可编辑。
        assert!(
            doc.constructs()
                .iter()
                .any(|construct| construct.kind() == menghunote_editor::ConstructKind::FencedCode),
            "必须保留 FencedCode 构造：{source:?}"
        );
    }
}

#[test]
fn objects_keep_source_order_across_kinds() {
    let source = "![a](asset://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)\n\n```mermaid\nflowchart TD\nA-->B\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
    let doc = parse(source);
    let kinds: Vec<ObjectKind> = doc.objects().iter().map(|object| object.kind()).collect();
    assert_eq!(
        kinds,
        vec![ObjectKind::Image, ObjectKind::Mermaid, ObjectKind::Table],
        "三类对象共用一条按源码顺序的路径"
    );
    assert_eq!(doc.objects()[1].line(), 2);
    assert_eq!(doc.objects()[1].end_line(), 5);
}
