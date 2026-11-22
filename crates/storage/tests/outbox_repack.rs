//! S7-33：outbox 原子 repack（CAS 替换、迟到旧 ACK、注入回滚、重开恢复）。

mod support;

use support::*;

use menghunote_storage::{
    enqueue_sync_intent, mark_acked, mark_sent, outbox_parts, outbox_repack_candidates, outbox_row,
    repack_outbox, repack_required, set_repack_required, store_encrypted_parts,
    update_referenced_by_outbox, OutboxKey, RepackOutcome, RepackPart, RepackSpec,
    SyncEncryptedPart, SyncOutboxState, Vault, META_SYNC_REPACK_REQUIRED,
};

const OLD_MESSAGE: &str = "msg-old";
const OLD_EPOCH: u64 = 1;
const OLD_DIGEST: [u8; 32] = [9; 32];

fn seed_old_outbox(vault: &Vault) -> menghunote_storage::BodyDoc {
    let doc = seed_note(vault, "note-1", "doc-1");
    doc.insert_scalars(0, "body").expect("正文");
    commit_now(vault, &doc, "batch-1");
    enqueue_sync_intent(
        vault,
        OLD_MESSAGE,
        "batch-1",
        "doc-1",
        1,
        OLD_EPOCH,
        &[1; 32],
    )
    .expect("旧 intent");

    let parts = old_parts();
    store_encrypted_parts(vault, OLD_MESSAGE, OLD_EPOCH, &OLD_DIGEST, &parts).expect("旧 parts");
    doc
}

fn old_parts() -> Vec<SyncEncryptedPart> {
    vec![
        SyncEncryptedPart {
            part_no: 0,
            envelope: b"old-envelope-0".to_vec(),
            digest: [0xA1; 32],
        },
        SyncEncryptedPart {
            part_no: 1,
            envelope: b"old-envelope-1".to_vec(),
            digest: [0xA2; 32],
        },
    ]
}

fn old_key() -> OutboxKey {
    OutboxKey {
        message_id: OLD_MESSAGE.to_owned(),
        epoch: OLD_EPOCH,
        cipher_digest: OLD_DIGEST,
    }
}

fn new_spec(message_id: &str, epoch: u64, object: u8, digest: u8) -> RepackSpec {
    RepackSpec {
        message_id: message_id.to_owned(),
        object_id: [object; 32],
        epoch,
        parts: vec![
            RepackPart {
                part_no: 0,
                envelope: b"new-envelope-0".to_vec(),
                digest: [0xB1; 32],
            },
            RepackPart {
                part_no: 1,
                envelope: b"new-envelope-1".to_vec(),
                digest: [0xB2; 32],
            },
        ],
        cipher_digest: [digest; 32],
    }
}

