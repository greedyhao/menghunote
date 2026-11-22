//! S2-7a：块级语义 `BlockRun` 的契约测试。
//!
//! 覆盖：h1/h3 级别、行首空格不算标题、引用、列表、围栏代码内部行、
//! 空行、完整覆盖/不重叠、无标题全 Paragraph、控制器按 source 重建。

mod support;

use menghunote_editor::{BlockKind, BlockRun, EditorController, MarkdownDocument, UnicodeScalar};

fn parse(source: &str) -> MarkdownDocument {
    MarkdownDocument::parse(source).expect("扫描不应失败")
}

fn parse_runs(source: &str) -> Vec<BlockRun> {
    parse(source).block_runs().expect("块级语义不应失败")
}

fn layout(runs: &[BlockRun]) -> Vec<(usize, usize, BlockKind, Option<u8>)> {
    runs.iter()
        .map(|run| {
            (
                run.source().start().get(),
                run.source().end().get(),
                run.kind(),
                run.level(),
            )
        })
        .collect()
}

/// 有序、不重叠、非空、按行完整覆盖 `[0, source_len)`。
fn assert_coverage(source: &str, runs: &[BlockRun]) {
    let mut cursor = 0usize;
    for run in runs {
        assert_eq!(run.source().start().get(), cursor, "块级区间必须连续");
        assert!(
            run.source().end().get() > run.source().start().get(),
            "BlockRun 不能为空"
        );
        assert!(
            source.is_char_boundary(run.source().start().get())
                && source.is_char_boundary(run.source().end().get()),
            "BlockRun 边界必须在字符边界上"
        );
        cursor = run.source().end().get();
    }
    assert_eq!(cursor, source.len(), "块级区间必须覆盖整篇 source");
}

#[test]
fn heading_levels_one_and_three() {
    let source = "# h1\n### h3\n";
    let runs = parse_runs(source);
    assert_eq!(
        layout(&runs),
        vec![
            (0, 5, BlockKind::Heading, Some(1)),
            (5, 12, BlockKind::Heading, Some(3)),
        ]
    );
    assert_eq!(&source[0..5], "# h1\n");
    assert_eq!(&source[5..12], "### h3\n");
    assert_coverage(source, &runs);
}

#[test]
fn leading_spaces_do_not_make_a_heading() {
    // S2-1 按 CommonMark 允许 ≤3 空格缩进识别标题标记，但块级样式层不放大它。
    let source = "   # 缩进\n";
    let runs = parse_runs(source);
    assert_eq!(layout(&runs), vec![(0, 12, BlockKind::Paragraph, None)]);
    assert_coverage(source, &runs);

    // 行首没有空格才是标题。
    assert_eq!(
        layout(&parse_runs("# h\n")),
        vec![(0, 4, BlockKind::Heading, Some(1))]
    );
}

#[test]
fn blockquote_and_list_lines() {
    let source = "> quote\n- item\n1. ordered\n";
    let runs = parse_runs(source);
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].kind(), BlockKind::BlockQuote);
    assert_eq!(
        &source[runs[0].source().start().get()..runs[0].source().end().get()],
        "> quote\n"
    );
    assert_eq!(runs[1].kind(), BlockKind::ListItem);
    assert_eq!(
        &source[runs[1].source().start().get()..runs[1].source().end().get()],
        "- item\n"
    );
    assert_eq!(runs[2].kind(), BlockKind::ListItem);
    assert_eq!(
        &source[runs[2].source().start().get()..runs[2].source().end().get()],
        "1. ordered\n"
    );
    assert_coverage(source, &runs);
}

#[test]
fn fenced_code_inner_lines_are_code_not_headings() {
    let source = "```\n# inside\n```\n# outside\n";
    let runs = parse_runs(source);
    assert_eq!(runs.len(), 4);
    for run in &runs[0..3] {
        assert_eq!(run.kind(), BlockKind::FencedCode, "{run:?}");
        assert_eq!(run.level(), None);
    }
    assert_eq!(
        &source[runs[1].source().start().get()..runs[1].source().end().get()],
        "# inside\n"
    );
    assert_eq!(runs[3].kind(), BlockKind::Heading);
    assert_eq!(runs[3].level(), Some(1));
    assert_coverage(source, &runs);

    // 围栏内的空行同样算 FencedCode。
    let empty_inside = parse_runs("```\n\n```\n");
    assert_eq!(empty_inside.len(), 3);
    assert!(empty_inside
        .iter()
        .all(|run| run.kind() == BlockKind::FencedCode));
}

