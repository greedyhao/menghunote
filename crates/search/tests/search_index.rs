//! S4-1b 索引核心的端到端测试。
//!
//! 临时目录用 `CARGO_TARGET_TMPDIR`（AGENTS §6：cargo 子进程不能写 `%TEMP%`）。

use std::fs;
use std::path::PathBuf;

use menghunote_search::{
    markdown_to_plain, DocVersion, Fingerprint, SearchDoc, SearchError, SearchIndex,
    FINGERPRINT_FILE,
};

fn temp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("search-core-{name}"));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn version(seq: u64) -> DocVersion {
    DocVersion::new("body-doc", seq, Some(b"frontier"), seq, 0x1234)
}

fn search_doc(note_id: &str, title: &str, body: &str, seq: u64) -> SearchDoc {
    SearchDoc {
        note_id: note_id.to_owned(),
        title: title.to_owned(),
        body_text: body.to_owned(),
        version: version(seq),
    }
}

fn open(dir: &PathBuf) -> SearchIndex {
    SearchIndex::create(dir, &Fingerprint::current()).expect("创建索引失败")
}

#[test]
fn index_query_delete() {
    let dir = temp_dir("lifecycle");
    let mut index = open(&dir);

    index
        .upsert(search_doc("n1", "笔记流程", "这是关于笔记流程的说明", 1))
        .expect("upsert n1");
    index
        .upsert(search_doc(
            "n2",
            "GPUI 编辑器",
            "menghunote 使用 GPUI 构建桌面编辑器",
            2,
        ))
        .expect("upsert n2");
    index
        .upsert(search_doc("n3", "Rust 笔记", "笔记与流程并不相邻", 3))
        .expect("upsert n3");
    index.commit().expect("commit");

    let hits = index.query("笔记", 10).expect("query 笔记");
    assert_eq!(hits.len(), 2, "笔记应命中 n1/n3: {hits:?}");
    let ids: Vec<_> = hits.iter().map(|hit| hit.note_id.as_str()).collect();
    assert!(ids.contains(&"n1"));
    assert!(ids.contains(&"n3"));

    let hits = index.query("流程", 10).expect("query 流程");
    assert_eq!(hits.len(), 2, "流程应命中 n1/n3: {hits:?}");

    // analyzer 的 LowerCaser 让 `gpui` 命中正文里的 `GPUI`。
    let hits = index.query("gpui", 10).expect("query gpui");
    assert_eq!(hits.len(), 1, "gpui 应命中 n2: {hits:?}");
    assert_eq!(hits[0].note_id, "n2");

    // 引号 = 短语：n1 的「笔记流程」相邻；n3 的「笔记…流程」不相邻，不应命中。
    let hits = index.query("\"笔记流程\"", 10).expect("query 短语");
    assert_eq!(hits.len(), 1, "短语只应命中 n1: {hits:?}");
    assert_eq!(hits[0].note_id, "n1");

    // 空格 = AND：n1/n3 都含两个词。
    let hits = index.query("笔记 流程", 10).expect("query AND");
    assert_eq!(hits.len(), 2, "AND 应命中 n1/n3: {hits:?}");

    // 无空格中文串 = token 的 AND（显式），不是隐式短语；因此也会命中 n3。
    let hits = index.query("笔记流程", 10).expect("query 无空格 AND");
    assert_eq!(hits.len(), 2, "无空格串应按 AND 命中 n1/n3: {hits:?}");

    // snippet 区间必须落在 UTF-8 边界且指向 snippet_text 中的查询词。
    let hit = index
        .query("笔记", 10)
        .expect("query 笔记")
        .into_iter()
        .find(|hit| hit.note_id == "n1")
        .expect("缺 n1");
    assert!(!hit.snippet_text.is_empty());
    assert!(
        !hit.snippet_ranges.is_empty(),
        "snippet 应有高亮区间: {hit:?}"
    );
    for range in &hit.snippet_ranges {
        assert!(range.start < range.end);
        assert!(range.end <= hit.snippet_text.len());
        assert!(hit.snippet_text.is_char_boundary(range.start));
        assert!(hit.snippet_text.is_char_boundary(range.end));
    }
    assert!(
        hit.snippet_ranges
            .iter()
            .any(|range| &hit.snippet_text[range.clone()] == "笔记"),
        "高亮区间应对应「笔记」: {hit:?}"
    );

    // 删除后不再返回（墓碑不参与查询）。
    index.delete("n1").expect("delete n1");
    index.commit().expect("commit delete");
    let hits = index.query("笔记", 10).expect("query 笔记 after delete");
    assert_eq!(hits.len(), 1, "删除后只应剩 n3: {hits:?}");
    assert_eq!(hits[0].note_id, "n3");
    assert!(index
        .query("\"笔记流程\"", 10)
        .expect("query 短语 after delete")
        .is_empty());
}

