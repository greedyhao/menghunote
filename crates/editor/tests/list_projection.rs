//! ADR 0019 S1：列表 marker 的 `Replace` 投影（source ↔ display / 偏置 / 显现 / 复制）。
//!
//! 覆盖：bullet、有序编号右对齐、嵌套列表前导空白隐藏、caret 进入 marker 显现源码、
//! 端点偏置、display 选区反查 source（复制不得拿到 display bullet）、源码不变。

use menghunote_editor::{
    DisplayByte, MarkdownDocument, ProjectionMap, Selection, SourceByte, UnicodeScalar,
};

fn parse(text: &str) -> MarkdownDocument {
    MarkdownDocument::parse(text).expect("Markdown 解析不应失败")
}

fn project(doc: &MarkdownDocument, caret: usize) -> ProjectionMap {
    ProjectionMap::from_markdown_with_lists(doc, Selection::caret(UnicodeScalar::new(caret)))
}

fn caret_at_end(source: &str) -> usize {
    source.chars().count()
}

#[test]
fn bullet_replaces_marker_and_keeps_source() {
    let source = "- a\n- b\n";
    let doc = parse(source);
    let map = project(&doc, caret_at_end(source));
    assert_eq!(map.display_text().as_ref(), "• a\n• b\n");
    assert_eq!(map.source_len(), source.len());
    assert_eq!(map.display_len().get(), "• a\n• b\n".len());

    // 段表：Replace / Copy / Copy(换行) / Replace / Copy / Copy。
    let kinds: Vec<_> = map
        .segments()
        .iter()
        .map(menghunote_editor::Segment::kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            menghunote_editor::SegmentKind::Replace,
            menghunote_editor::SegmentKind::Copy,
            menghunote_editor::SegmentKind::Replace,
            menghunote_editor::SegmentKind::Copy,
        ]
    );

    // source → display：marker 内部取替换段起点；content 起点在 bullet 之后。
    for (source_at, expected) in [
        (0usize, 0usize),
        (1, 0),
        (2, 4), // content "a"（bullet `• ` 占 4 字节）
        (3, 5), // 换行
        (4, 6), // 第二个 marker 起点
    ] {
        assert_eq!(
            map.display_byte_at_source(SourceByte::new(source_at)),
            Some(DisplayByte::new(expected)),
            "source {source_at}"
        );
    }

    // display → source：bullet 内部取 marker 起点（点 bullet 会显现源码 marker）。
    for (display_at, expected) in [
        (0usize, 0usize),
        (1, 0),
        (2, 0),
        (3, 0),
        (4, 2), // bullet 之后 = 正文
        (5, 3),
    ] {
        assert_eq!(
            map.source_byte_at_display(DisplayByte::new(display_at)),
            Some(SourceByte::new(expected)),
            "display {display_at}"
        );
    }
    // 偏置：bullet 末尾 → Left 停在 marker 内、Right 进入正文。
    assert_eq!(
        map.source_byte_at_display_biased(DisplayByte::new(4), menghunote_editor::Bias::Left),
        Some(SourceByte::new(0))
    );
    assert_eq!(
        map.source_byte_at_display_biased(DisplayByte::new(4), menghunote_editor::Bias::Right),
        Some(SourceByte::new(2))
    );
}

#[test]
fn ordered_numbers_are_right_aligned_and_stable_width() {
    let source = "9. nine\n10. ten\n";
    let doc = parse(source);
    let map = project(&doc, caret_at_end(source));
    assert_eq!(map.display_text().as_ref(), " 9. nine\n10. ten\n");
    // 编号宽度一致：两个 marker 的 display 宽度相同（右对齐）。
    let items = doc.list_items();
    assert_eq!(items.len(), 2);
    let first = doc.list_marker_text(0).expect("marker");
    let second = doc.list_marker_text(1).expect("marker");
    assert_eq!(first, " 9. ");
    assert_eq!(second, "10. ");
    assert_eq!(first.len(), second.len());
    assert_eq!(doc.list_marker_source(0), Some("9. "));
    assert_eq!(doc.list_marker_source(1), Some("10. "));
}

#[test]
fn nested_list_indentation_is_hidden_and_layout_owns_it() {
    let source = "- a\n    - b\n";
    let doc = parse(source);
    let map = project(&doc, caret_at_end(source));
    // 4 个前导空格是布局缩进，display 里不应出现。
    assert_eq!(map.display_text().as_ref(), "• a\n• b\n");
    // 源码里那 4 个空格在投影中是 Elide（display 宽度 0）。
    let hidden: Vec<_> = map
        .segments()
        .iter()
        .filter(|segment| segment.kind() == menghunote_editor::SegmentKind::Elide)
        .map(|segment| (segment.source().start().get(), segment.source().end().get()))
        .collect();
    assert!(
        hidden.contains(&(4, 8)),
        "前导空白必须在隐藏段里：{hidden:?}"
    );
}