#[test]
fn repack_is_atomic_preserves_update_and_rejects_stale_ack() {
    let (dir, vault) = fresh_vault();
    seed_old_outbox(&vault);
    assert!(mark_sent(&vault, OLD_MESSAGE, OLD_EPOCH, &OLD_DIGEST).expect("旧行已发送"));

    let outcome = repack_outbox(&vault, &old_key(), &new_spec("msg-new", 2, 2, 8)).expect("repack");
    assert_eq!(outcome, RepackOutcome::Repacked);

    assert!(outbox_row(&vault, OLD_MESSAGE).expect("旧行查询").is_none());
    assert!(outbox_parts(&vault, OLD_MESSAGE, OLD_EPOCH)
        .expect("旧 parts")
        .is_empty());

    let row = outbox_row(&vault, "msg-new")
        .expect("新行查询")
        .expect("新行存在");
    assert_eq!(row.batch_id, "batch-1");
    assert_eq!(row.doc_id, "doc-1");
    assert_eq!(row.seq, 1);
    assert_eq!(row.key_epoch, 2);
    assert_eq!(row.object_id, Some([2; 32]));
    assert_eq!(row.cipher_digest, Some([8; 32]));
    assert_eq!(row.state, SyncOutboxState::Encrypted);

    let parts = outbox_parts(&vault, "msg-new", 2).expect("新 parts");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].envelope, b"new-envelope-0");
    assert_eq!(parts[0].digest, [0xB1; 32]);
    assert_eq!(parts[1].part_no, 1);
    assert!(update_referenced_by_outbox(&vault, "doc-1", 1).expect("引用"));

    // 迟到旧 ACK：不能删除新行/新 parts。
    assert!(!mark_acked(&vault, OLD_MESSAGE, OLD_EPOCH, &OLD_DIGEST).expect("旧 ACK"));
    assert!(!mark_acked(&vault, OLD_MESSAGE, OLD_EPOCH, &[7; 32]).expect("错 digest ACK"));
    assert!(outbox_row(&vault, "msg-new").expect("新行").is_some());
    assert_eq!(
        outbox_parts(&vault, "msg-new", 2).expect("新 parts").len(),
        2
    );

    // 重开：新行与新 parts 仍在，旧行/旧 parts 不会复活。
    drop(vault);
    let vault = open_vault(&vault_path(&dir));
    assert!(outbox_row(&vault, OLD_MESSAGE).expect("旧行").is_none());
    assert!(outbox_parts(&vault, OLD_MESSAGE, OLD_EPOCH)
        .expect("旧 parts")
        .is_empty());
    assert_eq!(
        outbox_row(&vault, "msg-new")
            .expect("新行")
            .expect("存在")
            .key_epoch,
        2
    );
    assert_eq!(
        outbox_parts(&vault, "msg-new", 2).expect("新 parts").len(),
        2
    );
}

#[test]
fn mismatched_old_key_and_acked_row_change_nothing() {
    let (_dir, vault) = fresh_vault();
    seed_old_outbox(&vault);

    // message 不存在。
    let missing = OutboxKey {
        message_id: "msg-missing".to_owned(),
        epoch: OLD_EPOCH,
        cipher_digest: OLD_DIGEST,
    };
    assert_eq!(
        repack_outbox(&vault, &missing, &new_spec("msg-new", 2, 2, 8)).expect("missing"),
        RepackOutcome::NotFound
    );

    // epoch / digest 不匹配。
    let wrong_epoch = OutboxKey {
        epoch: 9,
        ..old_key()
    };
    assert_eq!(
        repack_outbox(&vault, &wrong_epoch, &new_spec("msg-new", 2, 2, 8)).expect("epoch"),
        RepackOutcome::Conflict
    );
    let wrong_digest = OutboxKey {
        cipher_digest: [0xEE; 32],
        ..old_key()
    };
    assert_eq!(
        repack_outbox(&vault, &wrong_digest, &new_spec("msg-new", 2, 2, 8)).expect("digest"),
        RepackOutcome::Conflict
    );

    // 旧行被标成 acked（异常残留）也必须拒绝。
    vault
        .exec(
            "UPDATE sync_outbox SET state='acked' WHERE message_id=?1",
            &[menghunote_storage::Value::Text(OLD_MESSAGE.to_owned())],
        )
        .expect("标 acked");
    assert_eq!(
        repack_outbox(&vault, &old_key(), &new_spec("msg-new", 2, 2, 8)).expect("acked"),
        RepackOutcome::Conflict
    );
    vault
        .exec(
            "UPDATE sync_outbox SET state='sent' WHERE message_id=?1",
            &[menghunote_storage::Value::Text(OLD_MESSAGE.to_owned())],
        )
        .expect("恢复 sent");

    // 新 parts 不连续：直接拒绝，旧行不受影响。
    let mut bad = new_spec("msg-new", 2, 2, 8);
    bad.parts[1].part_no = 9;
    assert!(repack_outbox(&vault, &old_key(), &bad).is_err());

    // 新 message 已存在（另一行）时冲突。
    enqueue_sync_intent(&vault, "msg-occupied", "batch-2", "doc-1", 1, 2, &[3; 32])
        .expect("占用新 id");
    assert_eq!(
        repack_outbox(&vault, &old_key(), &new_spec("msg-occupied", 2, 2, 8)).expect("occupied"),
        RepackOutcome::Conflict
    );

    let row = outbox_row(&vault, OLD_MESSAGE)
        .expect("旧行")
        .expect("旧行仍在");
    assert_eq!(row.key_epoch, OLD_EPOCH);
    assert_eq!(row.cipher_digest, Some(OLD_DIGEST));
    assert_eq!(row.state, SyncOutboxState::Sent);
    let parts = outbox_parts(&vault, OLD_MESSAGE, OLD_EPOCH).expect("旧 parts");
    assert_eq!(parts.len(), 2);
    assert!(outbox_row(&vault, "msg-new").expect("新行").is_none());
}

