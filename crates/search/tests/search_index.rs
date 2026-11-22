//! S4-1b 索引核心的端到端测试。
//!
//! 临时目录用 `CARGO_TARGET_TMPDIR`（AGENTS §6：cargo 子进程不能写 `%TEMP%`）。

use std::fs;
use std::path::PathBuf;

use menghunote_search::{
    markdown_to_plain, DocVersion, Fingerprint, QueryOptions, QueryResult, SearchDoc, SearchError,
    SearchIndex, FINGERPRINT_FILE,
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
    assert!(!hit.snippets[0].text.is_empty());
    assert!(
        !hit.snippets[0].ranges.is_empty(),
        "snippet 应有高亮区间: {hit:?}"
    );
    for range in &hit.snippets[0].ranges {
        assert!(range.start < range.end);
        assert!(range.end <= hit.snippets[0].text.len());
        assert!(hit.snippets[0].text.is_char_boundary(range.start));
        assert!(hit.snippets[0].text.is_char_boundary(range.end));
    }
    assert!(
        hit.snippets[0]
            .ranges
            .iter()
            .any(|range| &hit.snippets[0].text[range.clone()] == "笔记"),
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
    assert!(!hit.snippets[0].text.is_empty());
    for range in &hit.snippets[0].ranges {
        assert!(hit.snippets[0].text.is_char_boundary(range.start));
        assert!(hit.snippets[0].text.is_char_boundary(range.end));
        assert!(range.end <= hit.snippets[0].text.len());
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

#[test]
fn all_hits_get_snippets() {
    let dir = temp_dir("snippet-all");
    let mut index = open(&dir);
    for i in 0..7 {
        index
            .upsert(search_doc(
                &format!("n{i}"),
                &format!("文档 {i}"),
                "共同词 用于验证所有结果都有摘要",
                i as u64 + 1,
            ))
            .expect("upsert");
    }
    index.commit().expect("commit");

    let hits = index.query("共同词", 10).expect("query");
    assert_eq!(hits.len(), 7);
    for (position, hit) in hits.iter().enumerate() {
        assert!(
            !hit.snippets[0].text.is_empty(),
            "第 {position} 条也必须有摘要: {hit:?}"
        );
        assert!(!hit.snippets[0].ranges.is_empty());
        for range in &hit.snippets[0].ranges {
            assert!(hit.snippets[0].text.is_char_boundary(range.start));
            assert!(hit.snippets[0].text.is_char_boundary(range.end));
        }
    }
}

#[test]
fn tail_match_in_one_megabyte_body_is_snippeted() {
    let dir = temp_dir("snippet-tail");
    let mut index = open(&dir);
    let mut body = "a".repeat(1_000_000);
    body.push_str("末尾笔记");
    index
        .upsert(search_doc("long", "长文", &body, 1))
        .expect("upsert");
    index.commit().expect("commit");

    let hits = index.query("笔记", 10).expect("query");
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].snippets[0].text.contains("笔记"),
        "1MB 末尾命中必须有摘要: {hits:?}"
    );
    assert!(!hits[0].snippets[0].ranges.is_empty());
    for range in &hits[0].snippets[0].ranges {
        assert!(hits[0].snippets[0].text.is_char_boundary(range.start));
        assert!(hits[0].snippets[0].text.is_char_boundary(range.end));
        assert!(range.end <= hits[0].snippets[0].text.len());
    }
}

#[test]
fn empty_body_and_title_only_match_do_not_panic() {
    let dir = temp_dir("snippet-empty");
    let mut index = open(&dir);
    index
        .upsert(search_doc("empty", "空正文标题", "", 1))
        .expect("upsert");
    index.commit().expect("commit");

    assert!(index.query("不存在", 10).expect("query").is_empty());
    let hits = index.query("空正文标题", 10).expect("query title");
    assert_eq!(hits.len(), 1);
    assert!(!hits[0].snippets[0].text.is_empty(), "标题命中应有标题摘要");
}

#[test]
fn total_is_reported_before_limit() {
    let dir = temp_dir("total");
    let mut index = open(&dir);
    for i in 0..3 {
        index
            .upsert(search_doc(
                &format!("n{i}"),
                &format!("总数 {i}"),
                "共同词 三篇都命中",
                i + 1,
            ))
            .expect("upsert");
    }
    index.commit().expect("commit");

    let result: QueryResult = index
        .query_with_options("共同词", 2, QueryOptions::default())
        .expect("query");
    assert_eq!(result.total, 3);
    assert_eq!(result.hits.len(), 2);
}

#[test]
fn case_sensitive_toggle_uses_cs_fields() {
    let dir = temp_dir("case");
    let mut index = open(&dir);
    index
        .upsert(search_doc("upper", "大写", "GPUI", 1))
        .expect("upsert upper");
    index
        .upsert(search_doc("lower", "小写", "gpui", 2))
        .expect("upsert lower");
    index.commit().expect("commit");

    let cs = QueryOptions {
        case_sensitive: true,
        whole_word: true,
    };
    let hits = index
        .query_with_options("GPUI", 10, cs)
        .expect("query cs")
        .hits;
    assert_eq!(hits.len(), 1, "cs=true 只应命中原样 GPUI: {hits:?}");
    assert_eq!(hits[0].note_id, "upper");

    let insensitive = QueryOptions::default();
    let hits = index
        .query_with_options("gpui", 10, insensitive)
        .expect("query ci")
        .hits;
    assert_eq!(hits.len(), 2, "cs=false 应命中两篇: {hits:?}");
}

#[test]
fn whole_word_false_matches_substrings() {
    let dir = temp_dir("substring");
    let mut index = open(&dir);
    for (id, body) in [
        ("exact", "独立 addr 词"),
        ("address", "address book"),
        ("thaddr", "prefix thaddr suffix"),
    ] {
        index
            .upsert(search_doc(id, "标题", body, 1))
            .expect("upsert");
    }
    index.commit().expect("commit");

    let substring = QueryOptions {
        case_sensitive: false,
        whole_word: false,
    };
    let hits = index
        .query_with_options("addr", 10, substring)
        .expect("substring")
        .hits;
    assert_eq!(hits.len(), 3, "子串应命中 address/thaddr: {hits:?}");

    let whole = QueryOptions {
        case_sensitive: false,
        whole_word: true,
    };
    let hits = index
        .query_with_options("addr", 10, whole)
        .expect("whole word")
        .hits;
    assert_eq!(hits.len(), 1, "整词只应命中独立 addr: {hits:?}");
    assert_eq!(hits[0].note_id, "exact");
}

#[test]
fn chinese_substring_matches_without_analyzer_token() {
    let dir = temp_dir("chinese-substring");
    let mut index = open(&dir);
    index
        .upsert(search_doc("n", "标题", "笔记流程", 1))
        .expect("upsert");
    index.commit().expect("commit");

    let substring = QueryOptions {
        case_sensitive: false,
        whole_word: false,
    };
    let hits = index
        .query_with_options("记流", 10, substring)
        .expect("substring")
        .hits;
    assert_eq!(hits.len(), 1, "中文子串应命中: {hits:?}");

    let whole = QueryOptions {
        case_sensitive: false,
        whole_word: true,
    };
    assert!(
        index
            .query_with_options("记流", 10, whole)
            .expect("whole")
            .hits
            .is_empty(),
        "非 analyzer token 的串在整词模式下不应命中"
    );
}

#[test]
fn case_sensitive_substring_is_supported() {
    let dir = temp_dir("case-substring");
    let mut index = open(&dir);
    index
        .upsert(search_doc("upper", "标题", "GPUI address", 1))
        .expect("upsert upper");
    index
        .upsert(search_doc("lower", "标题", "gpui address", 2))
        .expect("upsert lower");
    index.commit().expect("commit");

    let options = QueryOptions {
        case_sensitive: true,
        whole_word: false,
    };
    let hits = index
        .query_with_options("GPUI", 10, options)
        .expect("query")
        .hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].note_id, "upper");
}

