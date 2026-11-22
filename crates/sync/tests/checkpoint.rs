//! S7-14：初始槽上传 + checkpoint 创建/读回。
//!
//! FakeRelay（内存 relay）+ 真 SQLite；不 sleep。

mod support;

use support::*;

use menghunote_storage::{
    append_updates, create_note_slot, ensure_structure_row, insert_asset, link_asset_ref,
    load_body_doc, mark_integrated, store_inbox_verified, BodyDoc, UpdateRecord, Value,
    META_SYNC_LOG_ID,
};
use menghunote_sync::checkpoint::{
    asset_object_id, CheckpointWriter, EmptyAssets, SlotSecrets, StaticAssets, StorageAssets,
    KIND_BODY, KIND_STRUCTURE,
};
use menghunote_sync::engine::{EngineIdentity, SyncEngine, RELAY_CURSOR_META};
use menghunote_sync::genesis;
use menghunote_sync::relay::{CheckpointRef, CheckpointRole, RelayClient};
use menghunote_sync::SyncError;
use menghunote_sync_proto::UpdateV1;
use sha2::{Digest, Sha256};

fn setup() -> (
    tempfile::TempDir,
    menghunote_storage::Vault,
    genesis::GenesisMaterials,
) {
    let (dir, vault) = fresh_vault();
    ensure_structure_row(&vault).expect("structure 行");
    // 真实设备在 genesis/bootstrap 后必有 relay 游标；checkpoint validate 会用它
    // 证明 S 之前的日志事件已处理。
    vault
        .set_meta(menghunote_sync::engine::RELAY_CURSOR_META, b"c0")
        .expect("写 relay 游标");
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

fn asset_marker_count(vault: &menghunote_storage::Vault) -> usize {
    vault
        .query(
            "SELECT COUNT(*) FROM meta WHERE substr(key,1,?1)=?2",
            &[
                Value::Integer("sync.asset_uploaded.".len() as i64),
                Value::Text("sync.asset_uploaded.".to_owned()),
            ],
        )
        .expect("读 asset marker")
        .first()
        .and_then(|row| row.first())
        .and_then(|value| match value {
            Value::Integer(value) => usize::try_from(*value).ok(),
            _ => None,
        })
        .expect("marker 计数")
}

fn clear_asset_markers(vault: &menghunote_storage::Vault) {
    vault
        .exec(
            "DELETE FROM meta WHERE substr(key,1,?1)=?2",
            &[
                Value::Integer("sync.asset_uploaded.".len() as i64),
                Value::Text("sync.asset_uploaded.".to_owned()),
            ],
        )
        .expect("清 asset marker");
}

fn asset_part_puts(relay: &FakeRelay, object_id: &[u8; 32]) -> usize {
    relay
        .put_history
        .iter()
        .filter(|(recorded, _, _)| recorded == object_id)
        .count()
}

/// 普通同步已经传过的资产，checkpoint 不得再按维护 reservation 重传（否则真实 relay
/// 会因"同一对象全部分片必须同预算来源"返回 ID_CONFLICT）；反过来，checkpoint 已完整
/// 上传的对象，普通同步也只落 durable 标记、不重传。
#[test]
fn checkpoint_reuses_assets_uploaded_by_ordinary_sync_and_marks_them_back() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "asset note",
        &uuid_like(306),
        [0x36; 32],
    );
    let data = b"ordinary sync asset before checkpoint";
    let hash = real_asset(&vault, "note-1", data);
    let object_id = asset_object_id(&materials.root, &hash, data.len() as u64, "text/plain")
        .expect("asset object_id");

    // 完整普通同步（含 pull 自己的 ASSET 事件：checkpoint coverage 只认 durable 事实）。
    {
        let identity = EngineIdentity {
            device_id: materials.device_id,
            signing_key: materials.signing_key.clone(),
            peer_id: materials.peer_id_u64(),
        };
        let mut sync = SyncEngine::new(&vault, &mut relay, identity, materials.root.clone());
        let report = sync.sync_once().expect("普通同步");
        assert_eq!(report.assets.uploaded, 1);
        assert_eq!(report.assets.failed, 0);
    }
    assert_eq!(asset_marker_count(&vault), 1);
    let puts_after_sync = asset_part_puts(&relay, &object_id);
    assert_eq!(puts_after_sync, 1, "单分片资产");

    // checkpoint：标记复用，不再上传该资产分片。
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
            .create_checkpoint(&StorageAssets)
            .expect("普通同步之后创建 checkpoint 必须成功（不得 ID_CONFLICT）")
    };
    assert!(report.validated);
    assert_eq!(report.assets.len(), 1);
    assert_eq!(
        asset_part_puts(&relay, &object_id),
        puts_after_sync,
        "checkpoint 不得重传普通同步已上传的资产"
    );

    // 反向：清掉标记后，普通同步必须复用 checkpoint 已完整的对象（只提交+落标记）。
    clear_asset_markers(&vault);
    let puts_before_adopt = asset_part_puts(&relay, &object_id);
    {
        let identity = EngineIdentity {
            device_id: materials.device_id,
            signing_key: materials.signing_key.clone(),
            peer_id: materials.peer_id_u64(),
        };
        let mut sync = SyncEngine::new(&vault, &mut relay, identity, materials.root.clone());
        let report = sync.upload_pending_assets().expect("复用 checkpoint 对象");
        assert_eq!(report.uploaded, 0);
        assert_eq!(report.reused_existing, 1);
        assert_eq!(report.failed, 0);
    }
    assert_eq!(
        asset_part_puts(&relay, &object_id),
        puts_before_adopt,
        "checkpoint 已完整上传的对象不得被普通同步重传"
    );
    assert_eq!(asset_marker_count(&vault), 1, "复用后必须落 durable 标记");
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
    assert!(!prepared.report.validated, "prepare 产物不得标记 validated");
    assert_eq!(prepared.report.published_at, None, "prepare 不得 publish");
    let checkpoint = relay.checkpoint.as_ref().expect("lease 已创建");
    assert!(!checkpoint.published, "prepare 不得 publish 可 GC 基线");
    assert!(relay.published_checkpoints.is_empty());

    let baseline = {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut relay,
            VAULT_ID,
            materials.device_id,
            materials.signing_key.clone(),
            materials.root.clone(),
            materials.peer_id_u64(),
        );
        writer.validate(&prepared).expect("validate")
    };
    assert_eq!(
        baseline.imported_docs(),
        prepared.report.docs.len(),
        "validate 必须实际导入每个 doc snapshot"
    );
    assert_eq!(baseline.verified_assets(), 0);
    assert_ne!(
        baseline.imported_vv_digest(),
        [0u8; 32],
        "validate 必须产出 actual VV 证据"
    );
    assert!(
        relay.published_checkpoints.is_empty(),
        "validate 通过但 promote 前不得 publish"
    );

    let promoted = {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut relay,
            VAULT_ID,
            materials.device_id,
            materials.signing_key.clone(),
            materials.root.clone(),
            materials.peer_id_u64(),
        );
        writer.promote(&baseline).expect("promote")
    };
    assert_eq!(promoted.checkpoint_id, prepared.report.checkpoint_id);
    assert_eq!(promoted.covered_seq, prepared.report.covered_seq);
    assert_eq!(promoted.cursor, format!("c{}", prepared.report.covered_seq));
    assert_eq!(promoted.published_at, 20_000);
    assert!(relay.checkpoint.as_ref().expect("checkpoint").published);
    assert_eq!(relay.published_checkpoints.len(), 1);

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
        writer.create_checkpoint(&EmptyAssets).expect("create")
    };
    assert!(
        report.validated,
        "create_checkpoint 必须走 validate+promote"
    );
    assert!(report.published_at.is_some());
    assert!(
        report.commit_digest.is_some(),
        "report 必须携带 promote 实际签名的 commit 摘要（T 只能取这里）"
    );

    // task-122：生产路径签的是验证修订 2，且 validation_digest 已绑定覆盖证据。
    let published = relay.published_checkpoints.last().expect("published");
    let commit = menghunote_sync_proto::CheckpointCommit::decode(
        published.signed_commit.as_deref().expect("signed commit"),
    )
    .expect("decode checkpoint commit");
    assert_eq!(
        commit.validation_version,
        menghunote_sync_proto::CheckpointCommit::CURRENT_VALIDATION_VERSION
    );
    assert_ne!(
        commit.validation_digest, [0u8; 32],
        "v2 必须绑定验证证据摘要"
    );
    assert!(commit.require_current_validation().is_ok());
    assert_eq!(
        report.commit_digest,
        Some(Sha256::digest(published.signed_commit.as_deref().expect("signed commit")).into()),
        "report 的 commit 摘要必须等于 relay 持久化的签名字节 SHA256"
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
    // 单 writer lease：第一份必须完成 validate+promote 后，才能申请第二份。
    let first = writer.prepare(&EmptyAssets).expect("prepare 1");
    let first = writer.validate(&first).expect("validate 1");
    let a = writer.promote(&first).expect("promote 1");
    let second = writer.prepare(&EmptyAssets).expect("prepare 2");
    let second = writer.validate(&second).expect("validate 2");
    let b = writer.promote(&second).expect("promote 2");
    assert_ne!(a.checkpoint_id, b.checkpoint_id);
}