#[test]
fn candidates_list_pending_and_sent_with_parts_summary() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "body").expect("正文");
    commit_now(&vault, &doc, "batch-1");

    // pending（未加密）旧 epoch。
    enqueue_sync_intent(&vault, "msg-pending", "batch-1", "doc-1", 1, 1, &[1; 32])
        .expect("pending");
    // encrypted 旧 epoch：parts 已 durable、尚未发送，也必须一起 repack。
    enqueue_sync_intent(&vault, "msg-encrypted", "batch-1", "doc-1", 1, 1, &[2; 32])
        .expect("encrypted");
    store_encrypted_parts(&vault, "msg-encrypted", 1, &[2; 32], &old_parts())
        .expect("encrypted parts");
    // sent 旧 epoch：包含 parts 摘要。
    enqueue_sync_intent(&vault, "msg-sent", "batch-1", "doc-1", 1, 1, &[3; 32]).expect("sent");
    store_encrypted_parts(&vault, "msg-sent", 1, &[3; 32], &old_parts()).expect("sent parts");
    assert!(mark_sent(&vault, "msg-sent", 1, &[3; 32]).expect("mark sent"));
    // 当前 epoch 的 pending 不列出。
    enqueue_sync_intent(&vault, "msg-current", "batch-1", "doc-1", 1, 2, &[4; 32])
        .expect("current");

    let candidates = outbox_repack_candidates(&vault, 2).expect("candidates");
    let ids = candidates
        .iter()
        .map(|candidate| candidate.message_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["msg-encrypted", "msg-pending", "msg-sent"]);

    let encrypted = &candidates[0];
    assert_eq!(encrypted.state, SyncOutboxState::Encrypted);
    assert_eq!(encrypted.key_epoch, 1);
    assert_eq!(encrypted.parts.len(), 2);

    let pending = &candidates[1];
    assert_eq!(pending.state, SyncOutboxState::Pending);
    assert_eq!(pending.key_epoch, 1);
    assert!(pending.parts.is_empty());

    let sent = &candidates[2];
    assert_eq!(sent.state, SyncOutboxState::Sent);
    assert_eq!(sent.batch_id, "batch-1");
    assert_eq!(sent.doc_id, "doc-1");
    assert_eq!(sent.seq, 1);
    assert_eq!(sent.object_id, Some([3; 32]));
    assert_eq!(sent.cipher_digest, Some([3; 32]));
    assert_eq!(sent.parts.len(), 2);
    assert_eq!(sent.parts[0].part_no, 0);
    assert_eq!(sent.parts[0].envelope_len, b"old-envelope-0".len());
    assert_eq!(sent.parts[0].digest, [0xA1; 32]);
}

