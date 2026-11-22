//! S7-14：初始槽上传 + checkpoint 创建/读回。
//!
//! FakeRelay（内存 relay）+ 真 SQLite；不 sleep。

mod support;

use support::*;

use menghunote_storage::{
    create_note_slot, ensure_structure_row, insert_asset, link_asset_ref, load_body_doc,
};
use menghunote_sync::checkpoint::{
    CheckpointWriter, EmptyAssets, SlotSecrets, StorageAssets, KIND_BODY, KIND_STRUCTURE,
};
use menghunote_sync::genesis;
use menghunote_sync::relay::{CheckpointRef, CheckpointRole, RelayClient};
use menghunote_sync::SyncError;
use sha2::{Digest, Sha256};

fn setup() -> (
    tempfile::TempDir,
    menghunote_storage::Vault,
    genesis::GenesisMaterials,
) {
    let (dir, vault) = fresh_vault();
    ensure_structure_row(&vault).expect("structure 行");
    let materials = genesis::build_materials(
        VAULT_ID,
        [0x44; 16],
        "pw",
        1_700_000_000,
        &mut TestRandom::new(0x88_14),
    )
    .expect("genesis 材料");
    (dir, vault, materials)
}

fn slot_secrets(materials: &genesis::GenesisMaterials) -> SlotSecrets<'_> {
    SlotSecrets {
        password: Some("pw"),
        recovery_seed: Some(&materials.recovery_seed),
    }
}

fn relay_with(materials: &genesis::GenesisMaterials) -> FakeRelay {
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    relay
}

fn add_note(
    vault: &menghunote_storage::Vault,
    note_id: &str,
    peer: u64,
    text: &str,
    message_id: &str,
    object_id: [u8; 32],
) {
    let doc = seed_doc(vault, note_id, peer);
    create_note_slot(vault, note_id, note_id, note_id, 1_700_000_000).expect("notes 行");
    doc.insert_scalars(0, text).expect("正文");
    commit_with_intent(vault, &doc, message_id, object_id, EPOCH);
}

fn real_asset(vault: &menghunote_storage::Vault, note_id: &str, data: &[u8]) -> [u8; 32] {
    let hash: [u8; 32] = Sha256::digest(data).into();
    insert_asset(vault, &hash, data.len() as u64, Some("text/plain"), data).expect("insert_asset");
    link_asset_ref(vault, note_id, &hash, 0, 1).expect("link_asset_ref");
    hash
}

#[test]
fn initial_slots_upload_and_verify() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    let mut writer = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    let report = writer
        .upload_initial_slots(
            &materials.password_slot,
            &materials.recovery_slot,
            &slot_secrets(&materials),
        )
        .expect("槽上传");
    assert_eq!(report.password.state, "active");
    assert_eq!(report.recovery.state, "staging");
    assert_eq!(
        report.password.kind,
        menghunote_sync_proto::SlotKind::Password
    );
    assert_eq!(
        report.recovery.kind,
        menghunote_sync_proto::SlotKind::Recovery
    );
    assert!(report.active_password_rev >= 1);
    // GET 与真实 relay 一致，只返回 active 槽；staging recovery 由 PUT 响应/本地记录核对。
    let slots = relay.list_key_slots().expect("列槽");
    assert_eq!(slots.len(), 1);
    let password = slots
        .iter()
        .find(|slot| slot.kind == menghunote_sync_proto::SlotKind::Password)
        .expect("password");
    assert_eq!(password.state, "active");
    assert_eq!(
        password.descriptor,
        materials.password_slot.descriptor.encode().unwrap()
    );
}