#[test]
fn caret_inside_marker_reveals_source_marker() {
    let source = "- a\n- b\n";
    let doc = parse(source);
    // caret 在第一个 marker 起点（scalar 0）→ 该 marker 显现成源码 `- `，
    // 第二个 item 仍是 bullet。
    let map = project(&doc, 0);
    assert_eq!(map.display_text().as_ref(), "- a\n• b\n");
    // caret 在正文起点（scalar 2 = marker.end）→ 不显现，仍显示 bullet。
    let map = project(&doc, 2);
    assert_eq!(map.display_text().as_ref(), "• a\n• b\n");
    // 选区与 marker 相交 → 显现。
    let selection = Selection::new(UnicodeScalar::new(0), UnicodeScalar::new(1));
    let map = ProjectionMap::from_markdown_with_lists(&doc, selection);
    assert_eq!(map.display_text().as_ref(), "- a\n• b\n");
}

#[test]
fn copying_display_selection_takes_source_range_not_bullet() {
    let source = "- a\n- b\n";
    let doc = parse(source);
    let map = project(&doc, caret_at_end(source));
    // 选中第一行可见内容（bullet + "a"）：反查 source 必须是完整 `- a`，不是 bullet。
    let range = map
        .source_range_of_display_range(DisplayByte::new(0)..DisplayByte::new(5))
        .expect("映射必须成功");
    assert_eq!((range.start().get(), range.end().get()), (0, 3));
    assert_eq!(&source[0..3], "- a");

    // 只选正文 "a"（display 4..5）→ 不含 marker。
    let range = map
        .source_range_of_display_range(DisplayByte::new(4)..DisplayByte::new(5))
        .expect("映射必须成功");
    assert_eq!((range.start().get(), range.end().get()), (2, 3));
}

#[test]
fn source_text_roundtrip_is_unchanged() {
    let source = "# 标题\n\n- 第一项\n  - 嵌套\n\n1. 有序\n2. 有序二\n";
    let doc = parse(source);
    assert_eq!(doc.source(), source);
    let map = project(&doc, caret_at_end(source));
    // 显示变了（标记隐藏/替换），但 source 从未被改写。
    assert_eq!(doc.source(), source);
    // display → source → display 只承诺在**可见 Copy 段**往返一致；`Replace` 段内部
    // 多对一（bullet 的每个字节都映射到 marker 起点），不承诺反解。
    for segment in map.segments() {
        if segment.kind() != menghunote_editor::SegmentKind::Copy {
            continue;
        }
        for at in segment.display_start().get()..segment.display_end().get() {
            let source_at = map
                .source_byte_at_display(DisplayByte::new(at))
                .unwrap_or_else(|| panic!("display {at} 应有 source"));
            assert_eq!(
                map.display_byte_at_source(source_at),
                Some(DisplayByte::new(at)),
                "display {at}（Copy 段）往返失败"
            );
        }
    }
}

#[test]
fn quoted_list_projects_quote_prefix_and_bullet_together() {
    // 引用里的列表：`> ` 是 Elide，`- ` 是 Replace；display 只留 bullet + 内容，
    // 引用缩进由 app 的 block_metrics 提供（几何与标记分离）。
    let source = "> - item\n";
    let doc = parse(source);
    // caret 在引用 marker 上：引用显现成 `> `，列表 marker 仍是 bullet。
    let visible_quote = project(&doc, 0);
    assert_eq!(visible_quote.display_text().as_ref(), "> • item\n");
    // caret 在文末（引用外）：`> ` 隐藏，bullet 仍在。
    let hidden_quote = project(&doc, source.chars().count());
    assert_eq!(hidden_quote.display_text().as_ref(), "• item\n");
    let kinds: Vec<_> = hidden_quote
        .segments()
        .iter()
        .map(menghunote_editor::Segment::kind)
        .collect();
    assert!(kinds.contains(&menghunote_editor::SegmentKind::Elide));
    assert!(kinds.contains(&menghunote_editor::SegmentKind::Replace));
}

#[test]
fn image_syntax_is_displayed_raw_in_s1() {
    // S1 不做图片 Object：整段 `![alt](url)` 原样显示，不能被旧 scanner 的 Link
    // 部分隐藏成 `!alt`（图片进入 S2 后再切成 Object）。
    let source = "前 ![alt](asset://abc) 后\n";
    let doc = parse(source);
    let map = project(&doc, source.chars().count());
    assert_eq!(map.display_text().as_ref(), source);
    assert!(map.is_identity());
}
