//! P2 起点：本地 outbox MNE1 part 解密必须只认旧根/旧 epoch。

mod support;

use support::*;

use sha2::Digest;

use menghunote_storage::{mark_acked, outbox_repack_candidates, outbox_row, SyncOutboxPart, Vault};
use menghunote_sync::engine::{EngineIdentity, SyncEngine};
use menghunote_sync::keys::RootSecretV1;
use menghunote_sync::relay::RelayClient;
use menghunote_sync::rotation_repack::{reencrypt_old_parts, repack_outbox_after_rotation};
use menghunote_sync::upload::{decrypt_envelope, upload_object};
use menghunote_sync::ObjectId;
use menghunote_sync_proto::ObjectType;

fn engine<'a>(
    vault: &'a Vault,
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

#[test]
fn repack_real_vault_handles_sent_and_encrypted_and_ignores_old_ack() {
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x52);
    let mut relay = FakeRelay::new(&[&device]);
    let d1 = seed_doc(&vault, "doc-1", device.identity.peer_id);
    d1.insert_scalars(0, "first").expect("write 1");
    let d2 = seed_doc(&vault, "doc-2", device.identity.peer_id);
    d2.insert_scalars(0, "second").expect("write 2");
    let m1 = uuid_like(0x81);
    let m2 = uuid_like(0x82);
    let o1: [u8; 32] = [0x71; 32];
    let o2: [u8; 32] = [0x72; 32];
    commit_with_intent(&vault, &d1, &m1, o1, EPOCH);
    commit_with_intent(&vault, &d2, &m2, o2, EPOCH);

    {
        let mut sync = engine(&vault, &mut relay, &device);
        assert_eq!(sync.encrypt_pending(10).expect("encrypt"), 2);
    }
    // 推送失败一条 → sent；另一条成功被 ACK 删除。
    relay.fail_submit.insert(o1, "EPOCH");
    {
        let mut sync = engine(&vault, &mut relay, &device);
        let _ = sync.push().expect("push");
    }
    relay.fail_submit.clear();
    // 再造一条 encrypted（不推送）。
    let d3 = seed_doc(&vault, "doc-3", device.identity.peer_id);
    d3.insert_scalars(0, "third").expect("write 3");
    let m3 = uuid_like(0x83);
    let o3: [u8; 32] = [0x73; 32];
    commit_with_intent(&vault, &d3, &m3, o3, EPOCH);
    {
        let mut sync = engine(&vault, &mut relay, &device);
        assert_eq!(sync.encrypt_pending(10).expect("encrypt d3"), 1);
    }

    let candidates = outbox_repack_candidates(&vault, EPOCH + 1).expect("candidates");
    let old_keys: Vec<(String, u64, [u8; 32])> = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.message_id.clone(),
                candidate.key_epoch,
                candidate.cipher_digest.expect("digest"),
            )
        })
        .collect();
    assert_eq!(old_keys.len(), 2, "sent + encrypted 各一条");

    let new_root = RootSecretV1::new(
        VAULT_ID,
        EPOCH + 1,
        menghunote_sync::keys::Secret32::from_array([0x74; 32]),
    );
    let report = repack_outbox_after_rotation(&vault, &device.root, &new_root, device.device_id)
        .expect("repack");
    assert_eq!(report.repacked, 2, "sent + encrypted 都必须重包");
    assert_eq!(report.failed, 0);
    assert_eq!(report.new_message_ids.len(), 2);
    let meta = vault
        .get_meta(menghunote_sync::rotation_repack::REPACK_REPORT_META)
        .expect("report meta")
        .expect("有诊断 meta");
    let text = String::from_utf8_lossy(&meta);
    assert!(text.contains("repacked=2"), "meta: {text}");
    assert!(text.contains("failed=0"), "meta: {text}");

    let new_candidates = outbox_repack_candidates(&vault, EPOCH).expect("new candidates");
    assert_eq!(new_candidates.len(), 2);
    for candidate in &new_candidates {
        assert_eq!(candidate.key_epoch, EPOCH + 1);
        let parts =
            menghunote_storage::outbox_parts(&vault, &candidate.message_id, candidate.key_epoch)
                .expect("new parts");
        assert!(!parts.is_empty());
        let object_id = candidate.object_id.expect("new object id");
        let (_, plaintext) = decrypt_envelope(
            &new_root,
            ObjectType::Update,
            &object_id,
            &parts[0].envelope,
            &[],
        )
        .expect("新 epoch 可解");
        assert!(!plaintext.is_empty());
        assert!(
            decrypt_envelope(
                &device.root,
                ObjectType::Update,
                &object_id,
                &parts[0].envelope,
                &[]
            )
            .is_err(),
            "旧 epoch 解不开新 parts"
        );
    }
    // 旧 ACK 三元组 CAS 必须 false，且新行仍在。
    for (message, epoch, digest) in &old_keys {
        assert!(!mark_acked(&vault, message, *epoch, digest).expect("旧 ACK 不应报错"));
    }
    for message in &report.new_message_ids {
        assert!(outbox_row(&vault, message).expect("row").is_some());
    }
}

