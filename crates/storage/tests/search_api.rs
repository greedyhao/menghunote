//! S4-1a：storage 为检索提供的接口（只读快照 / 索引清单 / meta 指针 / 变更通知）。
//!
//! 这些测试都用**真实 SQLite**（`CARGO_TARGET_TMPDIR` 下，避免 `%TEMP%` 的访问问题），
//! 覆盖任务书第 5 条的五点：快照读已提交且不影响写、meta 往返、物化通知、无订阅者置位、
//! manifest 覆盖墓碑与空 body。

use std::path::PathBuf;
use std::time::Duration;

use menghunote_storage::{
    append_updates, list_index_manifest, list_tree, load_body_doc, StructureWriter, UpdateRecord,
    Vault,
};

const PEER: u64 = 4_100_001;

fn tmp_vault(name: &str) -> (PathBuf, Vault) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录失败");
    let path = dir.join("vault.sqlite3");
    let vault = Vault::open(&path).expect("打开 vault 失败");
    (path, vault)
}

/// 建一篇笔记并写入正文（走产品同一条路径：结构写入 + A 事务 + 物化）。
fn seed_note(vault: &Vault, title: &str, body: &str, now: i64) -> String {
    let mut writer = StructureWriter::open(vault, PEER).expect("打开结构写入口失败");
    let node = writer.doc().create_note(title, now).expect("创建笔记失败");
    writer.commit("seed").expect("提交结构失败");
    let (doc, head) = load_body_doc(vault, &node.body_doc_id, PEER, None).expect("加载正文失败");
    doc.insert_scalars(0, body).expect("写正文失败");
    let base = head.version_vector().expect("解码 vv 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    append_updates(
        vault,
        &node.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("seed", update)],
    )
    .expect("写 A 事务失败");
    node.note_id
}

#[test]
fn read_snapshot_sees_committed_data_and_does_not_block_writes() {
    let (_path, vault) = tmp_vault("s4-snapshot");
    seed_note(&vault, "第一篇", "正文一", 1);

    // 快照能读到已提交数据
    let snapshot = vault.open_read_snapshot().expect("打开只读快照失败");
    let rows = snapshot
        .query("SELECT COUNT(*) FROM notes", &[])
        .expect("查询失败");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], menghunote_storage::Value::Integer(1));

    // 快照打开期间写入照常进行（WAL 并发读不挡写）
    seed_note(&vault, "第二篇", "正文二", 2);
    let rows_after = snapshot
        .query("SELECT COUNT(*) FROM notes", &[])
        .expect("查询失败");
    // 同一条快照连接可能看到、也可能看不到新提交（取决于它是否复用读事务）；
    // 这里只要求"不报错"，最新值用新快照确认。
    let _ = rows_after;
    let fresh = vault.open_read_snapshot().expect("重开快照失败");
    let fresh_rows = fresh
        .query("SELECT COUNT(*) FROM notes", &[])
        .expect("查询失败");
    assert_eq!(fresh_rows[0][0], menghunote_storage::Value::Integer(2));
}

#[test]
fn meta_roundtrips_through_the_write_actor() {
    let (_path, vault) = tmp_vault("s4-meta");
    assert_eq!(
        vault
            .get_meta("search.active_generation")
            .expect("读 meta 失败"),
        None
    );

    vault
        .set_meta("search.active_generation", b"gen-1")
        .expect("写 meta 失败");
    assert_eq!(
        vault
            .get_meta("search.active_generation")
            .expect("读 meta 失败"),
        Some(b"gen-1".to_vec())
    );

    // 覆盖写
    vault
        .set_meta("search.active_generation", b"gen-2")
        .expect("覆盖写 meta 失败");
    assert_eq!(
        vault
            .get_meta("search.active_generation")
            .expect("读 meta 失败"),
        Some(b"gen-2".to_vec())
    );

    // 只读快照也能读到（它是普通表）
    let snapshot = vault.open_read_snapshot().expect("打开快照失败");
    let rows = snapshot
        .query(
            "SELECT value FROM meta WHERE key = 'search.active_generation'",
            &[],
        )
        .expect("查询失败");
    assert_eq!(
        rows[0][0],
        menghunote_storage::Value::Blob(b"gen-2".to_vec())
    );
}

