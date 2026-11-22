//! 评审 0002 §五 T12 剩余：pending/encrypted/sent 三状态同轮、跨 epoch 多 row、
//! repack 中途失败（逐条事务）后重试；原 update_id/Loro bytes/origin 不丢，
//! 迟到旧 ACK 不得清新 row/parts。全部用真实 SQLite/Loro + 真 MNE1 加解密。

mod support;

use support::*;

use menghunote_storage::{
    doc_updates_since, mark_acked, mark_sent, outbox_parts, outbox_repack_candidates, outbox_row,
    store_encrypted_parts, SyncEncryptedPart, SyncOutboxRow, SyncOutboxState,
};
use menghunote_sync::engine::{EngineIdentity, SyncEngine};
use menghunote_sync::keys::RootSecretV1;
use menghunote_sync::rotation_repack::{repack_outbox_after_rotation, RepackReport};
use menghunote_sync::upload::{decrypt_envelope, encrypt_local_parts, EncryptedLocalParts};
use menghunote_sync::ObjectId;
use menghunote_sync_proto::{ObjectType, UpdateV1};

fn engine<'a>(
    vault: &'a menghunote_storage::Vault,
    relay: &'a mut FakeRelay,
    device: &Device,
) -> SyncEngine<'a, FakeRelay> {
    SyncEngine::new(
        vault,
        relay,
        EngineIdentity {
            device_id: device.device_id,
            signing_key: device.identity.signing_key.clone(),
            peer_id: device.identity.peer_id,
        },
        device.root.clone(),
    )
}

fn rel_row(vault: &menghunote_storage::Vault, message: &str) -> SyncOutboxRow {
    outbox_row(vault, message)
        .expect("outbox row")
        .expect("row")
}

fn encrypt_under_epoch(
    root: &RootSecretV1,
    object_id: ObjectId,
    device_id: [u8; 16],
    seq: u64,
    plaintext: &[u8],
) -> (EncryptedLocalParts, [u8; 32]) {
    encrypt_local_parts(
        root,
        ObjectType::Update,
        &object_id,
        [0x42; 16],
        device_id,
        1,
        seq,
        plaintext,
        &[],
    )
    .expect("encrypt")
}

/// 造三类 outbox：doc-1=sent、doc-2=encrypted、doc-3=pending（同 epoch）。
fn seed_all_states(vault: &menghunote_storage::Vault, device: &Device) -> Vec<(String, String)> {
    let d1 = seed_doc(vault, "doc-1", device.identity.peer_id);
    d1.insert_scalars(0, "sent body").expect("d1");
    let d2 = seed_doc(vault, "doc-2", device.identity.peer_id);
    d2.insert_scalars(0, "encrypted body").expect("d2");

    let m1 = uuid_like(0x91);
    let m2 = uuid_like(0x92);
    let o1: [u8; 32] = [0x61; 32];
    let o2: [u8; 32] = [0x62; 32];
    commit_with_intent(vault, &d1, &m1, o1, EPOCH);
    commit_with_intent(vault, &d2, &m2, o2, EPOCH);
    {
        let mut relay = FakeRelay::new(&[device]);
        let mut sync = engine(vault, &mut relay, device);
        assert_eq!(sync.encrypt_pending(10).expect("encrypt"), 2);
    }
    // doc-1 已发送；doc-2 保持 encrypted；doc-3 在加密轮之后才入队 → pending。
    let sent = rel_row(vault, &m1);
    assert!(mark_sent(
        vault,
        &m1,
        sent.key_epoch,
        &sent.cipher_digest.expect("digest")
    )
    .expect("mark sent"));

    let d3 = seed_doc(vault, "doc-3", device.identity.peer_id);
    d3.insert_scalars(0, "pending body").expect("d3");
    let m3 = uuid_like(0x93);
    let o3: [u8; 32] = [0x63; 32];
    commit_with_intent(vault, &d3, &m3, o3, EPOCH);

    assert_eq!(rel_row(vault, &m1).state, SyncOutboxState::Sent);
    assert_eq!(rel_row(vault, &m2).state, SyncOutboxState::Encrypted);
    assert_eq!(rel_row(vault, &m3).state, SyncOutboxState::Pending);
    vec![
        (m1, "doc-1".to_owned()),
        (m2, "doc-2".to_owned()),
        (m3, "doc-3".to_owned()),
    ]
}

