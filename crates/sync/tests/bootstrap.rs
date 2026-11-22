//! S7-18：新设备从 relay 全量 bootstrap 的集成测试（FakeRelay + 真 SQLite/Loro）。
//!
//! 覆盖评审 0002 P1-4：
//! - body / structure / asset 各阶段失败或"崩溃"后**不推进水位**、journal 可续跑；
//! - 水位 CAS 失败不写半套单调值；
//! - 已有库的 pins/highwater 不被 bootstrap 降低（highwater = max(现有, S)）；
//! - fallback 只用自己的 S/cursor；
//! - 实际 VV/frontier 校验由 `bootstrap.rs` 单测覆盖（不只信 manifest 声明）。
//!
//! 全程无 sleep：失败点由 `BootstrapFault` 或真实 CAS 冲突制造，收敛点都是显式返回值。

mod support;

use support::*;

use menghunote_storage::{
    asset_bytes, baseline_apply_state, body_doc_meta, create_note_slot, ensure_structure_row,
    insert_asset, list_all_docs, list_notes, load_body_doc, sync_cursor_state, BaselineApplyStatus,
    BodyDoc, StructureWriter, Value, META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ,
    META_SYNC_RECEIVED_SEQ, STRUCTURE_DOC_ID,
};
use menghunote_sync::bootstrap::{
    self, control_head_hash, BootstrapFault, BootstrapPins, META_BOOTSTRAP_CONTROL_HASH,
    META_BOOTSTRAP_CONTROL_REV, META_BOOTSTRAP_EPOCH, META_BOOTSTRAP_INSTANCE,
};
use menghunote_sync::checkpoint::{CheckpointWriter, SlotSecrets, StorageAssets};
use menghunote_sync::engine::{EngineIdentity, SyncEngine, RELAY_CURSOR_META};
use menghunote_sync::genesis;
use menghunote_sync::keys::RootSecretV1;
use menghunote_sync::relay::RelayClient as _;
use menghunote_sync_proto::ObjectType;
use sha2::{Digest, Sha256};

const STRUCTURED_TEXT: &str = "结构化笔记正文";
const ASSET_DATA: &[u8] = b"bootstrap real asset";

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

fn new_relay(materials: &genesis::GenesisMaterials) -> FakeRelay {
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    relay
}