/// 用同一 root/object_id 重新加密对象并直接替换 relay 分片（模拟 prepare 产物被篡改）。
fn replace_object(
    relay: &mut FakeRelay,
    root: &menghunote_sync::keys::RootSecretV1,
    object_type: menghunote_sync_proto::ObjectType,
    object_id: [u8; 32],
    generation: u64,
    plaintext: &[u8],
) -> ([u8; 32], u32, u64) {
    let (parts, digest) = menghunote_sync::upload::encrypt_local_parts(
        root,
        object_type,
        &object_id,
        [0x42; 16],
        [0x11; 16],
        generation,
        0,
        plaintext,
        &[],
    )
    .expect("重新加密对象");
    for (index, envelope, _) in &parts {
        relay.parts.insert((object_id, *index), envelope.clone());
    }
    (digest, parts.len() as u32, plaintext.len() as u64)
}

fn recompute_refs_digest(report: &menghunote_sync::checkpoint::CheckpointReport) -> [u8; 32] {
    use menghunote_sync_proto::{refs_digest_input, ObjectRefV1, ObjectType};
    let mut refs = Vec::new();
    for doc in &report.docs {
        refs.push(ObjectRefV1 {
            object_id: doc.object.object_id,
            object_type: ObjectType::Snapshot,
            generation: doc.generation,
            key_epoch: report.epoch,
            part_count: doc.object.part_count,
            total_plain_len: doc.object.total_plain_len,
            object_digest: doc.object.object_digest,
        });
    }
    for asset in &report.assets {
        refs.push(ObjectRefV1 {
            object_id: asset.object.object_id,
            object_type: ObjectType::Asset,
            generation: 0,
            key_epoch: report.epoch,
            part_count: asset.object.part_count,
            total_plain_len: asset.object.total_plain_len,
            object_digest: asset.object.object_digest,
        });
    }
    Sha256::digest(refs_digest_input(&refs).expect("refs")).into()
}

fn prepared_with_note(
    vault: &menghunote_storage::Vault,
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
    message_id: &str,
    object_id: [u8; 32],
) -> menghunote_sync::checkpoint::PreparedCheckpoint {
    add_note(
        vault,
        "note-1",
        materials.peer_id_u64(),
        "validated text",
        message_id,
        object_id,
    );
    CheckpointWriter::new(
        vault,
        relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    )
    .prepare(&EmptyAssets)
    .expect("prepare")
}

#[test]
fn stale_prepared_rejected_after_real_integration_and_reprepare_passes() {
    use menghunote_sync::relay::RelayLogKind;
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    let log_id = relay.log_id;
    vault
        .set_meta(META_SYNC_LOG_ID, &log_id)
        .expect("写 log_id");
    vault
        .set_meta(menghunote_sync::engine::RELAY_CURSOR_META, b"c0")
        .expect("写 relay 游标");
    // 本地先建空文档；relay 上已有一条 <=S 的远端 UPDATE。
    seed_doc(&vault, "note-1", materials.peer_id_u64());
    relay.publish_event(RelayLogKind::Update, [0x71; 32]);
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
        writer.prepare(&EmptyAssets).expect("prepare")
    };
    assert_eq!(prepared.report.covered_seq, 1);
    // prepare 时本地尚未 integrate 这条 <=S 的 UPDATE：stale 快照连 S 都没覆盖，先拒绝。
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
        assert!(
            writer.validate(&prepared).is_err(),
            "prepare 时落后于 S 的 UPDATE 必须被拒绝"
        );
    }
    // prepare 之后本地才真实 integrate 这条 <=S 的 UPDATE（等价 SyncEngine::integrate）。
    let remote = remote_update_for("note-1", 0x42, "remote text", &uuid_like(711));
    integrate_remote_update(&vault, &log_id, 1, [0x71; 32], &remote);

    // 当前本地 VV/integrated 都已追平 S，但导出快照仍是 prepare 时的旧状态 → 必须拒绝，
    // 不能靠"当前 integrated >= S"放行。
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
        let err = writer
            .validate(&prepared)
            .expect_err("stale prepared 必须拒绝");
        assert!(
            format!("{err}").contains("重新 prepare"),
            "拒绝时必须提示重新 prepare：{err}"
        );
    }
    assert!(
        !relay.checkpoint.as_ref().expect("lease").published,
        "stale validate 失败不得 publish"
    );
    assert!(relay.published_checkpoints.is_empty());

    // 真实环境由 relay 超时/调用方放弃 stale lease；这里直接释放，再 prepare 一次。
    relay.checkpoint = None;
    let baseline = {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut relay,
            VAULT_ID,
            materials.device_id,
            materials.signing_key.clone(),
            materials.root.clone(),
            materials.peer_id_u64(),
        );
        let fresh = writer.prepare(&EmptyAssets).expect("重新 prepare");
        assert_eq!(fresh.report.covered_seq, 1, "S 仍是 1");
        let baseline = writer
            .validate(&fresh)
            .expect("重新 prepare 覆盖后必须通过");
        assert_eq!(baseline.imported_docs(), fresh.report.docs.len());
        baseline
    };
    assert_eq!(baseline.covered_seq(), 1);
}

/// 构造一条"远端设备"产生的 UpdateV1（本地文档此时为空，用于模拟 prepare 后到达的更新）。
fn remote_update_for(doc_id: &str, remote_peer: u64, text: &str, message_id: &str) -> UpdateV1 {
    let remote = BodyDoc::create(doc_id, remote_peer).expect("远端文档");
    remote.insert_scalars(0, text).expect("远端正文");
    let pending = remote
        .pending_update(&loro::VersionVector::new())
        .expect("远端增量");
    UpdateV1 {
        doc_id: doc_id.to_owned(),
        generation: 1,
        codec_version: menghunote_storage::CODEC_VERSION.to_owned(),
        app_doc_schema: menghunote_storage::APP_DOC_SCHEMA as u32,
        min_reader: menghunote_storage::MIN_READER as u32,
        update_id: message_id.to_owned(),
        batch_id: format!("batch-{message_id}"),
        origin_device_id: [0x42; 16],
        origin_seq: 1,
        base_vv: Vec::new(),
        end_vv: pending.end_vv,
        end_frontier: pending.end_frontier,
        loro_update_bytes: pending.payload,
    }
}

/// 等价 `SyncEngine::integrate`：inbox verified → import Loro → A 事务 append → integrated。
fn integrate_remote_update(
    vault: &menghunote_storage::Vault,
    log_id: &[u8; 16],
    relay_seq: u64,
    object_id: [u8; 32],
    update: &UpdateV1,
) {
    let payload = update.encode().expect("encode UpdateV1");
    store_inbox_verified(vault, log_id, relay_seq, &object_id, EPOCH, &payload)
        .expect("inbox verified");
    let (doc, head) = load_body_doc(vault, &update.doc_id, 0x9999, None).expect("本地文档");
    let before = doc.vv();
    doc.import(&update.loro_update_bytes)
        .expect("导入远端 update");
    let pending = doc.pending_update(&before).expect("本地增量");
    assert!(!pending.payload.is_empty(), "远端 update 必须产生本地增量");
    append_updates(
        vault,
        &update.doc_id,
        head.head_seq,
        &[UpdateRecord::new(update.batch_id.clone(), pending)],
    )
    .expect("A 事务");
    mark_integrated(vault, log_id, relay_seq).expect("mark integrated");
}

