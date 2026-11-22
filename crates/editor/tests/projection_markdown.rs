//! S2-2：Markdown 隐藏区间 → 真实投影（display ↔ source）的契约测试。
//!
//! 覆盖：标记隐藏/内容可见、caret 进入构造回显、选区跨构造、嵌套构造、行内代码、
//! 未闭合不隐藏、偏置规则、空文档 identity 等价、不变量与确定性。

use menghunote_editor::{
    DisplayByte, MarkdownDocument, ProjectionMap, Selection, SourceByte, UnicodeScalar,
};

fn parse(text: &str) -> MarkdownDocument {
    MarkdownDocument::parse(text).expect("Markdown 扫描不应失败")
}

fn project(doc: &MarkdownDocument, selection: Selection) -> ProjectionMap {
    ProjectionMap::from_markdown(doc, selection)
}

fn caret(at: usize) -> Selection {
    Selection::caret(UnicodeScalar::new(at))
}

#[test]
fn hidden_bold_markers_disappear_but_content_stays() {
    let source = "a**b**c";
    let doc = parse(source);
    let map = project(&doc, caret(0)); // caret 在粗体构造之外

    assert_eq!(map.display_text().as_ref(), "abc", "** 不出现，内容保留");
    assert_eq!(map.display_len(), DisplayByte::new(3));
    assert_eq!(map.source_len(), 7);
    assert!(!map.is_identity());

    // 段表：Copy a / Elide ** / Copy b / Elide ** / Copy c。
    let kinds: Vec<_> = map
        .segments()
        .iter()
        .map(|segment| segment.kind())
        .collect();
    assert_eq!(
        kinds,
        vec![
            menghunote_editor::SegmentKind::Copy,
            menghunote_editor::SegmentKind::Elide,
            menghunote_editor::SegmentKind::Copy,
            menghunote_editor::SegmentKind::Elide,
            menghunote_editor::SegmentKind::Copy,
        ]
    );

    // source → display：省略段内部取段起点；省略段末尾取下一可见段起点。
    for (source_at, expected) in [
        (0usize, 0usize),
        (1, 1), // 省略段起点
        (2, 1), // 省略段内部 → 段起点
        (3, 1), // 省略段末尾 → 下一可见段起点
        (4, 2), // 第二个省略段起点
        (5, 2), // 内部 → 段起点
        (6, 2),
        (7, 3), // 文档尾 → display 末尾
    ] {
        assert_eq!(
            map.display_byte_at_source(SourceByte::new(source_at)),
            Some(DisplayByte::new(expected)),
            "source {source_at}"
        );
    }

    // display → source：接缝取 After（右侧下一可见段起点）；display 末尾 = 文档尾。
    for (display_at, expected) in [
        (0usize, 0usize),
        (1, 3),
        (2, 6),
        (3, 7), // display 末尾端点定义为 source_len
    ] {
        assert_eq!(
            map.source_byte_at_display(DisplayByte::new(display_at)),
            Some(SourceByte::new(expected)),
            "display {display_at}"
        );
    }
    assert_eq!(map.source_byte_at_display(DisplayByte::new(4)), None);
    assert_eq!(
        map.display_byte_at_source(SourceByte::new(8)),
        None,
        "source 越界必须是 None，不是 0"
    );
}

#[test]
fn source_range_of_display_range_is_minimal() {
    let doc = parse("a**b**c");
    let map = project(&doc, caret(0)); // display = "abc"
    let display_range = |start: usize, end: usize| {
        map.source_range_of_display_range(DisplayByte::new(start)..DisplayByte::new(end))
    };
    let source_range = |start: usize, end: usize| {
        Some(
            menghunote_editor::SourceRange::new(SourceByte::new(start), SourceByte::new(end), 7)
                .expect("测试区间合法"),
        )
    };

    // 只选可见 "a"/"b"/"c"：最小 source 区间，不含两端相邻的隐藏标记。
    assert_eq!(display_range(0, 1), source_range(0, 1));
    assert_eq!(display_range(1, 2), source_range(3, 4));
    assert_eq!(display_range(2, 3), source_range(6, 7));
    // 跨过内部隐藏段：包含严格位于区间内部的省略段。
    assert_eq!(display_range(0, 2), source_range(0, 4));
    assert_eq!(display_range(1, 3), source_range(3, 7));
    // 整段 display：起点 0，终点最后一个可见段末尾（无尾部隐藏 → source_len）。
    assert_eq!(display_range(0, 3), source_range(0, 7));
    // 空区间折叠到 After 偏置位置。
    assert_eq!(display_range(1, 1), source_range(3, 3));
    assert_eq!(display_range(3, 3), source_range(7, 7));
    // 端点倒序 / 越界 → None。
    assert_eq!(display_range(2, 1), None);
    assert_eq!(display_range(0, 4), None);
}

