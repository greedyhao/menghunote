//! 阶段 G/H：activate 签名材料、grant 解根拒绝、本地原子推广与 journal 收尾。

mod support;

use support::*;

use menghunote_storage::{outbox_parts, store_encrypted_parts, SyncEncryptedPart, Vault};
use menghunote_sync::device::ManagementKey;
use menghunote_sync::identity::{root_secret_for_epoch, IdentityRotationStateV1, LocalIdentityV1};
use menghunote_sync::keys::{RootSecretV1, Secret32};
use menghunote_sync::rotation_activate::{
    finish_promotion, promote_identity_atomic, promote_identity_with_material,
    promotion_material_from_recovery, sync_parent_directory, ActivatedOutcome, ActivationInput,
    AuthenticatedActivation, AuthenticatedPinV1, DirectoryDurability, PromotionMaterial,
};
use menghunote_sync::rotation_journal::{RotationJournal, RotationPhase};
use menghunote_sync::rotation_slots::{
    build_activation_t, build_manager_transition, candidate_asset_set_digest, ActivationTMaterial,
};
use menghunote_sync::rotation_stage::ReencryptedAsset;
use menghunote_sync::upload::{decrypt_envelope, encrypt_local_parts};
use menghunote_sync::{ids, DeviceId, VaultId};
use menghunote_sync_proto::{activation_t_signing_input, ActivationTV1, NewManagerV1, ObjectType};

fn identity(vault_id: VaultId, device_id: DeviceId) -> LocalIdentityV1 {
    LocalIdentityV1 {
        vault_id,
        device_id,
        peer_id: ids::PeerId::from_bytes([0x31; 16]),
        ed25519_seed: Secret32::from_array([0x32; 32]),
        x25519_secret: Secret32::from_array([0x33; 32]),
        current_epoch: EPOCH,
        current_root: Secret32::from_array([0x34; 32]),
        manager_seed: None,
        pinned_instance: [0x35; 16],
        pinned_manager: [0x36; 32],
        control_rev: 3,
        pending_rotation: None,
    }
}

/// 管理设备身份：旧管理 seed 与 pin 住的旧公钥一致。
fn manager_identity(vault_id: VaultId, device_id: DeviceId) -> LocalIdentityV1 {
    let mut identity = identity(vault_id, device_id);
    let old_seed = Secret32::from_array([0x37; 32]);
    identity.pinned_manager = ManagementKey::from_seed(old_seed.clone()).public_key();
    identity.manager_seed = Some(old_seed);
    identity
}

fn seed(byte: u8) -> Secret32 {
    Secret32::from_array([byte; 32])
}

/// 管理设备的推广材料：新管理 seed 由 byte 指定，公钥从 seed 派生（真实路径由 R' 派生）。
fn material(
    root_byte: u8,
    manager_seed_byte: u8,
    control_rev: u64,
    manager_rev: u64,
) -> PromotionMaterial {
    let new_seed = seed(manager_seed_byte);
    let new_manager_public = ManagementKey::from_seed(new_seed.clone()).public_key();
    PromotionMaterial {
        candidate_root: RootSecretV1::new(VAULT_ID, EPOCH + 1, seed(root_byte)),
        new_manager_seed: Some(new_seed),
        new_manager_public,
        new_manager_rev: manager_rev,
        control_rev,
        authentication: None,
    }
}

/// 本次 activation 的已验证 pin 值（测试固定字节）。
fn authenticated(control_rev: u64) -> AuthenticatedActivation {
    AuthenticatedActivation {
        instance_id: [0x35; 16],
        control_rev,
        activation_hash: [0x38; 32],
    }
}

/// 加密并落库一条 outbox part（epoch 由 root 决定；先写真实 update 满足外键）。
fn store_part(
    vault: &menghunote_storage::Vault,
    doc_id: &str,
    message_id: &str,
    root: &RootSecretV1,
    object_id: [u8; 32],
    device_id: DeviceId,
    plaintext: &[u8],
) {
    let doc = seed_doc(vault, doc_id, 0x5A5A);
    doc.insert_scalars(0, "outbox body").expect("write body");
    commit_with_intent(vault, &doc, message_id, object_id, root.key_epoch);
    let (parts, digest) = encrypt_local_parts(
        root,
        ObjectType::Update,
        &object_id,
        [0x11; 16],
        device_id,
        1,
        1,
        plaintext,
        &[],
    )
    .expect("encrypt part");
    let stored: Vec<SyncEncryptedPart> = parts
        .into_iter()
        .map(|(part_no, envelope, digest)| SyncEncryptedPart {
            part_no,
            envelope,
            digest,
        })
        .collect();
    store_encrypted_parts(vault, message_id, root.key_epoch, &digest, &stored)
        .expect("store parts");
}