#[test]
fn two_checkpoints_are_independently_importable() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "第一行正文",
        &uuid_like(301),
        [0x31; 32],
    );
    add_note(
        &vault,
        "note-2",
        materials.peer_id_u64(),
        "second note",
        &uuid_like(302),
        [0x32; 32],
    );

    let mut writer = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    writer
        .upload_initial_slots(
            &materials.password_slot,
            &materials.recovery_slot,
            &slot_secrets(&materials),
        )
        .expect("槽上传");

    real_asset(&vault, "note-1", b"real storage asset bytes");
    let assets = StorageAssets;
    let first = writer
        .create_checkpoint(&assets)
        .expect("第一份 checkpoint");
    assert_eq!(first.docs.len(), 3, "structure + 2 个 body");
    assert!(first.docs.iter().any(|doc| doc.kind == KIND_STRUCTURE));
    assert_eq!(
        first
            .docs
            .iter()
            .filter(|doc| doc.kind == KIND_BODY)
            .count(),
        2
    );
    assert_eq!(first.assets.len(), 1);
    let verified = writer.read_back(&first).expect("第一份读回");
    assert!(verified.manifest_checked);
    assert_eq!(verified.asset_count, 1);
    let note1 = verified
        .docs
        .iter()
        .find(|(doc_id, _)| doc_id == "note-1")
        .and_then(|(_, text)| text.clone())
        .expect("note-1 正文");
    assert_eq!(note1, "第一行正文");
    let structure = verified
        .docs
        .iter()
        .find(|(doc_id, _)| doc_id == menghunote_storage::STRUCTURE_DOC_ID)
        .expect("structure");
    assert!(structure.1.is_none());

    let second = writer
        .create_checkpoint(&assets)
        .expect("第二份 checkpoint");
    assert_ne!(first.checkpoint_id, second.checkpoint_id);
    let first_note1 = first
        .docs
        .iter()
        .find(|doc| doc.doc_id == "note-1")
        .expect("first note-1");
    let second_note1 = second
        .docs
        .iter()
        .find(|doc| doc.doc_id == "note-1")
        .expect("second note-1");
    assert_ne!(first_note1.object.object_id, second_note1.object.object_id);
    let verified2 = writer.read_back(&second).expect("第二份读回");
    assert!(verified2.manifest_checked);
    assert_eq!(verified2.asset_count, 1);
    // 两份都必须能独立导入空库：这里用 body 快照直接 restore 再比对正文。
    let snapshot_bytes = {
        let (_header, bytes) = menghunote_sync::upload::fetch_object(
            &mut relay,
            &materials.root,
            menghunote_sync_proto::ObjectType::Snapshot,
            &second_note1.object.object_id,
            second_note1.object.part_count,
            second_note1.object.total_plain_len,
            &second_note1.object.object_digest,
            &[],
        )
        .expect("读回快照");
        menghunote_sync_proto::SnapshotV1::decode(&bytes)
            .expect("SnapshotV1")
            .loro_snapshot_bytes
    };
    let imported = menghunote_storage::BodyDoc::restore(
        "note-1",
        materials.peer_id_u64(),
        Some(&snapshot_bytes),
        &[],
    )
    .expect("从快照导入");
    assert_eq!(imported.text(), "第一行正文");
}

#[test]
fn asset_nonce_is_deterministic_across_writers() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "asset note",
        &uuid_like(305),
        [0x35; 32],
    );
    real_asset(&vault, "note-1", b"same asset bytes");
    real_asset(&vault, "note-1", b"another asset");
    let assets = StorageAssets;
    let first = {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut relay,
            VAULT_ID,
            materials.device_id,
            materials.signing_key.clone(),
            materials.root.clone(),
            materials.peer_id_u64(),
        );
        writer.create_checkpoint(&assets).expect("第一份")
    };
    assert_eq!(first.assets.len(), 2);
    assert_ne!(
        first.assets[0].object.object_id, first.assets[1].object.object_id,
        "不同资产必须不同 object_id"
    );
    let first_ids: Vec<[u8; 32]> = first
        .assets
        .iter()
        .map(|asset| asset.object.object_id)
        .collect();
    let first_parts: Vec<Vec<Vec<u8>>> = first
        .assets
        .iter()
        .map(|asset| {
            (0..asset.object.part_count)
                .map(|part| {
                    relay
                        .parts
                        .get(&(asset.object.object_id, part))
                        .cloned()
                        .expect("资产分片")
                })
                .collect()
        })
        .collect();
    let first_digests: Vec<[u8; 32]> = first
        .assets
        .iter()
        .map(|asset| asset.object.object_digest)
        .collect();

    // 新的 writer（模拟进程重启，资产缓存为空）重新导出同一资产：
    // 确定性 nonce 必须产生完全相同的密文字节，relay 幂等而不是 ID_CONFLICT。
    let second = {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut relay,
            VAULT_ID,
            materials.device_id,
            materials.signing_key.clone(),
            materials.root.clone(),
            materials.peer_id_u64(),
        );
        writer
            .create_checkpoint(&assets)
            .expect("同资产重传必须幂等（ADR §19）")
    };
    let second_ids: Vec<[u8; 32]> = second
        .assets
        .iter()
        .map(|asset| asset.object.object_id)
        .collect();
    let second_digests: Vec<[u8; 32]> = second
        .assets
        .iter()
        .map(|asset| asset.object.object_digest)
        .collect();
    assert_eq!(first_ids, second_ids);
    assert_eq!(first_digests, second_digests);
    for (index, asset) in first.assets.iter().enumerate() {
        for part in 0..asset.object.part_count {
            assert_eq!(
                relay.parts.get(&(asset.object.object_id, part)),
                Some(&first_parts[index][part as usize]),
                "确定性 nonce 下密文字节必须一致"
            );
        }
    }
}