#[test]
fn injected_failure_rolls_back_and_reopen_recovers() {
    let (dir, vault) = fresh_vault();
    seed_old_outbox(&vault);

    vault
        .exec_batch(
            "CREATE TRIGGER fail_repack BEFORE INSERT ON sync_outbox \
             WHEN NEW.message_id='msg-new' BEGIN SELECT RAISE(ABORT,'injected'); END;",
        )
        .expect("注入触发器");

    let error = repack_outbox(&vault, &old_key(), &new_spec("msg-new", 2, 2, 8));
    assert!(error.is_err(), "注入失败必须返回错误而不是半成品");

    let row = outbox_row(&vault, OLD_MESSAGE)
        .expect("旧行")
        .expect("旧行仍在");
    assert_eq!(row.key_epoch, OLD_EPOCH);
    assert_eq!(row.cipher_digest, Some(OLD_DIGEST));
    assert_eq!(
        outbox_parts(&vault, OLD_MESSAGE, OLD_EPOCH)
            .expect("旧 parts")
            .len(),
        2
    );
    assert!(outbox_row(&vault, "msg-new").expect("新行").is_none());
    assert!(outbox_parts(&vault, "msg-new", 2)
        .expect("新 parts")
        .is_empty());

    // 撤销注入后同一 repack 成功；重开仍保持新行。
    vault
        .exec_batch("DROP TRIGGER fail_repack")
        .expect("撤销注入");
    assert_eq!(
        repack_outbox(&vault, &old_key(), &new_spec("msg-new", 2, 2, 8)).expect("恢复 repack"),
        RepackOutcome::Repacked
    );
    drop(vault);

    let vault = open_vault(&vault_path(&dir));
    assert!(outbox_row(&vault, OLD_MESSAGE).expect("旧行").is_none());
    assert!(outbox_row(&vault, "msg-new").expect("新行").is_some());
    assert_eq!(
        outbox_parts(&vault, "msg-new", 2).expect("新 parts").len(),
        2
    );
}

#[test]
fn repack_fence_uses_sync_meta_key() {
    let (dir, vault) = fresh_vault();
    assert!(!repack_required(&vault).expect("初始 false"));

    // 与 sync 阶段 H 的写入方式保持一致：直接写同一个 meta 键。
    vault
        .set_meta(META_SYNC_REPACK_REQUIRED, b"1")
        .expect("sync 写 fence");
    assert!(repack_required(&vault).expect("sync fence 为 true"));

    set_repack_required(&vault, false).expect("清除");
    assert!(!repack_required(&vault).expect("清除后 false"));
    set_repack_required(&vault, true).expect("设置");
    assert!(repack_required(&vault).expect("设置后 true"));
    drop(vault);

    let vault = open_vault(&vault_path(&dir));
    assert!(repack_required(&vault).expect("重开后 true"));
    set_repack_required(&vault, false).expect("重开后清除");
    assert!(!repack_required(&vault).expect("最终 false"));
}

// ============================================================ 评审 0002 T12 补充
// multi-doc 的 repack 必须逐条 CAS、互不污染；任一 SQL 点失败都回滚；
// 原 op bytes/update_id 保留，迟到旧 ACK 不得清新 row/parts。

fn update_rows(vault: &Vault, doc_id: &str) -> Vec<(u64, String, Vec<u8>)> {
    menghunote_storage::doc_updates_since(vault, doc_id, 0)
        .expect("doc_updates_since")
        .into_iter()
        .map(|update| (update.seq, update.update_id, update.payload))
        .collect()
}