#[test]
fn promote_identity_is_atomic_and_sets_repack_fence() {
    ensure_test_identity_wrap();
    let (dir, vault) = fresh_vault();
    let device_id = [0x41; 16];
    let old = manager_identity(VAULT_ID, device_id);
    let path = dir.path().join("identity.bin");
    std::fs::write(&path, old.protect().expect("old identity")).expect("write old");

    let material = material(0x42, 0x43, 9, 4);
    let promoted = promote_identity_with_material(&vault, &path, &old, &material).expect("promote");
    assert_eq!(promoted.current_epoch, EPOCH + 1);
    assert_eq!(promoted.pinned_manager, material.new_manager_public);
    // control_rev 取控制链 rev，不是 manager rev。
    assert_eq!(promoted.control_rev, 9);
    // 身份里的 manager_seed 必须对应 pin 住的新管理公钥。
    let promoted_seed = promoted.manager_seed.as_ref().expect("new manager seed");
    assert_eq!(
        ManagementKey::from_seed(promoted_seed.clone()).public_key(),
        promoted.pinned_manager
    );
    assert!(promoted_seed.ct_eq(material.new_manager_seed.as_ref().unwrap()));
    // 旁路状态：历史根 / 控制链 rev / 管理 rev 分开落盘。
    let state = IdentityRotationStateV1::load_current(&vault, &promoted)
        .expect("load state")
        .expect("state");
    assert_eq!(state.control_rev, 9);
    assert_eq!(state.manager_rev, 4);
    assert!(state
        .root_for_epoch(EPOCH)
        .expect("previous root")
        .ct_eq(&old.current_root));
    // 磁盘上的身份也是新值（DPAPI 重新加密后 rename 提交）。
    let loaded = LocalIdentityV1::unprotect(
        &std::fs::read(&path).expect("read promoted"),
        &VAULT_ID,
        &device_id,
    )
    .expect("load promoted");
    assert_eq!(loaded.current_epoch, EPOCH + 1);
    assert_eq!(loaded.pinned_manager, material.new_manager_public);
    assert_eq!(
        vault
            .get_meta("sync.repack_required")
            .expect("meta")
            .as_deref(),
        Some(b"1".as_slice())
    );

    // 失败路径：候选 epoch 不高于当前身份时拒绝，旧身份字节不变、不写屏障。
    let (second_dir, second_vault) = fresh_vault();
    let second_path = second_dir.path().join("identity.bin");
    std::fs::write(&second_path, promoted.protect().expect("promoted")).expect("write");
    let before = std::fs::read(&second_path).expect("before");
    assert!(
        promote_identity_with_material(&second_vault, &second_path, &promoted, &material).is_err()
    );
    assert_eq!(std::fs::read(&second_path).expect("after"), before);
    assert!(second_vault
        .get_meta("sync.repack_required")
        .expect("meta")
        .is_none());
}

#[test]
fn promotion_keeps_old_root_for_old_outbox_and_new_root_for_new() {
    ensure_test_identity_wrap();
    let (dir, vault) = fresh_vault();
    let device_id = [0x71; 16];
    let old = identity(VAULT_ID, device_id);
    let old_root = RootSecretV1::new(VAULT_ID, EPOCH, old.current_root.clone());
    let old_object: [u8; 32] = [0xA1; 32];
    let old_plaintext = b"old epoch outbox payload";
    store_part(
        &vault,
        "note-old",
        "msg-old",
        &old_root,
        old_object,
        device_id,
        old_plaintext,
    );

    let path = dir.path().join("identity.bin");
    std::fs::write(&path, old.protect().expect("old identity")).expect("write old");
    let material = material(0x42, 0x43, 9, 4);
    let promoted = promote_identity_with_material(&vault, &path, &old, &material).expect("promote");
    // 模拟进程重启：只从磁盘读回身份，再从旁路状态取旧根。
    let restarted = LocalIdentityV1::unprotect(
        &std::fs::read(&path).expect("read identity"),
        &VAULT_ID,
        &device_id,
    )
    .expect("load promoted identity");
    assert_eq!(restarted.current_epoch, promoted.current_epoch);

    // 推广后仍能按 epoch 取回旧根并解开旧 outbox part。
    let recovered_old = root_secret_for_epoch(&vault, &restarted, EPOCH)
        .expect("old root lookup")
        .expect("old root");
    assert!(recovered_old.root.ct_eq(&old.current_root));
    let old_parts = outbox_parts(&vault, "msg-old", EPOCH).expect("old parts");
    let mut opened = Vec::new();
    for part in &old_parts {
        let (_, plaintext) = decrypt_envelope(
            &recovered_old,
            ObjectType::Update,
            &old_object,
            &part.envelope,
            &[],
        )
        .expect("old part decrypt");
        opened.extend_from_slice(&plaintext);
    }
    assert_eq!(opened, old_plaintext);
    // 新根绝不能解开旧 part（证明两者确实是不同 epoch 的根）。
    let current = root_secret_for_epoch(&vault, &promoted, EPOCH + 1)
        .expect("new root")
        .expect("current root");
    assert!(decrypt_envelope(
        &current,
        ObjectType::Update,
        &old_object,
        &old_parts[0].envelope,
        &[],
    )
    .is_err());

    // 新 epoch 的 part 用新根存/解。
    let new_object: [u8; 32] = [0xB2; 32];
    let new_plaintext = b"new epoch outbox payload";
    store_part(
        &vault,
        "note-new",
        "msg-new",
        &current,
        new_object,
        device_id,
        new_plaintext,
    );
    let new_parts = outbox_parts(&vault, "msg-new", EPOCH + 1).expect("new parts");
    let (_, plaintext) = decrypt_envelope(
        &current,
        ObjectType::Update,
        &new_object,
        &new_parts[0].envelope,
        &[],
    )
    .expect("new part decrypt");
    assert_eq!(plaintext.as_slice(), new_plaintext);
}

