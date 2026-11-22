//! 轮换阶段 B/C（task-102）：begin 丢响应找回、integrate 必须到 S。

mod support;

use support::*;

use menghunote_storage::{
    load_body_doc, mark_inbox_quarantine, mark_integrated, mark_sent, pending_encryption,
    pending_send, set_confirmed_highwater, store_encrypted_parts, store_inbox_verified,
    SyncEncryptedPart, Vault, META_SYNC_INTEGRATED_SEQ,
};
use menghunote_sync::api_version::RelayApiVersion;
use menghunote_sync::device::ManagementKey;
use menghunote_sync::engine::{EngineIdentity, SyncEngine};
use menghunote_sync::genesis;
use menghunote_sync::http_relay::ControlPin;
use menghunote_sync::keys::Secret32;
use menghunote_sync::relay::ManagerChainEntry;
use menghunote_sync::rotation;
use menghunote_sync::rotation_journal::{RotationJournal, RotationPhase};

fn materials() -> genesis::GenesisMaterials {
    genesis::build_materials(
        VAULT_ID,
        [0x44; 16],
        "pw",
        1_700_000_000,
        &mut TestRandom::new(0x98_28),
    )
    .expect("genesis 材料")
}

fn setup() -> (
    tempfile::TempDir,
    Vault,
    genesis::GenesisMaterials,
    FakeRelay,
) {
    let (dir, vault) = fresh_vault();
    let materials = materials();
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    (dir, vault, materials, relay)
}

/// 模拟调用方从已验证 `/control` 链尾拿到的 pin（`control_head()` 的 `(rev, record_hash)`）。
fn verified_control_pin(relay: &FakeRelay, record_hash: [u8; 32]) -> ControlPin {
    ControlPin::new(relay.instance_id, relay.control_rev).with_record_hash(record_hash)
}

/// 造一条合法的管理转换记录（old/new 双签，rev 严格 +1）。
fn transition_entry(
    instance_id: [u8; 16],
    old: &ManagementKey,
    new: &ManagementKey,
    old_rev: u64,
    from_epoch: u64,
    to_epoch: u64,
) -> ManagerChainEntry {
    let transition = rotation::ManagerTransitionV1 {
        vault_id: VAULT_ID,
        instance_id,
        rotation_id: [0x71; 16],
        from_epoch,
        to_epoch,
        old_rev,
        new_rev: old_rev + 1,
        new_pubkey: new.public_key(),
        covered_seq: from_epoch,
    };
    let bytes = transition.encode().expect("transition");
    ManagerChainEntry {
        rev: old_rev + 1,
        pubkey: new.public_key(),
        transition: bytes.clone(),
        old_rev,
        old_signature: Some(old.sign(&bytes)),
        new_signature: Some(new.sign(&bytes)),
    }
}

#[test]
fn begin_response_loss_is_recovered_by_request_id() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x71; 16], relay.control_rev, [0x81; 32])
            .expect("freeze");
    relay.drop_begin_response = true;
    journal.confirm_r_prime_backed_up(&vault).expect("R' 备份");
    let manager =
        ManagementKey::from_recovery(&materials.recovery_seed, &VAULT_ID).expect("管理 key");
    let genesis_pin = verified_control_pin(&relay, [0xA1; 32]);
    rotation::verify_management_chain_and_pin(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &manager,
        Some(manager.public_key()),
        genesis_pin,
    )
    .expect("阶段 A pin");
    let instance_id = relay.instance_id;
    let result = rotation::begin_rotation(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &manager,
        &instance_id,
        &[],
        RelayApiVersion::V1,
    );
    let view = result.expect("丢响应后应按 request_id 找回");
    assert_eq!(journal.rotation_id, Some(view.rotation_id));
    assert_eq!(
        journal.phase,
        RotationPhase::Begun,
        "查询找回后必须进入 Begun"
    );
    let rotation_id = journal.rotation_id.expect("rotation id");
    assert!(relay.rotations.contains_key(&rotation_id));
    assert_eq!(
        relay.rotation_requests.get(&journal.request_id()),
        Some(&rotation_id)
    );
    // journal 已持久化：重新加载仍是 Begun。
    let reloaded = RotationJournal::load(&vault)
        .expect("load")
        .expect("journal");
    assert_eq!(reloaded.phase, RotationPhase::Begun);
    assert_eq!(reloaded.rotation_id, Some(rotation_id));
}