fn expected_updates(
    vault: &menghunote_storage::Vault,
    docs: &[(String, String)],
) -> Vec<(String, u64, String, Vec<u8>)> {
    let mut out = Vec::new();
    for (_, doc_id) in docs {
        for update in doc_updates_since(vault, doc_id, 0).expect("updates") {
            out.push((doc_id.clone(), update.seq, update.update_id, update.payload));
        }
    }
    out
}

fn outbox_count_epoch(vault: &menghunote_storage::Vault, epoch: u64) -> i64 {
    let rows = vault
        .query(
            "SELECT COUNT(*) FROM sync_outbox WHERE key_epoch=?1",
            &[menghunote_storage::Value::Integer(
                i64::try_from(epoch).expect("epoch"),
            )],
        )
        .expect("outbox count");
    match rows[0][0] {
        menghunote_storage::Value::Integer(value) => value,
        _ => panic!("COUNT 不是 integer"),
    }
}

fn outbox_docs_epoch(vault: &menghunote_storage::Vault, epoch: u64) -> Vec<String> {
    let rows = vault
        .query(
            "SELECT doc_id FROM sync_outbox WHERE key_epoch=?1 ORDER BY doc_id",
            &[menghunote_storage::Value::Integer(
                i64::try_from(epoch).expect("epoch"),
            )],
        )
        .expect("outbox docs");
    rows.iter()
        .map(|row| match &row[0] {
            menghunote_storage::Value::Text(text) => text.clone(),
            _ => panic!("doc_id 不是 text"),
        })
        .collect()
}

fn assert_new_updates_preserve_origin(
    vault: &menghunote_storage::Vault,
    old_root: &RootSecretV1,
    new_root: &RootSecretV1,
    device_id: [u8; 16],
    expected: &[(String, u64, String, Vec<u8>)],
    report: &RepackReport,
) {
    assert_eq!(report.failed, 0, "三状态都必须成功: {report:?}");
    assert_eq!(report.repacked, expected.len());
    assert_eq!(
        outbox_count_epoch(vault, new_root.key_epoch),
        i64::try_from(expected.len()).expect("len")
    );
    for message in &report.new_message_ids {
        let row = outbox_row(vault, message)
            .expect("new row")
            .expect("new row");
        assert_eq!(row.key_epoch, new_root.key_epoch);
        assert_eq!(row.state, SyncOutboxState::Encrypted);
        let parts = outbox_parts(vault, message, row.key_epoch).expect("parts");
        let object_id = row.object_id.expect("object id");
        let (_, plaintext) = decrypt_envelope(
            new_root,
            ObjectType::Update,
            &object_id,
            &parts[0].envelope,
            &[],
        )
        .expect("新 epoch 解密");
        let update = UpdateV1::decode(&plaintext).expect("UpdateV1");
        let (_, seq, update_id, payload) = expected
            .iter()
            .find(|(doc_id, seq, _, _)| doc_id == &row.doc_id && *seq == row.seq)
            .expect("原 update");
        assert_eq!(&update.update_id, update_id, "原 update_id 必须保留");
        assert_eq!(update.origin_device_id, device_id, "origin 设备必须保留");
        assert_eq!(update.origin_seq, *seq);
        assert_eq!(
            &update.loro_update_bytes, payload,
            "原 Loro bytes 必须逐字节保留"
        );
        assert_eq!(update.doc_id, row.doc_id);
        assert_eq!(update.batch_id, row.batch_id);
        // 旧根不得解开新密文。
        assert!(decrypt_envelope(
            old_root,
            ObjectType::Update,
            &object_id,
            &parts[0].envelope,
            &[]
        )
        .is_err());
    }
}