#[test]
fn finish_promotion_advances_activated_promoted_completed() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x91; 16], 7, [0x92; 32]).expect("freeze");
    journal.confirm_r_prime_backed_up(&vault).expect("R' 备份");
    journal
        .mark_begun([0x93; 16], 5, [0x94; 16])
        .expect("begun");
    for phase in [
        RotationPhase::Integrated,
        RotationPhase::Reencrypted,
        RotationPhase::BaselinesReady,
        RotationPhase::GrantsReady,
        RotationPhase::Activated,
    ] {
        journal.advance(phase).expect("phase");
    }
    journal.set_candidate(
        RootSecretV1::new(VAULT_ID, EPOCH + 1, seed(0x95)),
        [0x96; 32],
        2,
        true,
    );
    journal.new_slot_bodies.push(vec![0x97; 8]);
    journal.save(&vault).expect("save activated");

    // Activated → Promoted → Completed，不跳步、不报错。
    finish_promotion(&mut journal, &vault).expect("finish promotion");
    assert_eq!(journal.phase, RotationPhase::Completed);
    assert!(journal.candidate_root.is_none(), "候选根必须清除");
    assert!(journal.new_slot_bodies.is_empty(), "候选槽体必须清除");
    let loaded = RotationJournal::load(&vault)
        .expect("load")
        .expect("journal");
    assert_eq!(loaded.phase, RotationPhase::Completed);
    assert!(loaded.candidate_root.is_none());
    // 崩溃后重入幂等。
    finish_promotion(&mut journal, &vault).expect("finish again");

    // Promoted 是合法的重入点。
    let (_second_dir, second_vault) = fresh_vault();
    let mut promoted_journal =
        RotationJournal::freeze(&second_vault, EPOCH, [0x98; 16], 7, [0x99; 32]).expect("freeze");
    promoted_journal
        .confirm_r_prime_backed_up(&second_vault)
        .expect("R' 备份");
    promoted_journal
        .mark_begun([0x9A; 16], 5, [0x9B; 16])
        .expect("begun");
    for phase in [
        RotationPhase::Integrated,
        RotationPhase::Reencrypted,
        RotationPhase::BaselinesReady,
        RotationPhase::GrantsReady,
        RotationPhase::Activated,
        RotationPhase::Promoted,
    ] {
        promoted_journal.advance(phase).expect("phase");
    }
    finish_promotion(&mut promoted_journal, &second_vault).expect("finish from promoted");
    assert_eq!(promoted_journal.phase, RotationPhase::Completed);

    // 其它 phase（例如 Begun）不能直接 finish。
    let (_third_dir, third_vault) = fresh_vault();
    let mut begun =
        RotationJournal::freeze(&third_vault, EPOCH, [0x9C; 16], 7, [0x9D; 32]).expect("freeze");
    begun.mark_begun([0x9E; 16], 5, [0x9F; 16]).expect("begun");
    assert!(finish_promotion(&mut begun, &third_vault).is_err());
    assert_eq!(begun.phase, RotationPhase::Begun);
}

#[test]
fn control_rev_and_manager_rev_are_not_confused() {
    ensure_test_identity_wrap();
    let (dir, vault) = fresh_vault();
    let device_id = [0xA1; 16];
    let old = manager_identity(VAULT_ID, device_id);
    let path = dir.path().join("identity.bin");
    std::fs::write(&path, old.protect().expect("old")).expect("write");
    let material = material(0x42, 0x43, 12, 3);
    // activate 回执绑定的 control_rev 才是控制链 rev。
    let outcome = ActivatedOutcome {
        epoch: EPOCH + 1,
        control_rev: 12,
        auth_rev: 4,
        member_set_hash: [0x51; 32],
        primary_checkpoint: [0x52; 16],
        fallback_checkpoint: [0x53; 16],
        activation_commit: vec![0x54; 8],
    };
    let bound = PromotionMaterial::from_activated(
        &outcome,
        material.candidate_root.clone(),
        material.new_manager_seed.clone(),
        material.new_manager_public,
        material.new_manager_rev,
    );
    assert_eq!(bound.control_rev, 12);
    let promoted =
        promote_identity_with_material(&vault, &path, &old, &bound).expect("promote from receipt");
    assert_eq!(promoted.control_rev, 12);
    let state = IdentityRotationStateV1::load_current(&vault, &promoted)
        .expect("state")
        .expect("state");
    assert_eq!(state.control_rev, 12);
    assert_eq!(state.manager_rev, 3);
    assert_ne!(state.control_rev, state.manager_rev);

    // 6 参数兼容入口：写入本次 activation 已验证的 control_rev，不再保留旧值。
    let (second_dir, second_vault) = fresh_vault();
    let path2 = second_dir.path().join("identity.bin");
    let old2 = identity(VAULT_ID, [0xA2; 16]);
    std::fs::write(&path2, old2.protect().expect("old")).expect("write");
    let authentication = authenticated(12);
    let promoted2 = promote_identity_atomic(
        &second_vault,
        &path2,
        &old2,
        &RootSecretV1::new(VAULT_ID, EPOCH + 1, seed(0x55)),
        &authentication,
        [0x56; 32],
        8,
    )
    .expect("legacy promote");
    assert_eq!(
        promoted2.control_rev, 12,
        "推广必须写入 activation 的 control_rev，而不是旧身份的 3"
    );
    assert_ne!(promoted2.control_rev, old2.control_rev);
    assert_ne!(promoted2.control_rev, 8, "control_rev 不是 manager rev");
    assert_eq!(promoted2.pinned_instance, [0x35; 16]);
    let state2 = IdentityRotationStateV1::load_current(&second_vault, &promoted2)
        .expect("state")
        .expect("state");
    assert_eq!(state2.manager_rev, 8);
    assert_eq!(state2.control_rev, 12);
    let pin = AuthenticatedPinV1::load_current(&second_vault, &promoted2)
        .expect("pin")
        .expect("pin");
    assert_eq!(pin.epoch, EPOCH + 1);
    assert_eq!(pin.control_rev, 12);
    assert_eq!(pin.instance_id, [0x35; 16]);
    assert_eq!(pin.activation_hash, [0x38; 32]);
}