fn seed_source(
    source: &menghunote_storage::Vault,
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) {
    // 真实设备在 genesis 后已有 relay 游标；checkpoint validate 依赖它核对 S 之前的日志。
    source
        .set_meta(RELAY_CURSOR_META, b"c0")
        .expect("写 relay 游标");
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
    let hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    insert_asset(
        source,
        &hash,
        ASSET_DATA.len() as u64,
        Some("text/plain"),
        ASSET_DATA,
    )
    .expect("资产");

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
    source.set_meta(RELAY_CURSOR_META, b"c0").expect("cursor");
    source
        .set_meta(META_SYNC_RECEIVED_SEQ, &[0u8; 8])
        .expect("received");
    source
        .set_meta(META_SYNC_INTEGRATED_SEQ, &[0u8; 8])
        .expect("integrated");
    let mut engine = SyncEngine::new(
        source,
        relay,
        EngineIdentity {
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

/// 造一份"结构文档 + 一条已推到 relay 的正文"的源；返回正文 doc id。
///
/// relay head 推到 1 后打两份 checkpoint（S=1），让"水位是否推进"可观察。
fn seed_structured_source(
    source: &menghunote_storage::Vault,
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) -> String {
    let peer = materials.peer_id_u64();
    source
        .set_meta(RELAY_CURSOR_META, b"c0")
        .expect("写 relay 游标");
    source
        .set_meta(META_SYNC_RECEIVED_SEQ, &[0u8; 8])
        .expect("received");
    source
        .set_meta(META_SYNC_INTEGRATED_SEQ, &[0u8; 8])
        .expect("integrated");

    let mut writer = StructureWriter::open(source, peer).expect("structure writer");
    let seed = writer
        .doc()
        .create_note("结构标题", 1_700_000_000)
        .expect("结构笔记");
    writer.commit("structure").expect("结构提交");
    let body_doc_id = seed.body_doc_id.clone();
    let doc = BodyDoc::restore(body_doc_id.clone(), peer, None, &[]).expect("正文 doc");
    doc.insert_scalars(0, STRUCTURED_TEXT).expect("正文");
    commit_with_intent(source, &doc, &uuid_like(801), [0xD1; 32], EPOCH);

    let hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    insert_asset(
        source,
        &hash,
        ASSET_DATA.len() as u64,
        Some("text/plain"),
        ASSET_DATA,
    )
    .expect("资产");

    let mut engine = SyncEngine::new(
        source,
        relay,
        EngineIdentity {
            device_id: materials.device_id,
            signing_key: materials.signing_key.clone(),
            peer_id: peer,
        },
        materials.root.clone(),
    );
    engine.sync_once().expect("push 正文 update");

    let mut writer = CheckpointWriter::new(
        source,
        relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        peer,
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
    body_doc_id
}

fn set_u64_meta(vault: &menghunote_storage::Vault, key: &str, value: u64) {
    vault
        .set_meta(key, &value.to_be_bytes())
        .expect("写 BE64 meta");
}

fn relay_state_and_control_hash(relay: &mut FakeRelay) -> ([u8; 16], [u8; 32]) {
    let state = relay.state().expect("relay state");
    let chain = relay.management_chain().expect("management chain");
    (state.instance_id, control_head_hash(&state, &chain))
}

fn assert_no_sync_watermarks(vault: &menghunote_storage::Vault) {
    assert!(
        vault
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor meta")
            .is_none(),
        "失败路径不得推进 relay 游标"
    );
    assert!(
        vault
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received meta")
            .is_none(),
        "失败路径不得推进 received"
    );
    assert!(
        vault
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated meta")
            .is_none(),
        "失败路径不得推进 integrated"
    );
    assert!(
        vault
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater meta")
            .is_none(),
        "失败路径不得推进 highwater"
    );
}

fn assert_staging_journal(vault: &menghunote_storage::Vault) -> [u8; 16] {
    let journal = baseline_apply_state(vault)
        .expect("journal state")
        .expect("staging journal 必须保留，供续跑");
    assert_eq!(journal.status, BaselineApplyStatus::Staging);
    journal.checkpoint_id
}

#[test]
fn bootstrap_restores_all_docs_and_assets() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
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
    let asset_hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    let asset = asset_bytes(&target, asset_hash).expect("asset_bytes");
    assert_eq!(asset.as_deref(), Some(ASSET_DATA));
    // 水位/cursor 已写入：engine 之后从 S 继续，不会重复导入。
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor meta")
            .as_deref(),
        Some(report.cursor.as_bytes())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received")
            .as_deref(),
        Some(report.covered_seq.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated")
            .as_deref(),
        Some(report.covered_seq.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(report.covered_seq.to_be_bytes().as_slice())
    );
    // staging journal 成功后必须清掉。
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn structured_source_imports_structure_and_projects_notes() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    let body_doc_id = seed_structured_source(&source, &mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap structured");
    assert_eq!(report.covered_seq, relay.head_seq);
    assert_eq!(report.cursor, format!("c{}", relay.head_seq));
    // structure 文档在报告里没有正文文本。
    assert!(report
        .docs
        .iter()
        .any(|(doc_id, text)| doc_id == STRUCTURE_DOC_ID && text.is_none()));
    let notes = list_notes(&target).expect("list_notes");
    assert_eq!(notes.len(), 1, "结构投影必须建出 1 篇笔记");
    let body = load_body_doc(&target, &body_doc_id, materials.peer_id_u64(), None)
        .expect("结构化正文")
        .0;
    assert_eq!(body.text(), STRUCTURED_TEXT);
}

#[test]
fn crash_after_bodies_resumes_without_advancing_watermarks() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    let body_doc_id = seed_structured_source(&source, &mut relay, &materials);
    let s = relay.head_seq;
    assert!(s > 0, "本测试要求 baseline S>0 才能区分是否推进水位");

    let (_target_dir, target) = fresh_vault();
    let error = bootstrap::run_with_fault(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        BootstrapFault::AfterBodies,
    )
    .expect_err("注入故障必须失败");
    assert!(
        format!("{error}").contains("注入故障"),
        "错误应来自注入点：{error}"
    );

    // 正文已经落库（可续跑），但结构/资产/水位都没有。
    let body = load_body_doc(&target, &body_doc_id, materials.peer_id_u64(), None)
        .expect("正文已导入")
        .0;
    assert_eq!(body.text(), STRUCTURED_TEXT);
    assert!(list_notes(&target).expect("list_notes").is_empty());
    assert_no_sync_watermarks(&target);
    let journal_checkpoint = assert_staging_journal(&target);
    assert_eq!(
        journal_checkpoint,
        relay
            .published_checkpoints
            .last()
            .expect("primary checkpoint")
            .checkpoint_id
    );

    // 续跑：同一基线幂等重放，完成后水位才推进。
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("续跑成功");
    assert_eq!(report.covered_seq, s);
    assert_eq!(
        target
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(report.cursor.as_bytes())
    );
    assert_eq!(list_notes(&target).expect("list_notes").len(), 1);
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn structure_projection_failure_keeps_cursor_and_resumes() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let s = relay.head_seq;

    let (_target_dir, target) = fresh_vault();
    bootstrap::run_with_fault(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        BootstrapFault::BeforeStructureProjection,
    )
    .expect_err("结构投影前崩溃必须失败");

    // structure 文档已写入，但投影还没发生：notes 仍为空，水位未推进。
    assert!(
        body_doc_meta(&target, STRUCTURE_DOC_ID)
            .expect("structure meta")
            .is_some(),
        "structure 文档应已落库"
    );
    assert!(list_notes(&target).expect("list_notes").is_empty());
    assert_no_sync_watermarks(&target);
    assert_staging_journal(&target);

    // 续跑补齐投影与水位。
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("续跑成功");
    assert_eq!(report.covered_seq, s);
    assert_eq!(list_notes(&target).expect("list_notes").len(), 1);
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
}

#[test]
fn asset_write_conflict_keeps_cursor_and_resumes_after_fix() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let s = relay.head_seq;
    assert!(s > 0);

    let (_target_dir, target) = fresh_vault();
    // 预置同 asset_hash、同长度但内容不同的资产行：apply_baseline_assets 必须冲突。
    let asset_hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    let mut wrong = ASSET_DATA.to_vec();
    wrong[0] ^= 0xFF;
    target
        .exec(
            "INSERT INTO assets(asset_hash,byte_len,media_type,data,orphan_since) \
             VALUES(?1,?2,?3,?4,NULL)",
            &[
                Value::Blob(asset_hash.to_vec()),
                Value::Integer(ASSET_DATA.len() as i64),
                Value::Text("text/plain".to_owned()),
                Value::Blob(wrong),
            ],
        )
        .expect("预置冲突资产");

    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("资产写失败必须冒泡，不能用半库换基线");
    assert!(
        format!("{error}").contains("资产"),
        "错误应指向资产冲突：{error}"
    );
    assert_no_sync_watermarks(&target);
    assert_staging_journal(&target);
    // 已导入的正文仍在（可续跑），冲突资产未被覆盖。
    assert_eq!(
        load_body_doc(&target, "note-1", materials.peer_id_u64(), None)
            .expect("正文已导入")
            .0
            .text(),
        "第一行正文"
    );
    let stored = asset_bytes(&target, asset_hash).expect("asset_bytes");
    assert_ne!(stored.as_deref(), Some(ASSET_DATA));

    // 修掉冲突后同一 journal 续跑成功。
    target
        .exec(
            "DELETE FROM assets WHERE asset_hash=?1",
            &[Value::Blob(asset_hash.to_vec())],
        )
        .expect("删除冲突资产");
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("修复后续跑");
    assert_eq!(report.covered_seq, s);
    let stored = asset_bytes(&target, asset_hash).expect("asset_bytes");
    assert_eq!(stored.as_deref(), Some(ASSET_DATA));
    assert_eq!(
        target
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn crash_after_assets_resumes_without_duplicate_writes() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let s = relay.head_seq;
    let asset_hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();

    let (_target_dir, target) = fresh_vault();
    bootstrap::run_with_fault(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        BootstrapFault::AfterAssets,
    )
    .expect_err("资产写完后崩溃必须失败，水位不得推进");
    // 资产与结构都已落库，但水位仍未写；journal 记录 applied_assets 供续跑。
    assert_eq!(
        asset_bytes(&target, asset_hash)
            .expect("asset_bytes")
            .as_deref(),
        Some(ASSET_DATA)
    );
    assert_eq!(list_notes(&target).expect("list_notes").len(), 1);
    assert_no_sync_watermarks(&target);
    let journal = baseline_apply_state(&target)
        .expect("journal state")
        .expect("journal 必须保留");
    assert_eq!(journal.applied_assets, 1);

    // 续跑：同资产幂等（不重复插入、不再要求不同明文），水位才推进。
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("资产阶段崩溃后续跑");
    assert_eq!(report.covered_seq, s);
    assert_eq!(report.assets, 1);
    assert_eq!(
        asset_bytes(&target, asset_hash)
            .expect("asset_bytes")
            .as_deref(),
        Some(ASSET_DATA)
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn cursor_cas_conflict_rolls_back_all_watermarks_and_retry_succeeds() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let s = relay.head_seq;
    assert!(s > 0);

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run_with_fault(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        BootstrapFault::CursorCasConflict,
    );
    assert!(report.is_err(), "游标 CAS 冲突必须失败");
    // 事务整笔回滚：并发写者留下的游标保留，其余单调值一个都没写。
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(b"fault-cursor".as_slice())
    );
    assert!(target
        .get_meta(META_SYNC_RECEIVED_SEQ)
        .expect("received")
        .is_none());
    assert!(target
        .get_meta(META_SYNC_INTEGRATED_SEQ)
        .expect("integrated")
        .is_none());
    assert!(target
        .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
        .expect("highwater")
        .is_none());
    assert_staging_journal(&target);

    // 重试：从当前游标 CAS 前进，单调且幂等。
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("重试成功");
    assert_eq!(report.covered_seq, s);
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(report.cursor.as_bytes())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn existing_pins_and_highwater_are_not_lowered() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    // 让 primary 的 manifest 腐坏，强制选 fallback（fallback 的 S=0 < 本地水位 1）。
    let primary_manifest = relay
        .published_checkpoints
        .last()
        .and_then(|checkpoint| checkpoint.manifest_ref)
        .expect("primary manifest");
    relay
        .parts
        .get_mut(&(primary_manifest, 0))
        .expect("primary manifest part")[0] ^= 0x01;
    let (instance_id, control_hash) = relay_state_and_control_hash(&mut relay);

    let (_target_dir, target) = fresh_vault();
    // 已有库：一篇本地编辑过的正文 + 已确认的水位/游标/pins。
    let local_peer = materials.peer_id_u64().wrapping_add(1);
    let local = seed_doc(&target, "note-1", local_peer);
    local.insert_scalars(0, "local edit ").expect("本地正文");
    commit_local(&target, &local);
    target
        .set_meta(menghunote_storage::META_SYNC_LOG_ID, &relay.log_id)
        .expect("log id");
    target.set_meta(RELAY_CURSOR_META, b"c2").expect("cursor");
    set_u64_meta(&target, META_SYNC_RECEIVED_SEQ, 2);
    set_u64_meta(&target, META_SYNC_INTEGRATED_SEQ, 2);
    set_u64_meta(&target, META_SYNC_CONFIRMED_HIGHWATER, 2);
    target
        .set_meta(META_BOOTSTRAP_INSTANCE, &instance_id)
        .expect("instance pin");
    set_u64_meta(&target, META_BOOTSTRAP_EPOCH, EPOCH);
    set_u64_meta(&target, META_BOOTSTRAP_CONTROL_REV, relay.control_rev);
    target
        .set_meta(META_BOOTSTRAP_CONTROL_HASH, &control_hash)
        .expect("control hash pin");

    let report = bootstrap::run(&target, &mut relay, VAULT_ID, local_peer, &materials.root)
        .expect("已有库 bootstrap 必须成功");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, 1, "fallback 必须用自己的 S");
    assert_eq!(report.cursor, "c1");

    // 单调值全部保持原值，绝不被 baseline S 降级。
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(b"c2".as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received")
            .as_deref(),
        Some(2u64.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated")
            .as_deref(),
        Some(2u64.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(2u64.to_be_bytes().as_slice())
    );
    // pins 不变。
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_INSTANCE)
            .expect("instance pin")
            .as_deref(),
        Some(instance_id.as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_HASH)
            .expect("control hash pin")
            .as_deref(),
        Some(control_hash.as_slice())
    );
    // 已有本地编辑按 VV 合并，未被跳过/覆盖。
    let merged = load_body_doc(&target, "note-1", local_peer, None)
        .expect("note-1")
        .0;
    assert!(
        merged.text().contains("local edit") && merged.text().contains("第一行正文"),
        "已有 doc 必须按 VV 合并，实际={:?}",
        merged.text()
    );
}

#[test]
fn pin_regressions_reject_before_any_write() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let (instance_id, control_hash) = relay_state_and_control_hash(&mut relay);
    let state = relay.state().expect("state");
    let correct = BootstrapPins {
        instance_id: Some(instance_id),
        log_id: Some(relay.log_id),
        epoch: Some(EPOCH),
        control_rev: Some(relay.control_rev),
        control_hash: Some(control_hash),
        highwater: Some(0),
    };
    let regressions = [
        (
            "instance",
            BootstrapPins {
                instance_id: Some([0x99; 16]),
                ..correct
            },
        ),
        (
            "log_id",
            BootstrapPins {
                log_id: Some([0x99; 16]),
                ..correct
            },
        ),
        (
            "epoch",
            BootstrapPins {
                epoch: Some(EPOCH + 1),
                ..correct
            },
        ),
        (
            "control rev",
            BootstrapPins {
                control_rev: Some(relay.control_rev + 1),
                ..correct
            },
        ),
        (
            "control hash",
            BootstrapPins {
                control_hash: Some([0x99; 32]),
                ..correct
            },
        ),
        (
            "highwater",
            BootstrapPins {
                highwater: Some(state.head_seq + 1),
                ..correct
            },
        ),
    ];
    for (name, pins) in regressions {
        let (_target_dir, target) = fresh_vault();
        let error = bootstrap::run_with_pins(
            &target,
            &mut relay,
            VAULT_ID,
            materials.peer_id_u64(),
            &materials.root,
            &pins,
        )
        .expect_err(&format!("pins {name} 回退必须拒绝"));
        assert!(!format!("{error}").is_empty());
        assert!(
            list_all_docs(&target).expect("list_all_docs").is_empty(),
            "pins {name} 回退时不得写半库"
        );
        assert_no_sync_watermarks(&target);
        assert!(
            baseline_apply_state(&target).expect("journal").is_none(),
            "校验失败不得创建 journal"
        );
    }

    // 正确 pins 必须通过，并把 pins 持久化。
    let (_target_dir, target) = fresh_vault();
    bootstrap::run_with_pins(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        &correct,
    )
    .expect("正确 pins 应通过");
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_INSTANCE)
            .expect("instance pin")
            .as_deref(),
        Some(instance_id.as_slice())
    );
}