#[test]
fn validate_rejects_corrupt_snapshot_without_publishing() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    let prepared = prepared_with_note(&vault, &mut relay, &materials, &uuid_like(712), [0x72; 32]);
    let snapshot_id = prepared
        .report
        .docs
        .iter()
        .find(|doc| doc.doc_id == "note-1")
        .expect("body doc")
        .object
        .object_id;
    relay.parts.get_mut(&(snapshot_id, 0)).expect("part")[0] ^= 0x01;
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
        assert!(
            writer.validate(&prepared).is_err(),
            "损坏快照必须让 validate 失败"
        );
    }
    assert!(!relay.checkpoint.as_ref().expect("lease").published);
    assert!(relay.published_checkpoints.is_empty());
}

#[test]
fn validate_rejects_corrupt_asset_without_publishing() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "asset text",
        &uuid_like(713),
        [0x73; 32],
    );
    real_asset(&vault, "note-1", b"checkpoint asset bytes");
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
        writer.prepare(&StorageAssets).expect("prepare")
    };
    assert_eq!(prepared.report.assets.len(), 1);
    let asset_id = prepared.report.assets[0].object.object_id;
    relay.parts.get_mut(&(asset_id, 0)).expect("part")[0] ^= 0x01;
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
        assert!(
            writer.validate(&prepared).is_err(),
            "损坏资产必须让 validate 失败（而且 object_id 已提交 MIME/长度）"
        );
    }
    assert!(!relay.checkpoint.as_ref().expect("lease").published);
    assert!(relay.published_checkpoints.is_empty());
}

#[test]
fn publish_failure_leaves_no_published_baseline() {
    use menghunote_sync::device::DeviceSigningKey;
    use menghunote_sync::keys::Secret32;
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    let prepared = prepared_with_note(&vault, &mut relay, &materials, &uuid_like(714), [0x74; 32]);
    let baseline = {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut relay,
            VAULT_ID,
            materials.device_id,
            materials.signing_key.clone(),
            materials.root.clone(),
            materials.peer_id_u64(),
        );
        writer.validate(&prepared).expect("validate")
    };
    // 未注册的签名私钥：relay publish 验签失败 → promote 失败。
    let bad_signing = DeviceSigningKey::from_seed(Secret32::from_array([0x79; 32]));
    let mut writer = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        bad_signing,
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    assert!(writer.promote(&baseline).is_err(), "publish 必须失败");
    drop(writer);
    assert!(
        !relay.checkpoint.as_ref().expect("lease").published,
        "publish 失败不得留下可 GC 基线"
    );
    assert!(relay.published_checkpoints.is_empty());
}

#[test]
fn validate_rejects_manifest_inflated_vv_without_publishing() {
    use menghunote_sync::upload::UploadedObject;
    use menghunote_sync_proto::{ManifestV1, ObjectType, SnapshotV1};
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    let mut prepared =
        prepared_with_note(&vault, &mut relay, &materials, &uuid_like(715), [0x75; 32]);

    // 1) 读出 manifest，把一个 body doc 的 VV 虚高。
    let manifest_id = prepared.report.manifest.object_id;
    let (_h, manifest_bytes) = menghunote_sync::upload::fetch_object(
        &mut relay,
        &materials.root,
        ObjectType::Manifest,
        &manifest_id,
        prepared.report.manifest.part_count,
        prepared.report.manifest.total_plain_len,
        &prepared.report.manifest.object_digest,
        &[],
    )
    .expect("读 manifest");
    let mut manifest = ManifestV1::decode(&manifest_bytes).expect("manifest");
    let body_index = manifest
        .docs
        .iter()
        .position(|doc| doc.kind == KIND_BODY)
        .expect("body doc");
    let mut inflated = loro::VersionVector::decode(&manifest.docs[body_index].vv).expect("vv");
    inflated.insert(0xDEAD, 7);
    manifest.docs[body_index].vv = inflated.encode();
    let manifest_plain = manifest.encode().expect("encode manifest");
    let (manifest_digest, manifest_parts, manifest_len) = replace_object(
        &mut relay,
        &materials.root,
        ObjectType::Manifest,
        manifest_id,
        0,
        &manifest_plain,
    );
    prepared.report.manifest = UploadedObject {
        object_id: manifest_id,
        part_count: manifest_parts,
        total_plain_len: manifest_len,
        object_digest: manifest_digest,
    };

    // 2) snapshot 载荷也自报同一个虚高 VV（但 Loro 字节仍是真实历史）。
    let snapshot_id = prepared.report.docs[body_index].object.object_id;
    let (_h, snapshot_bytes) = menghunote_sync::upload::fetch_object(
        &mut relay,
        &materials.root,
        ObjectType::Snapshot,
        &snapshot_id,
        prepared.report.docs[body_index].object.part_count,
        prepared.report.docs[body_index].object.total_plain_len,
        &prepared.report.docs[body_index].object.object_digest,
        &[],
    )
    .expect("读 snapshot");
    let mut snapshot = SnapshotV1::decode(&snapshot_bytes).expect("snapshot");
    snapshot.vv = inflated.encode();
    let snapshot_plain = snapshot.encode().expect("encode snapshot");
    let (snapshot_digest, snapshot_parts, snapshot_len) = replace_object(
        &mut relay,
        &materials.root,
        ObjectType::Snapshot,
        snapshot_id,
        prepared.report.docs[body_index].generation,
        &snapshot_plain,
    );
    prepared.report.docs[body_index].object.object_digest = snapshot_digest;
    prepared.report.docs[body_index].object.part_count = snapshot_parts;
    prepared.report.docs[body_index].object.total_plain_len = snapshot_len;
    prepared.report.refs_digest = recompute_refs_digest(&prepared.report);

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
        assert!(
            writer.validate(&prepared).is_err(),
            "manifest/载荷宣称 VV 虚高必须被实际 Loro 导入识破"
        );
    }
    assert!(!relay.checkpoint.as_ref().expect("lease").published);
    assert!(relay.published_checkpoints.is_empty());
}

#[test]
fn validate_rejects_snapshot_missing_ops_without_publishing() {
    use menghunote_sync::upload::UploadedObject;
    use menghunote_sync_proto::{ObjectType, SnapshotV1};
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    let mut prepared =
        prepared_with_note(&vault, &mut relay, &materials, &uuid_like(716), [0x76; 32]);
    let body_index = prepared
        .report
        .docs
        .iter()
        .position(|doc| doc.kind == KIND_BODY)
        .expect("body doc");
    let snapshot_id = prepared.report.docs[body_index].object.object_id;
    let (_h, snapshot_bytes) = menghunote_sync::upload::fetch_object(
        &mut relay,
        &materials.root,
        ObjectType::Snapshot,
        &snapshot_id,
        prepared.report.docs[body_index].object.part_count,
        prepared.report.docs[body_index].object.total_plain_len,
        &prepared.report.docs[body_index].object.object_digest,
        &[],
    )
    .expect("读 snapshot");
    let mut snapshot = SnapshotV1::decode(&snapshot_bytes).expect("snapshot");
    // 替换成空 Loro 历史（缺 op），但 vv/frontier 仍按原 manifest 宣称。
    let empty = loro::LoroDoc::new()
        .export(loro::ExportMode::Snapshot)
        .expect("empty snapshot");
    snapshot.loro_snapshot_bytes = empty;
    let snapshot_plain = snapshot.encode().expect("encode snapshot");
    let (snapshot_digest, snapshot_parts, snapshot_len) = replace_object(
        &mut relay,
        &materials.root,
        ObjectType::Snapshot,
        snapshot_id,
        prepared.report.docs[body_index].generation,
        &snapshot_plain,
    );
    prepared.report.docs[body_index].object = UploadedObject {
        object_id: snapshot_id,
        part_count: snapshot_parts,
        total_plain_len: snapshot_len,
        object_digest: snapshot_digest,
    };
    prepared.report.refs_digest = recompute_refs_digest(&prepared.report);
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
        assert!(
            writer.validate(&prepared).is_err(),
            "缺 op 的快照必须被空 LoroDoc 导入识破"
        );
    }
    assert!(!relay.checkpoint.as_ref().expect("lease").published);
    assert!(relay.published_checkpoints.is_empty());
}