#[test]
fn integrate_refuses_ready_when_log_does_not_reach_s() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    set_confirmed_highwater(&vault, 0).expect("highwater");
    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x72; 16], relay.control_rev, [0x82; 32])
            .expect("freeze");
    let manager =
        ManagementKey::from_recovery(&materials.recovery_seed, &VAULT_ID).expect("管理 key");
    journal.confirm_r_prime_backed_up(&vault).expect("R' 备份");
    let genesis_pin = verified_control_pin(&relay, [0xA2; 32]);
    rotation::verify_management_chain_and_pin(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &manager,
        Some(manager.public_key()),
        genesis_pin,
    )
    .expect("阶段 A pin");
    let instance_id = relay.instance_id;
    rotation::begin_rotation(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &manager,
        &instance_id,
        &[],
        RelayApiVersion::V1,
    )
    .expect("begin");
    let s = journal.freeze_seq.expect("S") + 5;
    let mut engine = SyncEngine::new(
        &vault,
        &mut relay,
        EngineIdentity {
            device_id: materials.device_id,
            signing_key: materials.signing_key.clone(),
            peer_id: materials.peer_id_u64(),
        },
        materials.root.clone(),
    );
    let error = rotation::integrate_to_s(&vault, &mut engine, &mut journal, s, 0)
        .expect_err("日志没有到 S，不能 ready");
    assert!(matches!(error, menghunote_sync::SyncError::Unsupported(_)));
    assert_eq!(journal.phase, RotationPhase::Begun);
    assert!(journal.local_cut.is_none());
}

#[test]
fn management_chain_is_verified_and_tamper_rejected() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    let manager =
        ManagementKey::from_recovery(&materials.recovery_seed, &VAULT_ID).expect("管理 key");
    let new_manager = ManagementKey::from_seed(Secret32::from_array([0x5A; 32]));
    let transition = rotation::ManagerTransitionV1 {
        vault_id: VAULT_ID,
        instance_id: relay.instance_id,
        rotation_id: [0x71; 16],
        from_epoch: 1,
        to_epoch: 2,
        old_rev: 1,
        new_rev: 2,
        new_pubkey: new_manager.public_key(),
        covered_seq: 10,
    };
    let bytes = transition.encode().expect("transition");
    relay.manager_chain = vec![ManagerChainEntry {
        rev: 2,
        pubkey: new_manager.public_key(),
        transition: bytes.clone(),
        old_rev: 1,
        old_signature: Some(manager.sign(&bytes)),
        new_signature: Some(new_manager.sign(&bytes)),
    }];
    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x73; 16], relay.control_rev, [0x83; 32])
            .expect("freeze");
    let mut bad_relay = FakeRelay::new(&[]);
    bad_relay.instance_id = relay.instance_id;
    bad_relay.control_rev = relay.control_rev;
    bad_relay.manager_chain = relay.manager_chain.clone();
    bad_relay.manager_chain[0].transition[0] ^= 0x01;
    let bad_pin = verified_control_pin(&bad_relay, [0xA3; 32]);
    assert!(
        rotation::verify_management_chain_and_pin(
            &vault,
            &mut bad_relay,
            &mut journal,
            VAULT_ID,
            &new_manager,
            Some(manager.public_key()),
            bad_pin,
        )
        .is_err(),
        "篡改 transition 必败"
    );
    let good_pin = verified_control_pin(&relay, [0xA3; 32]);
    rotation::verify_management_chain_and_pin(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &new_manager,
        Some(manager.public_key()),
        good_pin,
    )
    .expect("管理链校验");
    assert_eq!(journal.control_chain_rev, Some(2));
    assert_eq!(journal.pinned_control_hash, Some([0xA3; 32]));
    assert!(journal.pinned_instance_id.is_some());
    assert!(journal.pinned_log_id.is_some());
}

