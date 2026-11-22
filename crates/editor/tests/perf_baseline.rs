//! ADR 0019 S1：10 万字节混合文档的解析/投影基线（只记录，不把 CI 卡死）。

use std::time::Instant;

use menghunote_editor::{MarkdownDocument, ProjectionMap, Selection, UnicodeScalar};

fn mixed_document() -> String {
    let mut source = String::new();
    let mut index = 0usize;
    while source.len() < 100_000 {
        source.push_str(&format!(
            "# 标题 {index}\n\n- item **bold** [link](url) `code`\n  - nested\n\n> quote *em*\n\n```\ncode\n```\n\n"
        ));
        index += 1;
    }
    source
}

#[test]
fn hundred_kilobyte_parse_and_projection_baseline() {
    let source = mixed_document();
    let caret = UnicodeScalar::new(source.chars().count());

    let parse_started = Instant::now();
    let doc = MarkdownDocument::parse(&source).expect("解析不应失败");
    let parse_elapsed = parse_started.elapsed();

    let project_started = Instant::now();
    let selection = Selection::caret(caret);
    let projection = ProjectionMap::from_markdown_with_lists(&doc, selection);
    let style_runs = doc.style_runs(&projection);
    let project_elapsed = project_started.elapsed();

    assert!(!doc.constructs().is_empty());
    assert!(!doc.list_items().is_empty());
    assert!(!projection.segments().is_empty());
    assert!(!style_runs.is_empty());
    // 极端宽松上限：正常应 < 300ms；这里只防“退化成平方级卡死”。
    assert!(parse_elapsed.as_secs() < 5, "解析异常：{parse_elapsed:?}");
    assert!(
        project_elapsed.as_secs() < 5,
        "投影异常：{project_elapsed:?}"
    );
    println!(
        "[editor-perf] bytes={} parse_ms={} project_ms={} constructs={} items={} segments={} display_bytes={}",
        source.len(),
        parse_elapsed.as_millis(),
        project_elapsed.as_millis(),
        doc.constructs().len(),
        doc.list_items().len(),
        projection.segments().len(),
        projection.display_len().get()
    );
}
