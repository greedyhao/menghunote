//! S7-4 storage 同步桥：迁移、outbox parts、ACK CAS、inbox 本地 seq、崩溃重放。

mod support;

use support::*;

use menghunote_storage::{
    allocate_local_seq_for_import, append_updates_with_sync_intents, enqueue_sync_intent,
    load_head, mark_acked, mark_integrated, mark_sent, mark_skipped_event_received, outbox_parts,
    outbox_row, pending_encryption, pending_send, set_sync_log_id, store_encrypted_parts,
    store_inbox_verified, sync_cursor_state, take_inbox_ready, update_referenced_by_outbox,
    SyncEncryptedPart, SyncInboxState, SyncIntent, SyncOutboxState, UpdateRecord, Value, Vault,
};

fn failed_parts(_message_id: &str, bytes: &[u8]) -> Vec<SyncEncryptedPart> {
    vec![
        SyncEncryptedPart {
            part_no: 0,
            envelope: bytes.to_vec(),
            digest: [1; 32],
        },
        SyncEncryptedPart {
            part_no: 1,
            envelope: vec![bytes[0], 2],
            digest: [2; 32],
        },
    ]
}

#[test]
fn migration_v1_to_v2_is_idempotent_and_adds_sync_tables() {
    let dir = temp_dir();
    let path = vault_path(&dir);
    std::fs::create_dir_all(path.parent().expect("父目录")).expect("创建父目录");

    // 手工建一个 v1 库（与历史用户库一致），再交给 Vault 自动升级。
    {
        let conn = rusqlite::Connection::open(&path).expect("打开 v1 连接");
        conn.execute_batch(include_str!("../src/schema/v1.sql"))
            .expect("建 v1 schema");
        conn.pragma_update(None, "user_version", 1)
            .expect("写 v1 版本");
    }

    let vault = Vault::open(&path).expect("打开并升级");
    assert_eq!(vault.user_version().expect("版本"), 3);

    let columns = |vault: &Vault| -> Vec<String> {
        vault
            .query("PRAGMA table_info(sync_outbox)", &[])
            .expect("table_info")
            .into_iter()
            .filter_map(|row| match row.get(1) {
                Some(Value::Text(name)) => Some(name.clone()),
                _ => None,
            })
            .collect()
    };
    let columns = columns(&vault);
    for name in ["object_id", "state", "cipher_digest"] {
        assert!(columns.iter().any(|column| column == name), "{columns:?}");
    }
    let tables = vault
        .query(
            "SELECT name FROM sqlite_master WHERE type='table' AND name IN('sync_outbox_parts','sync_inbox')",
            &[],
        )
        .expect("表");
    assert_eq!(tables.len(), 2);

    drop(vault);

    // 重复打开幂等：版本与表数量不变。
    let before = Vault::open(&path)
        .expect("reopen")
        .query(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            &[],
        )
        .expect("count")[0][0]
        .clone();
    let vault = Vault::open(&path).expect("reopen2");
    assert_eq!(vault.user_version().expect("version"), 3);
    let after = vault
        .query(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            &[],
        )
        .expect("count")[0][0]
        .clone();
    assert_eq!(after, before);
}

#[test]
fn a_transaction_enqueues_intent_atomically() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "hello").expect("写正文");
    let head = load_head(&vault, "doc-1").expect("head");
    let base = head.version_vector().expect("vv");
    let update = doc.pending_update(&base).expect("update");
    let intent = SyncIntent {
        message_id: "msg-1".to_owned(),
        batch_id: "batch-1".to_owned(),
        key_epoch: 1,
        object_id: [9; 32],
    };
    let head = append_updates_with_sync_intents(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("batch-1", update)],
        std::slice::from_ref(&intent),
    )
    .expect("A+intent");
    assert_eq!(head.head_seq, 1);

    let pending = pending_encryption(&vault, 10).expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].message_id, "msg-1");
    assert_eq!(pending[0].state, SyncOutboxState::Pending);
    assert_eq!(pending[0].object_id, Some([9; 32]));
    assert!(update_referenced_by_outbox(&vault, "doc-1", 1).expect("ref"));

    // A 事务的 head CAS 失败时，intent 也必须一起回滚。
    let err = append_updates_with_sync_intents(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new(
            "batch-2",
            doc.pending_update(&base).expect("u2"),
        )],
        &[SyncIntent {
            message_id: "msg-2".to_owned(),
            batch_id: "batch-2".to_owned(),
            key_epoch: 1,
            object_id: [8; 32],
        }],
    );
    assert!(err.is_err());
    assert!(outbox_row(&vault, "msg-2").expect("row").is_none());
}