#[test]
fn materialization_publishes_an_event() {
    let (_path, vault) = tmp_vault("s4-events");
    let subscriber = vault.subscribe_events(menghunote_storage::DEFAULT_EVENT_QUEUE);

    // 结构提交会发 StructureProjected
    let note_id = seed_note(&vault, "会被物化的笔记", "正文", 1);
    let first = subscriber
        .recv_timeout(Duration::from_secs(2))
        .expect("应收到结构投影事件");
    assert!(matches!(
        first,
        menghunote_storage::StorageEvent::StructureProjected { .. }
    ));

    // 物化会发 BodyMaterialized（带上笔记 ID）
    let entries = list_tree(&vault).expect("读树失败");
    let entry = entries
        .iter()
        .find(|entry| entry.note_id.as_deref() == Some(note_id.as_str()))
        .expect("找不到刚建的笔记");
    let doc_id = entry.body_doc_id.clone().expect("笔记应有正文文档");
    menghunote_storage::materialize_body(&vault, &note_id, &doc_id, 1, PEER).expect("物化失败");
    let event = subscriber
        .recv_timeout(Duration::from_secs(2))
        .expect("应收到物化事件");
    match event {
        menghunote_storage::StorageEvent::BodyMaterialized { note_id: got } => {
            assert_eq!(got, note_id, "事件只带 ID，且必须是这篇笔记");
        }
        other => panic!("期望 BodyMaterialized，实际 {other:?}"),
    }
}

#[test]
fn no_subscriber_sets_reconcile_required() {
    let (_path, vault) = tmp_vault("s4-reconcile");
    // 一开始没有待对账标记
    assert!(!vault.take_reconcile_required());

    // 没有订阅者时写结构 → 事件无处投递 → 置位
    seed_note(&vault, "无人订阅", "正文", 1);
    assert!(
        vault.take_reconcile_required(),
        "无订阅者时必须置 reconcile_required"
    );
    // take 之后清零
    assert!(!vault.take_reconcile_required());
}

#[test]
fn manifest_covers_tombstones_and_empty_bodies() {
    let (_path, vault) = tmp_vault("s4-manifest");
    let alive = seed_note(&vault, "活着的笔记", "有正文", 1);
    let tomb = seed_note(&vault, "会进回收站的笔记", "也会被删", 2);

    // 把第二篇删掉（墓碑）。注意 `delete_note` 收的是**树节点 ID**，不是 note_id。
    let entries = list_tree(&vault).expect("读树失败");
    let tomb_node = entries
        .iter()
        .find(|entry| entry.note_id.as_deref() == Some(tomb.as_str()))
        .expect("找不到要删的节点")
        .node_id
        .clone();
    let mut writer = StructureWriter::open(&vault, PEER).expect("打开结构写入口失败");
    writer
        .doc()
        .delete_note(&tomb_node, 3)
        .expect("删除笔记失败");
    writer.commit("delete").expect("提交结构失败");

    let snapshot = vault.open_read_snapshot().expect("打开快照失败");
    let manifest = list_index_manifest(&snapshot).expect("读清单失败");
    assert_eq!(manifest.len(), 2, "墓碑必须也在清单里");

    let alive_row = manifest
        .iter()
        .find(|row| row.note_id == alive)
        .expect("缺活着的笔记");
    assert!(!alive_row.deleted);
    assert_eq!(alive_row.title, "活着的笔记");
    assert!(!alive_row.body_doc_id.is_empty());

    let tomb_row = manifest
        .iter()
        .find(|row| row.note_id == tomb)
        .expect("墓碑缺失（清单不能过滤 deleted=1）");
    assert!(tomb_row.deleted, "墓碑必须带 deleted=true");
    assert_eq!(tomb_row.title, "会进回收站的笔记");

    // 新库的两篇都还没物化 → body_md 必须是 None（而不是空串）
    assert!(alive_row.body_md.is_none(), "未物化的 body_md 应为 None");
    assert_eq!(alive_row.body_seq, 0);
    assert!(alive_row.body_frontier.is_none());
}