#[test]
fn control_pin_advances_but_never_rolls_back() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("首次 bootstrap");
    let first_rev = relay.control_rev;
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_REV)
            .expect("control rev")
            .as_deref(),
        Some(first_rev.to_be_bytes().as_slice())
    );
    let first_hash = target
        .get_meta(META_BOOTSTRAP_CONTROL_HASH)
        .expect("control hash")
        .expect("hash pin");

    // control_rev 前进：允许并刷新 pin。
    relay.control_rev = first_rev + 1;
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("control_rev 前进必须允许");
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_REV)
            .expect("control rev")
            .as_deref(),
        Some((first_rev + 1).to_be_bytes().as_slice())
    );
    let second_hash = target
        .get_meta(META_BOOTSTRAP_CONTROL_HASH)
        .expect("control hash")
        .expect("hash pin");
    assert_ne!(first_hash, second_hash, "rev 前进时控制头摘要必须更新");

    // control_rev 回退：必须 fail-stop，且不改任何本地状态。
    relay.control_rev = first_rev;
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("control_rev 回退必须拒绝");
    assert!(!format!("{error}").is_empty());
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_REV)
            .expect("control rev")
            .as_deref(),
        Some((first_rev + 1).to_be_bytes().as_slice())
    );
    assert!(load_body_doc(&target, "note-1", materials.peer_id_u64(), None).is_ok());
}