#[test]
fn relay_replacing_genesis_manager_key_is_rejected() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    let genesis =
        ManagementKey::from_recovery(&materials.recovery_seed, &VAULT_ID).expect("管理 key");
    let attacker = ManagementKey::from_seed(Secret32::from_array([0x9A; 32]));
    let new_manager = ManagementKey::from_seed(Secret32::from_array([0x5B; 32]));
    // relay 把 rev1 genesis 换成攻击者 key，再用攻击者私钥重签第一条转换。
    // 旧实现盲信 rev1 entry（prev_pub = entry.pubkey），会以攻击者公钥为锚点接受整条伪造链。
    let mut forged = vec![ManagerChainEntry {
        rev: 1,
        pubkey: attacker.public_key(),
        transition: Vec::new(),
        old_rev: 0,
        old_signature: None,
        new_signature: None,
    }];
    forged.push(transition_entry(
        relay.instance_id,
        &attacker,
        &new_manager,
        1,
        1,
        2,
    ));
    relay.manager_chain = forged;
    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x74; 16], relay.control_rev, [0x84; 32])
            .expect("freeze");
    let pin = verified_control_pin(&relay, [0xA4; 32]);
    let error = rotation::verify_management_chain_and_pin(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &new_manager,
        Some(genesis.public_key()),
        pin,
    )
    .expect_err("relay 替换 genesis 管理 key 必须拒绝");
    assert!(matches!(error, menghunote_sync::SyncError::RelayWire(_)));
    assert!(journal.pinned_instance_id.is_none());
    assert!(journal.pinned_control_hash.is_none());
}

#[test]
fn missing_genesis_trust_anchor_fails_closed() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    let manager =
        ManagementKey::from_recovery(&materials.recovery_seed, &VAULT_ID).expect("管理 key");
    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x75; 16], relay.control_rev, [0x85; 32])
            .expect("freeze");
    let pin = verified_control_pin(&relay, [0xA5; 32]);
    let error = rotation::verify_management_chain_and_pin(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &manager,
        None,
        pin,
    )
    .expect_err("缺 genesis 信任锚必须 fail-closed");
    assert!(matches!(error, menghunote_sync::SyncError::Unsupported(_)));
    assert!(journal.pinned_instance_id.is_none());
    assert!(journal.pinned_log_id.is_none());
    assert!(journal.pinned_control_hash.is_none());
    assert!(journal.control_chain_rev.is_none());
}

#[test]
fn restart_epoch_jump_without_plus_one_is_accepted() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    let genesis =
        ManagementKey::from_recovery(&materials.recovery_seed, &VAULT_ID).expect("管理 key");
    let new_manager = ManagementKey::from_seed(Secret32::from_array([0x5C; 32]));
    // restart 允许 1 → 3 跳 epoch（旧实现硬要求 +1 会拒绝合法链）。
    relay.manager_chain = vec![transition_entry(
        relay.instance_id,
        &genesis,
        &new_manager,
        1,
        1,
        3,
    )];
    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x76; 16], relay.control_rev, [0x86; 32])
            .expect("freeze");
    let pin = verified_control_pin(&relay, [0xA6; 32]);
    rotation::verify_management_chain_and_pin(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &new_manager,
        Some(genesis.public_key()),
        pin,
    )
    .expect("restart 跳 epoch 必须通过");
    assert_eq!(journal.control_chain_rev, Some(2));
    assert_eq!(journal.pinned_control_hash, Some([0xA6; 32]));
    assert_eq!(journal.pinned_instance_id, Some(relay.instance_id));
    assert_eq!(journal.pinned_log_id, Some(relay.log_id));
    let reloaded = RotationJournal::load(&vault)
        .expect("load")
        .expect("journal");
    assert_eq!(reloaded.control_chain_rev, Some(2));
    assert_eq!(reloaded.pinned_control_hash, Some([0xA6; 32]));

    // epoch 仍必须前进：1 → 1 不是合法转换。
    relay.manager_chain = vec![transition_entry(
        relay.instance_id,
        &genesis,
        &new_manager,
        1,
        1,
        1,
    )];
    let pin = verified_control_pin(&relay, [0xA6; 32]);
    let error = rotation::verify_management_chain_and_pin(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &new_manager,
        Some(genesis.public_key()),
        pin,
    )
    .expect_err("epoch 不前进必须拒绝");
    assert!(matches!(error, menghunote_sync::SyncError::RelayWire(_)));
}

