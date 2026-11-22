//! P1：候选槽新秘密 / slot_set_digest / manager transition 双签。

mod support;

use support::*;

use menghunote_sync::device::{DeviceSigningKey, ManagementKey};
use menghunote_sync::keys::{RootSecretV1, Secret32};
use menghunote_sync::recovery::RecoverySeed;
use menghunote_sync::relay::RelayClient;
use menghunote_sync::rotation::ManagerTransitionV1;
use menghunote_sync::rotation_slots::{
    build_manager_transition, generate_candidate_slots, slot_set_digest, upload_candidate_slots,
    verify_new_manager, verify_recovery_code, ActivationTMaterial,
};
use menghunote_sync::slot::{KeySlotV1, WrappedRoot};
use menghunote_sync::upload::fetch_object_with_key;
use menghunote_sync::SyncError;
use menghunote_sync_proto::{
    object_digest_input, Envelope, EnvelopeHeader, ObjectType, SlotDescriptorV1,
};
use sha2::{Digest, Sha256};

fn secret(bytes: [u8; 32]) -> menghunote_sync::keys::Secret32 {
    menghunote_sync::keys::Secret32::from_array(bytes)
}

#[test]
fn candidate_slots_use_new_secrets_and_match_digest_rules() {
    let mut rng = TestRandom::new(0x71_31);
    let candidate_root =
        menghunote_sync::keys::RootSecretV1::new(VAULT_ID, EPOCH + 1, secret([0x3A; 32]));
    let slots = generate_candidate_slots(
        VAULT_ID,
        EPOCH + 1,
        "a-brand-new-password",
        2,
        1_700_000_000,
        [0x44; 16],
        &candidate_root,
        &mut rng,
    )
    .expect("candidate slots");
    assert_eq!(slots.password.descriptor.key_epoch, EPOCH + 1);
    assert_eq!(slots.recovery.descriptor.key_epoch, EPOCH + 1);
    assert_ne!(
        slots.password.descriptor.slot_id,
        slots.recovery.descriptor.slot_id
    );
    verify_recovery_code(&slots).expect("R' 编码可解析");
    verify_new_manager(&slots, VAULT_ID).expect("新管理 key 由 R' 派生");
    assert_eq!(
        slots.recovery_code.as_str(),
        slots.recovery_seed.to_code().expect("code").as_str()
    );

    let digest = slot_set_digest(&[
        slots.password.descriptor.clone(),
        slots.recovery.descriptor.clone(),
    ])
    .expect("digest");
    let reversed = slot_set_digest(&[
        slots.recovery.descriptor.clone(),
        slots.password.descriptor.clone(),
    ])
    .expect("reversed");
    assert_eq!(digest, reversed, "slot_set_digest 必须按 slot_id 排序");
}

