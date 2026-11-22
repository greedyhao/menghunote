//! P1：候选槽新秘密 / slot_set_digest / manager transition 双签。

mod support;

use support::*;

use menghunote_sync::device::ManagementKey;
use menghunote_sync::recovery::RecoverySeed;
use menghunote_sync::rotation::ManagerTransitionV1;
use menghunote_sync::rotation_slots::{
    build_manager_transition, generate_candidate_slots, slot_set_digest, verify_new_manager,
    verify_recovery_code,
};

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
    let transition = build_manager_transition(
        VAULT_ID,
        [0x31; 16],
        [0x32; 16],
        EPOCH,
        EPOCH + 1,
        1,
        2,
        new_manager.public_key(),
        7,
        &old_manager,
        &new_manager,
    )
    .expect("transition");
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
}