#[test]
fn empty_vault_checkpoint_contains_only_structure() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    let mut writer = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    writer
        .upload_initial_slots(
            &materials.password_slot,
            &materials.recovery_slot,
            &slot_secrets(&materials),
        )
        .expect("槽上传");
    let report = writer
        .create_checkpoint(&EmptyAssets)
        .expect("空库 checkpoint");
    assert_eq!(report.docs.len(), 1);
    assert_eq!(report.docs[0].doc_id, menghunote_storage::STRUCTURE_DOC_ID);
    assert!(report.assets.is_empty());
    let verified = writer.read_back(&report).expect("读回");
    assert!(verified.manifest_checked);
    assert_eq!(verified.docs.len(), 1);
    assert!(verified.docs[0].1.is_none());
}

#[test]
fn tampered_snapshot_or_signature_is_rejected() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "tamper me",
        &uuid_like(311),
        [0x41; 32],
    );
    let report = {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut relay,
            VAULT_ID,
            materials.device_id,
            materials.signing_key.clone(),
            materials.root.clone(),
            materials.peer_id_u64(),
        );
        writer
            .upload_initial_slots(
                &materials.password_slot,
                &materials.recovery_slot,
                &slot_secrets(&materials),
            )
            .expect("槽上传");
        writer.create_checkpoint(&EmptyAssets).expect("checkpoint")
    };

    // 篡改分片：读回必须失败（digest/AEAD）。
    relay.tamper_parts = true;
    {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut relay,
            VAULT_ID,
            materials.device_id,
            materials.signing_key.clone(),
            materials.root.clone(),
            materials.peer_id_u64(),
        );
        assert!(writer.read_back(&report).is_err());
    }
    relay.tamper_parts = false;

    // 篡改创建事实里的 refs_digest：读回必须发现不一致。
    let mut corrupted = report.clone();
    corrupted.refs_digest[0] ^= 0x01;
    {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut relay,
            VAULT_ID,
            materials.device_id,
            materials.signing_key.clone(),
            materials.root.clone(),
            materials.peer_id_u64(),
        );
        assert!(matches!(
            writer.read_back(&corrupted),
            Err(SyncError::Object(_))
        ));
    }

    // 用未注册的私钥签发 checkpoint commit：relay publish 必须拒绝。
    let mut bad_relay = relay_with(&materials);
    let bad_signing = menghunote_sync::device::DeviceSigningKey::from_seed(
        menghunote_sync::keys::Secret32::from_array([0x77; 32]),
    );
    let mut bad_writer = CheckpointWriter::new(
        &vault,
        &mut bad_relay,
        VAULT_ID,
        materials.device_id,
        bad_signing,
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    assert!(matches!(
        bad_writer.create_checkpoint(&EmptyAssets),
        Err(SyncError::RelayApi { .. })
    ));
}

#[test]
fn checkpoint_refs_reject_manifest_as_data() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    let report = {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut relay,
            VAULT_ID,
            materials.device_id,
            materials.signing_key.clone(),
            materials.root.clone(),
            materials.peer_id_u64(),
        );
        writer
            .upload_initial_slots(
                &materials.password_slot,
                &materials.recovery_slot,
                &slot_secrets(&materials),
            )
            .expect("槽上传");
        writer.create_checkpoint(&EmptyAssets).expect("checkpoint")
    };
    assert!(matches!(
        relay.add_checkpoint_refs(
            report.checkpoint_id,
            vec![CheckpointRef {
                object_id: report.manifest.object_id,
                role: CheckpointRole::Data,
            }],
        ),
        Err(SyncError::RelayApi { .. })
    ));
}