#[test]
fn bootstrap_cursor_pulls_updates_after_s() {
    let (_dir, source, materials) = setup();
    let (_target_dir, target) = fresh_vault();
    let mut relay = new_relay(&materials);
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
        let mut engine = SyncEngine::new(
            &source,
            &mut relay,
            EngineIdentity {
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
        let mut engine = SyncEngine::new(
            &target,
            &mut relay,
            EngineIdentity {
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
    let mut relay = new_relay(&materials);
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
    assert_eq!(first.cursor, second.cursor);
    let (note1, _) =
        load_body_doc(&target, "note-1", materials.peer_id_u64(), None).expect("note-1");
    assert_eq!(note1.text(), "第一行正文");
}

#[test]
fn fallback_is_used_when_primary_manifest_is_tampered() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
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
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received meta"),
        Some(fallback.covered_seq.to_be_bytes().to_vec())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated meta"),
        Some(fallback.covered_seq.to_be_bytes().to_vec())
    );
    assert!(load_body_doc(&target, "note-1", materials.peer_id_u64(), None).is_ok());
}

#[test]
fn fallback_is_used_when_primary_snapshot_is_tampered() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
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
    let mut relay = new_relay(&materials);
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
    let mut relay = new_relay(&materials);
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
        list_all_docs(&target).expect("list_all_docs").is_empty(),
        "校验失败前不得写半库: {error:?}"
    );
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn tampered_snapshot_is_rejected_without_partial_import() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
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
        list_all_docs(&target).expect("list_all_docs").is_empty(),
        "失败不得写半库: {error:?}"
    );
}