#[test]
fn validate_rejects_snapshot_older_than_prepare_export_point() {
    use menghunote_sync::upload::UploadedObject;
    use menghunote_sync_proto::{ManifestV1, ObjectType, SnapshotV1};
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    let mut prepared =
        prepared_with_note(&vault, &mut relay, &materials, &uuid_like(719), [0x79; 32]);
    let body_index = prepared
        .report
        .docs
        .iter()
        .position(|doc| doc.kind == KIND_BODY)
        .expect("body doc");

    // 1) body snapshot 换成空 Loro 历史，并让载荷自报 VV/frontier 与空历史一致。
    let snapshot_id = prepared.report.docs[body_index].object.object_id;
    let (_h, snapshot_bytes) = menghunote_sync::upload::fetch_object(
        &mut relay,
        &materials.root,
        ObjectType::Snapshot,
        &snapshot_id,
        prepared.report.docs[body_index].object.part_count,
        prepared.report.docs[body_index].object.total_plain_len,
        &prepared.report.docs[body_index].object.object_digest,
        &[],
    )
    .expect("读 snapshot");
    let mut snapshot = SnapshotV1::decode(&snapshot_bytes).expect("snapshot");
    snapshot.loro_snapshot_bytes = loro::LoroDoc::new()
        .export(loro::ExportMode::Snapshot)
        .expect("empty snapshot");
    snapshot.vv.clear();
    snapshot.frontier.clear();
    let snapshot_plain = snapshot.encode().expect("encode snapshot");
    let (snapshot_digest, snapshot_parts, snapshot_len) = replace_object(
        &mut relay,
        &materials.root,
        ObjectType::Snapshot,
        snapshot_id,
        prepared.report.docs[body_index].generation,
        &snapshot_plain,
    );
    prepared.report.docs[body_index].object = UploadedObject {
        object_id: snapshot_id,
        part_count: snapshot_parts,
        total_plain_len: snapshot_len,
        object_digest: snapshot_digest,
    };

    // 2) manifest 同步改成空 VV/frontier，让旧快照在 manifest 自洽性检查上"看起来合法"。
    let manifest_id = prepared.report.manifest.object_id;
    let (_h, manifest_bytes) = menghunote_sync::upload::fetch_object(
        &mut relay,
        &materials.root,
        ObjectType::Manifest,
        &manifest_id,
        prepared.report.manifest.part_count,
        prepared.report.manifest.total_plain_len,
        &prepared.report.manifest.object_digest,
        &[],
    )
    .expect("读 manifest");
    let mut manifest = ManifestV1::decode(&manifest_bytes).expect("manifest");
    manifest.docs[body_index].vv.clear();
    manifest.docs[body_index].frontier.clear();
    manifest.docs[body_index].part_count = snapshot_parts;
    manifest.docs[body_index].total_plain_len = snapshot_len;
    manifest.docs[body_index].object_digest = snapshot_digest;
    let manifest_plain = manifest.encode().expect("encode manifest");
    let (manifest_digest, manifest_parts, manifest_len) = replace_object(
        &mut relay,
        &materials.root,
        ObjectType::Manifest,
        manifest_id,
        0,
        &manifest_plain,
    );
    prepared.report.manifest = UploadedObject {
        object_id: manifest_id,
        part_count: manifest_parts,
        total_plain_len: manifest_len,
        object_digest: manifest_digest,
    };
    prepared.report.refs_digest = recompute_refs_digest(&prepared.report);

    // 3) manifest/载荷自洽且 refs_digest 正确，但 actual VV 落后于 prepare 导出点：
    //    旧的 validate 会放过，新的绑定必须拒绝。
    let mut writer = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    let err = writer
        .validate(&prepared)
        .expect_err("旧快照必须被 prepare 导出点 VV 拒绝");
    assert!(
        format!("{err}").contains("导出点"),
        "错误必须指出未覆盖 prepare 导出点：{err}"
    );
    assert!(!relay.checkpoint.as_ref().expect("lease").published);
    assert!(relay.published_checkpoints.is_empty());
}

#[test]
fn validate_rejects_quarantine_row_below_s() {
    use menghunote_sync::relay::RelayLogKind;
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    relay.publish_event(RelayLogKind::Update, [0xAB; 32]);
    let prepared = prepared_with_note(&vault, &mut relay, &materials, &uuid_like(717), [0x77; 32]);
    assert_eq!(prepared.report.covered_seq, 1);
    // 假装 integrated 已经追平，但 <=S 有 quarantine：必须拒绝。
    vault
        .set_meta(menghunote_storage::META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    vault
        .set_meta(
            menghunote_storage::META_SYNC_INTEGRATED_SEQ,
            &1u64.to_be_bytes(),
        )
        .expect("写 integrated");
    vault
        .exec(
            "INSERT INTO sync_inbox(log_id,relay_seq,object_id,epoch,state) \
             VALUES(?1,1,?2,1,'quarantine')",
            &[
                menghunote_storage::Value::Blob(vec![0x11; 16]),
                menghunote_storage::Value::Blob(vec![0xAB; 32]),
            ],
        )
        .expect("写 quarantine");
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
        "quarantine <= S 必须让 validate 失败"
    );
    assert!(!relay.checkpoint.as_ref().expect("lease").published);
}

fn checkpoint_writer<'a>(
    vault: &'a menghunote_storage::Vault,
    relay: &'a mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) -> CheckpointWriter<'a, FakeRelay> {
    CheckpointWriter::new(
        vault,
        relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    )
}

// ============================================================ 评审 0003 P0-B
// coverage 假覆盖 S 残余：空页/游标停滞/序号缺口/无持久 ASSET 证据都不能 Ok。

/// 反例：S=1、integrated=0、durable cursor=c0；relay 仍报 head=1，但隐藏 UPDATE
/// 让 `/log` 返回空页。validate 必须失败，且不得产 VerifiedBaseline / publish。
#[test]
fn validate_rejects_hidden_update_when_coverage_page_is_empty() {
    use menghunote_sync::relay::RelayLogKind;
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "hidden coverage",
        &uuid_like(721),
        [0x71; 32],
    );
    // relay 的 head 已到 1（S=1），但日志事件被隐藏：从 c0 查询只拿到空页。
    relay.publish_event(RelayLogKind::Update, [0x72; 32]);
    relay.log.clear();
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect("prepare 本身只固定 S/导出对象");
    assert_eq!(prepared.report.covered_seq, 1);
    let err = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect_err("空页且未累计证明到 S 必须拒绝");
    assert!(matches!(err, SyncError::Object(_)), "{err}");
    assert!(format!("{err}").contains("未证明覆盖到 S"), "{err}");
    assert!(
        !relay.checkpoint.as_ref().expect("lease").published,
        "validate 失败不得 publish"
    );
    assert!(relay.published_checkpoints.is_empty());
}

/// 正常覆盖：S=1 的 UPDATE 已真实 integrate，导出快照/账簿都能证明到 S → 必须通过。
#[test]
fn validate_accepts_fully_integrated_coverage_to_s() {
    use menghunote_sync::relay::RelayLogKind;
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    let log_id = relay.log_id;
    vault
        .set_meta(META_SYNC_LOG_ID, &log_id)
        .expect("写 log_id");
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "covered text",
        &uuid_like(722),
        [0x73; 32],
    );
    relay.publish_event(RelayLogKind::Update, [0x74; 32]);
    let remote = remote_update_for("note-1", 0x42, "remote covered", &uuid_like(723));
    integrate_remote_update(&vault, &log_id, 1, [0x74; 32], &remote);
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect("prepare");
    assert_eq!(prepared.report.covered_seq, 1);
    let baseline = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect("已真实 integrate 到 S 的 coverage 必须通过");
    assert_eq!(baseline.covered_seq(), 1);
}

/// ≤S 的 ASSET 缺口：prepare 可以记录 pending 候选（完整 ObjectCommit），但
/// validate 必须真实下载验证。事件声明合法却**没有真实密文对象** → 拒绝。
#[test]
fn validate_rejects_pending_asset_without_real_ciphertext() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "asset tail",
        &uuid_like(724),
        [0x75; 32],
    );
    // 结构合法（message_id 按根派生）、但没有上传任何 parts 的 ASSET 事件。
    let missing_object = [0xAB; 32];
    publish_asset_declaration(
        &mut relay,
        &materials.signing_key,
        &materials.root,
        missing_object,
    );
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect("prepare 只记录 pending 候选");
    assert_eq!(prepared.report.covered_seq, 1);
    assert_eq!(
        coverage_receipt_rows(&vault).len(),
        1,
        "prepare 必须把候选声明落成 pending receipt"
    );
    let err = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect_err("没有真实密文的 pending 候选必须拒绝");
    assert!(
        format!("{err}").contains("NOT_FOUND") || format!("{err}").contains("读回"),
        "{err}"
    );
    assert!(
        relay.published_checkpoints.is_empty(),
        "validate 失败不得 publish"
    );
}