#[test]
fn management_device_requires_and_verifies_new_manager_seed() {
    ensure_test_identity_wrap();
    let (dir, vault) = fresh_vault();
    let device_id = [0xB1; 16];
    let old = manager_identity(VAULT_ID, device_id);
    let path = dir.path().join("identity.bin");
    std::fs::write(&path, old.protect().expect("old")).expect("write");
    let before = std::fs::read(&path).expect("before");

    // 6 参数入口没有新管理 seed：管理设备必须失败（旧 seed 不能继续冒充 current）。
    let candidate = RootSecretV1::new(VAULT_ID, EPOCH + 1, seed(0x42));
    assert!(
        promote_identity_atomic(
            &vault,
            &path,
            &old,
            &candidate,
            &authenticated(9),
            [0x43; 32],
            4
        )
        .is_err(),
        "管理设备缺新管理 seed 时必须拒绝"
    );
    assert_eq!(std::fs::read(&path).expect("unchanged"), before);
    assert!(vault
        .get_meta("sync.repack_required")
        .expect("meta")
        .is_none());

    // seed 与公钥不一致：拒绝且不落盘。
    let mut mismatched = material(0x42, 0x43, 9, 4);
    mismatched.new_manager_public = [0x44; 32];
    assert!(promote_identity_with_material(&vault, &path, &old, &mismatched).is_err());
    assert_eq!(std::fs::read(&path).expect("unchanged"), before);
    assert!(IdentityRotationStateV1::load_current(&vault, &old)
        .expect("state")
        .is_none());

    // 正确材料：seed 对应新公钥，旧 seed 被丢弃（不保留成 current）。
    let good = material(0x42, 0x43, 9, 4);
    let promoted = promote_identity_with_material(&vault, &path, &old, &good).expect("promote");
    let new_seed = promoted.manager_seed.as_ref().expect("new seed");
    assert_eq!(
        ManagementKey::from_seed(new_seed.clone()).public_key(),
        promoted.pinned_manager
    );
    assert!(!new_seed.ct_eq(old.manager_seed.as_ref().unwrap()));
}

#[test]
fn promotion_material_from_recovery_derives_manager_seed() {
    ensure_test_identity_wrap();
    use menghunote_sync::recovery::{management_seed, RecoverySeed};
    let recovery_seed = RecoverySeed::from_array([0x61; 32]);
    let outcome = ActivatedOutcome {
        epoch: EPOCH + 1,
        control_rev: 13,
        auth_rev: 2,
        member_set_hash: [0x62; 32],
        primary_checkpoint: [0x63; 16],
        fallback_checkpoint: [0x64; 16],
        activation_commit: vec![0x65; 8],
    };
    let material = promotion_material_from_recovery(
        &outcome,
        RootSecretV1::new(VAULT_ID, EPOCH + 1, seed(0x42)),
        &recovery_seed,
        &VAULT_ID,
        4,
    )
    .expect("material");
    let expected = management_seed(&recovery_seed, &VAULT_ID).expect("seed");
    assert!(material.new_manager_seed.as_ref().unwrap().ct_eq(&expected));
    assert_eq!(
        ManagementKey::from_seed(expected).public_key(),
        material.new_manager_public
    );
    assert_eq!(material.control_rev, outcome.control_rev);
    assert_ne!(material.control_rev, material.new_manager_rev);
}

fn t_material(new_manager_public: [u8; 32]) -> ActivationTMaterial {
    ActivationTMaterial {
        vault_id: VAULT_ID,
        instance_id: [0x61; 16],
        rotation_id: [0x62; 16],
        from_epoch: EPOCH,
        to_epoch: EPOCH + 1,
        freeze_seq: 17,
        old_manager_rev: 3,
        new_manager_rev: 4,
        new_manager_public,
        prev_control_hash: [0x63; 32],
        expected_control_rev: 12,
        primary_checkpoint: [0x64; 16],
        primary_commit_digest: [0x65; 32],
        fallback_checkpoint: [0x66; 16],
        fallback_commit_digest: [0x67; 32],
        slot_set_digest: [0x68; 32],
        member_cert_digest: [0x69; 32],
        grants_digest: [0x6A; 32],
        old_asset_set_digest: [0x6B; 32],
        new_asset_set_digest: [0x6C; 32],
    }
}