#[test]
fn multi_doc_repack_is_per_doc_and_late_old_ack_never_clears_new_row() {
    let (dir, vault) = fresh_vault();

    // doc-1：已加密且 sent；doc-2：已加密但未发送（一覆盖一未覆盖的 outbox 形态）。
    let doc1 = seed_note(&vault, "note-1", "doc-1");
    doc1.insert_scalars(0, "doc one").expect("doc1 正文");
    commit_now(&vault, &doc1, "batch-1");
    enqueue_sync_intent(&vault, "msg-1", "batch-1", "doc-1", 1, OLD_EPOCH, &[1; 32])
        .expect("doc1 intent");
    store_encrypted_parts(&vault, "msg-1", OLD_EPOCH, &OLD_DIGEST, &old_parts())
        .expect("doc1 parts");
    assert!(mark_sent(&vault, "msg-1", OLD_EPOCH, &OLD_DIGEST).expect("doc1 sent"));

    let doc2 = seed_note(&vault, "note-2", "doc-2");
    doc2.insert_scalars(0, "doc two").expect("doc2 正文");
    commit_now(&vault, &doc2, "batch-2");
    enqueue_sync_intent(&vault, "msg-2", "batch-2", "doc-2", 1, OLD_EPOCH, &[2; 32])
        .expect("doc2 intent");
    store_encrypted_parts(&vault, "msg-2", OLD_EPOCH, &[2; 32], &old_parts()).expect("doc2 parts");

    let rows1_before = update_rows(&vault, "doc-1");
    let rows2_before = update_rows(&vault, "doc-2");
    let candidates = outbox_repack_candidates(&vault, 2).expect("candidates");
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.message_id.as_str())
            .collect::<Vec<_>>(),
        vec!["msg-1", "msg-2"],
        "两条旧 epoch outbox 都必须进入候选"
    );

    // 只 repack doc-1：doc-2 的旧 row/parts/update 必须原样保留（不能整批假装原子）。
    assert_eq!(
        repack_outbox(
            &vault,
            &OutboxKey {
                message_id: "msg-1".to_owned(),
                epoch: OLD_EPOCH,
                cipher_digest: OLD_DIGEST,
            },
            &new_spec("msg-1-new", 2, 0x21, 0x31),
        )
        .expect("doc1 repack"),
        RepackOutcome::Repacked
    );
    assert!(outbox_row(&vault, "msg-1").expect("old doc1").is_none());
    let old2 = outbox_row(&vault, "msg-2")
        .expect("doc2 row")
        .expect("doc2 旧行仍在");
    assert_eq!(old2.key_epoch, OLD_EPOCH);
    assert_eq!(old2.cipher_digest, Some([2; 32]));
    assert_eq!(
        outbox_parts(&vault, "msg-2", OLD_EPOCH)
            .expect("doc2 parts")
            .len(),
        2
    );
    assert_eq!(
        update_rows(&vault, "doc-1"),
        rows1_before,
        "doc1 原 op 不变"
    );
    assert_eq!(
        update_rows(&vault, "doc-2"),
        rows2_before,
        "doc2 原 op 不变"
    );

    // 再 repack doc-2；随后迟到的旧 ACK 对两个旧三元组都必须 false，且新 row/parts 仍在。
    assert_eq!(
        repack_outbox(
            &vault,
            &OutboxKey {
                message_id: "msg-2".to_owned(),
                epoch: OLD_EPOCH,
                cipher_digest: [2; 32],
            },
            &new_spec("msg-2-new", 2, 0x22, 0x32),
        )
        .expect("doc2 repack"),
        RepackOutcome::Repacked
    );
    assert!(!mark_acked(&vault, "msg-1", OLD_EPOCH, &OLD_DIGEST).expect("旧 ACK 1"));
    assert!(!mark_acked(&vault, "msg-2", OLD_EPOCH, &[2; 32]).expect("旧 ACK 2"));
    for (message, object) in [("msg-1-new", 0x21u8), ("msg-2-new", 0x22u8)] {
        let row = outbox_row(&vault, message).expect("新行").expect("存在");
        assert_eq!(row.key_epoch, 2);
        assert_eq!(row.object_id, Some([object; 32]));
        assert_eq!(
            outbox_parts(&vault, message, 2).expect("新 parts").len(),
            2,
            "旧 ACK 不得清新 row/parts"
        );
    }
    assert_eq!(update_rows(&vault, "doc-1"), rows1_before);
    assert_eq!(update_rows(&vault, "doc-2"), rows2_before);

    // 重开：两条新行都在，旧行不会复活。
    drop(vault);
    let vault = open_vault(&vault_path(&dir));
    assert!(outbox_row(&vault, "msg-1").expect("旧1").is_none());
    assert!(outbox_row(&vault, "msg-2").expect("旧2").is_none());
    assert!(outbox_row(&vault, "msg-1-new").expect("新1").is_some());
    assert!(outbox_row(&vault, "msg-2-new").expect("新2").is_some());
}

