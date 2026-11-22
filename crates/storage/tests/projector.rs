//! 结构投影器：`tree` / `notes` / `tags` / `note_tags` 行、幂等重建、回收站语义。

mod support;

use loro::VersionVector;
use support::*;

use menghunote_storage::{
    append_updates, ensure_structure_row, load_body_state, load_head, load_structure,
    materialize_body, project, trashed_notes, visible_notes, BodyDoc, MaterializeOutcome,
    StructureDoc, StructureWriter, UpdateRecord, Value, Vault,
};

const NOTES_DUMP: &str = "SELECT note_id, body_doc_id, title, created_at, updated_at, deleted, \
                          is_favorite, is_pinned, meta_seq FROM notes ORDER BY note_id";
const TREE_DUMP: &str =
    "SELECT node_id, parent_id, note_id, title, sibling_rank, deleted, meta_seq \
                         FROM tree ORDER BY node_id";
const TAGS_DUMP: &str = "SELECT tag_id, name, normalized_name FROM tags ORDER BY tag_id";
const LINKS_DUMP: &str = "SELECT note_id, tag_id FROM note_tags ORDER BY note_id, tag_id";

fn dump(vault: &Vault, sql: &str) -> Vec<String> {
    vault
        .query(sql, &[])
        .expect("dump 查询")
        .into_iter()
        .map(|row| format!("{row:?}"))
        .collect()
}

fn dumps(vault: &Vault) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
    (
        dump(vault, NOTES_DUMP),
        dump(vault, TREE_DUMP),
        dump(vault, TAGS_DUMP),
        dump(vault, LINKS_DUMP),
    )
}

fn open_writer(vault: &Vault, peer: u64) -> StructureWriter {
    StructureWriter::open(vault, peer).expect("打开结构写入口")
}

#[test]
fn writer_persists_structure_doc_and_projects_rows() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault, PEER_A);

    let first = writer.doc().create_note("笔记一", 100).expect("建笔记一");
    let second = writer.doc().create_note("笔记二", 200).expect("建笔记二");
    let report = writer.commit("s1").expect("提交结构");

    assert_eq!(report.nodes, 2);
    assert_eq!(report.notes, 2);
    assert_eq!(report.tags, 0);
    assert_eq!(report.tag_links, 0);

    // 结构文档行：kind='structure'，head_seq 前进到 1
    let structure = vault
        .query(
            "SELECT kind, head_seq FROM loro_docs WHERE doc_id = 'structure'",
            &[],
        )
        .expect("查结构行");
    assert_eq!(structure.len(), 1);
    assert_eq!(structure[0][0], Value::Text("structure".to_owned()));
    assert_eq!(structure[0][1], Value::Integer(1));

    // 每篇笔记都有 body 文档行（投影器负责保证）
    for seed in [&first, &second] {
        let body = vault
            .query(
                "SELECT kind FROM loro_docs WHERE doc_id = ?1",
                &[Value::Text(seed.body_doc_id.clone())],
            )
            .expect("查 body 行");
        assert_eq!(body.len(), 1, "缺 body_doc 行：{}", seed.body_doc_id);
        assert_eq!(body[0][0], Value::Text("body".to_owned()));

        let mapping = vault
            .query(
                "SELECT body_doc_id FROM notes WHERE note_id = ?1",
                &[Value::Text(seed.note_id.clone())],
            )
            .expect("查映射");
        assert_eq!(mapping.len(), 1);
        assert_eq!(
            mapping[0][0],
            Value::Text(seed.body_doc_id.clone()),
            "note.body_doc_id 必须指向它的 body 文档行"
        );
    }

    // notes / tree 行内容
    let notes = vault
        .query(
            "SELECT note_id, title, created_at, updated_at, deleted, is_favorite, is_pinned, meta_seq \
             FROM notes ORDER BY updated_at DESC",
            &[],
        )
        .expect("查 notes");
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0][0], Value::Text(second.note_id.clone()));
    assert_eq!(notes[0][1], Value::Text("笔记二".to_owned()));
    assert_eq!(notes[0][2], Value::Integer(200));
    assert_eq!(notes[0][3], Value::Integer(200));
    assert_eq!(notes[0][4], Value::Integer(0));
    assert_eq!(notes[0][7], Value::Integer(1), "meta_seq = 结构 head_seq");

    let tree = vault
        .query(
            "SELECT parent_id, sibling_rank, deleted FROM tree ORDER BY sibling_rank",
            &[],
        )
        .expect("查 tree");
    assert_eq!(tree.len(), 2);
    assert_eq!(tree[0][0], Value::Null, "两个根级节点");
    assert_eq!(tree[0][1], Value::Integer(0));
    assert_eq!(tree[1][1], Value::Integer(1));

    // 默认查询：两条都可见，按 updated_at 倒序
    let visible = visible_notes(&vault).expect("默认查询");
    assert_eq!(visible.len(), 2);
    assert_eq!(visible[0].note_id, second.note_id);
    assert_eq!(visible[1].note_id, first.note_id);
    assert!(visible.iter().all(|note| note.parent_id.is_none()));
}