#[test]
fn build_activation_t_covers_all_fields_and_rejects_invalid_bounds() {
    ensure_test_identity_wrap();
    let old_manager = ManagementKey::from_seed(Secret32::from_array([0x71; 32]));
    let new_manager = ManagementKey::from_seed(Secret32::from_array([0x72; 32]));
    let material = t_material(new_manager.public_key());
    let activation_t = build_activation_t(&material).expect("activation T");
    assert_eq!(activation_t.vault_id, material.vault_id);
    assert_eq!(activation_t.instance_id, material.instance_id);
    assert_eq!(activation_t.rotation_id, material.rotation_id);
    assert_eq!(activation_t.from_epoch, material.from_epoch);
    assert_eq!(activation_t.to_epoch, material.to_epoch);
    assert_eq!(activation_t.freeze_seq, material.freeze_seq);
    assert_eq!(activation_t.old_manager_rev, 3);
    assert_eq!(activation_t.new_manager_rev, 4);
    assert_eq!(activation_t.new_manager_pubkey, material.new_manager_public);
    assert_eq!(activation_t.prev_control_hash, material.prev_control_hash);
    assert_eq!(
        activation_t.expected_control_rev,
        material.expected_control_rev
    );
    assert_eq!(activation_t.primary_cid, material.primary_checkpoint);
    assert_eq!(
        activation_t.primary_commit_digest,
        material.primary_commit_digest
    );
    assert_eq!(activation_t.fallback_cid, material.fallback_checkpoint);
    assert_eq!(
        activation_t.fallback_commit_digest,
        material.fallback_commit_digest
    );
    assert_eq!(activation_t.slot_set_digest, material.slot_set_digest);
    assert_eq!(activation_t.member_cert_digest, material.member_cert_digest);
    assert_eq!(activation_t.grants_digest, material.grants_digest);
    assert_eq!(
        activation_t.old_asset_set_digest,
        material.old_asset_set_digest
    );
    assert_eq!(
        activation_t.new_asset_set_digest,
        material.new_asset_set_digest
    );
    let encoded = activation_t.encode().expect("T encode");
    assert_eq!(
        ActivationTV1::decode(&encoded).expect("T decode"),
        activation_t
    );

    // rev 必须连续、to_epoch 必须 > from_epoch、digest 不得全零。
    let mut bad = material.clone();
    bad.new_manager_rev = bad.old_manager_rev + 2;
    assert!(build_activation_t(&bad).is_err());
    let mut bad = material.clone();
    bad.to_epoch = bad.from_epoch;
    assert!(build_activation_t(&bad).is_err());
    let mut bad = material.clone();
    bad.slot_set_digest = [0u8; 32];
    assert!(build_activation_t(&bad).is_err());
    let mut bad = material.clone();
    bad.old_manager_rev = 0;
    assert!(build_activation_t(&bad).is_err());

    // 双签必须覆盖 `MN/v1/activation-t\0 || CDE(T)`，而不是 transition。
    let new_manager_record = build_manager_transition(&material, &old_manager, &new_manager)
        .expect("new manager payload");
    let t_bytes = new_manager_record
        .activation_t
        .as_deref()
        .expect("activation T bytes");
    assert_eq!(t_bytes, encoded.as_slice());
    let t_input = activation_t_signing_input(&activation_t).expect("T signing input");
    for (label, public, signature) in [
        (
            "旧管理 T 签名",
            old_manager.public_key(),
            new_manager_record.old_t_signature.expect("old T sig"),
        ),
        (
            "新管理 T 签名",
            new_manager.public_key(),
            new_manager_record.new_t_signature.expect("new T sig"),
        ),
    ] {
        assert_eq!(signature.len(), 64);
        menghunote_sync::device::verify_signature(
            &public,
            &t_input,
            &signature.as_slice().try_into().expect("64"),
        )
        .unwrap_or_else(|error| panic!("{label}: {error}"));
    }
}

#[test]
fn new_manager_wire_rejects_missing_or_short_t_and_signatures() {
    ensure_test_identity_wrap();
    let old_manager = ManagementKey::from_seed(Secret32::from_array([0x73; 32]));
    let new_manager = ManagementKey::from_seed(Secret32::from_array([0x74; 32]));
    let material = t_material(new_manager.public_key());
    let payload = build_manager_transition(&material, &old_manager, &new_manager)
        .expect("new manager payload");
    // 完整 payload 可往返。
    let wire = payload.encode().expect("encode");
    assert_eq!(NewManagerV1::decode(&wire).expect("decode"), payload);
    assert!(payload
        .activation_t
        .as_ref()
        .is_some_and(|bytes| !bytes.is_empty()));

    // 63B T 签名、缺一条 T 签名、缺 T 本体：都必须拒绝（3–5 all-or-none）。
    for mutate in [
        (|value: &mut NewManagerV1| {
            value.old_t_signature = Some(vec![0xAA; 63]);
        }) as fn(&mut NewManagerV1),
        |value: &mut NewManagerV1| {
            value.new_t_signature = None;
        },
        |value: &mut NewManagerV1| {
            value.activation_t = None;
        },
        |value: &mut NewManagerV1| {
            value.activation_t = None;
            value.old_t_signature = None;
            value.new_t_signature = None;
        },
    ] {
        let mut bad = payload.clone();
        mutate(&mut bad);
        let result = bad.encode();
        if bad.activation_t.is_none()
            && bad.old_t_signature.is_none()
            && bad.new_t_signature.is_none()
        {
            // 历史 all-none wire 仍可编码，但 relay activate 会 409；这里只锁定它不是
            // “新 activate 路径”的产物。
            assert!(result.is_ok());
        } else {
            assert!(result.is_err(), "T/双签不全必须拒绝编码");
        }
    }
}

#[test]
fn candidate_asset_set_digest_is_sorted_and_epoch_bound() {
    ensure_test_identity_wrap();
    let make = |object_id: u8, object_digest: u8| ReencryptedAsset {
        real_sha256: [0u8; 32],
        byte_len: 10,
        media_type: "text/plain".to_owned(),
        object_id: [object_id; 32],
        object_digest: [object_digest; 32],
    };
    let first = make(0x01, 0xA1);
    let second = make(0x02, 0xB2);
    let forward = candidate_asset_set_digest(EPOCH + 1, &[first.clone(), second.clone()])
        .expect("candidate digest");
    let reverse =
        candidate_asset_set_digest(EPOCH + 1, &[second, first]).expect("candidate digest reversed");
    assert_eq!(forward, reverse, "ObjectRefV1 集合必须按 object_id 排序");
    let other_epoch = candidate_asset_set_digest(EPOCH + 2, &[make(0x01, 0xA1), make(0x02, 0xB2)])
        .expect("other epoch");
    assert_ne!(forward, other_epoch, "key_epoch 必须参与摘要");
}

