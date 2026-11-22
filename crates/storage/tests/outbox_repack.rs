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
