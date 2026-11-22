//! Markdown 语义扫描 + 可隐藏区间（S2 第一层）的契约测试。
//!
//! 覆盖：块级（标题/引用嵌套/列表/围栏代码）、四种行内、相邻构造、未闭合、
//! 转义、代码内嵌 `**`、caret 内外、选区跨构造、空文档，以及不变量与确定性。

use menghunote_editor::{
    ConstructKind, MarkdownConstruct, MarkdownDocument, Selection, SourceRange, UnicodeScalar,
};

fn parse(text: &str) -> MarkdownDocument {
    MarkdownDocument::parse(text).expect("扫描不应失败")
}

fn span_of(construct: &MarkdownConstruct) -> (usize, usize) {
    (construct.span().start().get(), construct.span().end().get())
}

fn markers_of(construct: &MarkdownConstruct) -> Vec<(usize, usize)> {
    construct
        .markers()
        .iter()
        .map(|range| (range.start().get(), range.end().get()))
        .collect()
}

fn content_of(construct: &MarkdownConstruct) -> (usize, usize) {
    (
        construct.content().start().get(),
        construct.content().end().get(),
    )
}

fn pairs(ranges: &[SourceRange]) -> Vec<(usize, usize)> {
    ranges
        .iter()
        .map(|range| (range.start().get(), range.end().get()))
        .collect()
}

fn hidden_caret(doc: &MarkdownDocument, caret: usize) -> Vec<(usize, usize)> {
    pairs(
        &doc.hidden_ranges_for_caret(UnicodeScalar::new(caret))
            .expect("caret 合法"),
    )
}

fn hidden_selection(doc: &MarkdownDocument, start: usize, end: usize) -> Vec<(usize, usize)> {
    pairs(
        &doc.hidden_ranges_for_selection(Selection::new(
            UnicodeScalar::new(start),
            UnicodeScalar::new(end),
        ))
        .expect("选区合法"),
    )
}

/// 一个隐藏区间是否能被“所有构造的标记区间并集”完整覆盖。
fn covered_by_markers(doc: &MarkdownDocument, range: &SourceRange) -> bool {
    let mut cursor = range.start().get();
    while cursor < range.end().get() {
        let Some(marker) = doc
            .constructs()
            .iter()
            .flat_map(MarkdownConstruct::markers)
            .find(|marker| marker.start().get() <= cursor && cursor < marker.end().get())
        else {
            return false;
        };
        cursor = marker.end().get();
    }
    true
}

#[test]
fn heading_marker_is_hashes_plus_one_space() {
    let doc = parse("# Title");
    assert_eq!(doc.constructs().len(), 1);
    let heading = &doc.constructs()[0];
    assert_eq!(heading.kind(), ConstructKind::Heading);
    assert!(heading.kind().is_block());
    assert_eq!(span_of(heading), (0, 7));
    assert_eq!(markers_of(heading), vec![(0, 2)]);
    assert_eq!(content_of(heading), (2, 7));

    // 六级标题；`#######`（7 个 #）与 `#nospace` 都不是标题。
    assert_eq!(markers_of(&parse("###### h").constructs()[0]), vec![(0, 7)]);
    assert!(parse("####### h").constructs().is_empty());
    assert!(parse("#nospace").constructs().is_empty());
    // `#` 单独一行没有空格：按 Markdown 语法要求，不渲染为标题。
    let bare = parse("#");
    assert!(bare.constructs().is_empty());
    // 行首 ≤3 个空格的缩进不进入标记（`# ` 才是标记）。
    assert_eq!(markers_of(&parse("  # h").constructs()[0]), vec![(2, 4)]);
}

