//! Wiki links L2：重命名联动改写（纯函数计划 + 逐篇 SessionDriver 执行）。

mod support;

use std::collections::BTreeSet;
use std::time::Duration;

use support::*;

use menghunote_storage::rename_links::{
    link_sources_for_note, plan_wiki_link_renames, rewrite_link_sources, scalar_edits_for_renames,
    RenameLinkOptions, WikiLinkRename,
};
use menghunote_storage::{materialize_body, BodyDoc, Value, Vault};

fn targets(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// 按计划把 target 片段替换成 new_target（测试里的“应用器”；只用于断言）。
fn apply_renames(source: &str, renames: &[WikiLinkRename]) -> String {
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for rename in renames {
        out.push_str(&source[cursor..rename.start]);
        out.push_str(&rename.new_target);
        cursor = rename.end;
    }
    out.push_str(&source[cursor..]);
    out
}

fn seed_tree_note(
    vault: &Vault,
    node_id: &str,
    note_id: &str,
    doc_id: &str,
    title: &str,
) -> BodyDoc {
    let doc = seed_note(vault, note_id, doc_id);
    vault
        .exec(
            "INSERT INTO tree(node_id, parent_id, note_id, title, sibling_rank, deleted, meta_seq) \
             VALUES(?1, NULL, ?2, ?3, 1, 0, 1)",
            &[
                Value::Text(node_id.to_owned()),
                Value::Text(note_id.to_owned()),
                Value::Text(title.to_owned()),
            ],
        )
        .expect("插入 tree 行");
    doc
}

fn commit_and_materialize(vault: &Vault, note_id: &str, doc: &BodyDoc) {
    let head = commit_now(vault, doc, &format!("{note_id}-seed"));
    materialize_body(vault, note_id, doc.doc_id(), head.head_seq, PEER_A).expect("物化正文");
}

fn body_md(vault: &Vault, note_id: &str) -> String {
    let rows = vault
        .query(
            "SELECT body_md FROM notes WHERE note_id = ?1",
            &[Value::Text(note_id.to_owned())],
        )
        .expect("查 body_md");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Text(text)) => text.clone(),
        other => panic!("body_md 读回异常：{other:?}"),
    }
}

fn link_rows(vault: &Vault, from_note_id: &str) -> Vec<(String, Option<String>)> {
    let rows = vault
        .query(
            "SELECT target, to_note_id FROM links WHERE from_note_id = ?1 ORDER BY start_byte",
            &[Value::Text(from_note_id.to_owned())],
        )
        .expect("查 links");
    rows.into_iter()
        .map(|row| {
            let target = match &row[0] {
                Value::Text(text) => text.clone(),
                other => panic!("target 读回异常：{other:?}"),
            };
            let to_note_id = match &row[1] {
                Value::Text(text) => Some(text.clone()),
                Value::Null => None,
                other => panic!("to_note_id 读回异常：{other:?}"),
            };
            (target, to_note_id)
        })
        .collect()
}

#[test]
fn plan_rewrites_all_supported_forms() {
    let source = "前 [[旧名]] 中 [[旧名|别名]] 后 [[旧名#标题]] 再 [[文件夹/旧名]] 末";
    let linked = targets(&["旧名", "文件夹/旧名"]);
    let renames = plan_wiki_link_renames(source, "旧名", "新名", &linked);

    assert_eq!(renames.len(), 4, "{renames:?}");
    assert!(
        renames.windows(2).all(|pair| pair[0].start < pair[1].start),
        "计划必须按 start 升序：{renames:?}"
    );
    for rename in &renames {
        assert_eq!(
            source.get(rename.start..rename.end),
            Some(rename.old_target.as_str()),
            "替换区间必须恰好是 target 片段"
        );
    }
    assert_eq!(
        apply_renames(source, &renames),
        "前 [[新名]] 中 [[新名|别名]] 后 [[新名#标题]] 再 [[文件夹/新名]] 末",
        "只替换 target，`[[`/`]]`/alias/heading 必须逐字保留"
    );
}

#[test]
fn plan_preserves_path_md_whitespace_and_offsets() {
    // 混合：前后空白、`.md`、嵌套路径、末尾 `/`、多字节字符、同名但不同路径。
    let source = "甲 [[ 旧名 ]] 乙 [[a/b/旧名.md]] 丙 [[旧名/]] 丁 [[其它/旧名x]] 戊";
    let linked = targets(&["旧名", "a/b/旧名.md", "旧名/"]);
    let renames = plan_wiki_link_renames(source, "旧名", "新标题", &linked);

    assert_eq!(
        renames.len(),
        3,
        "[[旧名/]] 与 [[其它/旧名x]] 不匹配 basename：{renames:?}"
    );
    assert_eq!(
        apply_renames(source, &renames),
        "甲 [[ 新标题 ]] 乙 [[a/b/新标题.md]] 丙 [[新标题/]] 丁 [[其它/旧名x]] 戊",
        "路径前缀、末尾 `/`、`.md`、target 两侧空白都要保留"
    );

    // 多字节前的目标偏移必须按字节、且落在 char 边界。
    let unicode_source = "😀[[旧名]]尾";
    let renames = plan_wiki_link_renames(unicode_source, "旧名", "新名", &targets(&["旧名"]));
    assert_eq!(renames.len(), 1);
    assert_eq!(
        unicode_source.get(renames[0].start..renames[0].end),
        Some("旧名")
    );
    assert_eq!(apply_renames(unicode_source, &renames), "😀[[新名]]尾");
}

