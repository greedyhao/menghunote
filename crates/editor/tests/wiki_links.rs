//! Wiki 双链 L1：解析元数据 + 投影构造（隐藏括号/alias 的 target）。

use menghunote_editor::{ConstructKind, MarkdownDocument};

fn parse(text: &str) -> MarkdownDocument {
    MarkdownDocument::parse(text).expect("解析不应失败")
}

#[test]
fn wiki_construct_hides_brackets_and_keeps_alias_visible() {
    let source = "前 [[Note]] 后 [[Note|别名]] 尾";
    let doc = parse(source);
    let constructs: Vec<_> = doc
        .constructs()
        .iter()
        .filter(|construct| construct.kind() == ConstructKind::WikiLink)
        .collect();
    assert_eq!(constructs.len(), 2);

    let plain = constructs[0];
    assert_eq!(plain.markers().len(), 2, "无 alias 只隐藏两对括号");
    assert_eq!(
        source
            .get(plain.content().start().get()..plain.content().end().get())
            .expect("内容切片"),
        "Note"
    );

    let aliased = constructs[1];
    assert_eq!(aliased.markers().len(), 3, "alias 还要隐藏 target + `|`");
    assert_eq!(
        source
            .get(aliased.content().start().get()..aliased.content().end().get())
            .expect("内容切片"),
        "别名"
    );

    // 元数据保留 target/alias/heading，供 app 点击与后续重命名改写。
    let links = doc.wiki_links();
    assert_eq!(links.len(), 2);
    assert_eq!(links[0].target_text(source), "Note");
    assert_eq!(
        links[0].display().start().get(),
        links[0].target().start().get()
    );
    assert_eq!(links[1].target_text(source), "Note");
    assert_eq!(
        source
            .get(links[1].display().start().get()..links[1].display().end().get())
            .expect("display 切片"),
        "别名"
    );
}

#[test]
fn wiki_construct_survives_inline_fallback() {
    // 旧 scanner 对 `**a****b**` 会走 fallback；wiki 构造不能在 fallback 里消失。
    let source = "**a****b** [[Note]]";
    let doc = parse(source);
    assert_eq!(
        doc.constructs()
            .iter()
            .filter(|construct| construct.kind() == ConstructKind::WikiLink)
            .count(),
        1
    );
    assert_eq!(doc.wiki_links().len(), 1);
}

#[test]
fn ordinary_link_metadata_keeps_destination_and_content() {
    let source = "前 [链接](https://example.com/x) 后";
    let doc = parse(source);
    assert_eq!(doc.links().len(), 1);
    let link = &doc.links()[0];
    assert_eq!(link.dest(), "https://example.com/x");
    assert_eq!(
        source
            .get(link.content().start().get()..link.content().end().get())
            .expect("content 切片"),
        "链接"
    );
}