#[test]
fn nested_blockquotes_record_each_prefix() {
    let doc = parse("> > hi");
    assert_eq!(doc.constructs().len(), 1);
    let quote = &doc.constructs()[0];
    assert_eq!(quote.kind(), ConstructKind::BlockQuote);
    assert_eq!(span_of(quote), (0, 6));
    assert_eq!(markers_of(quote), vec![(0, 2), (2, 4)]);
    assert_eq!(content_of(quote), (4, 6));

    let compact = parse(">> hi");
    assert_eq!(markers_of(&compact.constructs()[0]), vec![(0, 1), (1, 3)]);
    assert_eq!(content_of(&compact.constructs()[0]), (3, 5));

    // 引用里可以再有标题，两类构造都记录。
    let mixed = parse("> # h");
    let kinds: Vec<_> = mixed
        .constructs()
        .iter()
        .map(MarkdownConstruct::kind)
        .collect();
    assert_eq!(
        kinds,
        vec![ConstructKind::BlockQuote, ConstructKind::Heading]
    );
    assert_eq!(markers_of(&mixed.constructs()[0]), vec![(0, 2)]);
    assert_eq!(content_of(&mixed.constructs()[0]), (2, 5));
    assert_eq!(span_of(&mixed.constructs()[1]), (2, 5));
    assert_eq!(markers_of(&mixed.constructs()[1]), vec![(2, 4)]);
}

#[test]
fn list_markers_cover_bullet_or_ordered_marker() {
    for (text, span, markers, content) in [
        ("- a", (0, 3), vec![(0, 2)], (2, 3)),
        ("* b", (0, 3), vec![(0, 2)], (2, 3)),
        ("+ c", (0, 3), vec![(0, 2)], (2, 3)),
        ("1. d", (0, 4), vec![(0, 3)], (3, 4)),
        ("12. e", (0, 5), vec![(0, 4)], (4, 5)),
    ] {
        let doc = parse(text);
        assert_eq!(doc.constructs().len(), 1, "{text}");
        let item = &doc.constructs()[0];
        assert_eq!(item.kind(), ConstructKind::ListItem, "{text}");
        assert_eq!(span_of(item), span, "{text}");
        assert_eq!(markers_of(item), markers, "{text}");
        assert_eq!(content_of(item), content, "{text}");
    }

    // `*a*` 不是列表项（`*` 后没有空格），而是斜体。
    assert_eq!(parse("*a*").constructs()[0].kind(), ConstructKind::Emphasis);
    // 列表项里的行内构造正常解析。
    let nested = parse("- **x**");
    assert_eq!(nested.constructs().len(), 2);
    assert_eq!(nested.constructs()[1].kind(), ConstructKind::Bold);
}

#[test]
fn fenced_code_suppresses_inline_parsing() {
    let source = "```rust\nlet x = **not bold**;\n```\n";
    let doc = parse(source);
    assert_eq!(doc.constructs().len(), 1, "围栏块内的 ** 不得产生行内构造");
    let fence = &doc.constructs()[0];
    assert_eq!(fence.kind(), ConstructKind::FencedCode);
    assert_eq!(span_of(fence), (0, 33));
    assert_eq!(markers_of(fence), vec![(0, 7), (30, 33)]);
    assert_eq!(content_of(fence), (8, 30));
    let (content_start, content_end) = content_of(fence);
    assert_eq!(
        &source[content_start..content_end],
        "let x = **not bold**;\n"
    );

    // caret 在围栏整段（含两端）内：标记全部可见；在块外：两行围栏都隐藏。
    assert!(hidden_caret(&doc, 0).is_empty());
    assert!(hidden_caret(&doc, 20).is_empty());
    assert!(
        hidden_caret(&doc, 33).is_empty(),
        "caret 在闭围栏行尾（含两端）仍算块内"
    );
    assert_eq!(hidden_caret(&doc, 34), vec![(0, 7), (30, 33)]);

    // `~~~` 围栏同样生效。
    let tilde = parse("~~~\nx\n~~~\n");
    assert_eq!(tilde.constructs().len(), 1);
    assert_eq!(tilde.constructs()[0].kind(), ConstructKind::FencedCode);
}

#[test]
fn unclosed_fence_emits_no_construct() {
    let source = "```\n**still code**\n";
    let doc = parse(source);
    assert!(
        doc.constructs().is_empty(),
        "未闭合围栏不产生构造（也就不隐藏）"
    );
    assert!(hidden_caret(&doc, 0).is_empty());
    assert!(hidden_caret(&doc, 10).is_empty());
}