#[test]
fn wrong_root_rejects_bootstrap_before_write() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let wrong = RootSecretV1::new(
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
    assert!(list_all_docs(&target).expect("list_all_docs").is_empty());
}

#[test]
fn bootstrap_body_doc_head_is_one_full_update() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
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

#[test]
fn empty_baseline_at_zero_is_idempotent() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    source.set_meta(RELAY_CURSOR_META, b"c0").expect("cursor");
    let mut writer = CheckpointWriter::new(
        &source,
        &mut relay,
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
        .expect("空库 checkpoint 1");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("空库 checkpoint 2");
    assert_eq!(relay.head_seq, 0);

    let (_target_dir, target) = fresh_vault();
    let first = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("空基线 bootstrap");
    assert_eq!(first.covered_seq, 0);
    assert_eq!(first.cursor, "c0");
    assert_eq!(
        sync_cursor_state(&target)
            .expect("cursor")
            .confirmed_highwater,
        Some(0)
    );
    assert!(body_doc_meta(&target, STRUCTURE_DOC_ID)
        .expect("structure meta")
        .is_some());
    assert!(list_notes(&target).expect("list_notes").is_empty());

    // S=0 的第二次 bootstrap 必须幂等（游标 c0 -> c0 的 CAS 也要通过）。
    let second = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("空基线幂等重放");
    assert_eq!(first, second);
}

#[test]
fn cursor_metadata_reflects_written_watermarks() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap");
    let state = sync_cursor_state(&target).expect("cursor state");
    assert_eq!(state.received_seq, report.covered_seq);
    assert_eq!(state.integrated_seq, report.covered_seq);
    assert_eq!(state.confirmed_highwater, Some(report.covered_seq));
    assert_eq!(state.log_id, Some(relay.log_id));
}

// ============================================================ 评审 0002 T16/T17 补充
// primary 的 commit/signature/asset 各阶段腐坏 → 整体切 fallback，且只用 fallback 自己的 S/cursor；
// 两份都坏 → 失败且零写入/零水位（不能"两坏也 ACK"）。

fn tamper_checkpoint_commit(relay: &mut FakeRelay, index: usize) {
    let checkpoint = relay
        .published_checkpoints
        .get_mut(index)
        .expect("checkpoint");
    checkpoint.signed_commit.as_mut().expect("signed commit")[0] ^= 0x01;
}

fn tamper_checkpoint_signature(relay: &mut FakeRelay, index: usize) {
    let checkpoint = relay
        .published_checkpoints
        .get_mut(index)
        .expect("checkpoint");
    checkpoint.signature.as_mut().expect("signature")[0] ^= 0x01;
}

fn checkpoint_asset_refs(relay: &FakeRelay, index: usize) -> Vec<[u8; 32]> {
    relay
        .published_checkpoints
        .get(index)
        .expect("checkpoint")
        .data_refs
        .iter()
        .filter(|object_id| {
            relay
                .objects
                .get(*object_id)
                .is_some_and(|object| object.commit.object_type == ObjectType::Asset)
        })
        .copied()
        .collect()
}

#[test]
fn fallback_uses_its_own_lower_s_and_cursor_after_primary_commit_corruption() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);

    let last = relay.published_checkpoints.len() - 1;
    let fallback = relay.published_checkpoints[last - 1].clone();
    let primary = relay.published_checkpoints[last].clone();
    assert!(
        fallback.covered_seq < primary.covered_seq,
        "前置：fallback S 必须低于 primary S（S0<S1），实际 {} vs {}",
        fallback.covered_seq,
        primary.covered_seq
    );

    // commit 字节腐坏：不是"自洽 hash 通过"就能信，必须整体切 fallback。
    tamper_checkpoint_commit(&mut relay, last);

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("commit 腐坏后必须用 fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback.covered_seq);
    assert_eq!(
        report.cursor,
        format!("c{}", fallback.covered_seq),
        "cursor 必须用 fallback 自己的 S"
    );
    assert_eq!(
        target.get_meta(META_SYNC_RECEIVED_SEQ).expect("received"),
        Some(fallback.covered_seq.to_be_bytes().to_vec())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated"),
        Some(fallback.covered_seq.to_be_bytes().to_vec())
    );
    assert!(target
        .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
        .expect("highwater")
        .is_some());
    let (note, _) =
        load_body_doc(&target, "note-1", materials.peer_id_u64(), None).expect("note-1");
    assert!(note.text().contains("第一行正文"));
}