#[test]
fn repack_all_states_preserves_original_updates_and_rejects_old_ack() {
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x52);
    let docs = seed_all_states(&vault, &device);
    let expected = expected_updates(&vault, &docs);
    let old = outbox_repack_candidates(&vault, EPOCH + 1).expect("old candidates");
    let old_keys: Vec<(String, u64, Option<[u8; 32]>)> = old
        .iter()
        .map(|candidate| {
            (
                candidate.message_id.clone(),
                candidate.key_epoch,
                candidate.cipher_digest,
            )
        })
        .collect();
    let new_root = RootSecretV1::new(
        VAULT_ID,
        EPOCH + 1,
        menghunote_sync::keys::Secret32::from_array([0x74; 32]),
    );
    let report = repack_outbox_after_rotation(&vault, &device.root, &new_root, device.device_id)
        .expect("repack");
    assert_eq!(report.repacked, 3);
    assert_new_updates_preserve_origin(
        &vault,
        &device.root,
        &new_root,
        device.device_id,
        &expected,
        &report,
    );

    // 迟到旧 ACK 三态全部 false，新 row/parts 不被清。
    for (message, epoch, digest) in &old_keys {
        assert!(
            !mark_acked(&vault, message, *epoch, &digest.unwrap_or([0u8; 32])).expect("old ack")
        );
    }
    assert_eq!(outbox_count_epoch(&vault, EPOCH), 0, "旧 epoch 不得残留");
}

#[test]
fn mid_repack_failure_is_per_row_atomic_and_retry_finishes() {
    let (dir, vault) = fresh_vault();
    let device = Device::new(0x53);
    let docs = seed_all_states(&vault, &device);
    let expected = expected_updates(&vault, &docs);
    let new_root = RootSecretV1::new(
        VAULT_ID,
        EPOCH + 1,
        menghunote_sync::keys::Secret32::from_array([0x75; 32]),
    );

    // 第二条（doc-2）写新 row 时失败：第一条已提交，doc-2/doc-3 旧行原样保留。
    vault
        .exec_batch(
            r#"CREATE TRIGGER fail_repack_doc2 BEFORE INSERT ON sync_outbox WHEN NEW.key_epoch = 2 AND NEW.doc_id = 'doc-2' BEGIN SELECT RAISE(ABORT,'injected mid-repack'); END;"#,
        )
        .expect("注入触发器");
    let error = repack_outbox_after_rotation(&vault, &device.root, &new_root, device.device_id)
        .expect_err("中途失败必须返回错误");
    assert!(!format!("{error:?}").is_empty());
    let remaining = outbox_docs_epoch(&vault, EPOCH);
    assert!(
        remaining.contains(&"doc-2".to_owned()),
        "失败行必须保留旧 epoch row"
    );
    assert!(
        remaining.contains(&"doc-3".to_owned()),
        "后续行不得被半提交"
    );
    assert_eq!(remaining.len(), 2);
    assert_eq!(
        outbox_count_epoch(&vault, EPOCH + 1),
        1,
        "只允许第一条完成 repack"
    );
    // 原 op bytes/update_id 不受影响。
    assert_eq!(expected_updates(&vault, &docs), expected);

    // 撤销注入后重跑：剩余两条完成，原 update 仍逐字节保留。
    vault
        .exec_batch("DROP TRIGGER fail_repack_doc2")
        .expect("撤销注入");
    let report = repack_outbox_after_rotation(&vault, &device.root, &new_root, device.device_id)
        .expect("retry repack");
    assert_eq!(report.repacked, 2, "{report:?}");
    assert_eq!(report.failed, 0);
    assert_eq!(outbox_count_epoch(&vault, EPOCH), 0);
    assert_eq!(outbox_count_epoch(&vault, EPOCH + 1), 3);
    assert_eq!(expected_updates(&vault, &docs), expected, "重试不得改原 op");

    // 重开仍稳定（不依赖内存状态）。
    drop(vault);
    let vault = menghunote_storage::Vault::open(dir.path().join("vaults").join("primary.sqlite3"))
        .expect("reopen");
    assert_eq!(outbox_count_epoch(&vault, EPOCH), 0);
    assert_eq!(outbox_count_epoch(&vault, EPOCH + 1), 3);
}