#[test]
fn plan_skips_code_inline_escaped_and_embed() {
    let source = "\
```md
[[旧名]]
```
正文 `[[旧名]]` 与 \\[[旧名]] 与 ![[旧名]]，最后 [[旧名]]
";
    let renames = plan_wiki_link_renames(source, "旧名", "新名", &targets(&["旧名"]));
    assert_eq!(renames.len(), 1, "只改代码/转义之外的真实链接：{renames:?}");
    let rewritten = apply_renames(source, &renames);
    assert!(
        rewritten.ends_with("最后 [[新名]]\n"),
        "真实链接必须改写：{rewritten:?}"
    );
    assert!(rewritten.contains("```md\n[[旧名]]\n```"));
    assert!(rewritten.contains("`[[旧名]]`"));
    assert!(rewritten.contains("\\[[旧名]]"));
    assert!(rewritten.contains("![[旧名]]"));
}

#[test]
fn plan_skips_unresolved_and_ambiguous_targets() {
    let source = "[[A/旧名]] [[B/旧名]] [[旧名]]";
    // 只有 A/旧名 由 links 证明解析到被重命名笔记；B/旧名 同名歧义（to_note_id=NULL），
    // [[旧名]] 未解析。两者都不能被连带改写。
    let renames = plan_wiki_link_renames(source, "旧名", "新名", &targets(&["A/旧名"]));
    assert_eq!(renames.len(), 1);
    assert_eq!(
        apply_renames(source, &renames),
        "[[A/新名]] [[B/旧名]] [[旧名]]"
    );

    // 没有任何已解析链接：完全不改（未解析默认不动）。
    assert!(plan_wiki_link_renames(source, "旧名", "新名", &targets(&[])).is_empty());
    // 标题没变同样不产生计划。
    assert!(plan_wiki_link_renames(source, "旧名", "旧名", &targets(&["旧名"])).is_empty());
}

#[test]
fn plan_is_sorted_deduped_and_idempotent() {
    let source = "[[旧名|a]]\n[[旧名#h]]\n[[文件夹/旧名]]\n";
    let linked = targets(&["旧名", "文件夹/旧名"]);
    let first = plan_wiki_link_renames(source, "旧名", "新名", &linked);
    assert_eq!(first.len(), 3);
    assert!(first.windows(2).all(|pair| pair[0].end <= pair[1].start));
    assert_eq!(
        apply_renames(source, &first),
        "[[新名|a]]\n[[新名#h]]\n[[文件夹/新名]]\n"
    );

    // 重复执行：已改过的链接 basename 不再是旧名 → 计划为空（幂等）。
    let after = apply_renames(source, &first);
    assert!(
        plan_wiki_link_renames(&after, "旧名", "新名", &linked).is_empty(),
        "重复执行不得再产生改写"
    );
}

#[test]
fn scalar_edits_use_scalar_coordinates_and_reject_stale_ranges() {
    let source = "😀[[旧名]]";
    let renames = plan_wiki_link_renames(source, "旧名", "新名", &targets(&["旧名"]));
    let edits = scalar_edits_for_renames(source, &renames).expect("换算 scalar");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].start.get(), 3, "😀 占 1 个 scalar + `[[` 2 个");
    assert_eq!(edits[0].end.get(), 5);
    assert_eq!(edits[0].replacement, "新名");

    let mut stale = renames;
    stale[0].start += 1;
    assert!(
        scalar_edits_for_renames(source, &stale).is_err(),
        "区间与正文不一致必须拒绝（防按过期偏移误改）"
    );
}

#[test]
fn rewrite_two_sources_refreshes_bodies_and_links() {
    let (_dir, vault) = fresh_vault();
    let target = seed_tree_note(&vault, "node-dst", "note-dst", "doc-dst", "目标");
    let source_a = seed_tree_note(&vault, "node-a", "note-a", "doc-a", "来源A");
    let source_b = seed_tree_note(&vault, "node-b", "note-b", "doc-b", "来源B");
    source_a
        .insert_scalars(0, "前 [[目标]] 后")
        .expect("写正文 A");
    source_b
        .insert_scalars(0, "[[目标|别名]] 和 [[目标#标题]]")
        .expect("写正文 B");
    target.insert_scalars(0, "目标正文").expect("写正文目标");
    commit_and_materialize(&vault, "note-a", &source_a);
    commit_and_materialize(&vault, "note-b", &source_b);
    commit_and_materialize(&vault, "note-dst", &target);

    let sources = link_sources_for_note(&vault, "note-dst").expect("收集来源");
    assert_eq!(sources.len(), 2, "{sources:?}");
    assert_eq!(sources[0].note_id, "note-a");
    assert_eq!(
        sources[0].targets,
        targets(&["目标"]),
        "targets 必须来自 links 行"
    );

    // 结构重命名先发生：只改投影标题（正文/links 不动，to_note_id 仍指向 note-dst）。
    vault
        .exec(
            "UPDATE tree SET title = '新目标' WHERE note_id = 'note-dst'",
            &[],
        )
        .expect("改标题");

    let report = rewrite_link_sources(
        &vault,
        &sources,
        "目标",
        "新目标",
        RenameLinkOptions {
            peer_id: PEER_A,
            sync: None,
            timeout: Duration::from_millis(1500),
        },
    );
    assert_eq!(report.notes_updated, 2, "{report:?}");
    assert_eq!(report.links_rewritten, 3, "{report:?}");
    assert_eq!(report.notes_failed, 0, "{report:?}");
    assert_eq!(report.sources, 2);

    assert_eq!(body_md(&vault, "note-a"), "前 [[新目标]] 后");
    assert_eq!(
        body_md(&vault, "note-b"),
        "[[新目标|别名]] 和 [[新目标#标题]]"
    );
    assert_eq!(body_md(&vault, "note-dst"), "目标正文");

    let rows_a = link_rows(&vault, "note-a");
    assert_eq!(
        rows_a,
        vec![("新目标".to_owned(), Some("note-dst".to_owned()))],
        "B 事务必须按新正文重建 links 且 to_note_id 仍指向被重命名笔记"
    );
    let rows_b = link_rows(&vault, "note-b");
    assert_eq!(rows_b.len(), 2);
    assert_eq!(rows_b[0].0, "新目标");
    assert_eq!(rows_b[1].0, "新目标");
    assert!(rows_b
        .iter()
        .all(|row| row.1.as_deref() == Some("note-dst")));

    // 幂等：再次收集执行，正文已无旧名，计划为空，不得重复写。
    let sources = link_sources_for_note(&vault, "note-dst").expect("再次收集来源");
    let again = rewrite_link_sources(
        &vault,
        &sources,
        "目标",
        "新目标",
        RenameLinkOptions {
            peer_id: PEER_A,
            sync: None,
            timeout: Duration::from_millis(1500),
        },
    );
    assert_eq!(again.notes_updated, 0, "{again:?}");
    assert_eq!(again.notes_unchanged, 2, "{again:?}");
    assert_eq!(again.notes_failed, 0, "{again:?}");
    assert_eq!(body_md(&vault, "note-a"), "前 [[新目标]] 后");
}

#[test]
fn rewrite_failure_does_not_roll_back_other_sources() {
    let (_dir, vault) = fresh_vault();
    let target = seed_tree_note(&vault, "node-dst", "note-dst", "doc-dst", "目标");
    let source = seed_tree_note(&vault, "node-ok", "note-ok", "doc-ok", "来源");
    source.insert_scalars(0, "[[目标]]").expect("写正文");
    target.insert_scalars(0, "目标正文").expect("写正文目标");
    commit_and_materialize(&vault, "note-ok", &source);
    commit_and_materialize(&vault, "note-dst", &target);

    // 故障注入：正常的第三篇来源先物化出 links 行，再把它的 head_frontier 改坏，
    // 让下一次会话打开时重放校验失败（不影响其它篇）。
    let broken = seed_tree_note(&vault, "node-broken", "note-broken", "doc-broken", "损坏");
    broken.insert_scalars(0, "[[目标]]").expect("写正文");
    commit_and_materialize(&vault, "note-broken", &broken);
    vault
        .exec(
            "UPDATE loro_docs SET head_frontier = X'DEADBEEF' WHERE doc_id = 'doc-broken'",
            &[],
        )
        .expect("破坏 head_frontier");

    vault
        .exec(
            "UPDATE tree SET title = '新目标' WHERE note_id = 'note-dst'",
            &[],
        )
        .expect("改标题");

    let sources = link_sources_for_note(&vault, "note-dst").expect("收集来源");
    assert_eq!(sources.len(), 2, "{sources:?}");
    let report = rewrite_link_sources(
        &vault,
        &sources,
        "目标",
        "新目标",
        RenameLinkOptions {
            peer_id: PEER_A,
            sync: None,
            timeout: Duration::from_millis(1500),
        },
    );
    assert_eq!(report.notes_updated, 1, "{report:?}");
    assert_eq!(report.links_rewritten, 1, "{report:?}");
    assert_eq!(report.notes_failed, 1, "{report:?}");
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].note_id, "note-broken");
    assert_eq!(body_md(&vault, "note-ok"), "[[新目标]]");
}
