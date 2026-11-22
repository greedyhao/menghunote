//! 侧栏树读接口（`list_tree` / `list_notes`）与 `create_folder`。

mod support;

use menghunote_storage::{list_notes, list_tree, NodeKind, StructureWriter, TreeEntry};
use support::*;

fn open_writer(vault: &menghunote_storage::Vault) -> StructureWriter {
    StructureWriter::open(vault, PEER_A).expect("打开结构写入口")
}

fn shape(entries: &[TreeEntry]) -> Vec<(String, usize, NodeKind)> {
    entries
        .iter()
        .map(|entry| (entry.title.clone(), entry.depth, entry.kind))
        .collect()
}

#[test]
fn empty_vault_lists_nothing() {
    let (_dir, vault) = fresh_vault();
    assert!(list_tree(&vault).expect("空库可读").is_empty());
    assert!(list_notes(&vault).expect("空库可读").is_empty());
}

#[test]
fn notes_and_folders_are_listed_in_preorder_with_depth() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);

    // 根级：目录「工作」+ 笔记「根笔记」（按创建顺序排 rank）
    let work = writer.doc().create_folder("工作", 10).expect("建目录");
    let root_note = writer.doc().create_note("根笔记", 11).expect("建笔记");

    // 「工作」下：子目录「子目录」，再下面挂笔记「子笔记」
    let sub = writer.doc().create_folder("子目录", 12).expect("建子目录");
    writer
        .doc()
        .move_node(&sub, Some(&work), 13)
        .expect("子目录挂到工作下");
    let sub_note = writer.doc().create_note("子笔记", 14).expect("建子笔记");
    writer
        .doc()
        .move_node(&sub_note.node_id, Some(&sub), 15)
        .expect("子笔记挂到子目录下");

    writer.commit("test-tree").expect("提交结构");

    let tree = list_tree(&vault).expect("读树");
    assert_eq!(
        shape(&tree),
        vec![
            ("工作".to_owned(), 0, NodeKind::Folder),
            ("子目录".to_owned(), 1, NodeKind::Folder),
            ("子笔记".to_owned(), 2, NodeKind::Note),
            ("根笔记".to_owned(), 0, NodeKind::Note),
        ],
        "必须是先序 DFS：工作 → 子目录 → 子笔记 → 根笔记"
    );

    // 目录：有 node_id / title / parent，没有 note_id / body_doc_id
    let folder_entry = &tree[0];
    assert_eq!(folder_entry.node_id, work);
    assert_eq!(folder_entry.parent_id, None);
    assert_eq!(folder_entry.note_id, None);
    assert_eq!(folder_entry.body_doc_id, None);
    assert!(folder_entry.is_folder() && !folder_entry.is_note());
    assert_eq!(folder_entry.sibling_rank, 0);

    // 子目录的 parent 是「工作」，子笔记的 parent 是「子目录」
    assert_eq!(tree[1].parent_id.as_deref(), Some(work.as_str()));
    assert_eq!(tree[2].parent_id.as_deref(), Some(tree[1].node_id.as_str()));
    assert_eq!(tree[2].note_id.as_deref(), Some(sub_note.note_id.as_str()));
    assert_eq!(
        tree[2].body_doc_id.as_deref(),
        Some(sub_note.body_doc_id.as_str()),
        "笔记要带 body_doc_id（打开编辑器用）"
    );
    // 根级 rank：工作 0、根笔记 1
    assert_eq!(tree[3].node_id, root_note.node_id);
    assert_eq!(tree[3].sibling_rank, 1);
}

#[test]
fn rename_is_reflected_immediately() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);
    let folder = writer.doc().create_folder("旧目录名", 10).expect("建目录");
    let note = writer.doc().create_note("旧标题", 11).expect("建笔记");
    writer.commit("create").expect("提交");
    assert_eq!(list_tree(&vault).expect("读树")[0].title, "旧目录名");

    writer
        .doc()
        .rename(&folder, "新目录名", 12)
        .expect("改目录名");
    writer
        .doc()
        .rename(&note.node_id, "新标题", 13)
        .expect("改标题");
    writer.commit("rename").expect("提交改名");

    let tree = list_tree(&vault).expect("读树");
    assert_eq!(tree[0].title, "新目录名");
    assert_eq!(tree[1].title, "新标题");
}

#[test]
fn order_is_stable_across_calls() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);
    let folder = writer.doc().create_folder("A", 10).expect("目录");
    let first = writer.doc().create_note("B", 11).expect("笔记");
    let second = writer.doc().create_note("C", 12).expect("笔记");
    writer
        .doc()
        .move_node(&second.node_id, Some(&folder), 13)
        .expect("挂到 A 下");
    writer.commit("create").expect("提交");
    assert!(!first.node_id.is_empty(), "笔记节点要有 node_id");

    let one = list_tree(&vault).expect("第一次");
    let two = list_tree(&vault).expect("第二次");
    assert_eq!(one, two, "同一树状态两次调用结果必须完全一致");
    assert_eq!(
        one.iter()
            .map(|entry| entry.title.as_str())
            .collect::<Vec<_>>(),
        vec!["A", "C", "B"],
        "rank 决定顺序，与创建先后无关（C 在 A 下 depth 1）"
    );
    assert_eq!(one[1].title, "C");
    assert_eq!(one[1].depth, 1);
    assert_eq!(one[2].title, "B");
    assert_eq!(one[2].depth, 0);
}

#[test]
fn list_notes_filters_folders_but_keeps_real_depth() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);
    let folder = writer.doc().create_folder("目录", 10).expect("目录");
    let inner = writer.doc().create_note("里面的笔记", 11).expect("笔记");
    writer
        .doc()
        .move_node(&inner.node_id, Some(&folder), 12)
        .expect("挂到目录下");
    let root = writer.doc().create_note("根笔记", 13).expect("笔记");
    writer.commit("create").expect("提交");

    let notes = list_notes(&vault).expect("读笔记");
    assert_eq!(notes.len(), 2);
    assert!(notes.iter().all(|entry| entry.is_note()));
    assert_eq!(notes[0].title, "里面的笔记");
    assert_eq!(notes[0].depth, 1, "过滤目录后 depth 仍是真实深度");
    assert_eq!(notes[1].title, "根笔记");
    assert_eq!(notes[1].node_id, root.node_id);
    assert_eq!(notes[1].depth, 0);
}

#[test]
fn deleted_note_disappears_from_both_views() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);
    let note = writer.doc().create_note("会被删的", 10).expect("笔记");
    let keep = writer.doc().create_note("留下的", 11).expect("笔记");
    writer.commit("create").expect("提交");
    assert_eq!(list_tree(&vault).expect("读树").len(), 2);

    writer
        .doc()
        .delete_note(&note.node_id, 12)
        .expect("删除笔记");
    writer.commit("delete").expect("提交删除");

    let tree = list_tree(&vault).expect("读树");
    assert_eq!(tree.len(), 1, "被删节点不返回");
    assert_eq!(tree[0].node_id, keep.node_id);
    assert_eq!(list_notes(&vault).expect("读笔记").len(), 1);
}

#[test]
fn folder_without_commit_is_not_listed_until_projected() {
    // 读的是投影表：未 commit（未投影）的结构改动不应出现——这是「读投影」的代价，写明以免误解
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault);
    writer.doc().create_folder("待提交", 10).expect("目录");
    assert!(
        list_tree(&vault).expect("读树").is_empty(),
        "未投影前读不到"
    );

    writer.commit("create").expect("提交");
    assert_eq!(list_tree(&vault).expect("读树")[0].title, "待提交");
}