#[test]
fn inline_bold_emphasis_code_strike_ranges() {
    let cases = [
        (
            "**a**",
            ConstructKind::Bold,
            (0, 5),
            vec![(0, 2), (3, 5)],
            (2, 3),
        ),
        (
            "*a*",
            ConstructKind::Emphasis,
            (0, 3),
            vec![(0, 1), (2, 3)],
            (1, 2),
        ),
        (
            "_a_",
            ConstructKind::Emphasis,
            (0, 3),
            vec![(0, 1), (2, 3)],
            (1, 2),
        ),
        (
            "`a`",
            ConstructKind::InlineCode,
            (0, 3),
            vec![(0, 1), (2, 3)],
            (1, 2),
        ),
        (
            "``a``",
            ConstructKind::InlineCode,
            (0, 5),
            vec![(0, 2), (3, 5)],
            (2, 3),
        ),
        (
            "~~a~~",
            ConstructKind::Strike,
            (0, 5),
            vec![(0, 2), (3, 5)],
            (2, 3),
        ),
    ];
    for (text, kind, span, markers, content) in cases {
        let doc = parse(text);
        assert_eq!(doc.constructs().len(), 1, "{text}");
        let construct = &doc.constructs()[0];
        assert_eq!(construct.kind(), kind, "{text}");
        assert_eq!(span_of(construct), span, "{text}");
        assert_eq!(markers_of(construct), markers, "{text}");
        assert_eq!(content_of(construct), content, "{text}");
        assert_eq!(
            &doc.source()[content.0..content.1],
            "a",
            "内容区间必须正好是内容文字"
        );
    }

    // `_` 不把 snake_case 误判成斜体。
    assert!(parse("snake_case_name").constructs().is_empty());
}

#[test]
fn link_markers_cover_brackets_and_destination() {
    let doc = parse("[text](url)");
    assert_eq!(doc.constructs().len(), 1);
    let link = &doc.constructs()[0];
    assert_eq!(link.kind(), ConstructKind::Link);
    assert_eq!(span_of(link), (0, 11));
    assert_eq!(markers_of(link), vec![(0, 1), (5, 11)]);
    assert_eq!(content_of(link), (1, 5));
    assert_eq!(&doc.source()[1..5], "text");

    let inline = parse("see [a](b) and");
    assert_eq!(inline.constructs().len(), 1);
    assert_eq!(span_of(&inline.constructs()[0]), (4, 10));
    assert_eq!(markers_of(&inline.constructs()[0]), vec![(4, 5), (6, 10)]);
}

#[test]
fn adjacent_bold_markers_are_split_into_two_constructs() {
    let doc = parse("**a****b**");
    assert_eq!(doc.constructs().len(), 2);
    let first = &doc.constructs()[0];
    let second = &doc.constructs()[1];
    assert_eq!(first.kind(), ConstructKind::Bold);
    assert_eq!(second.kind(), ConstructKind::Bold);
    assert_eq!(span_of(first), (0, 5));
    assert_eq!(markers_of(first), vec![(0, 2), (3, 5)]);
    assert_eq!(content_of(first), (2, 3));
    assert_eq!(span_of(second), (5, 10));
    assert_eq!(markers_of(second), vec![(5, 7), (8, 10)]);
    assert_eq!(content_of(second), (7, 8));
}

#[test]
fn unclosed_inline_markers_emit_no_construct() {
    for text in [
        "**a", "*a", "`a", "~~a", "[a](url", "[a]", "** a**", "**a **",
    ] {
        let doc = parse(text);
        assert!(
            doc.constructs().is_empty(),
            "未闭合/不完整标记不得产生构造：{text:?}"
        );
        assert!(
            hidden_caret(&doc, 0).is_empty(),
            "未闭合时绝不隐藏：{text:?}"
        );
    }
}

#[test]
fn escaped_markers_are_not_constructs() {
    // 被反斜杠转义的定界符不构成标记。
    assert!(parse("\\*not em\\*").constructs().is_empty());
    assert!(parse("\\`not code\\`").constructs().is_empty());
    assert!(parse("\\~~not strike~~").constructs().is_empty());
    assert!(parse("\\[a](b)").constructs().is_empty());

    // 转义的反斜杠不再转义下一个字符：`\\*em*` → 从第 3 个字节开始是斜体。
    let doc = parse("\\\\*em*");
    assert_eq!(doc.constructs().len(), 1);
    assert_eq!(doc.constructs()[0].kind(), ConstructKind::Emphasis);
    assert_eq!(span_of(&doc.constructs()[0]), (2, 6));
}