#[test]
fn activation_input_binds_four_digests_and_checkpoints() {
    ensure_test_identity_wrap();
    let input = ActivationInput {
        rotation_id: [0x51; 16],
        request_id: [0x52; 16],
        expected_control_rev: 7,
        primary_checkpoint: [0x53; 16],
        fallback_checkpoint: [0x54; 16],
        member_cert_digest: [0x55; 32],
        slot_set_digest: [0x56; 32],
        grants_digest: [0x57; 32],
        asset_set_digest: [0x58; 32],
        new_manager: None,
        candidate_root: RootSecretV1::new(VAULT_ID, EPOCH + 1, Secret32::from_array([0x59; 32])),
        new_manager_public: [0x5A; 32],
        new_manager_rev: 8,
        pv: 1,
    };
    let digest = menghunote_sync::rotation_activate::activate_body_digest(&input).expect("digest");
    assert_ne!(digest, [0u8; 32]);
    let body = input_body(&input);
    assert_eq!(body.member_cert_digest, Some([0x55; 32]));
    assert_eq!(body.slot_set_digest, Some([0x56; 32]));
    assert_eq!(body.grants_digest, Some([0x57; 32]));
    assert_eq!(body.asset_set_digest, Some([0x58; 32]));
    assert_eq!(body.primary_checkpoint, Some([0x53; 16]));
    assert_eq!(body.fallback_checkpoint, Some([0x54; 16]));
}

fn input_body(input: &ActivationInput) -> menghunote_sync_proto::RotationActivateBodyV1 {
    menghunote_sync_proto::RotationActivateBodyV1 {
        new_manager: input.new_manager.clone(),
        primary_checkpoint: Some(input.primary_checkpoint),
        fallback_checkpoint: Some(input.fallback_checkpoint),
        member_cert_digest: Some(input.member_cert_digest),
        grants_digest: Some(input.grants_digest),
        slot_set_digest: Some(input.slot_set_digest),
        asset_set_digest: Some(input.asset_set_digest),
    }
}

// ============================================================ 评审 0002 T06 补充
// 推广身份在 temp 写 / rotation_state DB / repack fence / rename 各点失败：
// 旧身份与秘密保持完整，或新身份整体生效；不得出现半推广。

fn old_identity_on_disk(
    dir: &tempfile::TempDir,
    device_id: DeviceId,
) -> (LocalIdentityV1, std::path::PathBuf) {
    let old = manager_identity(VAULT_ID, device_id);
    let path = dir.path().join("identity.bin");
    std::fs::write(&path, old.protect().expect("old identity")).expect("write old");
    (old, path)
}