#[test]
fn journal_control_hash_never_overrides_verified_control_tail() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    let manager =
        ManagementKey::from_recovery(&materials.recovery_seed, &VAULT_ID).expect("管理 key");
    // journal 自带的 control_hash 是本地旧字段（未经验证），不得当作 pin 事实。
    let forged = [0xEE; 32];
    let mut journal = RotationJournal::freeze(&vault, EPOCH, [0x77; 16], relay.control_rev, forged)
        .expect("freeze");
    let pin = verified_control_pin(&relay, [0xA7; 32]);
    rotation::verify_management_chain_and_pin(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &manager,
        Some(manager.public_key()),
        pin,
    )
    .expect("阶段 A pin");
    // journal 字段原样保留，但 pin 只认已验证链尾。
    assert_eq!(journal.control_hash, forged);
    assert_eq!(journal.pinned_control_hash, Some([0xA7; 32]));
    let reloaded = RotationJournal::load(&vault)
        .expect("load")
        .expect("journal");
    assert_eq!(reloaded.pinned_control_hash, Some([0xA7; 32]));
    assert_ne!(reloaded.pinned_control_hash, Some(forged));
}

#[test]
fn low_or_swapped_control_pin_is_rejected() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    let manager =
        ManagementKey::from_recovery(&materials.recovery_seed, &VAULT_ID).expect("管理 key");
    // 控制链已有 rev=2，rotation 也冻结在 rev=2。
    relay.control_rev = 2;
    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x78; 16], 2, [0x87; 32]).expect("freeze");
    // ① 低 pin：链尾 rev 落后于冻结的 control rev。
    let low = ControlPin::new(relay.instance_id, 1).with_record_hash([0xB1; 32]);
    let error = rotation::verify_management_chain_and_pin(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &manager,
        Some(manager.public_key()),
        low,
    )
    .expect_err("低 pin 必须拒绝");
    assert!(matches!(error, menghunote_sync::SyncError::RelayWire(_)));
    assert!(journal.pinned_control_hash.is_none());
    // ② 正常链尾 pin 成功。
    let head = ControlPin::new(relay.instance_id, 2).with_record_hash([0xC1; 32]);
    rotation::verify_management_chain_and_pin(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &manager,
        Some(manager.public_key()),
        head,
    )
    .expect("已验证链尾 pin");
    assert_eq!(journal.pinned_control_hash, Some([0xC1; 32]));
    // ③ 换链尾：同 rev 但 record_hash 不同必须拒绝，且不覆盖旧 pin。
    let swapped = ControlPin::new(relay.instance_id, 2).with_record_hash([0xC2; 32]);
    let error = rotation::verify_management_chain_and_pin(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &manager,
        Some(manager.public_key()),
        swapped,
    )
    .expect_err("同 rev 换链尾必须拒绝");
    assert!(matches!(error, menghunote_sync::SyncError::RelayWire(_)));
    assert_eq!(journal.pinned_control_hash, Some([0xC1; 32]));
}

#[test]
fn revoked_device_history_before_s_still_imports() {
    ensure_test_identity_wrap();
    let (_dir, source, materials, mut relay) = setup();
    let (_target_dir, target) = fresh_vault();
    seed_cursor(&source);
    seed_cursor(&target);
    // 目标端已有同一 doc 的空壳；历史 update 应能合并进去。
    let _target_doc = seed_doc(&target, "note-1", materials.peer_id_u64());
    let doc = seed_doc(&source, "note-1", materials.peer_id_u64());
    doc.insert_scalars(0, "history before S").expect("正文");
    commit_with_intent(&source, &doc, &uuid_like(801), [0x91; 32], EPOCH);
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
        engine.sync_once().expect("push history");
    }
    // 设备现在被吊销，但 relay 仍保留其 S 前证书。
    relay.revoked.insert(materials.device_id);
    let mut target_engine = SyncEngine::new(
        &target,
        &mut relay,
        EngineIdentity {
            device_id: materials.device_id,
            signing_key: materials.signing_key.clone(),
            peer_id: materials.peer_id_u64(),
        },
        materials.root.clone(),
    );
    target_engine.sync_once().expect("import history");
    let (imported, _) =
        load_body_doc(&target, "note-1", materials.peer_id_u64(), None).expect("note-1");
    assert_eq!(imported.text(), "history before S");

    // S 之后只在本机提交、未上传的编辑不会被自动纳入。
    doc.insert_scalars(0, "after S local only ")
        .expect("本地编辑");
    commit_local(&source, &doc);
    target_engine.sync_once().expect("no new events");
    let (again, _) =
        load_body_doc(&target, "note-1", materials.peer_id_u64(), None).expect("note-1");
    assert_eq!(again.text(), "history before S");
}