/// 有持久 receipt 也不能绕开游标停滞：`next_cursor` 不前进 → 直接失败。
#[test]
fn validate_rejects_coverage_cursor_stall() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "stall",
        &uuid_like(725),
        [0x76; 32],
    );
    relay.publish_signed_asset(&materials.signing_key, [0xAC; 32]);
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect("prepare 落 receipt");
    assert_eq!(prepared.report.covered_seq, 1);
    relay.freeze_next_cursor = true;
    let err = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect_err("游标停滞必须拒绝");
    assert!(matches!(err, SyncError::Object(_)), "{err}");
    assert!(format!("{err}").contains("next_cursor"), "{err}");
}

/// 有持久 receipt 也不能绕开序号缺口：日志删掉中间事件 → 直接失败。
#[test]
fn validate_rejects_coverage_sequence_gap() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "gap",
        &uuid_like(726),
        [0x77; 32],
    );
    relay.publish_signed_asset(&materials.signing_key, [0xAD; 32]);
    relay.publish_signed_asset(&materials.signing_key, [0xAE; 32]);
    relay.publish_signed_asset(&materials.signing_key, [0xAF; 32]);
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect("prepare 落 receipt");
    assert_eq!(prepared.report.covered_seq, 3);
    // 删掉 seq=2，但 relay 仍报 head=3：从固定起点扫描必须发现序号缺口。
    relay.log.remove(1);
    let err = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect_err("序号缺口必须拒绝");
    assert!(matches!(err, SyncError::Object(_)), "{err}");
    assert!(format!("{err}").contains("序号缺口"), "{err}");
}

// ============================================================ 候选基线 coverage epoch
// 候选基线（E1→E2）在 E1 冻结点导出：checkpoint 属于 to_epoch=2，但 activate 前 relay
// 仍是 E1，日志事件的 epoch 也是 E1。coverage 扫描必须绑定 prepare 时冻结的 relay
// epoch，不能拿 checkpoint 的 to_epoch/key_epoch 去比。

const COVERAGE_RECEIPT_PREFIX: &str = "sync.checkpoint.coverage_receipt.";
const COVERAGE_RECEIPT_MAGIC: &[u8] = b"MN/cp-pending/3\0";

fn coverage_receipt_rows(vault: &menghunote_storage::Vault) -> Vec<(String, Vec<u8>)> {
    vault
        .query(
            "SELECT key,value FROM meta WHERE substr(key,1,?1)=?2 ORDER BY key",
            &[
                Value::Integer(COVERAGE_RECEIPT_PREFIX.len() as i64),
                Value::Text(COVERAGE_RECEIPT_PREFIX.to_owned()),
            ],
        )
        .expect("读 coverage receipt meta")
        .iter()
        .map(|row| {
            let key = match row.first() {
                Some(Value::Text(key)) => key.clone(),
                _ => panic!("coverage receipt key 类型非法"),
            };
            let value = match row.get(1) {
                Some(Value::Blob(value)) => value.clone(),
                _ => panic!("coverage receipt value 类型非法"),
            };
            (key, value)
        })
        .collect()
}

/// 改写 receipt 的 `coverage_epoch`，**不重算 HMAC**：task-122 起回执用 epoch 根
/// 派生的密钥认证，篡改者无法伪造（旧实现只有 SHA256 校验和，可重算）。
fn rewrite_coverage_epoch(bytes: &mut [u8], epoch: u64) {
    assert!(bytes.starts_with(COVERAGE_RECEIPT_MAGIC), "receipt magic");
    let epoch_at = COVERAGE_RECEIPT_MAGIC.len() + 16 + 16 + 16; // vault + cid + log
    assert!(bytes.len() >= epoch_at + 8 + 32, "receipt 太短");
    bytes[epoch_at..epoch_at + 8].copy_from_slice(&epoch.to_be_bytes());
}

/// E1→E2 候选基线夹具：relay 仍停在 E1（日志事件全是 E1），checkpoint 用 E2 根导出。
/// 本地没有该 ASSET 的 inbox 行 / durable skipped-asset 记录，只能靠 prepare receipt。
fn candidate_baseline_at_e1() -> (
    tempfile::TempDir,
    menghunote_storage::Vault,
    FakeRelay,
    genesis::GenesisMaterials,
    menghunote_sync::keys::RootSecretV1,
    menghunote_sync::checkpoint::PreparedCheckpoint,
) {
    let (dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    assert_eq!(relay.epoch, EPOCH, "relay 必须仍在 from_epoch");
    relay.allow_candidate_epoch = true;
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "candidate coverage",
        &uuid_like(730),
        [0x81; 32],
    );
    // E1 的 ASSET 缺口：engine 没消费过（无 skipped-asset 记录、无 inbox 行）。
    // 用**真实 E1 加密对象**（旧 active 根），validate 才能实证下载；伪造/随机 OID
    // 不得再被当作覆盖证据。
    let _ = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        b"candidate orphan asset",
        "text/plain",
    );
    let candidate_root = menghunote_sync::keys::RootSecretV1::new(
        VAULT_ID,
        2,
        menghunote_sync::keys::Secret32::from_array([0x91; 32]),
    );
    let prepared = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        candidate_root.clone(),
        materials.peer_id_u64(),
    )
    .prepare(&EmptyAssets)
    .expect("prepare 候选基线");
    (dir, vault, relay, materials, candidate_root, prepared)
}

/// E1→E2 候选（S 覆盖 relay E1 事件）必须能 validate：coverage 用的是 prepare 冻结
/// epoch=1，而不是 checkpoint 的 to_epoch=2；ASSET 缺口由 receipt 持久证明。
#[test]
fn validate_candidate_baseline_coverage_uses_prepare_frozen_epoch() {
    let (_dir, vault, mut relay, materials, candidate_root, prepared) = candidate_baseline_at_e1();
    assert_eq!(prepared.report.epoch, 2, "checkpoint 是 to_epoch 产物");
    assert_eq!(prepared.report.covered_seq, 1, "S 覆盖 relay 的 E1 事件");
    assert_eq!(
        coverage_receipt_rows(&vault).len(),
        1,
        "候选基线的 ASSET 缺口必须有持久 receipt（旧实现在 epoch 比较处静默跳过）"
    );
    let baseline = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        candidate_root,
        materials.peer_id_u64(),
    )
    .with_coverage_root(materials.root.clone())
    .validate(&prepared)
    .expect("候选基线必须通过 coverage（绑定 from_epoch + 旧根实证）");
    assert_eq!(
        baseline.coverage_verified_candidates(),
        1,
        "E1 orphan 资产必须被真实下载验证"
    );
    assert_eq!(baseline.epoch(), 2, "validate 产物仍是 to_epoch");
    assert_eq!(baseline.covered_seq(), 1);
}

/// 反例：把 receipt 的 `coverage_epoch` 改成 checkpoint 的 to_epoch（2）→ 必须拒绝。
#[test]
fn validate_candidate_baseline_rejects_tampered_coverage_epoch() {
    let (_dir, vault, mut relay, materials, candidate_root, prepared) = candidate_baseline_at_e1();
    let mut rows = coverage_receipt_rows(&vault);
    assert_eq!(rows.len(), 1, "prepare 必须已落 receipt");
    let (key, mut bytes) = rows.remove(0);
    // 篡改者把 epoch 改成 2（= 当前错误实现的比较口径）；没有 HMAC 密钥，无法重签。
    rewrite_coverage_epoch(&mut bytes, 2);
    set_meta_value(&vault, &key, bytes);
    let err = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        candidate_root,
        materials.peer_id_u64(),
    )
    .with_coverage_root(materials.root.clone())
    .validate(&prepared)
    .expect_err("coverage_epoch 被篡改必须拒绝");
    assert!(matches!(err, SyncError::Object(_)), "{err}");
    assert!(
        format!("{err}").contains("coverage receipt")
            || format!("{err}").contains("coverage_epoch"),
        "{err}"
    );
}