#[test]
fn inline_code_content_is_not_parsed_as_markdown() {
    let doc = parse("`**x**`");
    assert_eq!(doc.constructs().len(), 1, "行内代码内部不得再解析");
    let code = &doc.constructs()[0];
    assert_eq!(code.kind(), ConstructKind::InlineCode);
    assert_eq!(span_of(code), (0, 7));
    assert_eq!(markers_of(code), vec![(0, 1), (6, 7)]);
    assert_eq!(content_of(code), (1, 6));
    assert_eq!(&doc.source()[1..6], "**x**");

    let mixed = parse("`a *b* c`");
    assert_eq!(mixed.constructs().len(), 1);
    assert_eq!(mixed.constructs()[0].kind(), ConstructKind::InlineCode);
}

#[test]
fn caret_inside_construct_reveals_only_that_construct() {
    let doc = parse("**a** *b*");
    // caret 落在粗体整段（0..=5）内：粗体显现，斜体仍隐藏。
    for caret in 0..=5 {
        assert_eq!(
            hidden_caret(&doc, caret),
            vec![(6, 7), (8, 9)],
            "caret={caret}"
        );
    }
    // caret 落在斜体整段（6..=9）内：斜体显现，粗体隐藏。
    for caret in 6..=9 {
        assert_eq!(
            hidden_caret(&doc, caret),
            vec![(0, 2), (3, 5)],
            "caret={caret}"
        );
    }

    // 两个构造之外（第二行纯文本）：两边的标记都隐藏。
    let outside = parse("**a**\nplain");
    assert_eq!(hidden_caret(&outside, 7), vec![(0, 2), (3, 5)]);
    // 行尾/构造尾边界算“在构造内”（inclusive end）。
    assert!(hidden_caret(&outside, 5).is_empty());
}

#[test]
fn selection_spanning_constructs_reveals_all_intersected() {
    let doc = parse("**a** *b*");

    // 全选：所有构造都显现。
    assert!(hidden_selection(&doc, 0, 9).is_empty());
    // 只碰到粗体：斜体保持隐藏。
    assert_eq!(hidden_selection(&doc, 0, 3), vec![(6, 7), (8, 9)]);
    // 只碰到斜体（选区从粗体结束处开始）：粗体保持隐藏。
    assert_eq!(hidden_selection(&doc, 5, 9), vec![(0, 2), (3, 5)]);
    // 落在两者之间的空格：两边都隐藏。
    assert_eq!(
        hidden_selection(&doc, 5, 6),
        vec![(0, 2), (3, 5), (6, 7), (8, 9)]
    );
    // 折叠选区等价于 caret。
    let collapsed = Selection::new(UnicodeScalar::new(2), UnicodeScalar::new(2));
    assert_eq!(
        pairs(
            &doc.hidden_ranges_for_selection(collapsed)
                .expect("折叠选区合法")
        ),
        hidden_caret(&doc, 2)
    );
}

#[test]
fn multi_line_selection_reveals_constructs_on_both_lines() {
    let doc = parse("# H\n**b**\n");
    // 选区从标题行跨到粗体行：两个构造都显现。
    assert!(hidden_selection(&doc, 0, 9).is_empty());
    // caret 在标题行：只显现标题，第二行粗体照旧隐藏。
    assert_eq!(hidden_caret(&doc, 2), vec![(4, 6), (7, 9)]);
    // caret 在粗体内容里：只显现粗体。
    assert_eq!(hidden_caret(&doc, 6), vec![(0, 2)]);
}