#[test]
fn corrupt_asset_on_primary_falls_back_and_both_corrupt_import_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    // 在 checkpoint 2 之后再插入一个**只被 primary 引用**的资产，才能单独腐坏 primary。
    let extra_asset = b"primary-only asset";
    let extra_hash: [u8; 32] = Sha256::digest(extra_asset).into();
    insert_asset(
        &source,
        &extra_hash,
        extra_asset.len() as u64,
        Some("text/plain"),
        extra_asset,
    )
    .expect("primary-only 资产");
    flush_pending_and_checkpoint(&source, &mut relay, &materials);

    let last = relay.published_checkpoints.len() - 1;
    let fallback = relay.published_checkpoints[last - 1].clone();
    let fallback_refs = fallback.data_refs.clone();
    let primary_only_assets: Vec<[u8; 32]> = checkpoint_asset_refs(&relay, last)
        .into_iter()
        .filter(|object_id| !fallback_refs.contains(object_id))
        .collect();
    assert!(
        !primary_only_assets.is_empty(),
        "前置：primary 必须引用了 fallback 没有的 ASSET 对象"
    );
    // 只腐坏 primary 独有的 ASSET 密文：下载/摘要/AEAD 阶段失败 → 整体 fallback。
    relay
        .parts
        .get_mut(&(primary_only_assets[0], 0))
        .expect("asset part")[0] ^= 0x01;

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("asset 腐坏必须用 fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback.covered_seq);
    assert_eq!(report.cursor, format!("c{}", fallback.covered_seq));
    let asset_rows = target
        .query("SELECT COUNT(*) FROM assets", &[])
        .expect("asset count");
    assert_eq!(
        asset_rows[0][0],
        Value::Integer(1),
        "只允许导入 fallback 自己的资产，不得写入 primary 独有资产"
    );
    assert!(
        menghunote_storage::asset_meta_by_hash(&target, &extra_hash)
            .expect("asset meta")
            .is_none(),
        "primary 独有资产不得出现在 fallback 导入结果里"
    );

    // 两份基线的 ASSET 都腐坏：零写入、零水位，绝不 ACK。
    let fallback_index = last - 1;
    for object_id in checkpoint_asset_refs(&relay, fallback_index) {
        if let Some(part) = relay.parts.get_mut(&(object_id, 0)) {
            part[0] ^= 0x01;
        }
    }
    let (_second_dir, second_target) = fresh_vault();
    let error = bootstrap::run(
        &second_target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("两份 asset 都坏必须失败");
    assert!(!format!("{error:?}").is_empty());
    assert!(
        list_all_docs(&second_target)
            .expect("list_all_docs")
            .is_empty(),
        "失败前不得写半库"
    );
    assert!(
        baseline_apply_state(&second_target)
            .expect("journal")
            .is_none(),
        "失败前不得建 staging journal"
    );
    assert_no_sync_watermarks(&second_target);
}

#[test]
fn corrupt_signature_on_both_baselines_fails_without_import_or_ack() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);

    for index in 0..relay.published_checkpoints.len() {
        tamper_checkpoint_signature(&mut relay, index);
    }

    let (_target_dir, target) = fresh_vault();
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("两份 checkpoint 签名都坏必须失败");
    assert!(!format!("{error:?}").is_empty());
    assert!(list_all_docs(&target).expect("list_all_docs").is_empty());
    assert!(baseline_apply_state(&target).expect("journal").is_none());
    assert_no_sync_watermarks(&target);
}

// ============================================================ 评审 0002 T16/T17 剩余
// fallback 阶段仍必须真实验证 structure/codec/actual VV；任一基线腐坏即整体回退，
// 两基线都坏必须零写入、零水位、零 journal、零 ACK。