#[test]
fn snippet_is_single_line_and_long_line_is_trimmed() {
    let dir = temp_dir("line-snippet");
    let mut index = open(&dir);
    index
        .upsert(search_doc("multi", "标题", "第一行\n唯一命中\n第三行", 1))
        .expect("upsert multi");
    index.commit().expect("commit");

    let hit = index
        .query("唯一", 10)
        .expect("query")
        .into_iter()
        .next()
        .expect("hit");
    assert_eq!(hit.snippets[0].text, "唯一命中");
    assert!(!hit.snippets[0].text.contains('\n'));
    assert!(!hit.snippets[0].ranges.is_empty());

    let dir = temp_dir("line-snippet-long");
    let mut index = open(&dir);
    let long = format!("{}命中{}", "a".repeat(300), "b".repeat(300));
    index
        .upsert(search_doc("long", "标题", &long, 1))
        .expect("upsert long");
    index.commit().expect("commit");
    let hit = index
        .query("命中", 10)
        .expect("query")
        .into_iter()
        .next()
        .expect("hit");
    assert!(hit.snippets[0].text.contains('…'));
    assert!(!hit.snippets[0].text.contains('\n'));
    for range in &hit.snippets[0].ranges {
        assert!(hit.snippets[0].text.is_char_boundary(range.start));
        assert!(hit.snippets[0].text.is_char_boundary(range.end));
    }
}