#[test]
fn promotion_fails_at_each_durable_point_without_half_identity() {
    ensure_test_identity_wrap();
    // 1) temp 身份写失败：父目录不存在 → 零副作用。
    {
        let (dir, vault) = fresh_vault();
        let device_id = [0x81; 16];
        let old = manager_identity(VAULT_ID, device_id);
        let missing = dir.path().join("missing").join("identity.bin");
        let error =
            promote_identity_with_material(&vault, &missing, &old, &material(0x42, 0x43, 9, 4))
                .expect_err("父目录不存在时 temp 写必须失败");
        assert!(!format!("{error:?}").is_empty());
        assert!(
            IdentityRotationStateV1::load_current(&vault, &old)
                .expect("state")
                .is_none(),
            "temp 写失败不得落 rotation_state"
        );
        assert!(
            vault
                .get_meta("sync.repack_required")
                .expect("meta")
                .is_none(),
            "temp 写失败不得置 repack 屏障"
        );
        assert!(!missing.exists());
    }

    // 2) rotation_state DB 写失败：正式身份文件与 repack 屏障都不变。
    {
        let (dir, vault) = fresh_vault();
        let device_id = [0x82; 16];
        let (old, path) = old_identity_on_disk(&dir, device_id);
        let before = std::fs::read(&path).expect("before");
        vault
            .exec_batch(
                "CREATE TRIGGER fail_rotation_state BEFORE INSERT ON meta \
                 WHEN NEW.key = 'sync.identity.rotation_state' \
                 BEGIN SELECT RAISE(ABORT,'injected state'); END;",
            )
            .expect("注入 state 失败");
        assert!(
            promote_identity_with_material(&vault, &path, &old, &material(0x42, 0x43, 9, 4))
                .is_err()
        );
        assert_eq!(
            std::fs::read(&path).expect("after"),
            before,
            "旧身份必须逐字节不变"
        );
        assert!(IdentityRotationStateV1::load_current(&vault, &old)
            .expect("state")
            .is_none());
        assert!(vault
            .get_meta("sync.repack_required")
            .expect("meta")
            .is_none());
        // 撤销注入后同一推广必须成功（失败不消耗材料）。
        vault
            .exec_batch("DROP TRIGGER fail_rotation_state")
            .expect("drop");
        let promoted =
            promote_identity_with_material(&vault, &path, &old, &material(0x42, 0x43, 9, 4))
                .expect("恢复后推广");
        assert_eq!(promoted.current_epoch, EPOCH + 1);
        assert_eq!(
            vault
                .get_meta("sync.repack_required")
                .expect("meta")
                .as_deref(),
            Some(b"1".as_slice())
        );
    }

    // 3) repack 屏障写失败：rotation_state 已领先，但旧身份 epoch 不匹配会被忽略；
    //    重启后仍只能用旧身份/旧根，且不得有半套新身份。
    {
        let (dir, vault) = fresh_vault();
        let device_id = [0x83; 16];
        let (old, path) = old_identity_on_disk(&dir, device_id);
        let before = std::fs::read(&path).expect("before");
        vault
            .exec_batch(
                "CREATE TRIGGER fail_repack_fence BEFORE INSERT ON meta \
                 WHEN NEW.key = 'sync.repack_required' \
                 BEGIN SELECT RAISE(ABORT,'injected fence'); END;",
            )
            .expect("注入 fence 失败");
        assert!(
            promote_identity_with_material(&vault, &path, &old, &material(0x42, 0x43, 9, 4))
                .is_err()
        );
        assert_eq!(
            std::fs::read(&path).expect("after"),
            before,
            "正式身份不得被替换"
        );
        assert!(vault
            .get_meta("sync.repack_required")
            .expect("meta")
            .is_none());
        let state = IdentityRotationStateV1::load(&vault, &VAULT_ID, &device_id)
            .expect("state")
            .expect("rotation_state 已写入");
        assert_eq!(state.current_epoch, EPOCH + 1);
        assert!(
            IdentityRotationStateV1::load_current(&vault, &old)
                .expect("state")
                .is_none(),
            "旁路状态 epoch 领先旧身份；load_current 必须忽略（旧身份仍权威）"
        );
        // 重开 vault 再读：旧身份仍是唯一可信身份，临时文件不生效。
        drop(vault);
        let vault = Vault::open(dir.path().join("vaults").join("primary.sqlite3")).expect("reopen");
        let reloaded = LocalIdentityV1::unprotect(
            &std::fs::read(&path).expect("read identity"),
            &VAULT_ID,
            &device_id,
        )
        .expect("旧身份可解");
        assert_eq!(reloaded.current_epoch, old.current_epoch);
        assert_eq!(reloaded.pinned_manager, old.pinned_manager);
        assert!(
            IdentityRotationStateV1::load_current(&vault, &reloaded)
                .expect("state")
                .is_none(),
            "epoch 不匹配的旁路状态不得被当成当前身份"
        );
    }

    // 4) rename 失败（目标路径是目录）：temp/旁路状态/屏障可能已写，但正式身份未提交；
    //    清障后重试必须整体成功。
    {
        let (dir, vault) = fresh_vault();
        let device_id = [0x84; 16];
        let old = manager_identity(VAULT_ID, device_id);
        let path = dir.path().join("identity.bin");
        std::fs::create_dir(&path).expect("用目录占位让 rename 失败");
        let error =
            promote_identity_with_material(&vault, &path, &old, &material(0x42, 0x43, 9, 4))
                .expect_err("rename 到目录必须失败");
        assert!(!format!("{error:?}").is_empty());
        assert!(path.is_dir(), "失败不得破坏目标");
        assert_eq!(
            vault
                .get_meta("sync.repack_required")
                .expect("meta")
                .as_deref(),
            Some(b"1".as_slice()),
            "rename 前必须已置 durable barrier（崩溃可恢复）"
        );
        // 清除占位目录后重试：同一推广必须整体成功。
        std::fs::remove_dir(&path).expect("remove placeholder");
        let promoted =
            promote_identity_with_material(&vault, &path, &old, &material(0x42, 0x43, 9, 4))
                .expect("清障后推广");
        assert_eq!(promoted.current_epoch, EPOCH + 1);
        let loaded =
            LocalIdentityV1::unprotect(&std::fs::read(&path).expect("read"), &VAULT_ID, &device_id)
                .expect("新身份可解");
        assert_eq!(loaded.current_epoch, EPOCH + 1);
        assert_eq!(loaded.pinned_manager, promoted.pinned_manager);
        assert_eq!(
            loaded
                .manager_seed
                .as_ref()
                .map(|seed| { ManagementKey::from_seed(seed.clone()).public_key() }),
            Some(promoted.pinned_manager),
            "新身份里的 manager_seed 必须对应 pin 住的新公钥"
        );
        assert_eq!(loaded.control_rev, 9, "control_rev 与 manager_rev 不混");
        assert_eq!(
            IdentityRotationStateV1::load_current(&vault, &loaded)
                .expect("state")
                .expect("state")
                .manager_rev,
            4
        );
    }
}

#[test]
fn promotion_writes_authenticated_pin_and_directory_durability_boundary() {
    ensure_test_identity_wrap();
    let (dir, vault) = fresh_vault();
    let device_id = [0x87; 16];
    let (old, path) = old_identity_on_disk(&dir, device_id);
    let mut material = material(0x42, 0x43, 12, 4);
    material.authentication = Some(authenticated(12));

    let promoted = promote_identity_with_material(&vault, &path, &old, &material).expect("promote");
    assert_eq!(promoted.current_epoch, EPOCH + 1);
    assert_eq!(promoted.control_rev, 12, "必须写 activation 的 control_rev");
    assert_eq!(promoted.pinned_instance, [0x35; 16]);
    assert_eq!(promoted.pinned_manager, material.new_manager_public);

    // 身份文件里也必须是新 pin（不是只改内存返回值）。
    let loaded =
        LocalIdentityV1::unprotect(&std::fs::read(&path).expect("disk"), &VAULT_ID, &device_id)
            .expect("identity");
    assert_eq!(loaded.current_epoch, EPOCH + 1);
    assert_eq!(loaded.control_rev, 12);
    assert_eq!(loaded.pinned_instance, [0x35; 16]);

    // authenticated pin：epoch 与身份对齐，control_rev/instance/hash 都是本次 activation 的值。
    let pin = AuthenticatedPinV1::load_current(&vault, &loaded)
        .expect("pin")
        .expect("pin");
    assert_eq!(
        pin,
        AuthenticatedPinV1 {
            epoch: EPOCH + 1,
            control_rev: 12,
            instance_id: [0x35; 16],
            activation_hash: [0x38; 32],
        }
    );
    assert_eq!(
        IdentityRotationStateV1::load_current(&vault, &loaded)
            .expect("state")
            .expect("state")
            .control_rev,
        12
    );

    // 目录 durability 的平台边界必须显式可验证，不能把 rename 成功冒充掉电持久。
    let durability = sync_parent_directory(&path).expect("目录 durability");
    #[cfg(unix)]
    assert_eq!(durability, DirectoryDurability::Synced);
    #[cfg(not(unix))]
    assert_eq!(
        durability,
        DirectoryDurability::PlatformUnsupported,
        "Windows 无目录 fsync：边界是 temp sync_all + rename 原子提交"
    );
}