/// 用真实 root 重加密 manifest 并重签 checkpoint commit（模拟 prepare 产物被篡改）。
fn install_manifest(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
    manifest: &menghunote_sync_proto::ManifestV1,
) {
    use menghunote_sync_proto::{
        checkpoint_signing_input, refs_digest_input, CheckpointCommit, ObjectRefV1, ObjectType,
    };
    let checkpoint = relay
        .published_checkpoints
        .get_mut(index)
        .expect("published checkpoint")
        .clone();
    let manifest_oid = checkpoint.manifest_ref.expect("manifest ref");
    let plaintext = manifest.encode().expect("manifest encode");
    let (parts, manifest_digest) = menghunote_sync::upload::encrypt_local_parts(
        root,
        ObjectType::Manifest,
        &manifest_oid,
        [0x42; 16],
        materials.device_id,
        0,
        0,
        &plaintext,
        &[],
    )
    .expect("reencrypt manifest");
    let old = relay
        .objects
        .get_mut(&manifest_oid)
        .expect("manifest object");
    for (part_no, envelope, _) in &parts {
        relay
            .parts
            .insert((manifest_oid, *part_no), envelope.clone());
    }
    old.commit.part_count = parts.len() as u32;
    old.commit.total_len = plaintext.len() as u64;
    old.commit.object_digest = manifest_digest;
    old.commit_bytes = old.commit.encode().expect("manifest commit");

    let refs: Vec<ObjectRefV1> = manifest
        .docs
        .iter()
        .map(|doc| ObjectRefV1 {
            object_id: doc.snapshot_object_id,
            object_type: ObjectType::Snapshot,
            generation: doc.generation,
            key_epoch: doc.key_epoch,
            part_count: doc.part_count,
            total_plain_len: doc.total_plain_len,
            object_digest: doc.object_digest,
        })
        .chain(manifest.assets.iter().map(|asset| ObjectRefV1 {
            object_id: asset.object_id,
            object_type: ObjectType::Asset,
            generation: 0,
            key_epoch: asset.key_epoch,
            part_count: asset.part_count,
            total_plain_len: asset.total_plain_len,
            object_digest: asset.object_digest,
        }))
        .collect();
    let refs_digest: [u8; 32] =
        Sha256::digest(refs_digest_input(&refs).expect("refs digest input")).into();
    let commit = CheckpointCommit {
        vault_id: VAULT_ID,
        checkpoint_id: checkpoint.checkpoint_id,
        epoch: checkpoint.epoch,
        covered_relay_seq: checkpoint.covered_seq,
        manifest_object_id: manifest_oid,
        manifest_digest,
        refs_digest,
        producer_device_id: materials.device_id,
        validation_version: 1,
    };
    let signed_commit = commit.encode().expect("checkpoint commit");
    let signature = materials
        .signing_key
        .sign(&checkpoint_signing_input(&commit).expect("checkpoint signing input"));
    let entry = relay
        .published_checkpoints
        .get_mut(index)
        .expect("published checkpoint");
    entry.signed_commit = Some(signed_commit);
    entry.signature = Some(signature);
}

fn load_manifest(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    index: usize,
) -> menghunote_sync_proto::ManifestV1 {
    let checkpoint = relay.published_checkpoints[index].clone();
    let manifest_oid = checkpoint.manifest_ref.expect("manifest ref");
    let commit = relay.objects[&manifest_oid].commit.clone();
    let (_, bytes) = menghunote_sync::upload::fetch_object(
        relay,
        root,
        ObjectType::Manifest,
        &manifest_oid,
        commit.part_count,
        commit.total_len,
        &commit.object_digest,
        &[],
    )
    .expect("fetch manifest");
    menghunote_sync_proto::ManifestV1::decode(&bytes).expect("decode manifest")
}

/// 重写某个 baseline 的 structure（或第一个 body）快照并同步 manifest/commit。
fn rewrite_snapshot(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
    kind: u64,
    mutate: impl FnOnce(&mut menghunote_sync_proto::SnapshotV1),
) {
    use menghunote_sync_proto::{ObjectType, SnapshotV1};
    let mut manifest = load_manifest(relay, root, index);
    let doc_index = manifest
        .docs
        .iter()
        .position(|doc| doc.kind == kind)
        .expect("目标 kind 文档");
    let doc = manifest.docs[doc_index].clone();
    let (_, bytes) = menghunote_sync::upload::fetch_object(
        relay,
        root,
        ObjectType::Snapshot,
        &doc.snapshot_object_id,
        doc.part_count,
        doc.total_plain_len,
        &doc.object_digest,
        &[],
    )
    .expect("fetch snapshot");
    let mut snapshot = SnapshotV1::decode(&bytes).expect("decode snapshot");
    mutate(&mut snapshot);
    let plaintext = snapshot.encode().expect("snapshot encode");
    let (parts, digest) = menghunote_sync::upload::encrypt_local_parts(
        root,
        ObjectType::Snapshot,
        &doc.snapshot_object_id,
        [0x43; 16],
        materials.device_id,
        doc.generation,
        0,
        &plaintext,
        &[],
    )
    .expect("reencrypt snapshot");
    {
        let object = relay
            .objects
            .get_mut(&doc.snapshot_object_id)
            .expect("snapshot object");
        for (part_no, envelope, _) in &parts {
            relay
                .parts
                .insert((doc.snapshot_object_id, *part_no), envelope.clone());
        }
        object.commit.part_count = parts.len() as u32;
        object.commit.total_len = plaintext.len() as u64;
        object.commit.object_digest = digest;
        object.commit_bytes = object.commit.encode().expect("snapshot commit");
    }
    manifest.docs[doc_index].part_count = parts.len() as u32;
    manifest.docs[doc_index].total_plain_len = plaintext.len() as u64;
    manifest.docs[doc_index].object_digest = digest;
    install_manifest(relay, root, materials, index, &manifest);
}

fn corrupt_structure_manifest(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
) {
    let mut manifest = load_manifest(relay, root, index);
    manifest.structure_doc_id = "missing-structure".to_owned();
    install_manifest(relay, root, materials, index, &manifest);
}

fn corrupt_codec(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
) {
    rewrite_snapshot(
        relay,
        root,
        materials,
        index,
        menghunote_sync::checkpoint::KIND_STRUCTURE,
        |snapshot| {
            snapshot.codec_version = "loro-v0".to_owned();
        },
    );
}