#[test]
fn caret_inside_construct_reveals_markers_again() {
    let source = "a**b**c";
    let doc = parse(source);

    // caret 在粗体 span（1..=6）内 → 该构造标记回到可见：没有 Elide 段。
    for at in 1..=6 {
        let map = project(&doc, caret(at));
        assert_eq!(map.display_text().as_ref(), source, "caret={at}");
        assert!(map.is_identity(), "caret={at} 时该构造无省略");
        assert!(
            map.segments()
                .iter()
                .all(|segment| segment.kind() == menghunote_editor::SegmentKind::Copy),
            "caret={at} 不应留下 Elide"
        );
    }

    // caret 在构造外（文档首）→ 标记隐藏。
    assert_eq!(project(&doc, caret(0)).display_text().as_ref(), "abc");
}

#[test]
fn selection_across_constructs_reveals_all_intersected() {
    let source = "**a** *b*";
    let doc = parse(source);

    // 全选：两个构造都显现，投影等价 identity。
    let all = project(
        &doc,
        Selection::new(UnicodeScalar::new(0), UnicodeScalar::new(9)),
    );
    assert_eq!(all.display_text().as_ref(), source);
    assert!(all.is_identity());

    // 只选中粗体：斜体标记仍隐藏（`*b*` 的星号消失）。
    let bold_only = project(
        &doc,
        Selection::new(UnicodeScalar::new(0), UnicodeScalar::new(3)),
    );
    assert_eq!(bold_only.display_text().as_ref(), "**a** b");

    // 只选中两者之间的空格：两个构造都隐藏。
    let gap = project(
        &doc,
        Selection::new(UnicodeScalar::new(5), UnicodeScalar::new(6)),
    );
    assert_eq!(gap.display_text().as_ref(), "a b");
}

#[test]
fn nested_block_and_inline_constructs_project_together() {
    let source = "# **h**\nplain";
    let doc = parse(source);

    // caret 在纯文本行（块外）→ 标题 `# ` 与粗体 `**` 都隐藏，h 与正文可见。
    let outside = project(&doc, caret(9));
    assert_eq!(outside.display_text().as_ref(), "h\nplain");
    assert_eq!(outside.display_len(), DisplayByte::new(7));

    // caret 在标题标记上（0）只显现标题本身；嵌套的粗体仍在隐藏态。
    assert_eq!(
        project(&doc, caret(0)).display_text().as_ref(),
        "# h\nplain",
        "标题显现而嵌套粗体保持隐藏"
    );
    // caret 进入粗体 span（2..=7）→ 标题与粗体都显现（嵌套构造一起回显）。
    for at in [2usize, 4] {
        let inside = project(&doc, caret(at));
        assert_eq!(inside.display_text().as_ref(), source, "caret={at}");
        assert!(inside.is_identity(), "caret={at}");
    }
}

#[test]
fn inline_code_content_keeps_markers_visible() {
    let source = "`a**b**`\nplain";
    let doc = parse(source);
    let map = project(&doc, caret(11)); // caret 在纯文本行

    // 行内代码内部不解析 Markdown：`**` 属于代码内容，必须保留。
    assert_eq!(map.display_text().as_ref(), "a**b**\nplain");
    assert!(map.display_text().contains("**"));
}

#[test]
fn unclosed_markers_are_never_hidden() {
    let source = "**unclosed\nplain";
    let doc = parse(source);
    let map = project(&doc, caret(12));
    assert_eq!(map.display_text().as_ref(), source, "未闭合不隐藏任何字符");
    assert!(map.is_identity());
}

#[test]
fn empty_and_plain_documents_equal_identity() {
    for source in ["", "hello 中文，全角标点。", "\\*not em\\*"] {
        let doc = parse(source);
        let projected = project(&doc, caret(0));
        let identity = ProjectionMap::identity(source).expect("identity 构造");
        assert_eq!(projected, identity, "{source:?}");
        assert_eq!(projected.display_text().as_ref(), source);
    }
}

#[test]
fn invalid_selection_falls_back_conservatively() {
    let doc = parse("a**b**c");
    let invalid = caret(99);

    // 严格入口报错，不静默编造坐标。
    assert!(ProjectionMap::try_from_markdown(&doc, invalid).is_err());

    // 宽松入口退回 identity（宁可不隐藏，也不隐藏错误内容）。
    let fallback = project(&doc, invalid);
    assert!(fallback.is_identity());
    assert_eq!(fallback.display_text().as_ref(), "a**b**c");
}

