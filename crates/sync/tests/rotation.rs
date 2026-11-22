//! 轮换阶段 B/C（task-102）：begin 丢响应找回、integrate 必须到 S。

mod support;

use support::*;

use menghunote_storage::{load_body_doc, set_confirmed_highwater, Vault};
use menghunote_sync::device::ManagementKey;
use menghunote_sync::engine::{EngineIdentity, SyncEngine};
use menghunote_sync::genesis;
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

#[test]
fn begin_response_loss_is_recovered_by_request_id() {
    let (_dir, vault, materials, mut relay) = setup();
    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x71; 16], relay.control_rev, [0x81; 32])
            .expect("freeze");
    relay.drop_begin_response = true;
    journal.confirm_r_prime_backed_up(&vault).expect("R' 备份");
    let manager =
        ManagementKey::from_recovery(&materials.recovery_seed, &VAULT_ID).expect("管理 key");
    rotation::verify_management_chain_and_pin(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &manager,
        None,
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
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    set_confirmed_highwater(&vault, 0).expect("highwater");
    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x72; 16], relay.control_rev, [0x82; 32])
            .expect("freeze");
    let manager =
        ManagementKey::from_recovery(&materials.recovery_seed, &VAULT_ID).expect("管理 key");
    journal.confirm_r_prime_backed_up(&vault).expect("R' 备份");
    rotation::verify_management_chain_and_pin(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &manager,
        None,
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
    bad_relay.manager_chain = relay.manager_chain.clone();
    bad_relay.manager_chain[0].transition[0] ^= 0x01;
    assert!(
        rotation::verify_management_chain_and_pin(
            &vault,
            &mut bad_relay,
            &mut journal,
            VAULT_ID,
            &new_manager,
            Some(manager.public_key()),
        )
        .is_err(),
        "篡改 transition 必败"
    );
    rotation::verify_management_chain_and_pin(
        &vault,
        &mut relay,
        &mut journal,
        VAULT_ID,
        &new_manager,
        Some(manager.public_key()),
    )
    .expect("管理链校验");
    assert_eq!(journal.control_chain_rev, Some(2));
    assert!(journal.pinned_instance_id.is_some());
    assert!(journal.pinned_log_id.is_some());
}

#[test]
fn revoked_device_history_before_s_still_imports() {
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
    )
    .is_err());
}

#[test]
fn frozen_engine_rejects_push_but_keeps_local_edits_writable() {
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