#[test]
fn hit_groups_all_lines_and_reports_matched_lines() {
    let dir = temp_dir("multi-line");
    let mut index = open(&dir);
    let body = (0..5)
        .map(|index| format!("第 {index} 行命中"))
        .collect::<Vec<_>>()
        .join("\n");
    index
        .upsert(search_doc("multi", "标题", &body, 1))
        .expect("upsert");
    index.commit().expect("commit");

    let hit = index
        .query("命中", 10)
        .expect("query")
        .into_iter()
        .next()
        .expect("hit");
    assert_eq!(hit.matched_lines, 5);
    assert_eq!(hit.snippets.len(), 5);
    assert_eq!(
        hit.snippets
            .iter()
            .map(|snippet| snippet.line_index)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4]
    );
    for snippet in &hit.snippets {
        assert!(!snippet.text.contains('\n'));
        assert!(!snippet.ranges.is_empty());
    }
}

#[test]
fn one_hundred_hit_lines_are_all_returned() {
    let dir = temp_dir("hundred-lines");
    let mut index = open(&dir);
    let body = (0..100)
        .map(|index| format!("第 {index} 行命中"))
        .collect::<Vec<_>>()
        .join("\n");
    index
        .upsert(search_doc("hundred", "标题", &body, 1))
        .expect("upsert");
    index.commit().expect("commit");

    let hit = index
        .query("命中", 10)
        .expect("query")
        .into_iter()
        .next()
        .expect("hit");
    assert_eq!(hit.matched_lines, 100);
    assert_eq!(hit.snippets.len(), 100);
    assert_eq!(hit.snippets[0].line_index, 0);
    assert_eq!(hit.snippets[99].line_index, 99);
}

#[test]
fn multiple_hits_on_one_long_line_are_one_snippet_with_all_ranges() {
    let dir = temp_dir("multi-hit-line");
    let mut index = open(&dir);
    let mut body = "a".repeat(200);
    body.push_str("命中-命中-命中");
    body.push_str(&"b".repeat(200));
    index
        .upsert(search_doc("long", "标题", &body, 1))
        .expect("upsert");
    index.commit().expect("commit");

    let hit = index
        .query("命中", 10)
        .expect("query")
        .into_iter()
        .next()
        .expect("hit");
    assert_eq!(hit.matched_lines, 1);
    assert_eq!(hit.snippets.len(), 1);
    assert_eq!(hit.snippets[0].ranges.len(), 3);
}

#[test]
fn empty_body_and_title_only_hit_has_title_snippet() {
    let dir = temp_dir("empty-body-group");
    let mut index = open(&dir);
    index
        .upsert(search_doc("empty", "标题命中", "", 1))
        .expect("upsert");
    index.commit().expect("commit");

    let hit = index
        .query("标题命中", 10)
        .expect("query")
        .into_iter()
        .next()
        .expect("hit");
    assert_eq!(hit.matched_lines, 1);
    assert_eq!(hit.snippets.len(), 1);
    assert!(!hit.snippets[0].text.is_empty());
}