#[test]
fn leading_and_trailing_hidden_bias() {
    // 行首隐藏（粗体标记在文档开头）。
    let doc = parse("**a**\nplain");
    let map = project(&doc, caret(7));
    assert_eq!(map.display_text().as_ref(), "a\nplain");
    for (source_at, expected) in [(0usize, 0usize), (1, 0), (2, 0), (3, 1), (4, 1)] {
        assert_eq!(
            map.display_byte_at_source(SourceByte::new(source_at)),
            Some(DisplayByte::new(expected)),
            "leading hidden source {source_at}"
        );
    }
    // 显示 0..1（"a"）反查只包含内容，不含行首的 `**`。
    assert_eq!(
        map.source_range_of_display_range(DisplayByte::new(0)..DisplayByte::new(1)),
        Some(
            menghunote_editor::SourceRange::new(SourceByte::new(2), SourceByte::new(3), 11,)
                .expect("测试区间合法")
        )
    );

    // 行尾隐藏（粗体标记在文档末尾）。
    let doc = parse("a**b**");
    let map = project(&doc, caret(0));
    assert_eq!(map.display_text().as_ref(), "ab");
    assert_eq!(map.display_len(), DisplayByte::new(2));
    assert_eq!(
        map.source_byte_at_display(map.display_len()),
        Some(SourceByte::new(6)),
        "display 末尾端点定义为 source_len"
    );
    assert_eq!(
        map.display_byte_at_source(SourceByte::new(5)),
        Some(DisplayByte::new(2)),
        "尾部省略段内部 → 该段起点"
    );
    // 整段 display 反查不吞掉行尾的 `**`。
    assert_eq!(
        map.source_range_of_display_range(DisplayByte::new(0)..map.display_len()),
        Some(
            menghunote_editor::SourceRange::new(SourceByte::new(0), SourceByte::new(4), 6,)
                .expect("测试区间合法")
        ),
        "末尾隐藏标记不包含在最小 source 区间内"
    );
}

#[test]
fn invariants_hold_on_complex_document() {
    let source = "# Title **bold**\n> > quote *em*\n- [link](url) `code`\nplain\n";
    let doc = parse(source);
    let selection = caret(source.chars().count() - 1); // 最后一行纯文本里
    let map = project(&doc, selection);

    // 1) 段表完整、有序、双向连续；缺省段 display 宽度为 0。
    let mut source_cursor = 0usize;
    let mut display_cursor = 0usize;
    let mut visible = String::new();
    for segment in map.segments() {
        assert_eq!(segment.source().start().get(), source_cursor, "source 连续");
        assert_eq!(
            segment.display_start().get(),
            display_cursor,
            "display 连续"
        );
        assert!(!segment.source().is_empty(), "段 source 非空");
        match segment.kind() {
            menghunote_editor::SegmentKind::Copy => {
                assert_eq!(segment.display_len(), segment.source().len());
                visible.push_str(
                    &source[segment.source().start().get()..segment.source().end().get()],
                );
            }
            menghunote_editor::SegmentKind::Elide => {
                assert_eq!(segment.display_len(), 0, "Elide 不产生 display 长度");
            }
            other => panic!("本层不应出现 {other:?} 段"),
        }
        source_cursor = segment.source().end().get();
        display_cursor = segment.display_end().get();
    }
    assert_eq!(source_cursor, source.len(), "完整覆盖 source");
    assert_eq!(display_cursor, map.display_len().get());
    assert_eq!(visible, map.display_text().as_ref(), "display 拼接一致");

    // 2) 可见位置往返：display → source → display 恒等。
    for at in 0..=map.display_len().get() {
        let display = DisplayByte::new(at);
        let source = map
            .source_byte_at_display(display)
            .unwrap_or_else(|| panic!("display {at} 应有 source 位置"));
        assert_eq!(
            map.display_byte_at_source(source),
            Some(display),
            "display {at} 往返失败（source {}）",
            source.get()
        );
    }

    // 3) 每个可见段的显示区间反查应回到自身 source 区间。
    for segment in map.segments() {
        if segment.kind() != menghunote_editor::SegmentKind::Copy {
            continue;
        }
        let range =
            map.source_range_of_display_range(segment.display_start()..segment.display_end());
        assert_eq!(range, Some(segment.source()), "{segment:?}");
    }

    // 4) 整段 display 的 source 区间是“最小”区间：从第一个可见段起点到最后可见段末尾
    //    （不包含两端的隐藏标记）。
    let whole = map
        .source_range_of_display_range(DisplayByte::new(0)..map.display_len())
        .expect("整段 display 可映射");
    let first_visible_start = map
        .segments()
        .iter()
        .find(|segment| segment.kind() == menghunote_editor::SegmentKind::Copy)
        .map_or(0, |segment| segment.source().start().get());
    let last_visible_end = map
        .segments()
        .iter()
        .rev()
        .find(|segment| segment.kind() == menghunote_editor::SegmentKind::Copy)
        .map_or(0, |segment| segment.source().end().get());
    assert_eq!(whole.start().get(), first_visible_start);
    assert_eq!(whole.end().get(), last_visible_end);
    assert!(whole.end().get() <= source.len());

    // 5) 确定性。
    assert_eq!(map, project(&doc, selection));
}