// ============================================================ 评审 0002 T04/T05 剩余
// `full_history` 标志位不是证据：Loro shallow snapshot 的 `oplog_vv` 仍是完整 VV，
// 只比较 VV/frontier 的 validate 会把它当成合法 full-history 基线。

fn shallow_snapshot_bytes() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    use loro::{ExportMode, LoroDoc};
    let source = LoroDoc::new();
    source.set_peer_id(7).expect("peer");
    let text = source.get_text("text");
    text.insert(0, "first history").expect("first");
    source.commit();
    let shallow_start = source.state_frontiers();
    text.insert(13, " second history").expect("second");
    source.commit();
    let bytes = source
        .export(ExportMode::shallow_snapshot(&shallow_start))
        .expect("shallow export");

    // 用独立空上下文导入，得到 shallow 快照**实际**可自报的 VV/frontier。
    let restored = LoroDoc::new();
    restored.import(&bytes).expect("import shallow");
    assert!(restored.is_shallow(), "前置：导出的必须是 shallow snapshot");
    (
        bytes,
        restored.oplog_vv().encode(),
        restored.state_frontiers().encode(),
    )
}

#[test]
fn full_history_snapshot_still_passes_integrity_check() {
    use menghunote_sync::checkpoint::KIND_BODY;
    use menghunote_sync_proto::SnapshotV1;
    let (_dir, vault, materials) = setup();
    let doc = seed_doc(&vault, "note-1", materials.peer_id_u64());
    doc.insert_scalars(0, "full history").expect("正文");
    let snapshot = SnapshotV1 {
        doc_id: "note-1".to_owned(),
        kind: KIND_BODY,
        generation: 0,
        codec_version: "loro-v1".to_owned(),
        app_doc_schema: 1,
        min_reader: 1,
        full_history: true,
        vv: doc.vv().encode(),
        frontier: doc.frontiers().encode(),
        loro_snapshot_bytes: doc.export_snapshot().expect("snapshot"),
    };
    menghunote_sync::rotation_stage::verify_snapshot_integrity(&snapshot)
        .expect("真实 full-history 快照必须通过");
}

#[test]
fn validate_rejects_shallow_snapshot_that_claims_full_history() {
    use menghunote_sync::upload::UploadedObject;
    use menghunote_sync_proto::{ManifestV1, ObjectType, SnapshotV1};
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    let mut prepared =
        prepared_with_note(&vault, &mut relay, &materials, &uuid_like(718), [0x78; 32]);
    let body_index = prepared
        .report
        .docs
        .iter()
        .position(|doc| doc.kind == KIND_BODY)
        .expect("body doc");
    let (shallow, shallow_vv, shallow_frontier) = shallow_snapshot_bytes();

    // 1) 把 body snapshot 换成 shallow 字节，并让自报 VV/frontier 与 shallow 实际一致。
    let snapshot_id = prepared.report.docs[body_index].object.object_id;
    let (_h, snapshot_bytes) = menghunote_sync::upload::fetch_object(
        &mut relay,
        &materials.root,
        ObjectType::Snapshot,
        &snapshot_id,
        prepared.report.docs[body_index].object.part_count,
        prepared.report.docs[body_index].object.total_plain_len,
        &prepared.report.docs[body_index].object.object_digest,
        &[],
    )
    .expect("读 snapshot");
    let mut snapshot = SnapshotV1::decode(&snapshot_bytes).expect("snapshot");
    snapshot.loro_snapshot_bytes = shallow;
    snapshot.vv = shallow_vv.clone();
    snapshot.frontier = shallow_frontier.clone();
    assert!(snapshot.full_history, "攻击面：标志位仍宣称 full-history");
    let snapshot_plain = snapshot.encode().expect("encode snapshot");
    let (snapshot_digest, snapshot_parts, snapshot_len) = replace_object(
        &mut relay,
        &materials.root,
        ObjectType::Snapshot,
        snapshot_id,
        prepared.report.docs[body_index].generation,
        &snapshot_plain,
    );
    prepared.report.docs[body_index].object = UploadedObject {
        object_id: snapshot_id,
        part_count: snapshot_parts,
        total_plain_len: snapshot_len,
        object_digest: snapshot_digest,
    };

    // 2) manifest 的 vv/frontier 同步改成 shallow 实际值，保证 VV 比较通过。
    let manifest_id = prepared.report.manifest.object_id;
    let (_h, manifest_bytes) = menghunote_sync::upload::fetch_object(
        &mut relay,
        &materials.root,
        ObjectType::Manifest,
        &manifest_id,
        prepared.report.manifest.part_count,
        prepared.report.manifest.total_plain_len,
        &prepared.report.manifest.object_digest,
        &[],
    )
    .expect("读 manifest");
    let mut manifest = ManifestV1::decode(&manifest_bytes).expect("manifest");
    manifest.docs[body_index].vv = shallow_vv;
    manifest.docs[body_index].frontier = shallow_frontier;
    manifest.docs[body_index].part_count = snapshot_parts;
    manifest.docs[body_index].total_plain_len = snapshot_len;
    manifest.docs[body_index].object_digest = snapshot_digest;
    let manifest_plain = manifest.encode().expect("encode manifest");
    let (manifest_digest, manifest_parts, manifest_len) = replace_object(
        &mut relay,
        &materials.root,
        ObjectType::Manifest,
        manifest_id,
        0,
        &manifest_plain,
    );
    prepared.report.manifest = UploadedObject {
        object_id: manifest_id,
        part_count: manifest_parts,
        total_plain_len: manifest_len,
        object_digest: manifest_digest,
    };
    prepared.report.refs_digest = recompute_refs_digest(&prepared.report);

    let mut writer = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    let result = writer.validate(&prepared);
    assert!(
        result.is_err(),
        "shallow snapshot 即使 full_history=true 且 VV/frontier 自洽，也必须被拒绝"
    );
    assert!(!relay.checkpoint.as_ref().expect("lease").published);
    assert!(relay.published_checkpoints.is_empty());
}

// ============================================================ 评审 0003 P0-B 残余
// ConsumedAsset 推断删除后：checkpoint 只认 engine pull 落盘的 durable
// skipped-asset 记录；隐藏 UPDATE / 篡改 seq/object_id / 伪造记录一律失败。

/// 用 genesis 材料构造 engine 并 pull 一页。
///
/// engine 与 checkpoint writer 必须共用设备签名密钥：skipped-asset 记录的 HMAC 由
/// 该密钥派生，validate 才能验证。
fn engine_pull(
    vault: &menghunote_storage::Vault,
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) {
    let identity = EngineIdentity {
        device_id: materials.device_id,
        signing_key: materials.signing_key.clone(),
        peer_id: materials.peer_id_u64(),
    };
    let mut sync = SyncEngine::new(vault, relay, identity, materials.root.clone());
    sync.pull().expect("engine pull");
}

fn skipped_asset_meta_rows(vault: &menghunote_storage::Vault) -> Vec<(String, Vec<u8>)> {
    vault
        .query(
            "SELECT key,value FROM meta WHERE substr(key,1,?1)=?2 ORDER BY key",
            &[
                Value::Integer("sync.skipped_asset.".len() as i64),
                Value::Text("sync.skipped_asset.".to_owned()),
            ],
        )
        .expect("读 skipped-asset meta")
        .iter()
        .map(|row| {
            let key = match row.first() {
                Some(Value::Text(key)) => key.clone(),
                _ => panic!("skipped-asset key 类型非法"),
            };
            let value = match row.get(1) {
                Some(Value::Blob(value)) => value.clone(),
                _ => panic!("skipped-asset value 类型非法"),
            };
            (key, value)
        })
        .collect()
}

fn set_meta_value(vault: &menghunote_storage::Vault, key: &str, value: Vec<u8>) {
    assert_eq!(
        vault
            .exec(
                "UPDATE meta SET value=?1 WHERE key=?2",
                &[Value::Blob(value), Value::Text(key.to_owned())],
            )
            .expect("改 meta value"),
        1
    );
}

fn rename_meta_key(vault: &menghunote_storage::Vault, old: &str, new: &str) {
    assert_eq!(
        vault
            .exec(
                "UPDATE meta SET key=?1 WHERE key=?2",
                &[Value::Text(new.to_owned()), Value::Text(old.to_owned())],
            )
            .expect("改 meta key"),
        1
    );
}