#[test]
fn blank_lines_are_paragraphs() {
    let source = "a\n\nb";
    let runs = parse_runs(source);
    assert_eq!(
        layout(&runs),
        vec![
            (0, 2, BlockKind::Paragraph, None),
            (2, 3, BlockKind::Paragraph, None),
            (3, 4, BlockKind::Paragraph, None),
        ]
    );
    assert_coverage(source, &runs);

    // 文本以 \n 结尾：不额外产生末尾空行 run。
    let trailing = parse_runs("a\n");
    assert_eq!(layout(&trailing), vec![(0, 2, BlockKind::Paragraph, None)]);
}

#[test]
fn multi_line_document_is_fully_covered_and_ordered() {
    let source = "# H1\n> quote\n- item\n\n```\ncode\n```\ntail";
    let runs = parse_runs(source);
    assert_eq!(
        runs.iter().map(BlockRun::kind).collect::<Vec<_>>(),
        vec![
            BlockKind::Heading,
            BlockKind::BlockQuote,
            BlockKind::ListItem,
            BlockKind::Paragraph,
            BlockKind::FencedCode,
            BlockKind::FencedCode,
            BlockKind::FencedCode,
            BlockKind::Paragraph,
        ]
    );
    assert_eq!(runs[0].level(), Some(1));
    assert_eq!(runs[4].level(), None);
    assert_coverage(source, &runs);

    // 每个 run 恰好对应一行（含行尾 \n，最后一行到文本尾）。
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    assert_eq!(runs.len(), lines.len());
    for (run, line) in runs.iter().zip(lines) {
        assert_eq!(
            &source[run.source().start().get()..run.source().end().get()],
            line
        );
    }
}

#[test]
fn no_headings_means_all_paragraphs() {
    let source = "plain text\n##not a heading\n####### seven\n";
    let runs = parse_runs(source);
    assert!(runs.iter().all(|run| run.kind() == BlockKind::Paragraph));
    assert!(runs.iter().all(|run| run.level().is_none()));
    assert_coverage(source, &runs);
}

#[test]
fn nested_prefix_priority_is_documented_behaviour() {
    let source = "> # h\n> - item\n> quote\n- # h2\n";
    let runs = parse_runs(source);
    assert_eq!(runs.len(), 4);
    // 标题优先于引用/列表。
    assert_eq!(runs[0].kind(), BlockKind::Heading);
    assert_eq!(runs[0].level(), Some(1));
    // 列表项优先于引用。
    assert_eq!(runs[1].kind(), BlockKind::ListItem);
    // 纯引用行。
    assert_eq!(runs[2].kind(), BlockKind::BlockQuote);
    // 列表里的标题仍取标题级别。
    assert_eq!(runs[3].kind(), BlockKind::Heading);
    assert_eq!(runs[3].level(), Some(1));
    assert_coverage(source, &runs);
}

#[test]
fn controller_rebuilds_block_runs_only_on_source_change() {
    let source = "# H\ntext";
    let mut controller =
        EditorController::open(support::snapshot(source)).expect("测试快照必须可打开");
    let initial = controller.block_runs().to_vec();
    assert_eq!(
        layout(&initial),
        vec![
            (0, 4, BlockKind::Heading, Some(1)),
            (4, 8, BlockKind::Paragraph, None),
        ]
    );

    // 只移动 caret：source 未变，块级语义缓存保持不变。
    controller
        .set_caret(UnicodeScalar::new(2))
        .expect("caret 合法");
    assert_eq!(controller.block_runs(), initial.as_slice());

    // 编辑追加一行标题：source 变 → block runs 重建。
    controller
        .set_caret(UnicodeScalar::new(8))
        .expect("末尾 caret 合法");
    let request = controller
        .compose_replace_selection("\n## New")
        .expect("输入必须成功")
        .expect("请求");
    assert_eq!(controller.source_text(), "# H\ntext\n## New");
    assert_eq!(
        controller
            .block_runs()
            .iter()
            .map(BlockRun::kind)
            .collect::<Vec<_>>(),
        vec![BlockKind::Heading, BlockKind::Paragraph, BlockKind::Heading,]
    );
    assert_eq!(controller.block_runs()[2].level(), Some(2));

    controller
        .apply_outcome(support::applied(
            &request.command_id,
            8,
            true,
            support::splice(8, 0, "\n## New"),
        ))
        .expect("本地回声确认");

    // undo（非本地回声）也应重建 block runs。
    controller
        .apply_outcome(support::applied(
            "undo-1",
            9,
            false,
            support::splice(8, 7, ""),
        ))
        .expect("undo 的 Applied 必须被接受");
    assert_eq!(controller.source_text(), source);
    assert_eq!(controller.block_runs(), initial.as_slice());
}

#[test]
fn empty_document_has_no_block_runs() {
    let runs = parse_runs("");
    assert!(runs.is_empty());
}