#[test]
fn empty_document_and_plain_text_are_safe() {
    let empty = parse("");
    assert!(empty.constructs().is_empty());
    assert_eq!(empty.source_len(), 0);
    assert!(hidden_caret(&empty, 0).is_empty());

    for text in [
        "hello world",
        "中文，全角标点。！？",
        "\\",
        "\\x",
        "1 + 2 = 3",
        "a_b_c",
    ] {
        let doc = parse(text);
        assert!(doc.constructs().is_empty(), "{text:?}");
        assert!(hidden_caret(&doc, 0).is_empty(), "{text:?}");
    }
}

#[test]
fn unicode_text_ranges_stay_on_char_boundaries() {
    let source = "中文，全角。**粗体**";
    let doc = parse(source);
    assert_eq!(doc.constructs().len(), 1);
    let bold = &doc.constructs()[0];
    assert_eq!(bold.kind(), ConstructKind::Bold);
    assert_eq!(span_of(bold), (18, 28));
    assert_eq!(markers_of(bold), vec![(18, 20), (26, 28)]);
    assert_eq!(content_of(bold), (20, 26));
    assert_eq!(&source[20..26], "粗体");

    // 非 ASCII 前缀下 caret（scalar）换算正确。
    let doc = parse("😀 **a**");
    assert_eq!(span_of(&doc.constructs()[0]), (5, 10));
    assert!(hidden_caret(&doc, 2).is_empty(), "caret 在粗体 span 内");
    assert_eq!(hidden_caret(&doc, 0), vec![(5, 7), (8, 10)]);
}

#[test]
fn hidden_ranges_are_merged_sorted_and_cover_only_markers() {
    // 引用两层前缀相邻 → 合并；粗体标记与引用前缀相邻 → 再合并；内容 `a` 不隐藏。
    let doc = parse("> > **a**\nplain");
    let hidden = hidden_caret(&doc, 11);
    assert_eq!(hidden, vec![(0, 6), (7, 9)]);
    for (start, end) in &hidden {
        assert!(start < end, "隐藏区间非空");
        assert!(*end <= doc.source_len(), "隐藏区间在范围内");
    }
    for window in hidden.windows(2) {
        assert!(window[0].1 <= window[1].0, "隐藏区间按序且不重叠");
    }
}

#[test]
fn invariants_and_determinism_hold_on_a_complex_document() {
    let source = "# Title **bold**\n> > quote *em*\n- [link](url) `code`\nplain\n";
    let doc = parse(source);

    // 构造有序、非空、标记在 span 内且互不重叠、内容在 span 内。
    for window in doc.constructs().windows(2) {
        assert!(
            window[0].span().start().get() <= window[1].span().start().get(),
            "构造按 span 起点有序"
        );
    }
    for construct in doc.constructs() {
        let (span_start, span_end) = span_of(construct);
        assert!(span_start < span_end, "构造 span 非空：{construct:?}");
        assert!(!construct.markers().is_empty(), "构造必须有标记");
        let mut previous_end = span_start;
        for marker in construct.markers() {
            let (start, end) = (marker.start().get(), marker.end().get());
            assert!(start < end, "标记非空：{construct:?}");
            assert!(span_start <= start && end <= span_end, "标记在 span 内");
            assert!(previous_end <= start, "同一构造的标记不重叠");
            previous_end = end;
        }
        let (content_start, content_end) = content_of(construct);
        assert!(
            span_start <= content_start && content_start <= content_end && content_end <= span_end,
            "内容区间在 span 内：{construct:?}"
        );
    }

    // caret 在最后一行纯文本里 → 所有标记都隐藏，且只隐藏标记。
    let plain_caret = source.chars().count() - 1;
    let hidden = doc
        .hidden_ranges_for_caret(UnicodeScalar::new(plain_caret))
        .expect("caret 合法");
    for range in &hidden {
        assert!(covered_by_markers(&doc, range), "隐藏区间只由标记组成");
        assert!(range.start().get() < range.end().get());
        assert!(range.end().get() <= doc.source_len());
    }
    for window in hidden.windows(2) {
        assert!(window[0].end().get() <= window[1].start().get());
    }

    // 确定性：同一输入两次扫描完全一致；同一 caret 两次隐藏结果一致。
    let again = parse(source);
    assert_eq!(doc, again);
    assert_eq!(
        pairs(&hidden),
        pairs(
            &again
                .hidden_ranges_for_caret(UnicodeScalar::new(plain_caret))
                .expect("caret 合法")
        )
    );

    // 空文档与纯文本已在单独用例覆盖；这里再确认构造数量符合预期：
    // 标题 + 粗体 + 引用 + 斜体 + 列表 + 链接 + 行内代码 = 7。
    assert_eq!(doc.constructs().len(), 7);
}