/// 正常 ASSET：engine 落 record、游标推进，validate 必须能靠该记录覆盖 S。
#[test]
fn validate_accepts_durable_skipped_asset_record_before_cursor() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    let _ = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        b"checkpoint durable skipped asset",
        "text/plain",
    );
    engine_pull(&vault, &mut relay, &materials);
    assert_eq!(
        vault.get_meta(RELAY_CURSOR_META).expect("读游标"),
        Some(b"c1".to_vec()),
        "ASSET 消费后游标必须推进"
    );
    assert_eq!(skipped_asset_meta_rows(&vault).len(), 1);

    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect("prepare");
    assert_eq!(prepared.report.covered_seq, 1);
    let baseline = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect("durable skipped-asset 记录必须能证明覆盖 S");
    assert_eq!(baseline.covered_seq(), 1);
}

/// 恶意/有 bug 的 relay：隐藏 seq=1 的 UPDATE，只返回 seq=2 的 ASSET 并把游标推到 c2。
/// seq=1 没有 durable record（engine 从未看到它），必须失败。
#[test]
fn validate_rejects_hidden_update_gap_without_skipped_asset_record() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    // 真实 ASSET 事件占据 seq=2；seq=1 的 UPDATE 被恶意 relay 隐藏。
    let _ = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        b"hidden gap real asset",
        "text/plain",
    );
    let event = relay.log.last_mut().expect("asset event");
    event.relay_seq = 2;
    relay.head_seq = 2;
    engine_pull(&vault, &mut relay, &materials);
    assert_eq!(
        vault.get_meta(RELAY_CURSOR_META).expect("读游标"),
        Some(b"c2".to_vec()),
        "relay 把游标推到 c2：seq=1 的缺口在游标之前"
    );
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect("prepare");
    assert_eq!(prepared.report.covered_seq, 2);
    let err = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect_err("没有 durable record 的游标前缺口必须失败");
    assert!(matches!(err, SyncError::Object(_)), "{err}");
    assert!(format!("{err}").contains("覆盖到 S"), "{err}");
    assert!(
        relay.published_checkpoints.is_empty(),
        "validate 失败不得 publish"
    );
}

/// 篡改 record 的 object_id（值被改、HMAC 不符）→ validate 失败。
#[test]
fn validate_rejects_tampered_skipped_asset_object_id() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    let (object_id, _) = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        b"tampered object id asset",
        "text/plain",
    );
    engine_pull(&vault, &mut relay, &materials);

    let (key, mut value) = skipped_asset_meta_rows(&vault)
        .into_iter()
        .next()
        .expect("record");
    let offset = value
        .windows(32)
        .position(|window| window == object_id)
        .expect("record 里必须能找到 object_id");
    value[offset] ^= 0x01;
    set_meta_value(&vault, &key, value);

    let err = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect_err("篡改 object_id 的 record 必须显式失败");
    assert!(matches!(err, SyncError::LocalCorrupt(_)), "{err}");
    assert!(format!("{err}").contains("skipped-asset"), "{err}");
}

/// 篡改 record 的 relay_seq（把 seq=1 的键改成 seq=2）→ 键与值/日志不一致，validate 失败。
#[test]
fn validate_rejects_tampered_skipped_asset_seq() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    let _ = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        b"tampered seq asset",
        "text/plain",
    );
    engine_pull(&vault, &mut relay, &materials);

    let (key, _) = skipped_asset_meta_rows(&vault)
        .into_iter()
        .next()
        .expect("record");
    let tampered = key.replace("00000000000000000001", "00000000000000000002");
    assert_ne!(tampered, key);
    rename_meta_key(&vault, &key, &tampered);

    let err = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect_err("篡改 seq 的 record 必须显式失败");
    assert!(matches!(err, SyncError::LocalCorrupt(_)), "{err}");
    assert!(format!("{err}").contains("skipped-asset"), "{err}");
}

/// 伪造一条结构看似完整的 record（没有 engine 消费事实、HMAC 也不对）→ 失败。
#[test]
fn validate_rejects_forged_skipped_asset_record() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    // relay 报 head=1 但隐藏全部事件；伪造的 seq=1 record 不能补这个缺口。
    relay.head_seq = 1;
    let key = format!(
        "sync.skipped_asset.{}.{:020}",
        menghunote_sync::base64url::encode(&relay.log_id),
        1u64
    );
    assert_eq!(
        vault
            .exec(
                "INSERT INTO meta(key,value) VALUES(?1,?2)",
                &[Value::Text(key), Value::Blob(vec![0xAB; 96])],
            )
            .expect("写伪造 record"),
        1
    );
    let err = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect_err("伪造 record 必须显式失败");
    assert!(matches!(err, SyncError::LocalCorrupt(_)), "{err}");
    assert!(format!("{err}").contains("skipped-asset"), "{err}");
}

// ============================================================ task-118 阻断 B
// coverage 证据必须实证：pending 候选只有真实下载验证后才能合并覆盖区间；
// writer 侧不再把 relay 的 ASSET 声明当成覆盖证明。

const VERIFIED_RECEIPT_PREFIX: &str = "sync.checkpoint.verified_receipt.";

fn verified_receipt_rows(vault: &menghunote_storage::Vault) -> Vec<String> {
    vault
        .query(
            "SELECT key FROM meta WHERE substr(key,1,?1)=?2 ORDER BY key",
            &[
                Value::Integer(VERIFIED_RECEIPT_PREFIX.len() as i64),
                Value::Text(VERIFIED_RECEIPT_PREFIX.to_owned()),
            ],
        )
        .expect("读 verified receipt meta")
        .iter()
        .map(|row| match row.first() {
            Some(Value::Text(key)) => key.clone(),
            _ => panic!("verified receipt key 类型非法"),
        })
        .collect()
}

/// 场景 ①：恶意 relay 抬高 lease 的 S，并把真实 UPDATE 位置伪报成**没有真实密文**
/// 的 ASSET（prepare/validate 看到稳定声明）。validate 必须失败，零成功 receipt /
/// VerifiedBaseline / publish，cursor 不变。
#[test]
fn validate_rejects_asset_masking_update_without_side_effects() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    let log_id = relay.log_id;
    vault
        .set_meta(META_SYNC_LOG_ID, &log_id)
        .expect("写 log_id");
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "masked update",
        &uuid_like(740),
        [0xE0; 32],
    );
    // seq=1 本应是 UPDATE；relay 伪造成结构自洽、但没有真实密文的 ASSET。
    let forged_object = [0xE1; 32];
    publish_asset_declaration(
        &mut relay,
        &materials.signing_key,
        &materials.root,
        forged_object,
    );
    let cursor_before = vault
        .get_meta(RELAY_CURSOR_META)
        .expect("读游标")
        .expect("游标存在");
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect("prepare 只记录 pending 候选");
    assert_eq!(prepared.report.covered_seq, 1);
    assert_eq!(
        coverage_receipt_rows(&vault).len(),
        1,
        "prepare 必须记录 pending 候选（不是成功覆盖证明）"
    );
    let error = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect_err("没有真实密文的伪 ASSET 不能让切点成立");
    assert!(
        format!("{error}").contains("NOT_FOUND") || format!("{error}").contains("读回"),
        "{error}"
    );
    assert!(
        relay.published_checkpoints.is_empty(),
        "validate 失败不得 publish 任何 VerifiedBaseline"
    );
    assert!(
        !relay.checkpoint.as_ref().expect("lease").published,
        "启动的候选 checkpoint 必须保持未发布"
    );
    assert_eq!(
        vault
            .get_meta(RELAY_CURSOR_META)
            .expect("读游标")
            .expect("游标存在"),
        cursor_before,
        "validate 失败不得推进 cursor"
    );
    assert!(
        verified_receipt_rows(&vault).is_empty(),
        "validate 失败不得写成功 verified receipt"
    );
}

