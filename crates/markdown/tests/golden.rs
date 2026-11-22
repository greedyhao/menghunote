//! ADR 0019 S1 解析器 golden 测试：source byte offsets / span adapter / 列表块树。
//!
//! 覆盖：嵌套列表、松紧、有序起始号、续行/子段落/内部围栏、转义、实体、引用式链接、
//! CRLF、中文/emoji、未完成构造、10 万字节混合文档基线。
//!
//! 断言全部用**原始源码切片**（`&source[span.start..span.end]`），不使用 AST 重新
//! 序列化结果——这正是 span adapter 要保证的东西。

use std::time::Instant;

use menghunote_markdown::{parse, ConstructKind, LineKind, Span};

fn slice(source: &str, span: Span) -> &str {
    &source[span.start..span.end]
}

fn construct(source: &str, kind: ConstructKind) -> menghunote_markdown::Construct {
    parse(source)
        .constructs
        .into_iter()
        .find(|construct| construct.kind == kind)
        .unwrap_or_else(|| panic!("缺少构造 {kind:?}：{source:?}"))
}

#[test]
fn heading_and_inline_offsets_are_utf8_bytes() {
    let source = "# 标题 **粗体**\n";
    let doc = parse(source);
    let heading = construct(source, ConstructKind::Heading(1));
    assert_eq!(slice(source, heading.span), "# 标题 **粗体**");
    assert_eq!(slice(source, heading.markers[0]), "# ");
    assert_eq!(slice(source, heading.content), "标题 **粗体**");

    let bold = construct(source, ConstructKind::Bold);
    assert_eq!(slice(source, bold.span), "**粗体**");
    assert_eq!(slice(source, bold.markers[0]), "**");
    assert_eq!(slice(source, bold.markers[1]), "**");
    assert_eq!(slice(source, bold.content), "粗体");
    // 中文每字 3 字节：span 端点必须落在 char 边界上，切片本身不 panic。
    assert!(source.is_char_boundary(bold.span.start));
    assert!(source.is_char_boundary(bold.span.end));
    let _ = doc;
}

#[test]
fn nested_list_tree_records_depth_numbers_and_ranges() {
    let source = "- a\n  - b\n\n    para\n\n    ```\n    code\n    ```\n1. one\n2. two\n";
    let doc = parse(source);
    assert_eq!(doc.items.len(), 4);
    let first = &doc.items[0];
    let nested = &doc.items[1];
    assert_eq!(first.depth, 0);
    assert_eq!(nested.depth, 1);
    assert_eq!(slice(source, first.marker), "- ");
    assert_eq!(slice(source, nested.marker), "- ");
    assert_eq!(first.first_line, 0);
    assert_eq!(nested.first_line, 1);
    // 松列表：中间有空行 + 子段落。
    assert!(first.loose, "有空行/子段落的列表必须是 loose");
    assert!(!doc.lists[0].tight);
    // 子段落与内部围栏都属于嵌套 item（续行缩进 4 字节）。
    assert!(nested.last_line >= 7);
    assert_eq!(
        doc.lines[3].list.as_ref().map(|list| list.depth),
        Some(1),
        "子段落续行仍归属嵌套 item"
    );
    assert_eq!(doc.lines[3].kind, LineKind::Paragraph);
    assert_eq!(
        slice(
            source,
            doc.lines[3].list.as_ref().expect("列表行").hidden_prefix
        ),
        "    ",
        "子段落前导空白由布局接管"
    );
    // 内部围栏：语义上有 FencedCode 构造，且行被标记为 FencedCode。
    let fence = construct(source, ConstructKind::FencedCode);
    assert_eq!(slice(source, fence.markers[0]), "```");
    assert_eq!(slice(source, fence.markers[1]), "```");
    assert_eq!(slice(source, fence.content), "    code\n");
    assert!(
        (5..=7).all(|line| doc.lines[line].kind == LineKind::FencedCode),
        "围栏三行都必须是 FencedCode"
    );

    // 有序列表：起始号、连续编号、右对齐宽度。
    let ordered = &doc.items[2];
    assert!(ordered.ordered);
    assert_eq!(ordered.number, 1);
    assert_eq!(doc.items[3].number, 2);
    assert_eq!(doc.lists[1].start, 1);
    assert_eq!(doc.lists[1].number_width, 1);
}