#[test]
fn manager_transition_has_nine_fields_and_both_signatures() {
    let old_manager =
        ManagementKey::from_recovery(&RecoverySeed::from_array([0x11; 32]), &VAULT_ID)
            .expect("old manager");
    let new_manager =
        ManagementKey::from_recovery(&RecoverySeed::from_array([0x22; 32]), &VAULT_ID)
            .expect("new manager");
    let material = ActivationTMaterial {
        vault_id: VAULT_ID,
        instance_id: [0x31; 16],
        rotation_id: [0x32; 16],
        from_epoch: EPOCH,
        to_epoch: EPOCH + 1,
        freeze_seq: 7,
        old_manager_rev: 1,
        new_manager_rev: 2,
        new_manager_public: new_manager.public_key(),
        prev_control_hash: [0x33; 32],
        expected_control_rev: 9,
        primary_checkpoint: [0x34; 16],
        primary_commit_digest: [0x35; 32],
        fallback_checkpoint: [0x36; 16],
        fallback_commit_digest: [0x37; 32],
        slot_set_digest: [0x38; 32],
        member_cert_digest: [0x39; 32],
        grants_digest: [0x3A; 32],
        old_asset_set_digest: [0x3B; 32],
        new_asset_set_digest: [0x3C; 32],
    };
    let transition =
        build_manager_transition(&material, &old_manager, &new_manager).expect("transition");
    let decoded = ManagerTransitionV1::decode(&transition.transition).expect("decode");
    assert_eq!(decoded.vault_id, VAULT_ID);
    assert_eq!(decoded.rotation_id, [0x32; 16]);
    assert_eq!(decoded.old_rev, 1);
    assert_eq!(decoded.new_rev, 2);
    assert_eq!(decoded.new_pubkey, new_manager.public_key());
    assert_eq!(decoded.covered_seq, 7);
    let old_sig = transition.old_signature.expect("old sig");
    let new_sig = transition.new_signature.expect("new sig");
    assert_eq!(old_sig.len(), 64);
    assert_eq!(new_sig.len(), 64);
    menghunote_sync::device::verify_signature(
        &old_manager.public_key(),
        &transition.transition,
        &old_sig.as_slice().try_into().expect("64"),
    )
    .expect("旧管理签名");
    menghunote_sync::device::verify_signature(
        &new_manager.public_key(),
        &transition.transition,
        &new_sig.as_slice().try_into().expect("64"),
    )
    .expect("新管理签名");

    // §34：NewManagerV1 必须同时携带 T 与旧/新 key 对 T 的双签。
    let t_bytes = transition.activation_t.expect("activation T");
    let activation_t =
        menghunote_sync_proto::ActivationTV1::decode(&t_bytes).expect("activation T decode");
    assert_eq!(activation_t.vault_id, VAULT_ID);
    assert_eq!(activation_t.rotation_id, [0x32; 16]);
    assert_eq!(activation_t.old_manager_rev, 1);
    assert_eq!(activation_t.new_manager_rev, 2);
    assert_eq!(activation_t.new_manager_pubkey, new_manager.public_key());
    assert_eq!(activation_t.prev_control_hash, [0x33; 32]);
    assert_eq!(activation_t.expected_control_rev, 9);
    assert_eq!(activation_t.primary_cid, [0x34; 16]);
    assert_eq!(activation_t.primary_commit_digest, [0x35; 32]);
    assert_eq!(activation_t.fallback_cid, [0x36; 16]);
    assert_eq!(activation_t.fallback_commit_digest, [0x37; 32]);
    assert_eq!(activation_t.slot_set_digest, [0x38; 32]);
    assert_eq!(activation_t.member_cert_digest, [0x39; 32]);
    assert_eq!(activation_t.grants_digest, [0x3A; 32]);
    assert_eq!(activation_t.old_asset_set_digest, [0x3B; 32]);
    assert_eq!(activation_t.new_asset_set_digest, [0x3C; 32]);
    let t_input = menghunote_sync_proto::activation_t_signing_input(&activation_t).expect("T 输入");
    for (label, public, signature) in [
        (
            "旧管理 T 签名",
            old_manager.public_key(),
            transition.old_t_signature.expect("old T sig"),
        ),
        (
            "新管理 T 签名",
            new_manager.public_key(),
            transition.new_t_signature.expect("new T sig"),
        ),
    ] {
        menghunote_sync::device::verify_signature(
            &public,
            &t_input,
            &signature.as_slice().try_into().expect("64"),
        )
        .unwrap_or_else(|error| panic!("{label}: {error}"));
    }
}

/// 标准读取路径的槽体 AAD：与 `recovery_enroll::unwrap_recovery_slot`（478–501 行）
/// 的读回重建逐字段一致——长度字段改成根 CDE 明文长度、`message_id = slot_id[..16]`。
fn reader_slot_aad(
    mut header: EnvelopeHeader,
    slot_id: &[u8; 32],
    root_plain_len: usize,
) -> impl Fn(&SlotDescriptorV1, &[u8; 24], usize) -> Result<Vec<u8>, SyncError> {
    header.total_plaintext_len = root_plain_len as u64;
    header.this_plaintext_len = u32::try_from(root_plain_len).expect("槽根明文长度");
    header.ciphertext_len = u32::try_from(root_plain_len + 16).expect("槽密文长度");
    header.message_id.copy_from_slice(&slot_id[..16]);
    move |descriptor: &SlotDescriptorV1, _nonce: &[u8; 24], _plain_len: usize| {
        Ok(header.aad(&descriptor.encode()?)?)
    }
}