#[test]
fn rename_move_tags_flags_then_delete_and_restore_reflect_in_projection() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault, PEER_A);
    let a = writer.doc().create_note("A", 100).expect("建 A");
    let b = writer.doc().create_note("B", 200).expect("建 B");
    writer.commit("s1").expect("提交");

    writer.doc().rename(&a.node_id, "A2", 300).expect("改名");
    writer
        .doc()
        .move_node(&b.node_id, Some(&a.node_id), 400)
        .expect("移动");
    let tag_id = writer
        .doc()
        .add_tag(&a.node_id, "工作", 500)
        .expect("打标签");
    writer
        .doc()
        .set_favorite(&a.node_id, true, 600)
        .expect("收藏");
    writer
        .doc()
        .set_pinned(&a.node_id, true, 700)
        .expect("置顶");
    writer.commit("s2").expect("提交");

    let note_a = vault
        .query(
            "SELECT title, updated_at, is_favorite, is_pinned FROM notes WHERE note_id = ?1",
            &[Value::Text(a.note_id.clone())],
        )
        .expect("查 A");
    assert_eq!(note_a[0][0], Value::Text("A2".to_owned()));
    assert_eq!(note_a[0][1], Value::Integer(700), "updated_at 取结构编辑戳");
    assert_eq!(note_a[0][2], Value::Integer(1));
    assert_eq!(note_a[0][3], Value::Integer(1));

    let tree_b = vault
        .query(
            "SELECT parent_id, sibling_rank FROM tree WHERE node_id = ?1",
            &[Value::Text(b.node_id.clone())],
        )
        .expect("查 B 节点");
    assert_eq!(tree_b[0][0], Value::Text(a.node_id.clone()));
    assert_eq!(tree_b[0][1], Value::Integer(0), "dense rank 从遍历顺序来");

    let tags = vault
        .query("SELECT tag_id, name, normalized_name FROM tags", &[])
        .expect("查标签");
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0][1], Value::Text("工作".to_owned()));
    assert_eq!(tags[0][2], Value::Text("工作".to_owned()));

    let links = vault
        .query("SELECT note_id, tag_id FROM note_tags", &[])
        .expect("查成员关系");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0][0], Value::Text(a.note_id.clone()));
    assert_eq!(links[0][1], Value::Text(tag_id.clone()));

    // 删除 → 进回收站（笔记行不删，deleted 列置 1）
    writer.doc().delete_note(&a.node_id, 800).expect("删除");
    writer.commit("s3").expect("提交");
    let deleted = vault
        .query(
            "SELECT notes.deleted, tree.deleted FROM notes JOIN tree ON tree.note_id = notes.note_id \
             WHERE notes.note_id = ?1",
            &[Value::Text(a.note_id.clone())],
        )
        .expect("查删除状态");
    assert_eq!(deleted[0][0], Value::Integer(1));
    assert_eq!(deleted[0][1], Value::Integer(1));
    assert_eq!(
        dump(&vault, "SELECT count(*) FROM notes"),
        vec!["[Integer(2)]".to_owned()],
        "删除不得减少 notes 行数（不用 SQL DELETE 表达删除）"
    );

    let visible = visible_notes(&vault).expect("默认查询");
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].note_id, b.note_id, "已删笔记不出现在默认查询");
    let trashed = trashed_notes(&vault).expect("回收站");
    assert_eq!(trashed.len(), 1);
    assert_eq!(trashed[0].note_id, a.note_id);
    // 回收站里仍能看到标签/标题这些结构投影
    assert_eq!(trashed[0].title, "A2");

    // 恢复 → 回到默认查询
    writer.doc().restore_note(&a.node_id, 900).expect("恢复");
    writer.commit("s4").expect("提交");
    assert_eq!(visible_notes(&vault).expect("默认查询").len(), 2);
    assert!(trashed_notes(&vault).expect("回收站").is_empty());
}

