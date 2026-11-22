//! ADR 0019 S1 解析器 golden 测试：source byte offsets / span adapter / 列表块树。
//!
//! 覆盖：嵌套列表、松紧、有序起始号、续行/子段落/内部围栏、转义、实体、引用式链接、
//! CRLF、中文/emoji、未完成构造、10 万字节混合文档基线。
//!
//! 断言全部用**原始源码切片**（`&source[span.start..span.end]`），不使用 AST 重新
//! 序列化结果——这正是 span adapter 要保证的东西。

use std::time::Instant;

use menghunote_markdown::{parse, ConstructKind, LineKind, Span, TableAlignment};

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
fn task_list_marker_range_covers_brackets_only() {
    // ADR 0019 S3：任务框的源码区间必须正好是 `[ ]` / `[x]` / `[X]` 两个方括号及
    // 其内部字符（3 字节），**不含** `]` 之后的空白，也不含内容。
    let source = "- [ ] todo\n- [x] done\n- [X] upper\n";
    let doc = parse(source);
    assert_eq!(doc.items.len(), 3);
    assert!(
        doc.items.iter().all(|item| item.task_marker.is_some()),
        "三个任务项都必须有任务框区间"
    );
    let texts: Vec<&str> = doc
        .items
        .iter()
        .map(|item| {
            let span = item.task_marker.expect("任务项必须有 task_marker");
            &source[span.start..span.end]
        })
        .collect();
    assert_eq!(texts, vec!["[ ]", "[x]", "[X]"]);
    // 区间终点正好在 `]` 之后：后续空格/正文不在区间内。
    let first = doc.items[0].task_marker.expect("box");
    assert_eq!(first.end, 5);
    assert_eq!(&source[first.end..first.end + 1], " ", "任务框不含后续空格");
    assert_eq!(doc.items[0].task, Some(false));
    assert_eq!(doc.items[1].task, Some(true));
    assert_eq!(doc.items[2].task, Some(true), "大写 X 也解析为勾选");
}

#[test]
fn task_list_marker_spans_work_in_nested_ordered_and_quoted_items() {
    let source = "- [ ] parent\n  - [x] child\n\n1. [X] ordered\n\n> - [ ] quoted\n";
    let doc = parse(source);
    let boxes: Vec<(String, bool)> = doc
        .items
        .iter()
        .filter_map(|item| {
            let span = item.task_marker?;
            Some((source[span.start..span.end].to_owned(), item.task?))
        })
        .collect();
    assert_eq!(
        boxes,
        vec![
            ("[ ]".to_owned(), false),
            ("[x]".to_owned(), true),
            ("[X]".to_owned(), true),
            ("[ ]".to_owned(), false),
        ]
    );
    // 每个 span 都必须是 3 字节且以方括号包住一个 GFM 允许的字符。
    for item in doc.items.iter().filter(|item| item.task_marker.is_some()) {
        let span = item.task_marker.expect("box");
        let slice = &source[span.start..span.end];
        assert_eq!(slice.len(), 3, "{slice:?}");
        assert!(slice.starts_with('[') && slice.ends_with(']'), "{slice:?}");
    }
}

#[test]
fn non_task_and_malformed_task_items_expose_no_box() {
    for source in [
        "- plain item\n",
        // `]` 后没有空白：GFM 不把它当任务项。
        "- [ ]no-space\n",
        // marker 后 5 个空格：pulldown 的任务框扫描只看 ≤3 个前导空格。
        "-     [ ] five-spaces\n",
        // 不是 GFM 允许的框字符。
        "- [ab] nope\n",
    ] {
        let doc = parse(source);
        assert!(
            doc.items.iter().all(|item| item.task.is_none()),
            "{source:?} 不应被解析成任务项"
        );
        assert!(
            doc.items.iter().all(|item| item.task_marker.is_none()),
            "{source:?} 不应暴露任务框区间"
        );
    }
}