/// 用标准 KEY_SLOT 读取函数（`fetch_object_with_key`）从 relay 读回 MNE 信封与槽体。
fn read_wrapped_slot(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    slot: &KeySlotV1,
    mne_key: &Secret32,
) -> (EnvelopeHeader, Vec<u8>) {
    let part = relay
        .part(&slot.descriptor.slot_id, 0)
        .expect("KEY_SLOT part");
    let envelope = Envelope::decode(&part).expect("MNE1 envelope");
    assert_eq!(envelope.header.object_type, ObjectType::KeySlot);
    assert_eq!(envelope.header.object_id, slot.descriptor.slot_id);
    let part_digest: [u8; 32] = Sha256::digest(&part).into();
    let digest_input = object_digest_input(1, &[part_digest]).expect("object digest input");
    let expected_digest: [u8; 32] = Sha256::digest(&digest_input).into();
    let descriptor = slot.descriptor.encode().expect("descriptor");
    fetch_object_with_key(
        relay,
        root,
        ObjectType::KeySlot,
        &slot.descriptor.slot_id,
        1,
        slot.wrapped_bytes().len() as u64,
        &expected_digest,
        &descriptor,
        mne_key,
    )
    .expect("标准 KEY_SLOT 读取路径")
}

#[test]
fn candidate_slots_unwrap_through_standard_reader_aad() {
    let mut rng = TestRandom::new(0x71_3321);
    let old_password = b"old-password";
    let old_recovery = RecoverySeed::from_array([0x77; 32]);
    let new_password = "a-brand-new-password";
    let device_id = [0x44; 16];
    let candidate_root = RootSecretV1::new(VAULT_ID, EPOCH + 1, secret([0x3A; 32]));
    let slots = generate_candidate_slots(
        VAULT_ID,
        EPOCH + 1,
        new_password,
        2,
        1_700_000_001,
        device_id,
        &candidate_root,
        &mut rng,
    )
    .expect("candidate slots");

    let signing_key = DeviceSigningKey::generate(&mut rng).expect("device signing key");
    let mut relay = FakeRelay::new(&[]);
    relay.epoch = EPOCH + 1;
    let reservation = [0x5A; 16];
    let digest = upload_candidate_slots(
        &mut relay,
        VAULT_ID,
        device_id,
        &signing_key,
        &candidate_root,
        &slots,
        new_password,
        &reservation,
    )
    .expect("upload candidate slots");
    assert_eq!(
        digest,
        slot_set_digest(&[
            slots.password.descriptor.clone(),
            slots.recovery.descriptor.clone(),
        ])
        .expect("slot_set_digest")
    );

    // password 槽：新口令经标准读取路径解出新根；旧口令解不开。
    let password_key = slots
        .password
        .mne_key(Some(new_password.as_bytes()), None)
        .expect("password MNE key");
    let (password_header, password_wrapped) =
        read_wrapped_slot(&mut relay, &candidate_root, &slots.password, &password_key);
    let password_wrapped_root = WrappedRoot::from_bytes(&password_wrapped).expect("wrapped root");
    let password_root_len = password_wrapped_root
        .ciphertext
        .len()
        .checked_sub(16)
        .expect("槽体至少含 tag");
    let password_aad = reader_slot_aad(
        password_header,
        &slots.password.descriptor.slot_id,
        password_root_len,
    );
    let unwrapped = slots
        .password
        .unwrap_password(new_password.as_bytes(), &password_aad)
        .expect("标准读取路径用新口令解出新根");
    assert_eq!(
        unwrapped.encode().expect("root encode"),
        candidate_root.encode().expect("root encode")
    );
    assert!(
        slots
            .password
            .unwrap_password(old_password, &password_aad)
            .is_err(),
        "旧口令不能解轮换后的新 password 槽"
    );

    // recovery 槽：新 R' 经标准读取路径解出新根；旧 R 解不开。
    let recovery_key = slots
        .recovery
        .mne_key(None, Some(&slots.recovery_seed))
        .expect("recovery MNE key");
    let (recovery_header, recovery_wrapped) =
        read_wrapped_slot(&mut relay, &candidate_root, &slots.recovery, &recovery_key);
    let recovery_wrapped_root = WrappedRoot::from_bytes(&recovery_wrapped).expect("wrapped root");
    let recovery_root_len = recovery_wrapped_root
        .ciphertext
        .len()
        .checked_sub(16)
        .expect("槽体至少含 tag");
    let recovery_aad = reader_slot_aad(
        recovery_header,
        &slots.recovery.descriptor.slot_id,
        recovery_root_len,
    );
    let unwrapped = slots
        .recovery
        .unwrap_recovery(&slots.recovery_seed, &recovery_aad)
        .expect("标准读取路径用新 R' 解出新根");
    assert_eq!(
        unwrapped.encode().expect("root encode"),
        candidate_root.encode().expect("root encode")
    );
    assert!(
        slots
            .recovery
            .unwrap_recovery(&old_recovery, &recovery_aad)
            .is_err(),
        "旧 R 不能解轮换后的新 recovery 槽"
    );
}