#[test]
fn cross_epoch_rows_are_all_repacked_not_left_behind() {
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x54);
    // epoch 1 的 encrypted row（本设备当前根）。
    let d1 = seed_doc(&vault, "doc-1", device.identity.peer_id);
    d1.insert_scalars(0, "epoch one").expect("d1");
    let m1 = uuid_like(0xA1);
    commit_with_intent(&vault, &d1, &m1, [0x71; 32], EPOCH);
    {
        let mut relay = FakeRelay::new(&[&device]);
        let mut sync = engine(&vault, &mut relay, &device);
        assert_eq!(sync.encrypt_pending(10).expect("encrypt"), 1);
    }
    // epoch 2 的 encrypted row（更早一次轮换遗留；跨 epoch 的第二份旧根）。
    let old_root2 = RootSecretV1::new(
        VAULT_ID,
        2,
        menghunote_sync::keys::Secret32::from_array([0x72; 32]),
    );
    let d2 = seed_doc(&vault, "doc-2", device.identity.peer_id);
    d2.insert_scalars(0, "epoch two").expect("d2");
    let m2 = uuid_like(0xA2);
    let (parts, digest) = encrypt_under_epoch(
        &old_root2,
        [0x72; 32],
        device.device_id,
        2,
        b"update-v1-epoch2",
    );
    // 先把 outbox 行入队到 epoch2，再写入与行一致的密文 parts。
    commit_with_intent(&vault, &d2, &m2, [0x72; 32], 2);
    let parts: Vec<SyncEncryptedPart> = parts
        .into_iter()
        .map(|(part_no, envelope, part_digest)| SyncEncryptedPart {
            part_no,
            envelope,
            digest: part_digest,
        })
        .collect();
    store_encrypted_parts(&vault, &m2, 2, &digest, &parts).expect("epoch2 parts");

    let old = outbox_repack_candidates(&vault, EPOCH + 2).expect("old candidates");
    assert_eq!(old.len(), 2, "必须同时存在 epoch1 与 epoch2 的旧 row");

    let new_root = RootSecretV1::new(
        VAULT_ID,
        EPOCH + 2,
        menghunote_sync::keys::Secret32::from_array([0x76; 32]),
    );
    // 旧根按 epoch 提供：epoch1 是本设备根，epoch2 是更早轮换遗留的根。
    let old_roots = [device.root.clone(), old_root2.clone()];
    let report = repack_outbox_after_rotation(&vault, &old_roots, &new_root, device.device_id)
        .expect("repack");
    assert_eq!(
        report.repacked, 2,
        "跨 epoch 的两条旧 row 都必须重包（不能只处理一个 old_root）"
    );
    assert_eq!(report.failed, 0, "{report:?}");
    assert_eq!(outbox_count_epoch(&vault, EPOCH + 2), 2);
    assert_eq!(outbox_count_epoch(&vault, EPOCH), 0, "epoch1 旧行不得残留");
    assert_eq!(outbox_count_epoch(&vault, 2), 0, "epoch2 旧行不得残留");
}

/// 缺根必须 fail-closed：整轮拒绝、旧行原样保留，不得跳过或删除。
#[test]
fn missing_old_epoch_root_fails_closed_without_touching_rows() {
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x55);
    let docs = seed_all_states(&vault, &device);
    let expected = expected_updates(&vault, &docs);
    let new_root = RootSecretV1::new(
        VAULT_ID,
        EPOCH + 1,
        menghunote_sync::keys::Secret32::from_array([0x77; 32]),
    );
    let no_roots: &[RootSecretV1] = &[];
    let error = repack_outbox_after_rotation(&vault, no_roots, &new_root, device.device_id)
        .expect_err("缺旧根必须 fail-closed");
    assert!(
        format!("{error:?}").contains("缺少对应旧根"),
        "错误必须明确指出缺根: {error:?}"
    );
    assert_eq!(outbox_count_epoch(&vault, EPOCH), 3, "旧行不得被跳过或删除");
    assert_eq!(outbox_count_epoch(&vault, EPOCH + 1), 0, "不得产生半轮重包");
    assert_eq!(
        expected_updates(&vault, &docs),
        expected,
        "原 op 不得被改动"
    );
}
