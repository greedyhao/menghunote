//! S7-18：新设备从 relay 全量 bootstrap 的集成测试（FakeRelay + 真 SQLite）。

mod support;

use support::*;

use menghunote_storage::{
    body_doc_meta, create_note_slot, ensure_structure_row, insert_asset, load_body_doc, BodyDoc,
};
use menghunote_sync::bootstrap;
use menghunote_sync::checkpoint::{CheckpointWriter, SlotSecrets, StorageAssets};
use menghunote_sync::genesis;
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
        [0x92; 16],
        "pw",
        1_700_000_000,
        &mut TestRandom::new(0x92_18),
    )
    .expect("genesis 材料");
    (dir, vault, materials)
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

fn seed_source(
    source: &menghunote_storage::Vault,
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) {
    add_note(
        source,
        "note-1",
        materials.peer_id_u64(),
        "第一行正文",
        &uuid_like(901),
        [0xA1; 32],
    );
    add_note(
        source,
        "note-2",
        materials.peer_id_u64(),
        "second note",
        &uuid_like(902),
        [0xA2; 32],
    );
    // 无 notes 行的墓碑文档（checkpoint 必须导出，bootstrap 必须恢复）。
    let orphan = seed_doc(source, "orphan-body", materials.peer_id_u64());
    orphan.insert_scalars(0, "orphan text").expect("墓碑正文");
    commit_local(source, &orphan);
    let data = b"bootstrap real asset";
    let hash: [u8; 32] = Sha256::digest(data).into();
    insert_asset(source, &hash, data.len() as u64, Some("text/plain"), data).expect("资产");

    let mut writer = CheckpointWriter::new(
        source,
        relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    let secrets = SlotSecrets {
        password: Some("pw"),
        recovery_seed: Some(&materials.recovery_seed),
    };
    writer
        .upload_initial_slots(&materials.password_slot, &materials.recovery_slot, &secrets)
        .expect("槽上传");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("checkpoint 1");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("checkpoint 2");
}

fn flush_pending_and_checkpoint(
    source: &menghunote_storage::Vault,
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) {
    source
        .set_meta(menghunote_sync::engine::RELAY_CURSOR_META, b"c0")
        .expect("cursor");
    source
        .set_meta(menghunote_storage::META_SYNC_RECEIVED_SEQ, &[0u8; 8])
        .expect("received");
    source
        .set_meta(menghunote_storage::META_SYNC_INTEGRATED_SEQ, &[0u8; 8])
        .expect("integrated");
    let mut engine = menghunote_sync::engine::SyncEngine::new(
        source,
        relay,
        menghunote_sync::engine::EngineIdentity {
            device_id: materials.device_id,
            signing_key: materials.signing_key.clone(),
            peer_id: materials.peer_id_u64(),
        },
        materials.root.clone(),
    );
    engine.sync_once().expect("flush seed updates");
    let mut writer = CheckpointWriter::new(
        source,
        relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    writer
        .create_checkpoint(&StorageAssets)
        .expect("checkpoint after flush");
}

#[test]
fn bootstrap_restores_all_docs_and_assets() {
    let (_dir, source, materials) = setup();
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    seed_source(&source, &mut relay, &materials);
    assert_eq!(relay.published_checkpoints.len(), 2);

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap");
    assert_eq!(
        report.covered_seq,
        relay.published_checkpoints[1].covered_seq
    );
    assert!(!report.used_fallback);
    assert!(report.docs.iter().any(|(doc_id, _)| doc_id == "note-1"));
    assert!(report
        .docs
        .iter()
        .any(|(doc_id, _)| doc_id == "orphan-body"));
    assert_eq!(report.assets, 1);
    let note1 = load_body_doc(&target, "note-1", materials.peer_id_u64(), None)
        .expect("note-1")
        .0;
    assert_eq!(note1.text(), "第一行正文");
    let orphan = load_body_doc(&target, "orphan-body", materials.peer_id_u64(), None)
        .expect("orphan")
        .0;
    assert_eq!(orphan.text(), "orphan text");
    assert!(body_doc_meta(&target, "note-2").expect("meta").is_some());
    let asset_hash: [u8; 32] = Sha256::digest(b"bootstrap real asset").into();
    let asset = menghunote_storage::asset_bytes(&target, asset_hash)
        .expect("asset_bytes")
        .expect("资产存在");
    assert_eq!(asset, b"bootstrap real asset");
    // 水位/cursor 已写入：engine 之后从 S 继续，不会重复导入。
    assert_eq!(
        target
            .get_meta(menghunote_sync::engine::RELAY_CURSOR_META)
            .expect("cursor meta")
            .as_deref(),
        Some(report.cursor.as_bytes())
    );
    assert_eq!(
        target
            .get_meta(menghunote_storage::META_SYNC_RECEIVED_SEQ)
            .expect("received")
            .as_deref(),
        Some(report.covered_seq.to_be_bytes().as_slice())
    );
}

#[test]
fn bootstrap_cursor_pulls_updates_after_s() {
    let (_dir, source, materials) = setup();
    let (_target_dir, target) = fresh_vault();
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let s = relay
        .published_checkpoints
        .last()
        .expect("checkpoint")
        .covered_seq;
    assert_eq!(relay.head_seq, s, "checkpoint 之后不应再有日志事件");

    // 源在 checkpoint 之后给已有 note-1 追加 1 条 update 并推到 relay。
    let (doc, _) = load_body_doc(&source, "note-1", materials.peer_id_u64(), None).expect("note-1");
    doc.insert_scalars(0, "after checkpoint ").expect("正文");
    commit_with_intent(&source, &doc, &uuid_like(990), [0xB1; 32], EPOCH);
    {
        let mut engine = menghunote_sync::engine::SyncEngine::new(
            &source,
            &mut relay,
            menghunote_sync::engine::EngineIdentity {
                device_id: materials.device_id,
                signing_key: materials.signing_key.clone(),
                peer_id: materials.peer_id_u64(),
            },
            materials.root.clone(),
        );
        let pushed = engine.sync_once().expect("source push after checkpoint");
        assert_eq!(pushed.push.uploaded, 1);
    }
    assert!(relay.head_seq > s);

    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap at S");
    assert_eq!(report.covered_seq, s);
    {
        let mut engine = menghunote_sync::engine::SyncEngine::new(
            &target,
            &mut relay,
            menghunote_sync::engine::EngineIdentity {
                device_id: materials.device_id,
                signing_key: materials.signing_key.clone(),
                peer_id: materials.peer_id_u64(),
            },
            materials.root.clone(),
        );
        let pulled = engine.sync_once().expect("target catch up after S");
        assert!(pulled.pull.events >= 1, "必须拉到 S 之后的 update");
    }
    let (doc, _) = load_body_doc(&target, "note-1", materials.peer_id_u64(), None).expect("note-1");
    assert!(
        doc.text().contains("after checkpoint"),
        "S 之后的 update 必须被 integrate，实际={:?}",
        doc.text()
    );
}

#[test]
fn bootstrap_is_idempotent_on_second_run() {
    let (_dir, source, materials) = setup();
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    seed_source(&source, &mut relay, &materials);
    let (_target_dir, target) = fresh_vault();
    let first = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("第一次 bootstrap");
    let second = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("第二次 bootstrap");
    assert_eq!(first.covered_seq, second.covered_seq);
    let (note1, _) =
        load_body_doc(&target, "note-1", materials.peer_id_u64(), None).expect("note-1");
    assert_eq!(note1.text(), "第一行正文");
}

#[test]
fn fallback_is_used_when_primary_manifest_is_tampered() {
    let (_dir, source, materials) = setup();
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let fallback = relay
        .published_checkpoints
        .get(relay.published_checkpoints.len() - 2)
        .expect("fallback checkpoint")
        .clone();
    let primary = relay
        .published_checkpoints
        .last()
        .and_then(|checkpoint| checkpoint.manifest_ref)
        .expect("primary manifest");
    assert_ne!(fallback.covered_seq, relay.head_seq);
    let bytes = relay.parts.get_mut(&(primary, 0)).expect("manifest part");
    bytes[0] ^= 0x01;

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback.covered_seq);
    assert_eq!(report.cursor, format!("c{}", fallback.covered_seq));
    assert_eq!(
        target
            .get_meta(menghunote_storage::META_SYNC_RECEIVED_SEQ)
            .expect("received meta"),
        Some(fallback.covered_seq.to_be_bytes().to_vec())
    );
    assert_eq!(
        target
            .get_meta(menghunote_storage::META_SYNC_INTEGRATED_SEQ)
            .expect("integrated meta"),
        Some(fallback.covered_seq.to_be_bytes().to_vec())
    );
    assert!(load_body_doc(&target, "note-1", materials.peer_id_u64(), None).is_ok());
}

#[test]
fn fallback_is_used_when_primary_snapshot_is_tampered() {
    let (_dir, source, materials) = setup();
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let fallback = relay
        .published_checkpoints
        .get(relay.published_checkpoints.len() - 2)
        .expect("fallback checkpoint")
        .clone();
    let primary_snapshot = relay
        .published_checkpoints
        .last()
        .and_then(|checkpoint| checkpoint.data_refs.first().copied())
        .expect("primary snapshot");
    relay
        .parts
        .get_mut(&(primary_snapshot, 0))
        .expect("snapshot part")[0] ^= 0x01;

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("fallback after snapshot tamper");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback.covered_seq);
    assert_eq!(report.cursor, format!("c{}", fallback.covered_seq));
}