/// 场景 ②：refs 内资产 + 不在 refs 的 orphan 资产都必须实证；refs 内资产在
/// validate 中只读回一次（coverage 精确命中正缓存）。
#[test]
fn validate_verifies_orphan_asset_and_reads_refs_asset_once() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    let refs_data = b"refs asset bytes";
    let (refs_object, _) = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        refs_data,
        "text/plain",
    );
    let orphan_data = b"orphan asset bytes";
    let (orphan_object, _) = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        orphan_data,
        "text/plain",
    );
    let assets =
        StaticAssets::new(vec![("text/plain".to_owned(), refs_data.to_vec())]).expect("静态资产源");
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&assets)
        .expect("prepare");
    assert_eq!(prepared.report.covered_seq, 2);
    let baseline = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect("refs/orphan 资产都必须真实验证");
    assert_eq!(
        baseline.coverage_verified_candidates(),
        2,
        "coverage 候选必须逐条实证（含不在 refs 的 orphan）"
    );
    assert_eq!(
        relay.part_get_count(&refs_object),
        1,
        "refs 内资产只读回一次：coverage 必须命中完整声明正缓存"
    );
    assert_eq!(
        relay.part_get_count(&orphan_object),
        1,
        "orphan 资产必须真实下载一次"
    );
    let promoted = checkpoint_writer(&vault, &mut relay, &materials)
        .promote(&baseline)
        .expect("publish");
    assert!(promoted.published_at > 0);
}

/// task-118 复核（非阻断建议 ①）：inbox 行的 OID 恰好命中 manifest 资产集合
/// **不是**覆盖证据。
///
/// 恶意成员可以伪造一条 `object_id` 指向某个被导出资产的合法 UPDATE；旧实现见到
/// OID 属于资产集合就直接把该 seq 判为 proven，跳过 `UpdateV1.end_vv` 与导出快照
/// actual VV 的绑定。这里让该 UPDATE 的 end_vv 明确不被快照覆盖：必须拒绝，
/// 不得靠"OID 看起来像资产"放行。
#[test]
fn validate_rejects_inbox_row_using_manifest_asset_oid_without_vv_binding() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    let log_id = relay.log_id;
    vault
        .set_meta(META_SYNC_LOG_ID, &log_id)
        .expect("写 log_id");
    add_note(
        &vault,
        "note-1",
        materials.peer_id_u64(),
        "asset oid inbox",
        &uuid_like(724),
        [0x76; 32],
    );
    // 真实导出的资产：manifest 会带同一个 OID。
    let asset_data = b"manifest exported asset bytes";
    real_asset(&vault, "note-1", asset_data);
    let (asset_object_id, _) = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        asset_data,
        "text/plain",
    );
    // 在 prepare 之前伪造 inbox 行：OID=资产 OID，但载荷是另一条未被导出快照
    // 覆盖的 UPDATE。故意不推进 integrated_seq，避免连续前缀掩盖单行判定。
    let remote = remote_update_for("note-1", 0x42, "remote not covered", &uuid_like(725));
    vault
        .exec(
            "INSERT INTO sync_inbox(log_id,relay_seq,object_id,epoch,state,verified_payload) \
             VALUES(?1,1,?2,?3,'integrated',?4)",
            &[
                Value::Blob(log_id.to_vec()),
                Value::Blob(asset_object_id.to_vec()),
                Value::Integer(i64::try_from(EPOCH).expect("epoch")),
                Value::Blob(remote.encode().expect("encode update")),
            ],
        )
        .expect("写伪造 inbox 行");

    let assets = StaticAssets::new(vec![("text/plain".to_owned(), asset_data.to_vec())])
        .expect("静态资产源");
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&assets)
        .expect("prepare");
    assert_eq!(prepared.report.covered_seq, 1);
    assert_eq!(
        prepared.report.assets[0].object.object_id, asset_object_id,
        "夹具前提：inbox OID 必须恰好等于 manifest 资产 OID"
    );
    let error = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect_err("资产 OID 不能替代 actual VV 绑定");
    assert!(matches!(error, SyncError::Object(_)), "{error}");
    assert!(
        format!("{error}").contains("导出快照覆盖"),
        "必须由 actual VV 绑定拒绝，而不是默认 proven：{error}"
    );
    assert!(
        !relay.checkpoint.as_ref().expect("lease").published,
        "validate 失败不得 publish"
    );
}

/// 场景 ③：E1 tail + E2 候选基线必须用旧 active 根实证；缺旧根不得猜根。
#[test]
fn candidate_baseline_without_coverage_root_pauses() {
    let (_dir, vault, mut relay, materials, candidate_root, prepared) = candidate_baseline_at_e1();
    let error = CheckpointWriter::new(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        candidate_root,
        materials.peer_id_u64(),
    )
    .validate(&prepared)
    .expect_err("候选基线缺旧 active 根必须暂停，不能拿新根猜");
    assert!(matches!(error, SyncError::Paused(_)), "{error}");
    assert!(
        relay.published_checkpoints.is_empty(),
        "缺旧根的失败不得 publish"
    );
}

/// 场景 ④：coverage 候选的密文错误（多片缺失 / AEAD 篡改 / OID 与明文头不符）
/// 都必须拒绝——validate 不再把"日志里有 ASSET 声明"当成覆盖证明。
#[test]
fn validate_rejects_bad_coverage_asset_ciphertexts() {
    // (a) 多片资产的第二片缺失。
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    let big = vec![0x5a_u8; 4 * 1024 * 1024 + 1024];
    let (big_object, _) = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        &big,
        "application/octet-stream",
    );
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect("prepare");
    assert!(
        relay.parts.remove(&(big_object, 1)).is_some(),
        "多片资产必须有第二片"
    );
    let error = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect_err("缺片必须拒绝");
    assert!(format!("{error}").contains("NOT_FOUND"), "{error}");

    // (b) 密文被篡改（AEAD/摘要失败）。
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    let _ = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        b"tampered coverage asset",
        "text/plain",
    );
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect("prepare");
    relay.tamper_parts = true;
    let error = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect_err("密文篡改必须拒绝");
    assert!(
        format!("{error}").contains("摘要")
            || format!("{error}").contains("AEAD")
            || format!("{error}").contains("magic"),
        "{error}"
    );

    // (c) 明文头与声明 OID 不符（canonical asset_object_id 重算失败）。
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    let data = b"oid mismatch data";
    let real: [u8; 32] = Sha256::digest(data).into();
    let plaintext = asset_plaintext_bytes(&real, data.len() as u64, "text/plain", data);
    let forged_object = [0x77; 32];
    let _ = upload_asset_plaintext(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        forged_object,
        &plaintext,
    );
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect("prepare");
    let error = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect_err("OID 与明文头不符必须拒绝");
    assert!(format!("{error}").contains("object_id"), "{error}");
}

/// 场景 ⑥：重复序号 / 候选声明变化必须拒绝；lease 后诚实追加（head>S）不得改变
/// 已固定的 S。
#[test]
fn coverage_scan_rejects_duplicates_and_declaration_changes() {
    // (a) 重复序号。
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    relay.publish_signed_asset(&materials.signing_key, [0xA1; 32]);
    relay.publish_signed_asset(&materials.signing_key, [0xA2; 32]);
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect("prepare 候选");
    relay.duplicate_log_event(0);
    let error = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect_err("重复序号必须拒绝");
    assert!(format!("{error}").contains("乱序"), "{error}");

    // (b) prepare 之后候选声明被改写（同一 seq 不同 commit）。
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    let _ = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        b"declaration change asset",
        "text/plain",
    );
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect("prepare 候选");
    tamper_last_asset_commit(&mut relay, |commit| {
        commit.object_digest[0] ^= 0x01;
    });
    let error = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect_err("候选声明变化必须拒绝");
    assert!(format!("{error}").contains("声明变化"), "{error}");
}

/// 场景 ⑥（正例）：lease 固定 S 后 relay 诚实追加 head>S，S 不变时仍成功。
#[test]
fn honest_append_after_lease_keeps_s_and_validates() {
    let (_dir, vault, materials) = setup();
    let mut relay = relay_with(&materials);
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    let _ = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        b"before lease",
        "text/plain",
    );
    let prepared = checkpoint_writer(&vault, &mut relay, &materials)
        .prepare(&EmptyAssets)
        .expect("prepare");
    assert_eq!(
        prepared.report.covered_seq, 1,
        "S 必须原样等于 lease 返回值"
    );
    let _ = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        b"honest append after lease",
        "text/plain",
    );
    assert!(relay.head_seq > prepared.report.covered_seq);
    let baseline = checkpoint_writer(&vault, &mut relay, &materials)
        .validate(&prepared)
        .expect("lease 后诚实追加必须允许（S 不变）");
    assert_eq!(baseline.covered_seq(), 1, "S 不得被 head 抬高");
    assert_eq!(baseline.coverage_verified_candidates(), 1);
}