#[test]
fn local_old_epoch_part_decrypts_with_old_root_and_fails_with_new_root() {
    let device = Device::new(0x51);
    let mut relay = FakeRelay::new(&[&device]);
    let object_id: ObjectId = [0x61; 32];
    let plaintext = b"update-payload-preserved";
    upload_object(
        &mut relay,
        VAULT_ID,
        device.device_id,
        &device.identity.signing_key,
        &device.root,
        ObjectType::Update,
        object_id,
        [0x62; 16],
        1,
        1,
        plaintext,
        &[],
        None,
    )
    .expect("upload");
    let envelope = relay.part(&object_id, 0).expect("part");

    let (header, decrypted) =
        decrypt_envelope(&device.root, ObjectType::Update, &object_id, &envelope, &[])
            .expect("old root decrypt");
    assert_eq!(&decrypted[..], plaintext);
    assert_eq!(header.key_epoch, EPOCH);

    let new_root = RootSecretV1::new(
        VAULT_ID,
        EPOCH + 1,
        menghunote_sync::keys::Secret32::from_array([0x63; 32]),
    );
    assert!(
        decrypt_envelope(&new_root, ObjectType::Update, &object_id, &envelope, &[]).is_err(),
        "新 epoch 根不得解开旧 part"
    );

    // P2：旧 part 解出的同一明文必须能用新 message/object/epoch 重加密，且旧根解不开新 part。
    let old_part = SyncOutboxPart {
        message_id: "00000000000000000000000000000001".to_owned(),
        epoch: EPOCH,
        part_no: 0,
        digest: sha2::Sha256::digest(&envelope).into(),
        envelope: envelope.clone(),
    };
    let (new_object_id, new_message_id, new_parts, cipher_digest) = reencrypt_old_parts(
        &device.root,
        &new_root,
        &object_id,
        &[old_part],
        device.device_id,
    )
    .expect("reencrypt");
    assert_ne!(new_object_id, object_id);
    assert!(!new_message_id.is_empty());
    assert_eq!(new_parts.len(), 1);
    assert_eq!(
        new_parts[0].digest,
        <[u8; 32]>::from(sha2::Sha256::digest(&new_parts[0].envelope))
    );
    let (header, decoded) = decrypt_envelope(
        &new_root,
        ObjectType::Update,
        &new_object_id,
        &new_parts[0].envelope,
        &[],
    )
    .expect("new root decrypt");
    assert_eq!(&decoded[..], plaintext);
    assert_eq!(header.key_epoch, EPOCH + 1);
    assert!(
        decrypt_envelope(
            &device.root,
            ObjectType::Update,
            &new_object_id,
            &new_parts[0].envelope,
            &[]
        )
        .is_err(),
        "旧 epoch 根不得解开新 part"
    );
    assert_eq!(cipher_digest.len(), 32);
}

/// T12 / 评审 0002 P1-3：pending（尚未加密、`cipher_digest=NULL`）的旧 epoch 意图
/// 也必须在阶段 J 保守重包，不能永远记为 failed。
///
/// pending 行没有密文可解，但 storage 仍保存原始 Loro update：按
/// `loro_updates`/update_id/batch_id 重建 `UpdateV1` 明文，先补齐旧 epoch 的
/// durable 密文（拿到可 CAS 的 digest），再用新 epoch 重加密成 new parts。
#[test]
fn pending_old_epoch_intent_is_conservatively_repacked() {
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x53);
    let doc = seed_doc(&vault, "doc-pending", device.identity.peer_id);
    doc.insert_scalars(0, "pending body").expect("正文");
    let message_id = uuid_like(0x91);
    let object_id: [u8; 32] = [0x91; 32];
    commit_with_intent(&vault, &doc, &message_id, object_id, EPOCH);
    let candidates = outbox_repack_candidates(&vault, EPOCH + 1).expect("candidates");
    assert_eq!(candidates.len(), 1);
    assert!(
        candidates[0].cipher_digest.is_none(),
        "前置：pending 行没有 ciphertext digest"
    );

    let new_root = RootSecretV1::new(
        VAULT_ID,
        EPOCH + 1,
        menghunote_sync::keys::Secret32::from_array([0x92; 32]),
    );
    let report = repack_outbox_after_rotation(&vault, &device.root, &new_root, device.device_id)
        .expect("repack");
    assert_eq!(report.failed, 0, "pending 行不得被当成永久失败: {report:?}");
    assert_eq!(report.repacked, 1, "pending 行必须保守重包: {report:?}");
}