#[test]
fn ordered_start_and_number_width_are_preserved() {
    let source = "9. nine\n10. ten\n11. eleven\n";
    let doc = parse(source);
    assert_eq!(doc.lists.len(), 1);
    assert_eq!(doc.lists[0].start, 9);
    assert_eq!(doc.lists[0].number_width, 2);
    assert_eq!(
        doc.items.iter().map(|item| item.number).collect::<Vec<_>>(),
        vec![9, 10, 11]
    );
    assert_eq!(slice(source, doc.items[1].marker), "10. ");
}

#[test]
fn tight_and_loose_lists_are_distinguished() {
    let tight = parse("- a\n- b\n");
    assert!(tight.lists[0].tight);
    assert!(tight.items.iter().all(|item| !item.loose));

    let loose = parse("- a\n\n- b\n");
    assert!(!loose.lists[0].tight);
    assert!(loose.items.iter().all(|item| item.loose));
}

#[test]
fn escapes_entities_and_reference_links_keep_raw_bytes() {
    // 转义：`\*` 不产生 Emphasis；`\[` 不产生 Link。
    let escaped = parse("\\*not em\\* and \\[a](b)\n");
    assert!(
        !escaped.constructs.iter().any(|construct| matches!(
            construct.kind,
            ConstructKind::Emphasis | ConstructKind::Link
        )),
        "转义后的定界符不得产生构造"
    );

    // 实体：display 不重写正文，构造只覆盖定界符，内容保留原始 `&amp;`。
    let source = "**a &amp; b**\n";
    let doc = parse(source);
    let bold = construct(source, ConstructKind::Bold);
    assert_eq!(slice(source, bold.markers[0]), "**");
    assert_eq!(slice(source, bold.markers[1]), "**");
    assert_eq!(slice(source, bold.content), "a &amp; b");
    assert_eq!(doc.constructs.len(), 1, "实体不得额外产生构造");

    // 引用式链接：`[t][lbl]` 的 marker 是 `[` 与 `][lbl]`，内容只有 `t`。
    let reference = "[t][lbl]\n\n[lbl]: /u\n";
    let doc = parse(reference);
    let link = construct(reference, ConstructKind::Link);
    assert_eq!(slice(reference, link.span), "[t][lbl]");
    assert_eq!(slice(reference, link.markers[0]), "[");
    assert_eq!(slice(reference, link.markers[1]), "][lbl]");
    assert_eq!(slice(reference, link.content), "t");
    // 定义行本身不产生构造。
    assert_eq!(doc.constructs.len(), 1);
}

#[test]
fn crlf_chinese_and_emoji_offsets_are_exact() {
    let source = "# 标题\r\n\r\n- 中文 😀\r\n";
    let doc = parse(source);
    let heading = construct(source, ConstructKind::Heading(1));
    assert_eq!(slice(source, heading.span), "# 标题");
    assert_eq!(slice(source, heading.markers[0]), "# ");
    assert_eq!(slice(source, heading.content), "标题");
    // 行区间包含 `\r\n`，内容区间不含。
    assert_eq!(doc.lines[0].span.end - doc.lines[0].span.start, 10);
    assert_eq!(slice(source, doc.lines[0].content), "# 标题");
    // 列表 item 的 marker/content 都必须落在 emoji/中文之后仍正确。
    let item = &doc.items[0];
    assert_eq!(slice(source, item.marker), "- ");
    assert_eq!(
        &source[item.content_start..doc.lines[2].content.end],
        "中文 😀"
    );
    // 所有端点都在 char 边界上。
    for line in &doc.lines {
        assert!(source.is_char_boundary(line.span.start));
        assert!(source.is_char_boundary(line.span.end));
        assert!(source.is_char_boundary(line.content.start));
        assert!(source.is_char_boundary(line.content.end));
    }
}