#[test]
fn activate_body_binds_four_digests_and_signature() {
    ensure_test_identity_wrap();
    use menghunote_sync_proto::{RotationActivateBodyV1, RotationActivateBodyV1 as Activate};
    let (_dir, _vault, materials, _relay) = setup();
    let manager =
        ManagementKey::from_recovery(&materials.recovery_seed, &VAULT_ID).expect("管理 key");
    let mut body = Activate {
        new_manager: None,
        primary_checkpoint: Some([0x11; 16]),
        fallback_checkpoint: Some([0x12; 16]),
        member_cert_digest: Some([0x21; 32]),
        grants_digest: Some([0x22; 32]),
        slot_set_digest: Some([0x23; 32]),
        asset_set_digest: Some([0x24; 32]),
    };
    let signed = rotation::sign_activate(
        &manager,
        &[0x55; 16],
        VAULT_ID,
        [0x66; 16],
        [0x77; 16],
        3,
        &body,
        RelayApiVersion::V1,
    )
    .expect("sign activate");
    let decoded = RotationActivateBodyV1::decode(&signed.body).expect("decode activate");
    assert_eq!(decoded.member_cert_digest, Some([0x21; 32]));
    assert_eq!(decoded.grants_digest, Some([0x22; 32]));
    assert_eq!(decoded.slot_set_digest, Some([0x23; 32]));
    assert_eq!(decoded.asset_set_digest, Some([0x24; 32]));
    assert_ne!(signed.signature, [0u8; 64]);

    body.asset_set_digest = None;
    assert!(rotation::sign_activate(
        &manager,
        &[0x55; 16],
        VAULT_ID,
        [0x66; 16],
        [0x77; 16],
        3,
        &body,
        RelayApiVersion::V1,
    )
    .is_err());
}

#[test]
fn integrate_allows_ongoing_local_edits() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    let log_id = [0x77; 16];
    // 合法把 integrated 推到 S=1（一个已验/已导入的入站事件）。
    store_inbox_verified(&vault, &log_id, 1, &[0x88; 32], EPOCH, b"payload").expect("inbox");
    mark_integrated(&vault, &log_id, 1).expect("integrated");
    set_confirmed_highwater(&vault, 1).expect("highwater");
    // 冻结后继续产生本地编辑：一条 pending、一条 encrypted。
    let doc = seed_doc(&vault, "note-local", materials.peer_id_u64());
    doc.insert_scalars(0, "continuous edit").expect("编辑");
    commit_with_intent(&vault, &doc, &uuid_like(900), [0x91; 32], EPOCH);
    let doc2 = seed_doc(&vault, "note-local-2", materials.peer_id_u64());
    doc2.insert_scalars(0, "another edit").expect("编辑");
    commit_with_intent(&vault, &doc2, &uuid_like(901), [0x92; 32], EPOCH);
    store_encrypted_parts(
        &vault,
        &uuid_like(901),
        EPOCH,
        &[0xCC; 32],
        &[SyncEncryptedPart {
            part_no: 0,
            envelope: vec![0xAA; 8],
            digest: [0xBB; 32],
        }],
    )
    .expect("encrypted");
    assert!(!pending_encryption(&vault, 10).expect("pending").is_empty());
    assert!(!pending_send(&vault, 10).expect("send").is_empty());

    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x72; 16], relay.control_rev, [0x82; 32])
            .expect("freeze");
    journal.mark_begun([0x83; 16], 1, log_id).expect("begun");
    let mut engine = SyncEngine::new(
        &vault,
        &mut relay,
        EngineIdentity {
            device_id: materials.device_id,
            signing_key: materials.signing_key.clone(),
            peer_id: materials.peer_id_u64(),
        },
        materials.root.clone(),
    );
    rotation::integrate_to_s(&vault, &mut engine, &mut journal, 1, 0)
        .expect("持续本地编辑不应阻断 C 阶段");
    assert_eq!(journal.phase, RotationPhase::Integrated);
    assert_eq!(journal.local_cut, Some(0));
    // 本地 outbox 只是不阻断，绝不能被门禁清掉。
    assert!(!pending_encryption(&vault, 10).expect("pending").is_empty());
    assert!(!pending_send(&vault, 10).expect("send").is_empty());
}