#[test]
fn inline_construct_content_is_parsed_recursively() {
    // 粗体里的行内代码：外层 Bold + 内层 InlineCode。
    let doc = parse("**a `code` b**");
    assert_eq!(doc.constructs().len(), 2);
    let bold = &doc.constructs()[0];
    let code = &doc.constructs()[1];
    assert_eq!(bold.kind(), ConstructKind::Bold);
    assert_eq!(span_of(bold), (0, 14));
    assert_eq!(markers_of(bold), vec![(0, 2), (12, 14)]);
    assert_eq!(content_of(bold), (2, 12));
    assert_eq!(code.kind(), ConstructKind::InlineCode);
    assert_eq!(span_of(code), (4, 10));
    assert_eq!(markers_of(code), vec![(4, 5), (9, 10)]);
    assert_eq!(content_of(code), (5, 9));

    // 粗体里的斜体。
    let doc = parse("**a *b* c**");
    assert_eq!(doc.constructs().len(), 2);
    assert_eq!(doc.constructs()[0].kind(), ConstructKind::Bold);
    assert_eq!(doc.constructs()[1].kind(), ConstructKind::Emphasis);
    assert_eq!(span_of(&doc.constructs()[0]), (0, 11));
    assert_eq!(content_of(&doc.constructs()[0]), (2, 9));
    assert_eq!(span_of(&doc.constructs()[1]), (4, 7));
    assert_eq!(content_of(&doc.constructs()[1]), (5, 6));

    // 斜体里的粗体（不同种定界符，内容递短，递归必然终止）。
    let doc = parse("*a **b** c*");
    assert_eq!(doc.constructs().len(), 2);
    assert_eq!(doc.constructs()[0].kind(), ConstructKind::Emphasis);
    assert_eq!(doc.constructs()[1].kind(), ConstructKind::Bold);
    assert_eq!(span_of(&doc.constructs()[0]), (0, 11));
    assert_eq!(content_of(&doc.constructs()[0]), (1, 10));
    assert_eq!(span_of(&doc.constructs()[1]), (3, 8));

    // 链接文本里的斜体：只解析 `[]` 内文本，URL 原文不解析。
    let doc = parse("[a *b*](u)");
    assert_eq!(doc.constructs().len(), 2);
    assert_eq!(doc.constructs()[0].kind(), ConstructKind::Link);
    assert_eq!(doc.constructs()[1].kind(), ConstructKind::Emphasis);
    assert_eq!(span_of(&doc.constructs()[0]), (0, 10));
    assert_eq!(content_of(&doc.constructs()[0]), (1, 6));
    assert_eq!(span_of(&doc.constructs()[1]), (3, 6));

    // 行内代码内部仍然不解析（既有契约不变）。
    let code = parse("`a **b** c`");
    assert_eq!(code.constructs().len(), 1);
    assert_eq!(code.constructs()[0].kind(), ConstructKind::InlineCode);
}

#[test]
fn bare_markers_do_not_become_blocks_through_legacy_fallback() {
    // 新解析器不把无空格 marker 当块；旧 scanner 只允许行内 fallback，不能补成列表/标题。
    for source in ["-", "1.", "#"] {
        let doc = parse(source);
        assert!(
            !doc.constructs().iter().any(|construct| matches!(
                construct.kind(),
                ConstructKind::ListItem | ConstructKind::Heading
            )),
            "无空格 marker 不得变成块：{source:?}"
        );
    }
    for source in ["- ", "1. ", "# "] {
        let doc = parse(source);
        assert!(
            doc.constructs().iter().any(|construct| matches!(
                construct.kind(),
                ConstructKind::ListItem | ConstructKind::Heading
            )),
            "有空格 marker 应成为块：{source:?}"
        );
    }
}