#[test]
fn projection_is_idempotent_and_noop_commit_does_not_advance_head() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault, PEER_A);
    let seed = writer.doc().create_note("幂等", 100).expect("建笔记");
    writer
        .doc()
        .add_tag(&seed.node_id, "标签", 200)
        .expect("打标签");
    writer.commit("s1").expect("提交");

    let meta_seq = writer.head().head_seq;
    let before = dumps(&vault);
    let report_one = project(&vault, writer.doc(), meta_seq).expect("投影一");
    let middle = dumps(&vault);
    let report_two = project(&vault, writer.doc(), meta_seq).expect("投影二");
    let after = dumps(&vault);

    assert_eq!(report_one, report_two);
    assert_eq!(before, middle, "投影结果必须与 A 事务后的结果一致");
    assert_eq!(middle, after, "同一结构 doc 重复投影必须完全一致");

    // 没有结构变更时 commit 只补投影，不能推进 head_seq
    writer.commit("noop").expect("空提交");
    assert_eq!(writer.head().head_seq, meta_seq);
    assert_eq!(dumps(&vault), before);
}

#[test]
fn rebuild_into_fresh_vault_matches_source_projection() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault, PEER_A);
    let a = writer.doc().create_note("重建一", 100).expect("建一");
    let b = writer.doc().create_note("重建二", 200).expect("建二");
    writer
        .doc()
        .rename(&a.node_id, "重建一改", 300)
        .expect("改名");
    writer
        .doc()
        .move_node(&b.node_id, Some(&a.node_id), 400)
        .expect("移动");
    writer
        .doc()
        .add_tag(&b.node_id, "标签", 500)
        .expect("打标签");
    writer.doc().delete_note(&a.node_id, 600).expect("删除");
    writer.commit("s1").expect("提交");

    let meta_seq = writer.head().head_seq;
    let source_rows = dumps(&vault);
    let updates = writer.doc().export_all_updates().expect("导出结构 update");

    // 空库里只靠「结构 update + 一次投影」重建
    let (_dir2, vault2) = fresh_vault();
    ensure_structure_row(&vault2).expect("建结构行");
    let replayed =
        StructureDoc::restore(PEER_A, None, std::slice::from_ref(&updates)).expect("重放结构");
    project(&vault2, &replayed, meta_seq).expect("重建投影");

    assert_eq!(dumps(&vault2), source_rows, "投影必须能仅凭结构 CRDT 重建");
    assert_eq!(
        visible_notes(&vault2).expect("默认查询").len(),
        1,
        "重建后已删笔记同样被过滤"
    );
}

#[test]
fn body_edit_in_trash_does_not_revive_note_nor_clobber_body_md() {
    let (_dir, vault) = fresh_vault();
    let mut writer = open_writer(&vault, PEER_A);
    let seed = writer.doc().create_note("先删后写", 100).expect("建笔记");
    writer.commit("s1").expect("提交");
    writer.doc().delete_note(&seed.node_id, 200).expect("删除");
    writer.commit("s2").expect("提交");
    assert!(visible_notes(&vault).expect("默认查询").is_empty());

    // 回收站里继续写正文：走 A 事务 + B 物化（与结构投影互不干扰）
    let body = BodyDoc::restore(seed.body_doc_id.as_str(), PEER_A, None, &[]).expect("重放正文");
    body.insert_scalars(0, "回收站里的正文").expect("写正文");
    let head = load_head(&vault, &seed.body_doc_id).expect("读正文 head");
    assert_eq!(head.head_seq, 0);
    let update = body
        .pending_update(&VersionVector::default())
        .expect("导出增量");
    append_updates(
        &vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("body-b1", update)],
    )
    .expect("A 事务");
    let outcome =
        materialize_body(&vault, &seed.note_id, &seed.body_doc_id, 1, PEER_A).expect("B 物化");
    assert!(matches!(
        outcome,
        MaterializeOutcome::Applied { body_seq: 1, .. }
    ));

    // 结构投影再跑一遍：笔记仍然在回收站，正文不被清掉
    writer.commit("proj-again").expect("提交");
    assert!(
        visible_notes(&vault).expect("默认查询").is_empty(),
        "并发正文编辑不得复活笔记"
    );
    let state = load_body_state(&vault, &seed.note_id).expect("正文状态");
    assert_eq!(
        state.body_md.as_deref(),
        Some("回收站里的正文"),
        "结构投影不得碰 B 物化的列"
    );

    // 恢复之后可见，正文仍在
    writer.doc().restore_note(&seed.node_id, 300).expect("恢复");
    writer.commit("s3").expect("提交");
    assert_eq!(visible_notes(&vault).expect("默认查询").len(), 1);
    let state = load_body_state(&vault, &seed.note_id).expect("正文状态");
    assert_eq!(state.body_md.as_deref(), Some("回收站里的正文"));
}