#[test]
fn same_note_id_upsert_replaces_previous_document() {
    let dir = temp_dir("replace");
    let mut index = open(&dir);

    index
        .upsert(search_doc("same", "旧标题", "旧内容 alpha", 1))
        .expect("upsert old");
    index.commit().expect("commit old");
    assert_eq!(index.query("alpha", 10).expect("query alpha").len(), 1);

    index
        .upsert(search_doc("same", "新标题", "新内容 beta", 2))
        .expect("upsert new");
    index.commit().expect("commit new");

    assert!(
        index.query("alpha", 10).expect("query alpha").is_empty(),
        "旧内容必须被同名替换清掉"
    );
    let hits = index.query("beta", 10).expect("query beta");
    assert_eq!(hits.len(), 1, "新内容应只有一条: {hits:?}");
    assert_eq!(hits[0].title, "新标题");
    // 旧标题也不再命中。
    assert!(index
        .query("旧标题", 10)
        .expect("query old title")
        .is_empty());
}

#[test]
fn reopen_preserves_documents_and_rejects_fingerprint_mismatch() {
    let dir = temp_dir("reopen");
    {
        let mut index = open(&dir);
        index
            .upsert(search_doc("n1", "持久化", "索引重开后仍在", 1))
            .expect("upsert");
        index.commit().expect("commit");
    }

    let index = SearchIndex::open(&dir).expect("重新打开索引失败");
    assert_eq!(index.query("重开", 10).expect("query").len(), 1);

    // 篡改 fingerprint：open 必须拒绝（不能悄悄用错版本的索引）。
    fs::write(
        dir.join(FINGERPRINT_FILE),
        "schema_version=999\ntokenizer=cangjie_for_search\nanalyzer_version=1\nmarkdown_version=1\ndictionary_hash=0000000000000000\n",
    )
    .expect("写 fingerprint");
    match SearchIndex::open(&dir) {
        Err(SearchError::Fingerprint(_)) => {}
        Err(other) => panic!("期望 Fingerprint 错误，实际 {other:?}"),
        Ok(_) => panic!("期望 Fingerprint 错误，实际成功打开了索引"),
    }
}

#[test]
fn missing_fingerprint_is_rejected() {
    let dir = temp_dir("missing-fingerprint");
    fs::create_dir_all(&dir).expect("建目录");
    match SearchIndex::open(&dir) {
        Err(SearchError::MissingFingerprint) => {}
        Err(other) => panic!("期望 MissingFingerprint，实际 {other:?}"),
        Ok(_) => panic!("期望 MissingFingerprint，实际成功打开了索引"),
    }
}

#[test]
fn empty_input_and_empty_document_do_not_panic() {
    let dir = temp_dir("empty");
    let mut index = open(&dir);

    assert!(index.query("", 10).expect("空 query").is_empty());
    assert!(index.query("   ", 10).expect("空白 query").is_empty());
    assert!(index.query("anything", 0).expect("limit=0").is_empty());

    index
        .upsert(search_doc("empty", "", "", 1))
        .expect("空文档");
    index.commit().expect("commit empty");
    assert!(index.query("", 10).expect("空 query").is_empty());
    assert!(index.query("不存在", 10).expect("不存在").is_empty());
}

#[test]
fn snippet_ranges_are_utf8_boundaries_with_emoji() {
    let dir = temp_dir("utf8");
    let mut index = open(&dir);
    index
        .upsert(search_doc("emoji", "表情", "前😀笔记流程后", 1))
        .expect("upsert");
    index.commit().expect("commit");

    let hit = index
        .query("笔记", 10)
        .expect("query")
        .into_iter()
        .next()
        .expect("应有命中");
    assert!(!hit.snippet_text.is_empty());
    for range in &hit.snippet_ranges {
        assert!(hit.snippet_text.is_char_boundary(range.start));
        assert!(hit.snippet_text.is_char_boundary(range.end));
        assert!(range.end <= hit.snippet_text.len());
    }
}

#[test]
fn markdown_to_plain_is_used_before_indexing() {
    let plain =
        markdown_to_plain("# 笔记流程\n\n**重点** 与 `代码`\n\n[链接](https://example.com)");
    assert_eq!(plain, "笔记流程\n\n重点 与 代码\n\n链接");

    let dir = temp_dir("markdown");
    let mut index = open(&dir);
    index
        .upsert(search_doc("md", "标题", &plain, 1))
        .expect("upsert");
    index.commit().expect("commit");
    // 标记被去掉后，保留的可见文字仍可检索。
    assert_eq!(index.query("重点", 10).expect("query").len(), 1);
    assert_eq!(index.query("链接", 10).expect("query").len(), 1);
    // 标记本身不应成为检索词。
    assert!(index.query("**", 10).expect("query markers").is_empty());
}