fn corrupt_actual_vv(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
) {
    use menghunote_sync::checkpoint::KIND_STRUCTURE;
    let mut manifest = load_manifest(relay, root, index);
    let doc = manifest
        .docs
        .iter_mut()
        .find(|doc| doc.kind == KIND_STRUCTURE)
        .expect("structure doc");
    let mut vv = loro::VersionVector::decode(&doc.vv).expect("vv");
    vv.insert(0xDEAD, 7);
    doc.vv = vv.encode();
    install_manifest(relay, root, materials, index, &manifest);
}

fn run_bootstrap_with_target(
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) -> (
    tempfile::TempDir,
    menghunote_storage::Vault,
    Result<menghunote_sync::bootstrap::BootstrapReport, menghunote_sync::SyncError>,
) {
    let (dir, target) = fresh_vault();
    let result = bootstrap::run(
        &target,
        relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    );
    (dir, target, result)
}

#[test]
fn fallback_rejects_corrupt_structure_and_both_corrupt_writes_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let last = relay.published_checkpoints.len() - 1;

    // 只腐坏 primary 的 structure 声明 → 整体回退 fallback，且只写 fallback 的 S/cursor。
    corrupt_structure_manifest(&mut relay, &materials.root, &materials, last);
    let fallback_s = relay.published_checkpoints[last - 1].covered_seq;
    let (_target_dir, target, result) = run_bootstrap_with_target(&mut relay, &materials);
    let report = result.expect("structure 腐坏必须回退 fallback");
    assert!(
        report.used_fallback,
        "primary structure 腐坏必须 used_fallback"
    );
    assert_eq!(report.covered_seq, fallback_s);
    assert_eq!(report.cursor, format!("c{fallback_s}"));
    let notes = list_notes(&target).expect("notes");
    assert_eq!(notes.len(), 1, "fallback 必须恢复自己的结构笔记");
    let (body, _) = load_body_doc(
        &target,
        notes[0]
            .body_doc_id
            .as_deref()
            .expect("notes 行必须有正文 doc id"),
        materials.peer_id_u64(),
        None,
    )
    .expect("body");
    assert_eq!(body.text(), STRUCTURED_TEXT);

    // 两基线 structure 都坏 → 失败且零写入/零水位/零 journal。
    let acks_before = relay.acks.len();
    corrupt_structure_manifest(&mut relay, &materials.root, &materials, last - 1);
    let (_second_dir, second_target, result) = run_bootstrap_with_target(&mut relay, &materials);
    assert!(result.is_err(), "两基线 structure 都坏必须失败");
    assert_eq!(relay.acks.len(), acks_before, "失败不得 ACK");
    assert!(list_all_docs(&second_target).expect("docs").is_empty());
    assert!(baseline_apply_state(&second_target)
        .expect("journal")
        .is_none());
    assert_no_sync_watermarks(&second_target);
}

#[test]
fn fallback_rejects_unreadable_codec_and_both_corrupt_writes_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let last = relay.published_checkpoints.len() - 1;

    corrupt_codec(&mut relay, &materials.root, &materials, last);
    let fallback_s = relay.published_checkpoints[last - 1].covered_seq;
    let (_target_dir, target, result) = run_bootstrap_with_target(&mut relay, &materials);
    let report = result.expect("codec 腐坏必须回退 fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback_s);
    assert_eq!(report.cursor, format!("c{fallback_s}"));
    assert_eq!(list_notes(&target).expect("notes").len(), 1);

    let acks_before = relay.acks.len();
    corrupt_codec(&mut relay, &materials.root, &materials, last - 1);
    let (_second_dir, second_target, result) = run_bootstrap_with_target(&mut relay, &materials);
    assert!(result.is_err(), "两基线 codec 都坏必须失败");
    assert_eq!(relay.acks.len(), acks_before, "失败不得 ACK");
    assert!(list_all_docs(&second_target).expect("docs").is_empty());
    assert!(baseline_apply_state(&second_target)
        .expect("journal")
        .is_none());
    assert_no_sync_watermarks(&second_target);
}

#[test]
fn fallback_rejects_inflated_actual_vv_and_both_corrupt_writes_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let last = relay.published_checkpoints.len() - 1;

    // manifest 宣称的 VV 高于快照真实历史：不能只信声明，必须整体回退。
    corrupt_actual_vv(&mut relay, &materials.root, &materials, last);
    let fallback_s = relay.published_checkpoints[last - 1].covered_seq;
    let (_target_dir, target, result) = run_bootstrap_with_target(&mut relay, &materials);
    let report = result.expect("actual VV 虚高必须回退 fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback_s);
    assert_eq!(report.cursor, format!("c{fallback_s}"));
    assert_eq!(list_notes(&target).expect("notes").len(), 1);

    let acks_before = relay.acks.len();
    corrupt_actual_vv(&mut relay, &materials.root, &materials, last - 1);
    let (_second_dir, second_target, result) = run_bootstrap_with_target(&mut relay, &materials);
    assert!(result.is_err(), "两基线 actual VV 都虚高必须失败");
    assert_eq!(relay.acks.len(), acks_before, "失败不得 ACK");
    assert!(list_all_docs(&second_target).expect("docs").is_empty());
    assert!(baseline_apply_state(&second_target)
        .expect("journal")
        .is_none());
    assert_no_sync_watermarks(&second_target);
}