#[test]
fn existing_local_doc_is_merged_not_skipped() {
    let (_dir, source, materials) = setup();
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    seed_source(&source, &mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    // 本机 peer 必须与快照生产者不同，否则同 peer 的 op 会被 Loro 当作重复丢弃。
    let local_peer = materials.peer_id_u64().wrapping_add(1);
    let local = seed_doc(&target, "note-1", local_peer);
    local.insert_scalars(0, "local edit ").expect("本地正文");
    commit_local(&target, &local);

    let report = bootstrap::run(&target, &mut relay, VAULT_ID, local_peer, &materials.root)
        .expect("bootstrap with existing doc");
    assert!(!report.used_fallback);
    let (merged, _) = load_body_doc(&target, "note-1", local_peer, None).expect("note-1");
    assert!(
        merged.text().contains("local edit") && merged.text().contains("第一行正文"),
        "已有 doc 必须按 VV 合并，实际={:?}",
        merged.text()
    );
}

#[test]
fn tampered_manifest_on_both_baselines_writes_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    seed_source(&source, &mut relay, &materials);
    for checkpoint in &relay.published_checkpoints {
        if let Some(manifest) = checkpoint.manifest_ref {
            if let Some(bytes) = relay.parts.get_mut(&(manifest, 0)) {
                bytes[0] ^= 0x01;
            }
        }
    }
    let (_target_dir, target) = fresh_vault();
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("篡改 manifest 必须失败");
    assert!(
        menghunote_storage::list_all_docs(&target)
            .expect("list_all_docs")
            .is_empty(),
        "校验失败前不得写半库: {error:?}"
    );
}