#[test]
fn unfinished_constructs_are_not_claimed_as_projected() {
    // 未闭合粗体：没有 Bold 构造。
    assert!(!parse("**a\n")
        .constructs
        .iter()
        .any(|construct| construct.kind == ConstructKind::Bold));
    // 未闭合链接：没有 Link 构造。
    assert!(!parse("[a](b\n")
        .constructs
        .iter()
        .any(|construct| construct.kind == ConstructKind::Link));
    // 未闭合围栏：没有 FencedCode 构造（哪怕解析器知道它是代码块）。
    let unclosed = parse("```\ncode\n");
    assert!(!unclosed
        .constructs
        .iter()
        .any(|construct| construct.kind == ConstructKind::FencedCode));
    assert_eq!(unclosed.code_spans.len(), 1, "代码块本身仍要标记出来");
    // `***a***`：span adapter 无法给出无歧义的 marker 定位 → 行内不确定。
    let triple = parse("***a***\n");
    assert!(
        triple.uncertain_inline,
        "三种定界符嵌套必须标记为定位不确定"
    );
}

#[test]
fn task_list_markers_attach_to_their_own_item() {
    // 事件顺序 Start(Item) → TaskListMarker；必须挂在当前 item，而不是下一个。
    let doc = parse("- [ ] todo\n- [x] done\n");
    assert_eq!(doc.items.len(), 2);
    assert_eq!(doc.items[0].task, Some(false));
    assert_eq!(doc.items[1].task, Some(true));
}

#[test]
fn tables_and_setext_are_unsupported_spans_not_constructs() {
    let table = parse("| a | b |\n|---|---|\n| 1 | 2 |\n");
    assert!(!table.unsupported_spans.is_empty());
    assert_eq!(table.lines[0].kind, LineKind::Table);
    assert!(table
        .constructs
        .iter()
        .all(|construct| !construct.kind.is_inline()));

    let setext = parse("Title\n=====\n");
    assert!(!setext.unsupported_spans.is_empty());
    assert!(!setext
        .constructs
        .iter()
        .any(|construct| matches!(construct.kind, ConstructKind::Heading(_))));
}

#[test]
fn options_enable_the_three_gfm_extensions_explicitly() {
    use pulldown_cmark::Options;
    let options = menghunote_markdown::options();
    for required in [
        Options::ENABLE_TABLES,
        Options::ENABLE_TASKLISTS,
        Options::ENABLE_STRIKETHROUGH,
    ] {
        assert!(options.contains(required), "缺少 {required:?}");
    }
    assert!(
        !options.contains(Options::ENABLE_GFM),
        "ADR 明确禁止用 ENABLE_GFM 代替三项扩展"
    );
}

#[test]
fn hundred_kilobyte_mixed_document_baseline() {
    let mut source = String::new();
    let mut index = 0usize;
    while source.len() < 100_000 {
        source.push_str(&format!(
            "# 标题 {index}\n\n- item **bold** [link](url) `code`\n  - nested\n\n> quote *em*\n\n```\ncode\n```\n\n"
        ));
        index += 1;
    }
    let started = Instant::now();
    let doc = parse(&source);
    let elapsed = started.elapsed();
    assert!(!doc.constructs.is_empty());
    assert!(!doc.items.is_empty());
    // 基线只记录、不把 CI 卡死：极宽松上限（正常 < 100ms）。
    assert!(
        elapsed.as_millis() < 5_000,
        "100KB 解析耗时异常：{elapsed:?}"
    );
    println!(
        "[markdown-perf] bytes={} ms={} constructs={} items={} lines={}",
        source.len(),
        elapsed.as_millis(),
        doc.constructs.len(),
        doc.items.len(),
        doc.lines.len()
    );
}

#[test]
fn list_and_heading_markers_require_a_following_space() {
    for source in ["-\n", "1.\n", "#\n", "###\n"] {
        let doc = parse(source);
        assert!(
            !doc.constructs.iter().any(|construct| matches!(
                construct.kind,
                ConstructKind::Heading(_) | ConstructKind::ListItem
            )),
            "marker 后没有空格不应成为块构造：{source:?}"
        );
        assert!(doc.lists.is_empty(), "{source:?}");
    }
    for source in ["- item\n", "1. item\n", "# title\n"] {
        let doc = parse(source);
        assert!(
            doc.constructs.iter().any(|construct| matches!(
                construct.kind,
                ConstructKind::Heading(_) | ConstructKind::ListItem
            )),
            "marker 后有空格应成为块构造：{source:?}"
        );
    }
}
