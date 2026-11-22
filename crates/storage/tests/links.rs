//! Wiki links L2：正文 B 事务物化 `links` + 反向链接查询。

mod support;

use support::*;

use menghunote_storage::{list_backlinks, materialize_body, Value, Vault};

fn seed_tree_note(
    vault: &Vault,
    node_id: &str,
    note_id: &str,
    doc_id: &str,
    title: &str,
) -> menghunote_storage::BodyDoc {
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

#[test]
fn materialize_writes_resolved_wiki_links_and_backlinks() {
    let (_dir, vault) = fresh_vault();
    let source = seed_tree_note(&vault, "node-src", "note-src", "doc-src", "来源");
    seed_tree_note(&vault, "node-dst", "note-dst", "doc-dst", "目标");
    source.insert_scalars(0, "前 [[目标]] 后").expect("写正文");
    commit_now(&vault, &source, "src-1");

    let outcome = materialize_body(&vault, "note-src", "doc-src", 1, PEER_A).expect("物化");
    assert!(matches!(
        outcome,
        menghunote_storage::MaterializeOutcome::Applied { .. }
    ));
    let rows = vault
        .query(
            "SELECT to_note_id, target, start_byte, end_byte, body_seq FROM links WHERE from_note_id='note-src'",
            &[],
        )
        .expect("查 links");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0][0], Value::Text("note-dst".to_owned()));
    assert_eq!(rows[0][1], Value::Text("目标".to_owned()));
    assert_eq!(rows[0][4], Value::Integer(1));
    let backlinks = list_backlinks(&vault, "note-dst").expect("反向链接");
    assert_eq!(backlinks.len(), 1);
    assert_eq!(backlinks[0].from_note_id, "note-src");
    assert_eq!(backlinks[0].from_title, "来源");
    assert_eq!(backlinks[0].target, "目标");
}

#[test]
fn unresolved_wiki_links_stay_null_and_are_not_backlinks() {
    let (_dir, vault) = fresh_vault();
    let source = seed_tree_note(&vault, "node-src", "note-src", "doc-src", "来源");
    source.insert_scalars(0, "[[不存在]]").expect("写正文");
    commit_now(&vault, &source, "src-1");
    materialize_body(&vault, "note-src", "doc-src", 1, PEER_A).expect("物化");

    let rows = vault
        .query(
            "SELECT to_note_id, target FROM links WHERE from_note_id='note-src'",
            &[],
        )
        .expect("查 links");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Null);
    assert!(list_backlinks(&vault, "note-dst")
        .expect("反向链接")
        .is_empty());
}