#[test]
fn outbox_parts_roundtrip_conflict_and_ack_cas() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "body").expect("正文");
    commit_now(&vault, &doc, "batch-1");
    enqueue_sync_intent(&vault, "msg-1", "batch-1", "doc-1", 1, 1, &[7; 32]).expect("intent");

    let parts = failed_parts("msg-1", &[1, 2, 3]);
    let digest = [5; 32];
    store_encrypted_parts(&vault, "msg-1", 1, &digest, &parts).expect("parts");

    let row = outbox_row(&vault, "msg-1").expect("row").expect("exists");
    assert_eq!(row.state, SyncOutboxState::Encrypted);
    assert_eq!(row.cipher_digest, Some(digest));
    let stored = outbox_parts(&vault, "msg-1", 1).expect("parts read");
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[1].envelope, vec![1, 2]);
    assert_eq!(pending_send(&vault, 10).expect("send").len(), 1);

    // 同一 message/epoch 的相同字节幂等。
    store_encrypted_parts(&vault, "msg-1", 1, &digest, &parts).expect("idempotent");

    // 不同字节必须冲突，且不覆盖旧 parts。
    let mut changed = parts.clone();
    changed[0].envelope = vec![99];
    assert!(store_encrypted_parts(&vault, "msg-1", 1, &digest, &changed).is_err());
    assert_eq!(
        outbox_parts(&vault, "msg-1", 1).expect("parts")[0].envelope,
        vec![1, 2, 3]
    );

    assert!(mark_sent(&vault, "msg-1", 1, &digest).expect("sent"));
    assert_eq!(
        outbox_row(&vault, "msg-1")
            .expect("row")
            .expect("exists")
            .state,
        SyncOutboxState::Sent
    );
    assert!(mark_acked(&vault, "msg-1", 1, &digest).expect("ack"));
    assert!(outbox_row(&vault, "msg-1").expect("row").is_none());
    assert!(outbox_parts(&vault, "msg-1", 1).expect("parts").is_empty());
    assert!(!mark_acked(&vault, "msg-1", 1, &digest).expect("old ack"));
    assert!(!update_referenced_by_outbox(&vault, "doc-1", 1).expect("ref"));
}

#[test]
fn old_ack_does_not_delete_new_message_for_same_update() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "body").expect("正文");
    commit_now(&vault, &doc, "batch-1");

    // 旧 message 加密并 ACK。
    enqueue_sync_intent(&vault, "msg-old", "batch-1", "doc-1", 1, 1, &[1; 32]).expect("old");
    store_encrypted_parts(
        &vault,
        "msg-old",
        1,
        &[1; 32],
        &failed_parts("msg-old", &[1]),
    )
    .expect("old parts");
    assert!(mark_acked(&vault, "msg-old", 1, &[1; 32]).expect("old ack"));

    // 同一 update 的新 message 重新入队；旧 ACK 不能碰它。
    enqueue_sync_intent(&vault, "msg-new", "batch-2", "doc-1", 1, 1, &[2; 32]).expect("new");
    store_encrypted_parts(
        &vault,
        "msg-new",
        1,
        &[2; 32],
        &failed_parts("msg-new", &[2]),
    )
    .expect("new parts");
    assert!(!mark_acked(&vault, "msg-old", 1, &[1; 32]).expect("stale ack"));
    let new = outbox_row(&vault, "msg-new")
        .expect("row")
        .expect("new row");
    assert_eq!(new.state, SyncOutboxState::Encrypted);
    assert_eq!(outbox_parts(&vault, "msg-new", 1).expect("parts").len(), 2);
}

#[test]
fn outbox_parts_and_ack_survive_reopen() {
    let (dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "body").expect("正文");
    commit_now(&vault, &doc, "batch-1");
    enqueue_sync_intent(&vault, "msg-1", "batch-1", "doc-1", 1, 1, &[3; 32]).expect("intent");
    store_encrypted_parts(&vault, "msg-1", 1, &[4; 32], &failed_parts("msg-1", &[4]))
        .expect("parts");
    assert!(mark_sent(&vault, "msg-1", 1, &[4; 32]).expect("sent before crash"));
    drop(vault);

    let vault = open_vault(&vault_path(&dir));
    let row = outbox_row(&vault, "msg-1")
        .expect("row")
        .expect("row survived");
    assert_eq!(row.state, SyncOutboxState::Sent);
    assert_eq!(outbox_parts(&vault, "msg-1", 1).expect("parts").len(), 2);
    assert!(mark_acked(&vault, "msg-1", 1, &[4; 32]).expect("ack"));
    drop(vault);

    let vault = open_vault(&vault_path(&dir));
    assert!(outbox_row(&vault, "msg-1").expect("row").is_none());
    assert!(outbox_parts(&vault, "msg-1", 1).expect("parts").is_empty());
}