#[test]
fn failed_replace_keeps_old_identity_authoritative_and_ignores_future_pin() {
    ensure_test_identity_wrap();
    let (dir, vault) = fresh_vault();
    let device_id = [0x88; 16];
    let old = manager_identity(VAULT_ID, device_id);
    let mut material = material(0x42, 0x43, 12, 4);
    material.authentication = Some(authenticated(12));

    // rename 失败（目标被目录占位）：pin/旁路状态/fence 已写，但身份未提交。
    let placeholder = dir.path().join("blocked.bin");
    std::fs::create_dir(&placeholder).expect("占位目录");
    let error = promote_identity_with_material(&vault, &placeholder, &old, &material)
        .expect_err("rename 到目录必须失败");
    assert!(!format!("{error:?}").is_empty());
    assert!(placeholder.is_dir(), "失败不得破坏目标");
    assert!(
        !placeholder.with_extension("promote.tmp").exists(),
        "失败必须清理临时身份，避免半份 BLOB 影响重试"
    );
    // future pin 因 epoch 不匹配被忽略：旧身份仍是唯一权威，不得出现半套新身份。
    let future_pin = AuthenticatedPinV1::load(&vault)
        .expect("pin")
        .expect("pin 已写（rename 之前）");
    assert_eq!(future_pin.epoch, EPOCH + 1);
    assert_eq!(future_pin.control_rev, 12);
    assert!(
        AuthenticatedPinV1::load_current(&vault, &old)
            .expect("pin")
            .is_none(),
        "epoch 不匹配的 pin 是崩溃残留，旧身份仍权威"
    );
    assert!(
        IdentityRotationStateV1::load_current(&vault, &old)
            .expect("state")
            .is_none(),
        "旁路状态同样不得被当成当前身份"
    );
    // 清障后重试整体成功，pin 变成当前值。
    std::fs::remove_dir(&placeholder).expect("清障");
    let promoted =
        promote_identity_with_material(&vault, &placeholder, &old, &material).expect("重试");
    assert_eq!(promoted.control_rev, 12);
    let pin = AuthenticatedPinV1::load_current(&vault, &promoted)
        .expect("pin")
        .expect("pin");
    assert_eq!(pin.epoch, EPOCH + 1);
    assert_eq!(pin.activation_hash, [0x38; 32]);
}

#[test]
fn promotion_through_journal_keeps_activated_secrets_across_restart() {
    ensure_test_identity_wrap();
    let (dir, vault) = fresh_vault();
    let device_id = [0x85; 16];
    let (old, path) = old_identity_on_disk(&dir, device_id);
    let mut promoted =
        promote_identity_with_material(&vault, &path, &old, &material(0x42, 0x43, 9, 4))
            .expect("promote");
    assert_eq!(promoted.current_epoch, EPOCH + 1);

    // journal：Activated → Promoted → Completed；重启后 journal 已清，但身份/旧根仍在。
    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x86; 16], 9, [0x87; 32]).expect("freeze");
    journal.confirm_r_prime_backed_up(&vault).expect("R'");
    journal
        .mark_begun([0x88; 16], 7, [0x89; 16])
        .expect("begun");
    for phase in [
        RotationPhase::Integrated,
        RotationPhase::Reencrypted,
        RotationPhase::BaselinesReady,
        RotationPhase::GrantsReady,
        RotationPhase::Activated,
    ] {
        journal.advance(phase).expect("advance");
    }
    journal.save(&vault).expect("save journal");
    // journal 里的候选根/槽体密文与身份里的新根必须一致。
    journal.candidate_root = Some(material(0x42, 0x43, 9, 4).candidate_root);
    journal.save(&vault).expect("save candidate");
    finish_promotion(&mut journal, &vault).expect("finish promotion");
    assert_eq!(journal.phase, RotationPhase::Completed);
    assert!(journal.candidate_root.is_none(), "完成后必须清除候选裸根");

    drop(vault);
    let vault = Vault::open(dir.path().join("vaults").join("primary.sqlite3")).expect("reopen");
    promoted =
        LocalIdentityV1::unprotect(&std::fs::read(&path).expect("read"), &VAULT_ID, &device_id)
            .expect("restart identity");
    assert_eq!(promoted.current_epoch, EPOCH + 1);
    let restarted_journal = RotationJournal::load(&vault)
        .expect("journal")
        .expect("Completed journal 保留到 barrier 清理");
    assert_eq!(restarted_journal.phase, RotationPhase::Completed);
    assert!(restarted_journal.candidate_root.is_none());
    assert!(root_secret_for_epoch(&vault, &promoted, EPOCH)
        .expect("old root")
        .is_some());
    assert_eq!(
        vault
            .get_meta("sync.repack_required")
            .expect("meta")
            .as_deref(),
        Some(b"1".as_slice()),
        "完成 journal 不等于可清 repack 屏障"
    );
}
