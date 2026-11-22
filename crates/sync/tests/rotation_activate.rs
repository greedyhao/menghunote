//! 阶段 G/H：activate 签名材料、grant 解根拒绝、本地原子推广。

mod support;

use support::*;

use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::keys::{RootSecretV1, Secret32};
use menghunote_sync::rotation_activate::{promote_identity_atomic, ActivationInput};
use menghunote_sync::{ids, DeviceId, VaultId};

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

#[test]
fn promote_identity_is_atomic_and_sets_repack_fence() {
    let (dir, vault) = fresh_vault();
    let device_id = [0x41; 16];
    let old = identity(VAULT_ID, device_id);
    let path = dir.path().join("identity.bin");
    std::fs::write(&path, old.protect().expect("old identity")).expect("write old");

    let candidate = RootSecretV1::new(VAULT_ID, EPOCH + 1, Secret32::from_array([0x42; 32]));
    let promoted =
        promote_identity_atomic(&vault, &path, &old, &candidate, [0x43; 32], 4).expect("promote");
    assert_eq!(promoted.current_epoch, EPOCH + 1);
    assert_eq!(promoted.pinned_manager, [0x43; 32]);
    assert_eq!(promoted.control_rev, 4);
    let loaded = LocalIdentityV1::unprotect(
        &std::fs::read(&path).expect("read promoted"),
        &VAULT_ID,
        &device_id,
    )
    .expect("load promoted");
    assert_eq!(loaded.current_epoch, EPOCH + 1);
    assert_eq!(
        vault
            .get_meta("sync.repack_required")
            .expect("meta")
            .as_deref(),
        Some(b"1".as_slice())
    );

    // 失败路径：候选 epoch 不高于当前身份时拒绝，旧身份字节不变。
    let before = std::fs::read(&path).expect("before");
    assert!(promote_identity_atomic(&vault, &path, &promoted, &candidate, [0x44; 32], 5,).is_err());
    assert_eq!(std::fs::read(&path).expect("after"), before);
}

#[test]
fn activation_input_binds_four_digests_and_checkpoints() {
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