#[test]
fn tampered_snapshot_is_rejected_without_partial_import() {
    let (_dir, source, materials) = setup();
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    seed_source(&source, &mut relay, &materials);
    // 同时篡改两份基线的第一个 SNAPSHOT 对象，确保 fallback 也失败。
    for checkpoint in &relay.published_checkpoints {
        for object_id in &checkpoint.data_refs {
            if let Some(bytes) = relay.parts.get_mut(&(*object_id, 0)) {
                bytes[0] ^= 0x01;
            }
        }
    }
    let (_target_dir, target) = fresh_vault();
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("篡改快照必须失败");
    assert!(
        menghunote_storage::list_all_docs(&target)
            .expect("list_all_docs")
            .is_empty(),
        "失败不得写半库: {error:?}"
    );
}

#[test]
fn wrong_root_rejects_bootstrap_before_write() {
    let (_dir, source, materials) = setup();
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    seed_source(&source, &mut relay, &materials);
    let wrong = menghunote_sync::keys::RootSecretV1::new(
        VAULT_ID,
        EPOCH,
        menghunote_sync::keys::Secret32::from_array([0x5A; 32]),
    );
    let (_target_dir, target) = fresh_vault();
    assert!(bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &wrong
    )
    .is_err());
    assert!(menghunote_storage::list_all_docs(&target)
        .expect("list_all_docs")
        .is_empty());
}

#[test]
fn bootstrap_body_doc_head_is_one_full_update() {
    let (_dir, source, materials) = setup();
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    seed_source(&source, &mut relay, &materials);
    let (_target_dir, target) = fresh_vault();
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap");
    let rows = menghunote_storage::doc_updates_since(&target, "note-1", 0).expect("updates");
    assert_eq!(rows.len(), 1, "bootstrap 只写 1 条全量 update");
    assert_eq!(rows[0].seq, 1);
    // 不是 snapshot 行：snapshot_seq 必须保持 0（后续优化再补）。
    let meta = body_doc_meta(&target, "note-1")
        .expect("meta")
        .expect("row");
    assert_eq!(meta.head_seq, 1);
}

#[allow(dead_code)]
fn _keep_body_doc(_: BodyDoc) {}