#[test]
fn tables_have_a_block_tree_and_are_not_unsupported_spans() {
    // ADR 0019 S3a：表格是**已支持原子块**（有自己的块树），不再进 `unsupported_spans`。
    let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
    let table = parse(source);
    assert!(
        table.unsupported_spans.is_empty(),
        "表格不得再作为未支持块：{:?}",
        table.unsupported_spans
    );
    assert_eq!(table.lines[0].kind, LineKind::Table);
    assert_eq!(table.lines[2].kind, LineKind::Table);
    assert_eq!(table.tables.len(), 1);
    let block = &table.tables[0];
    assert_eq!(slice(source, block.span), "| a | b |\n|---|---|\n| 1 | 2 |");
    assert_eq!(block.columns, 2);
    assert_eq!(block.alignments, vec![TableAlignment::None; 2]);
    assert_eq!((block.first_line, block.last_line), (0, 2));
    assert_eq!(slice(source, block.delimiter), "|---|---|");
    assert_eq!(slice(source, block.header.span), "| a | b |");
    assert_eq!(
        block
            .header
            .cells
            .iter()
            .map(|cell| slice(source, *cell))
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    assert_eq!(block.row_count(), 1);
    let row = block.row(0).expect("必须有一条数据行");
    assert_eq!(slice(source, row.span), "| 1 | 2 |");
    assert_eq!(row.line, 2);
    assert_eq!(
        row.cells
            .iter()
            .map(|cell| slice(source, *cell))
            .collect::<Vec<_>>(),
        vec!["1", "2"]
    );
    // 表格内部不产生可投影构造（Object 是原子块）。
    assert!(table
        .constructs
        .iter()
        .all(|construct| construct.span.start < block.span.start
            || construct.span.end > block.span.end));

    // Setext 标题仍是未支持块（S1 决定不变）。
    let setext = parse("Title\n=====\n");
    assert!(!setext.unsupported_spans.is_empty());
    assert!(!setext
        .constructs
        .iter()
        .any(|construct| matches!(construct.kind, ConstructKind::Heading(_))));
}

#[test]
fn table_alignment_columns_and_escaped_pipes_are_pinned() {
    let source = "|a|b|c|d|\n|:--|:-:|--:|---|\n| 1 | 2 | 3 | 4 |\n| c\\|d | `x|y` | **e** |  |\n";
    let doc = parse(source);
    let table = doc.tables.first().expect("必须解析出一张表");
    assert_eq!(table.columns, 4);
    assert_eq!(
        table.alignments,
        vec![
            TableAlignment::Left,
            TableAlignment::Center,
            TableAlignment::Right,
            TableAlignment::None,
        ]
    );
    assert_eq!(table.alignment(0), TableAlignment::Left);
    assert_eq!(table.alignment(9), TableAlignment::None, "越界按默认对齐");
    assert_eq!(table.row_count(), 2);

    // escaped `|` 属于 cell 原文（源码逐字节保留，不把 `\\|` 当列边界）。
    let row = table.row(1).expect("第二行数据");
    assert_eq!(slice(source, row.cells[0]), "c\\|d");
    // 行内代码里的 `|` 按 GFM 是列边界（要写 `\\|` 才是字面 pipe）：
    // `` `x|y` `` 会被切分，plldown 也会抛掉多余列；这里只保证不 panic、列数稳定。
    assert_eq!(row.cells.len(), 4);
    // GFM 承认行内代码里的 `|` 仍是列边界（字面 pipe 必须写 `\|`）；`|  |` 多出的
    // 第 5 列被丢弃，第 4 格正是后面的 `**e**`。
    assert_eq!(slice(source, row.cells[3]), "**e**");
    // 行内格式在表格里不投影（原子块）：没有 Bold 构造。
    assert!(
        !doc.constructs
            .iter()
            .any(|construct| construct.kind == ConstructKind::Bold),
        "表格内部的 `**e**` 不应产生可投影构造"
    );

    // 对齐分隔行本身不是数据行，但整行区间可以按源码切片。
    assert_eq!(slice(source, table.delimiter), "|:--|:-:|--:|---|");
}

#[test]
fn escaped_pipes_inside_inline_code_are_one_cell() {
    // GFM：cell 里的字面 pipe 必须转义（**包括行内代码内部**），`\|` 不切列。
    let source = "| h | h |\n|---|---|\n| `a\\|b` | c |\n";
    let doc = parse(source);
    let table = doc.tables.first().expect("必须解析出表格");
    assert_eq!(table.columns, 2);
    let row = table.row(0).expect("一行数据");
    assert_eq!(
        slice(source, row.cells[0]),
        "`a\\|b`",
        "转义 pipe 属于 cell 原文"
    );
    assert_eq!(slice(source, row.cells[1]), "c");

    // 未转义的 pipe（即使写在行内代码里）按 GFM 是列边界：`` `a|b` `` 被切开，
    // 这里只钉住“不 panic、列数稳定、源码逐字节保留”的行为。
    let unescaped = "| h | h |\n|---|---|\n| `a|b` | c |\n";
    let doc = parse(unescaped);
    let table = doc.tables.first().expect("仍必须解析出表格");
    assert_eq!(table.columns, 2);
    let row = table.row(0).expect("一行数据");
    assert_eq!(slice(unescaped, row.cells[0]), "`a");
    assert_eq!(slice(unescaped, row.cells[1]), "b`");
}

#[test]
fn table_without_header_or_delimiter_is_not_a_table() {
    for source in [
        // 只有一行：没有分隔行，GFM 不认为它是表格。
        "| 1 | 2 |\n",
        // 分隔行放在第一行：pulldown 按普通段落处理。
        "|---|---|\n|1|2|\n",
    ] {
        let doc = parse(source);
        assert!(
            doc.tables.is_empty(),
            "{source:?} 不应该是表格：{:?}",
            doc.tables
        );
        assert!(
            doc.unsupported_spans.is_empty(),
            "{source:?} 也不应该被当成未支持块：{:?}",
            doc.unsupported_spans
        );
        assert!(doc.lines.iter().all(|line| line.kind != LineKind::Table));
    }

    // 表格前面的普通段落不影响表格从表头行开始（区间不吞前文）。
    let after_paragraph = parse("intro\n\n| a |\n|---|\n");
    let table = after_paragraph
        .tables
        .first()
        .expect("前有段落时仍必须解析出表格");
    assert_eq!(table.first_line, 2);
    assert_eq!(table.span.start, 7);

    // 空表头（`| | |`）仍是合法表格：两列都是空 cell。
    let empty_header = parse("| | |\n|---|---|\n|1|2|\n");
    let table = empty_header.tables.first().expect("空表头仍是表格");
    assert_eq!(table.columns, 2);
    // 空 cell 是“去掉首尾空白后”的空区间（`| |` 的中间空格被裁掉）。
    assert_eq!(table.header.cells, vec![Span::empty(2), Span::empty(4)]);
    assert_eq!(table.row_count(), 1);
}

#[test]
fn table_rows_are_padded_and_truncated_to_the_header_columns() {
    // 缺列：pulldown 会补空 cell（区间可能是空区间）；多列：多出的被丢弃。
    let source = "| a | b |\n|---|---|\n| 1 |\n| 1 | 2 | 3 |\n";
    let doc = parse(source);
    let table = doc.tables.first().expect("必须解析出表格");
    assert_eq!(table.columns, 2);
    assert_eq!(table.row_count(), 2);
    assert_eq!(slice(source, table.rows[0].cells[0]), "1");
    assert_eq!(slice(source, table.rows[0].cells[1]), "", "缺列补空 cell");
    assert_eq!(
        table.rows[0].cells[1].start, table.rows[0].cells[1].end,
        "补出来的 cell 必须是空区间"
    );
    assert_eq!(slice(source, table.rows[1].cells[0]), "1");
    assert_eq!(slice(source, table.rows[1].cells[1]), "2");
    assert_eq!(table.rows[1].cells.len(), 2, "多出的列被丢弃");
}

#[test]
fn table_span_excludes_the_trailing_newline_and_tracks_source_lines() {
    let source = "前\n\n| a |\n|---|\n| b |\n\n后\n";
    let doc = parse(source);
    let table = doc.tables.first().expect("必须解析出表格");
    let span_text = slice(source, table.span);
    assert_eq!(span_text, "| a |\n|---|\n| b |");
    assert!(
        !span_text.ends_with('\n'),
        "整表区间不得包含末尾换行（否则 Object 占位会和下一段粘行）"
    );
    assert_eq!((table.first_line, table.last_line), (2, 4));
    // object 判定的前提：表格从行内容起点开始。
    assert_eq!(table.span.start, doc.lines[table.first_line].content.start);

    // CRLF：行区间不含 `\r`，cell 也不含。
    let crlf = "| a | b |\r\n|---|---|\r\n| 1 | 2 |\r\n";
    let doc = parse(crlf);
    let table = doc.tables.first().expect("CRLF 表格也必须解析");
    assert_eq!(slice(crlf, table.header.span), "| a | b |");
    assert_eq!(slice(crlf, table.rows[0].span), "| 1 | 2 |");
    assert_eq!(slice(crlf, table.rows[0].cells[1]), "2");
}

#[test]
fn indented_quoted_and_list_tables_still_expose_spans() {
    // 表格在引用/列表里仍然有块树（渲染是否投影成 Object 由 editor 决定）。
    for (source, needle) in [
        ("  | a |\n  |---|\n", "| a |"),
        ("> | a |\n> |---|\n", "| a |"),
        ("- | a |\n  |---|\n", "| a |"),
    ] {
        let doc = parse(source);
        let table = doc
            .tables
            .first()
            .unwrap_or_else(|| panic!("{source:?} 必须解析出表格"));
        assert!(slice(source, table.span).starts_with(needle), "{source:?}");
        assert_eq!(table.columns, 1);
        assert_eq!(slice(source, table.header.cells[0]), "a");
        assert_eq!(doc.lines[table.first_line].kind, LineKind::Table);
    }
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

#[test]
fn image_construct_records_dest_alt_and_span_without_rewriting() {
    // ADR 0019 S2a：图片除 Construct（S1 起就有）外，还记录目标 URL。
    let source = "前言\n\n![示意图](asset://0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef)\n";
    let doc = parse(source);
    assert_eq!(doc.images.len(), 1, "必须解析出一个图片：{source:?}");
    let image = &doc.images[0];
    assert_eq!(slice(source, image.content), "示意图");
    assert_eq!(
        image.dest,
        "asset://0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
    // span 与 Construct 完全一致：投影层用同一段源码，不得重新序列化。
    let construct = construct(source, ConstructKind::Image);
    assert_eq!(image.span, construct.span);
    assert_eq!(
        slice(source, image.span),
        "![示意图](asset://0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef)"
    );
}

#[test]
fn reference_style_image_resolves_dest_but_keeps_source_bytes() {
    // 引用式图片：源码里没有 URL，dest 从定义处解析；span 仍是 `![alt][label]`。
    let source = "![图][logo]\n\n[logo]: asset://deadbeef\n";
    let doc = parse(source);
    assert_eq!(doc.images.len(), 1);
    let image = &doc.images[0];
    assert_eq!(slice(source, image.span), "![图][logo]");
    assert_eq!(slice(source, image.content), "图");
    assert_eq!(image.dest, "asset://deadbeef");
}

#[test]
fn image_inside_link_only_reports_the_image_span() {
    // 图片套在链接里（`[![a](b)](c)`）：图片的事件范围只覆盖 `![a](b)`。
    let source = "[![a](asset://aa)](asset://bb)\n";
    let doc = parse(source);
    assert_eq!(doc.images.len(), 1);
    assert_eq!(slice(source, doc.images[0].span), "![a](asset://aa)");
    assert_eq!(doc.images[0].dest, "asset://aa");
}

#[test]
fn wiki_links_parse_alias_heading_and_paths() {
    let source = "[[Note]] [[Note|别名]] [[Folder/Note#标题]] [[#当前标题]]\n";
    let doc = parse(source);
    assert_eq!(doc.wiki_links.len(), 4);
    let first = &doc.wiki_links[0];
    assert_eq!(slice(source, first.span), "[[Note]]");
    assert_eq!(slice(source, first.target), "Note");
    assert_eq!(slice(source, first.display), "Note");
    assert!(first.heading.is_none() && first.alias.is_none());

    let alias = &doc.wiki_links[1];
    assert_eq!(slice(source, alias.span), "[[Note|别名]]");
    assert_eq!(slice(source, alias.target), "Note");
    assert_eq!(slice(source, alias.alias.expect("alias")), "别名");
    assert_eq!(slice(source, alias.display), "别名");

    let heading = &doc.wiki_links[2];
    assert_eq!(slice(source, heading.target), "Folder/Note");
    assert_eq!(slice(source, heading.heading.expect("heading")), "标题");
    assert_eq!(slice(source, heading.display), "Folder/Note#标题");

    let current = &doc.wiki_links[3];
    assert!(current.target.is_empty());
    assert_eq!(slice(source, current.heading.expect("heading")), "当前标题");
    assert_eq!(slice(source, current.display), "#当前标题");

    // 投影构造：alias 时隐藏 target/`|`，其它只隐藏括号。
    let construct = doc
        .constructs
        .iter()
        .find(|construct| {
            construct.kind == ConstructKind::WikiLink && construct.content == alias.display
        })
        .expect("alias 构造");
    assert_eq!(construct.markers.len(), 3);
    assert_eq!(slice(source, construct.content), "别名");
}

#[test]
fn wiki_links_skip_code_escapes_and_embedded_images() {
    let source = "`[[code]]`\n\n```\n[[fence]]\n```\n\n\\[[escaped]] ![[embed]] [[ok]]\n";
    let doc = parse(source);
    assert_eq!(doc.wiki_links.len(), 1, "只应解析最后一个普通链接");
    assert_eq!(slice(source, doc.wiki_links[0].target), "ok");
    assert_eq!(
        doc.wiki_links[0].span.start,
        source.find("[[ok]]").expect("ok")
    );
}

#[test]
fn wiki_links_reject_empty_alias_and_cross_newline() {
    let doc = parse("[[a|]] [[a\nb]] [[unclosed\n");
    assert!(doc.wiki_links.is_empty(), "空别名/跨行/未闭合都不解析");
}

#[test]
fn ordinary_links_keep_resolved_destinations() {
    let source = "[示例](https://example.com/a?b=1) [邮箱](mailto:a@b.c) <https://auto.example>\n";
    let doc = parse(source);
    assert_eq!(doc.links.len(), 3);
    let destinations: Vec<&str> = doc.links.iter().map(|link| link.dest.as_str()).collect();
    assert_eq!(
        destinations,
        [
            "https://example.com/a?b=1",
            "mailto:a@b.c",
            "https://auto.example"
        ]
    );
    assert_eq!(slice(source, doc.links[0].content), "示例");
    assert_eq!(
        slice(source, doc.links[0].span),
        "[示例](https://example.com/a?b=1)"
    );
}