#[test]
fn integrate_rejects_quarantine_within_s() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    let log_id = [0x78; 16];
    store_inbox_verified(&vault, &log_id, 1, &[0x89; 32], EPOCH, b"bad").expect("inbox");
    mark_inbox_quarantine(&vault, &log_id, 1).expect("quarantine");
    // 模拟崩溃/手工修复造成 meta 与 inbox 不一致：integrated 声称到 1，但 ≤S 有隔离行。
    vault
        .set_meta(META_SYNC_INTEGRATED_SEQ, &1u64.to_be_bytes())
        .expect("integrated meta");
    set_confirmed_highwater(&vault, 1).expect("highwater");
    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x73; 16], relay.control_rev, [0x83; 32])
            .expect("freeze");
    journal.mark_begun([0x84; 16], 1, log_id).expect("begun");
    let mut engine = SyncEngine::new(
        &vault,
        &mut relay,
        EngineIdentity {
            device_id: materials.device_id,
            signing_key: materials.signing_key.clone(),
            peer_id: materials.peer_id_u64(),
        },
        materials.root.clone(),
    );
    let error = rotation::integrate_to_s(&vault, &mut engine, &mut journal, 1, 0)
        .expect_err("inbox ≤S 有 quarantine 必须拒绝");
    assert!(matches!(error, menghunote_sync::SyncError::Unsupported(_)));
    assert_eq!(journal.phase, RotationPhase::Begun);
    assert!(journal.local_cut.is_none());
}

#[test]
fn integrate_rejects_uploaded_but_unacked_updates() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    let log_id = [0x79; 16];
    store_inbox_verified(&vault, &log_id, 1, &[0x8A; 32], EPOCH, b"payload").expect("inbox");
    mark_integrated(&vault, &log_id, 1).expect("integrated");
    set_confirmed_highwater(&vault, 1).expect("highwater");
    // 已上传（sent）但未 ACK 的旧 epoch 更新阻断 ready。
    let doc = seed_doc(&vault, "note-sent", materials.peer_id_u64());
    doc.insert_scalars(0, "uploaded").expect("编辑");
    let message = uuid_like(902);
    commit_with_intent(&vault, &doc, &message, [0x93; 32], EPOCH);
    let digest = [0xCD; 32];
    store_encrypted_parts(
        &vault,
        &message,
        EPOCH,
        &digest,
        &[SyncEncryptedPart {
            part_no: 0,
            envelope: vec![0xAC; 8],
            digest: [0xBD; 32],
        }],
    )
    .expect("encrypted");
    assert!(mark_sent(&vault, &message, EPOCH, &digest).expect("sent"));
    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x74; 16], relay.control_rev, [0x84; 32])
            .expect("freeze");
    journal.mark_begun([0x85; 16], 1, log_id).expect("begun");
    let mut engine = SyncEngine::new(
        &vault,
        &mut relay,
        EngineIdentity {
            device_id: materials.device_id,
            signing_key: materials.signing_key.clone(),
            peer_id: materials.peer_id_u64(),
        },
        materials.root.clone(),
    );
    let error = rotation::integrate_to_s(&vault, &mut engine, &mut journal, 1, 0)
        .expect_err("已上传未 ACK 必须拒绝");
    assert!(matches!(error, menghunote_sync::SyncError::Unsupported(_)));
    assert_eq!(journal.phase, RotationPhase::Begun);
}

#[test]
fn frozen_engine_rejects_push_but_keeps_local_edits_writable() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let doc = seed_doc(&vault, "note-1", materials.peer_id_u64());
    doc.insert_scalars(0, "local edit").expect("本地编辑");
    let mut engine = SyncEngine::new(
        &vault,
        &mut relay,
        EngineIdentity {
            device_id: materials.device_id,
            signing_key: materials.signing_key.clone(),
            peer_id: materials.peer_id_u64(),
        },
        materials.root.clone(),
    );
    engine.freeze_rotation();
    assert!(matches!(
        engine.push(),
        Err(menghunote_sync::SyncError::Paused(
            menghunote_sync::relay::PauseReason::RotationFrozen
        ))
    ));
    // A 事务编辑仍可写库。
    doc.insert_scalars(0, "more ").expect("冻结期编辑");
    assert!(doc.pending_update(&loro::VersionVector::default()).is_ok());
    engine.thaw_rotation();
    assert!(engine.push().is_ok());
    let _ = relay;
}
