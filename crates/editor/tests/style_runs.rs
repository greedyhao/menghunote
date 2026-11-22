//! S2-4a：语义样式区间（display 坐标）的契约测试。
//!
//! 覆盖：粗体/相邻/嵌套（粗体内代码、链接内斜体）/行内代码/链接/未闭合/
//! caret 进出构造导致 display 变化/全角 emoji 与组合字符边界/完整覆盖/确定性。

mod support;

use menghunote_editor::{
    EditorController, MarkdownDocument, ProjectionMap, Selection, StyleKind, StyleRun,
    UnicodeScalar,
};

fn parse(source: &str) -> MarkdownDocument {
    MarkdownDocument::parse(source).expect("扫描不应失败")
}

fn project(doc: &MarkdownDocument, caret: usize) -> ProjectionMap {
    // ADR 0019 S1：生产路径是列表感知投影（列表 marker → bullet/编号）；
    // 本文件覆盖语义样式，断言必须与 controller/app 同一条路径。
    ProjectionMap::from_markdown_with_lists(doc, Selection::caret(UnicodeScalar::new(caret)))
}

/// 返回 `(display_text, runs)`。
fn styled(source: &str, caret: usize) -> (String, Vec<StyleRun>) {
    let doc = parse(source);
    let projection = project(&doc, caret);
    let display = projection.display_text().into_owned();
    let runs = doc.style_runs(&projection);
    (display, runs)
}

fn layout(runs: &[StyleRun]) -> Vec<(usize, usize, StyleKind)> {
    runs.iter()
        .map(|run| (run.start().get(), run.end().get(), run.kind()))
        .collect()
}

fn range<'a>(display: &'a str, run: &StyleRun) -> &'a str {
    &display[run.start().get()..run.end().get()]
}

/// 不变量：有序、不重叠、非空、覆盖 `[0, len)`、边界在字符边界、同 kind 相邻已合并。
fn assert_coverage(display: &str, runs: &[StyleRun]) {
    if display.is_empty() {
        assert!(runs.is_empty(), "空 display 不产生 runs");
        return;
    }
    let mut cursor = 0usize;
    for (index, run) in runs.iter().enumerate() {
        assert_eq!(
            run.start().get(),
            cursor,
            "runs 必须连续覆盖，第 {index} 段起点 {cursor}"
        );
        assert!(run.end().get() > run.start().get(), "run 不能为空");
        assert!(
            display.is_char_boundary(run.start().get())
                && display.is_char_boundary(run.end().get()),
            "run 边界必须落在 display 的字符边界上：{run:?}"
        );
        cursor = run.end().get();
    }
    assert_eq!(cursor, display.len(), "runs 必须覆盖到 display 末尾");
    for window in runs.windows(2) {
        assert_ne!(
            window[0].kind(),
            window[1].kind(),
            "相邻且同 kind 的 run 必须合并"
        );
    }
}

#[test]
fn bold_style_covers_content_not_markers() {
    // caret 在构造外：`**` 隐藏，只有“粗体”带 Bold。
    let (display, runs) = styled("**粗体** 普通", 9);
    assert_eq!(display, "粗体 普通");
    assert_eq!(
        layout(&runs),
        vec![
            (0, 6, StyleKind::Bold),
            (6, display.len(), StyleKind::Plain),
        ]
    );
    assert_eq!(range(&display, &runs[0]), "粗体");
    assert_coverage(&display, &runs);

    // caret 进入构造：标记显现，但样式仍只覆盖内容（标记是 Plain）。
    let (display, runs) = styled("**粗体** 普通", 2);
    assert_eq!(display, "**粗体** 普通");
    assert_eq!(
        layout(&runs),
        vec![
            (0, 2, StyleKind::Plain),
            (2, 8, StyleKind::Bold),
            (8, display.len(), StyleKind::Plain),
        ]
    );
    assert_eq!(range(&display, &runs[1]), "粗体");
    assert_coverage(&display, &runs);
}

#[test]
fn adjacent_constructs_split_without_overlap() {
    let (display, runs) = styled("**a** *b* plain", 15);
    assert_eq!(display, "a b plain");
    assert_eq!(
        layout(&runs),
        vec![
            (0, 1, StyleKind::Bold),
            (1, 2, StyleKind::Plain),
            (2, 3, StyleKind::Emphasis),
            (3, display.len(), StyleKind::Plain),
        ]
    );
    assert_coverage(&display, &runs);
}