#[test]
fn repack_rolls_back_at_every_sql_statement_and_retries_cleanly() {
    let (dir, vault) = fresh_vault();
    seed_old_outbox(&vault);
    assert!(mark_sent(&vault, OLD_MESSAGE, OLD_EPOCH, &OLD_DIGEST).expect("marked sent"));
    let rows_before = update_rows(&vault, "doc-1");

    // repack 事务的 4 类 SQL 点各失败一次：delete parts / delete row / insert row / insert parts。
    let points = [
        (
            "fail_delete_old_parts",
            "CREATE TRIGGER fail_delete_old_parts BEFORE DELETE ON sync_outbox_parts \
             BEGIN SELECT RAISE(ABORT,'injected delete parts'); END;",
        ),
        (
            "fail_delete_old_row",
            "CREATE TRIGGER fail_delete_old_row BEFORE DELETE ON sync_outbox \
             BEGIN SELECT RAISE(ABORT,'injected delete row'); END;",
        ),
        (
            "fail_insert_new_row",
            "CREATE TRIGGER fail_insert_new_row BEFORE INSERT ON sync_outbox \
             BEGIN SELECT RAISE(ABORT,'injected insert row'); END;",
        ),
        (
            "fail_insert_new_part",
            "CREATE TRIGGER fail_insert_new_part BEFORE INSERT ON sync_outbox_parts \
             BEGIN SELECT RAISE(ABORT,'injected insert part'); END;",
        ),
    ];
    for (name, sql) in points {
        vault.exec_batch(sql).expect("注入触发器");
        let error = repack_outbox(&vault, &old_key(), &new_spec("msg-new", 2, 2, 8));
        assert!(error.is_err(), "{name} 必须让 repack 失败");
        let row = outbox_row(&vault, OLD_MESSAGE)
            .expect("旧行查询")
            .expect("旧行必须保留");
        assert_eq!(row.key_epoch, OLD_EPOCH, "{name}");
        assert_eq!(row.cipher_digest, Some(OLD_DIGEST), "{name}");
        assert_eq!(row.state, SyncOutboxState::Sent, "{name}");
        assert_eq!(
            outbox_parts(&vault, OLD_MESSAGE, OLD_EPOCH)
                .expect("旧 parts")
                .len(),
            2,
            "{name}: 旧 parts 必须保留"
        );
        assert!(
            outbox_row(&vault, "msg-new").expect("新行").is_none(),
            "{name}: 不得留半条新行"
        );
        assert!(
            outbox_parts(&vault, "msg-new", 2)
                .expect("新 parts")
                .is_empty(),
            "{name}: 不得留半套新 parts"
        );
        assert_eq!(
            update_rows(&vault, "doc-1"),
            rows_before,
            "{name}: 原 op 不变"
        );
        vault
            .exec_batch(&format!("DROP TRIGGER {name}"))
            .expect("撤销触发器");
    }

    // 撤销注入后重试同一 repack；重开仍保持新行/新 parts。
    assert_eq!(
        repack_outbox(&vault, &old_key(), &new_spec("msg-new", 2, 2, 8)).expect("恢复 repack"),
        RepackOutcome::Repacked
    );
    drop(vault);
    let vault = open_vault(&vault_path(&dir));
    assert!(outbox_row(&vault, OLD_MESSAGE).expect("旧行").is_none());
    assert_eq!(
        outbox_parts(&vault, "msg-new", 2).expect("新 parts").len(),
        2
    );
    assert_eq!(update_rows(&vault, "doc-1"), rows_before);
}