#[test]
fn skipped_event_advances_received_and_integrated_prefix() {
    let (_dir, vault) = fresh_vault();
    let log_id = [9; 16];
    set_sync_log_id(&vault, &log_id).expect("log id");

    // seq1 是 ASSET（不写 inbox），seq2 是 UPDATE。
    mark_skipped_event_received(&vault, &log_id, 1).expect("skip seq1");
    store_inbox_verified(&vault, &log_id, 2, &[2; 32], 1, b"two").expect("seq2");
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 2, "ASSET 也必须计入连续接收前缀");
    let ready = take_inbox_ready(&vault, 10).expect("ready");
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].relay_seq, 2);

    // 集成 seq2 后，integrated 前缀同样要越过 ASSET（否则 ack 水位永远卡在 0）。
    mark_integrated(&vault, &log_id, 2).expect("integrate seq2");
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.integrated_seq, 2);
    assert!(sync_cursor_state(&vault).expect("cursor").log_id.is_some());

    // 标记幂等：重复登记同一非 inbox 事件不改变水位/不报错。
    mark_skipped_event_received(&vault, &log_id, 1).expect("幂等");
    assert_eq!(sync_cursor_state(&vault).expect("cursor").received_seq, 2);
}

#[test]
fn inbox_is_durable_allocates_local_seq_and_integrates() {
    let (dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    let log_id = [7; 16];
    set_sync_log_id(&vault, &log_id).expect("log id");

    // 依赖未齐：先存 seq2，再存 seq1；两条都 durable。
    store_inbox_verified(&vault, &log_id, 2, &[2; 32], 1, b"two").expect("seq2");
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 0, "seq1 未到，连续前缀不能前进");
    assert_eq!(cursor.log_id, Some(log_id));
    assert!(
        take_inbox_ready(&vault, 10).expect("ready").is_empty(),
        "缺口未补齐时 seq2 不能进入 ready"
    );

    store_inbox_verified(&vault, &log_id, 1, &[1; 32], 1, b"one").expect("seq1");
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 2);
    let ready = take_inbox_ready(&vault, 10).expect("ready");
    assert_eq!(ready.len(), 2);
    assert_eq!(ready[0].relay_seq, 1);
    assert_eq!(
        ready[1].verified_payload.as_deref(),
        Some(b"two".as_slice())
    );

    // 重复写同字节幂等；不同字节冲突。
    store_inbox_verified(&vault, &log_id, 2, &[2; 32], 1, b"two").expect("idempotent");
    assert!(store_inbox_verified(&vault, &log_id, 2, &[2; 32], 1, b"changed").is_err());
    assert!(store_inbox_verified(&vault, &[8; 16], 3, &[3; 32], 1, b"x").is_err());

    // 本地 seq 只取 head+1，不写库、不采用 sender_seq。
    let local = allocate_local_seq_for_import(&vault, "doc-1").expect("local seq");
    assert_eq!(local, 1);
    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 0);

    // 模拟 import 推进 head，然后 integrate；连续前缀推进到 2。
    doc.insert_scalars(0, "remote").expect("remote body");
    commit_now(&vault, &doc, "remote-batch");
    let _ = local;

    mark_integrated(&vault, &log_id, 1).expect("integrate1");
    mark_integrated(&vault, &log_id, 2).expect("integrate2");
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.integrated_seq, 2);
    let ready = take_inbox_ready(&vault, 10).expect("ready after integrate");
    assert!(ready.is_empty());
    let rows = vault
        .query("SELECT state FROM sync_inbox ORDER BY relay_seq", &[])
        .expect("inbox");
    assert!(rows
        .iter()
        .all(|row| matches!(row.first(), Some(Value::Text(state)) if state == "integrated")));
    assert_eq!(SyncInboxState::Integrated.as_str(), "integrated");
    drop(vault);

    // durable：重开后 inbox 与 cursor 都还在。
    let vault = open_vault(&vault_path(&dir));
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 2);
    assert_eq!(cursor.integrated_seq, 2);
    assert_eq!(
        vault
            .query("SELECT count(*) FROM sync_inbox", &[])
            .expect("count")[0][0],
        Value::Integer(2)
    );
}