#[test]
fn nested_constructs_take_innermost_style() {
    // 粗体里的行内代码：内层代码优先，其余是 Bold。
    let (display, runs) = styled("**a `code` b** plain", 20);
    assert_eq!(display, "a code b plain");
    assert_eq!(
        layout(&runs),
        vec![
            (0, 2, StyleKind::Bold),
            (2, 6, StyleKind::InlineCode),
            (6, 8, StyleKind::Bold),
            (8, display.len(), StyleKind::Plain),
        ]
    );
    assert_eq!(range(&display, &runs[1]), "code");
    assert_coverage(&display, &runs);

    // 链接文本里的斜体：斜体（最内层）覆盖 b，其余文本是 Link。
    let (display, runs) = styled("[a *b*](u) plain", 16);
    assert_eq!(display, "a b plain");
    assert_eq!(
        layout(&runs),
        vec![
            (0, 2, StyleKind::Link),
            (2, 3, StyleKind::Emphasis),
            (3, display.len(), StyleKind::Plain),
        ]
    );
    assert_eq!(range(&display, &runs[1]), "b");
    assert_coverage(&display, &runs);
}

#[test]
fn inline_code_and_link_styles() {
    let (display, runs) = styled("x `a*b` y", 9);
    assert_eq!(display, "x a*b y");
    assert_eq!(
        layout(&runs),
        vec![
            (0, 2, StyleKind::Plain),
            (2, 5, StyleKind::InlineCode),
            (5, display.len(), StyleKind::Plain),
        ]
    );
    assert_eq!(range(&display, &runs[1]), "a*b");
    assert_coverage(&display, &runs);

    let (display, runs) = styled("x [text](url) y", 15);
    assert_eq!(display, "x text y");
    assert_eq!(
        layout(&runs),
        vec![
            (0, 2, StyleKind::Plain),
            (2, 6, StyleKind::Link),
            (6, display.len(), StyleKind::Plain),
        ]
    );
    assert_eq!(range(&display, &runs[1]), "text");
    assert_coverage(&display, &runs);
}

#[test]
fn unclosed_markers_produce_no_style() {
    let (display, runs) = styled("**a plain", 9);
    assert_eq!(display, "**a plain");
    assert_eq!(layout(&runs), vec![(0, display.len(), StyleKind::Plain)]);
    assert_coverage(&display, &runs);
}

#[test]
fn caret_toggle_shifts_runs_and_keeps_coverage() {
    let source = "a **bold** b";

    let (outside, runs) = styled(source, 0); // caret 在构造外
    assert_eq!(outside, "a bold b");
    assert_eq!(
        layout(&runs),
        vec![
            (0, 2, StyleKind::Plain),
            (2, 6, StyleKind::Bold),
            (6, outside.len(), StyleKind::Plain),
        ]
    );
    assert_coverage(&outside, &runs);

    let (inside, runs) = styled(source, 5); // caret 在粗体内容里
    assert_eq!(inside, "a **bold** b");
    assert_eq!(
        layout(&runs),
        vec![
            (0, 4, StyleKind::Plain),
            (4, 8, StyleKind::Bold),
            (8, inside.len(), StyleKind::Plain),
        ]
    );
    // 显现后的 `**` 各自是 Plain，绝不算进 Bold。
    assert_eq!(&inside[0..4], "a **");
    assert_eq!(&inside[8..12], "** b");
    assert_coverage(&inside, &runs);

    // 再移出去：回到隐藏态，run 边界随 projection 一起回退。
    let (again, runs) = styled(source, 1);
    assert_eq!(again, outside);
    assert_eq!(layout(&runs), layout(&styled(source, 0).1));
    assert_coverage(&again, &runs);
}