#[test]
fn tombstone_docs_without_notes_are_included() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    // 只有 loro_docs 行、没有 notes 行的文档（tombstone/孤儿正文）。
    let orphan = seed_doc(&vault, "orphan-body", materials.peer_id_u64());
    orphan.insert_scalars(0, "orphan text").expect("正文");
    commit_local(&vault, &orphan);
    let mut writer = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    writer
        .upload_initial_slots(
            &materials.password_slot,
            &materials.recovery_slot,
            &slot_secrets(&materials),
        )
        .expect("槽上传");
    let report = writer
        .create_checkpoint(&menghunote_sync::checkpoint::StorageAssets)
        .expect("checkpoint");
    assert!(
        report.docs.iter().any(|doc| doc.doc_id == "orphan-body"),
        "list_all_docs 必须包含无 note 行的 tombstone 文档"
    );
    let verified = writer.read_back(&report).expect("读回");
    let text = verified
        .docs
        .iter()
        .find(|(doc_id, _)| doc_id == "orphan-body")
        .and_then(|(_, text)| text.clone())
        .expect("tombstone 正文");
    assert_eq!(text, "orphan text");
}

#[test]
fn body_snapshot_imports_without_local_updates() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "snapshot only",
        &uuid_like(321),
        [0x51; 32],
    );
    let mut writer = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    writer
        .upload_initial_slots(
            &materials.password_slot,
            &materials.recovery_slot,
            &slot_secrets(&materials),
        )
        .expect("槽上传");
    let report = writer.create_checkpoint(&EmptyAssets).expect("checkpoint");
    let verified = writer.read_back(&report).expect("读回");
    let text = verified
        .docs
        .iter()
        .find(|(doc_id, _)| doc_id == "note-1")
        .and_then(|(_, text)| text.clone())
        .expect("正文");
    assert_eq!(text, "snapshot only");
    // 从快照直接恢复的文档与库里重放结果一致。
    let (local, _) = load_body_doc(&vault, "note-1", materials.peer_id_u64(), None).expect("重放");
    assert_eq!(local.text(), text);
}

#[test]
fn checkpoint_prepare_validate_promote() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "text",
        &uuid_like(701),
        [0x61; 32],
    );
    let mut writer = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    writer
        .upload_initial_slots(
            &materials.password_slot,
            &materials.recovery_slot,
            &slot_secrets(&materials),
        )
        .expect("槽上传");
    let prepared = writer.prepare(&EmptyAssets).expect("prepare");
    assert!(!prepared.report.validated, "prepare 产物不得标记 validated");
    let baseline = writer.validate(&prepared).expect("validate");
    let promoted = writer.promote(&baseline).expect("promote");
    assert_eq!(promoted.checkpoint_id, prepared.report.checkpoint_id);
    assert_eq!(promoted.covered_seq, prepared.report.covered_seq);
    assert_eq!(promoted.cursor, format!("c{}", prepared.report.covered_seq));
    let report = writer.create_checkpoint(&EmptyAssets).expect("create");
    assert!(
        report.validated,
        "create_checkpoint 必须走 validate+promote"
    );
}

#[test]
fn checkpoint_validate_failure_cannot_promote() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "text",
        &uuid_like(702),
        [0x62; 32],
    );
    let prepared = {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut relay,
            VAULT_ID,
            materials.device_id,
            materials.signing_key.clone(),
            materials.root.clone(),
            materials.peer_id_u64(),
        );
        writer
            .upload_initial_slots(
                &materials.password_slot,
                &materials.recovery_slot,
                &slot_secrets(&materials),
            )
            .expect("槽上传");
        writer.prepare(&EmptyAssets).expect("prepare")
    };
    let manifest_id = prepared.report.manifest.object_id;
    relay
        .parts
        .get_mut(&(manifest_id, 0))
        .expect("manifest part")[0] ^= 0x01;
    let mut writer = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    assert!(
        writer.validate(&prepared).is_err(),
        "篡改后 validate 必须失败"
    );
}

#[test]
fn two_baselines_are_validated_before_promotion() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "text",
        &uuid_like(703),
        [0x63; 32],
    );
    let mut writer = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    writer
        .upload_initial_slots(
            &materials.password_slot,
            &materials.recovery_slot,
            &slot_secrets(&materials),
        )
        .expect("槽上传");
    let first = writer.prepare(&EmptyAssets).expect("prepare 1");
    let second = writer.prepare(&EmptyAssets).expect("prepare 2");
    let first = writer.validate(&first).expect("validate 1");
    let second = writer.validate(&second).expect("validate 2");
    let a = writer.promote(&first).expect("promote 1");
    let b = writer.promote(&second).expect("promote 2");
    assert_ne!(a.checkpoint_id, b.checkpoint_id);
}