#[test]
fn reopen_reloads_structure_and_continues_sequence() {
    let (dir, vault) = fresh_vault();
    {
        let mut writer = open_writer(&vault, PEER_A);
        writer.doc().create_note("第一篇", 100).expect("建笔记");
        writer.commit("s1").expect("提交");
    }
    drop(vault);

    // 重开库：结构 doc 从 snapshot/updates 重放出来，head 与投影都在
    let vault = open_vault(&vault_path(&dir));
    let (doc, head) = load_structure(&vault, PEER_A).expect("重放结构");
    assert_eq!(head.head_seq, 1);
    assert_eq!(doc.snapshot().expect("快照").nodes.len(), 1);
    assert_eq!(visible_notes(&vault).expect("默认查询").len(), 1);

    // 继续写：seq 从 1 接着到 2，不会 HeadConflict
    let mut writer = open_writer(&vault, PEER_A);
    writer.doc().create_note("第二篇", 200).expect("建第二篇");
    writer.commit("s2").expect("提交");
    assert_eq!(writer.head().head_seq, 2);
    assert_eq!(visible_notes(&vault).expect("默认查询").len(), 2);
    assert_eq!(
        dump(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'structure'"
        ),
        vec!["[Integer(2)]".to_owned()]
    );
}

#[test]
fn two_replicas_converge_and_project_identical_rows() {
    // 两个副本并发改结构，交换 update 后各自投影到独立 vault，行必须一致
    let a = StructureDoc::create(1).expect("副本 a");
    let n1 = a.create_note("一", 100).expect("建一");
    let n2 = a.create_note("二", 200).expect("建二");

    let b = StructureDoc::create(2).expect("副本 b");
    b.import(&a.export_all_updates().expect("导出基准"))
        .expect("b 导入");

    a.move_node(&n2.node_id, Some(&n1.node_id), 300)
        .expect("a 移动");
    b.add_tag(&n2.node_id, "标签", 310).expect("b 打标签");
    b.set_favorite(&n1.node_id, true, 320).expect("b 收藏");

    // 双向同步
    let a_bytes = a.export_all_updates().expect("导出 a");
    let b_bytes = b.export_all_updates().expect("导出 b");
    a.import(&b_bytes).expect("a 导入");
    b.import(&a_bytes).expect("b 导入");
    assert_eq!(
        a.snapshot().expect("a 快照"),
        b.snapshot().expect("b 快照"),
        "两副本结构必须收敛"
    );

    let (_dir_a, vault_a) = fresh_vault();
    ensure_structure_row(&vault_a).expect("a 结构行");
    project(&vault_a, &a, 1).expect("投影 a");

    let (_dir_b, vault_b) = fresh_vault();
    ensure_structure_row(&vault_b).expect("b 结构行");
    project(&vault_b, &b, 1).expect("投影 b");

    assert_eq!(dumps(&vault_a), dumps(&vault_b), "收敛后的投影必须一致");
    let tree = dump(
        &vault_a,
        "SELECT parent_id, sibling_rank FROM tree WHERE note_id = (SELECT note_id FROM notes WHERE title = '二')",
    );
    assert_eq!(tree, vec!["[Text(\"0@1\"), Integer(0)]".to_owned()]);
}