#[test]
fn unicode_boundaries_are_never_split() {
    // 全角 + emoji：Bold 只覆盖“粗体”的 6 个 UTF-8 bytes，emoji/中文完整。
    let (display, runs) = styled("😀**粗体**中", 8);
    assert_eq!(display, "😀粗体中");
    assert_eq!(
        layout(&runs),
        vec![
            (0, 4, StyleKind::Plain),
            (4, 10, StyleKind::Bold),
            (10, display.len(), StyleKind::Plain),
        ]
    );
    assert_eq!(range(&display, &runs[1]), "粗体");
    assert_coverage(&display, &runs);

    // 组合字符（e + U+0301）作为一个整体不被切开。
    let (display, runs) = styled("e\u{301}**x** y", 8);
    assert_eq!(display, "e\u{301}x y");
    assert_eq!(
        layout(&runs),
        vec![
            (0, 3, StyleKind::Plain),
            (3, 4, StyleKind::Bold),
            (4, display.len(), StyleKind::Plain),
        ]
    );
    assert_eq!(range(&display, &runs[1]), "x");
    assert_coverage(&display, &runs);
}

#[test]
fn block_nesting_keeps_style_on_inline_content() {
    // 标题里的粗体：块级标记 `# ` 隐藏，Bold 只覆盖“标题”。
    let (display, runs) = styled("# **标题**\nplain", 9);
    assert_eq!(display, "标题\nplain");
    assert_eq!(
        layout(&runs),
        vec![
            (0, 6, StyleKind::Bold),
            (6, display.len(), StyleKind::Plain),
        ]
    );
    assert_eq!(range(&display, &runs[0]), "标题");
    assert_coverage(&display, &runs);
}

#[test]
fn empty_and_plain_documents_are_fully_covered() {
    let (display, runs) = styled("", 0);
    assert_eq!(display, "");
    assert!(runs.is_empty());

    let (display, runs) = styled("hello 中文，。", 0);
    assert_eq!(layout(&runs), vec![(0, display.len(), StyleKind::Plain)]);
    assert!(runs[0].kind().is_plain());
    assert_coverage(&display, &runs);
}

#[test]
fn complex_document_is_covered_and_deterministic() {
    let source = "# **标题**\n> *引用*\n- [链接](u) `代码`\n普通";
    let doc = parse(source);
    let caret = source.chars().count();
    let projection = project(&doc, caret);
    let display = projection.display_text().into_owned();
    let runs = doc.style_runs(&projection);
    assert_coverage(&display, &runs);

    // 关键片段：标记不可见、内容带语义。
    let rendered: Vec<(&str, StyleKind)> = runs
        .iter()
        .map(|run| (range(&display, run), run.kind()))
        .collect();
    assert!(rendered.contains(&("标题", StyleKind::Bold)));
    assert!(rendered.contains(&("引用", StyleKind::Emphasis)));
    assert!(rendered.contains(&("链接", StyleKind::Link)));
    assert!(rendered.contains(&("代码", StyleKind::InlineCode)));

    // 确定性 + 控制器缓存的 runs 与独立计算结果一致。
    assert_eq!(layout(&runs), layout(&doc.style_runs(&projection)));

    let mut controller =
        EditorController::open(support::snapshot(source)).expect("测试快照必须可打开");
    controller
        .set_caret(UnicodeScalar::new(caret))
        .expect("末尾 caret 合法");
    assert_eq!(layout(controller.style_runs()), layout(&runs));
}

#[test]
fn list_items_render_inline_markdown() {
    let source = "> - `javascript:`: 拒绝，提示只显示 scheme。\n> - **粗** 和 [链接](https://e.com) 还有 *斜*\n";
    let doc = parse(source);
    let caret = source.chars().count();
    let projection = project(&doc, caret);
    let display = projection.display_text().into_owned();
    assert!(
        display.contains("javascript:: 拒绝"),
        "列表行内代码标记必须隐藏：{display:?}"
    );
    assert!(
        !display.contains('`'),
        "列表行内代码不得残留反引号：{display:?}"
    );
    assert!(
        display.contains("粗 和 链接 还有 斜"),
        "列表行内标记必须隐藏：{display:?}"
    );
    let runs = doc.style_runs(&projection);
    let rendered: Vec<(&str, StyleKind)> = runs
        .iter()
        .map(|run| (range(&display, run), run.kind()))
        .collect();
    assert!(
        rendered.contains(&("javascript:", StyleKind::InlineCode)),
        "{rendered:?}"
    );
    assert!(rendered.contains(&("粗", StyleKind::Bold)), "{rendered:?}");
    assert!(
        rendered.contains(&("链接", StyleKind::Link)),
        "{rendered:?}"
    );
    assert!(
        rendered.contains(&("斜", StyleKind::Emphasis)),
        "{rendered:?}"
    );
    assert_coverage(&display, &runs);
}
