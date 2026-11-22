//! 评审 0002 §五 T15：真实 HPKE / XChaCha20-Poly1305 下的密钥隔离。
//!
//! - 未吊销设备能用**自己的** HPKE 私钥解开为它封的新 epoch root；
//! - 被吊销设备的 HPKE 私钥 / 旧 root / 旧 password / 旧 R 都解不开新 epoch 材料；
//! - 旧 data 密文仍只能由旧 root 解，新 root 不得跨界解密旧对象（反之亦然）。

mod support;

use support::*;

use menghunote_sync::device::{DeviceHpkeKey, ManagementKey};
use menghunote_sync::hpke::open_epoch_grant;
use menghunote_sync::keys::{RootSecretV1, Secret32};
use menghunote_sync::recovery::RecoverySeed;
use menghunote_sync::rotation_slots::generate_candidate_slots;
use menghunote_sync::rotation_stage::plan_grants;
use menghunote_sync::upload::{decrypt_envelope, encrypt_local_parts};
use menghunote_sync::{DeviceId, ObjectId};
use menghunote_sync_proto::{
    EnvelopeHeader, GrantAadV1, ObjectType, SlotDescriptorV1, CIPHER_SUITE_XCHACHA20_POLY1305,
    HEADER_LEN, PROTOCOL_VERSION,
};

const NEW_EPOCH: u64 = 2;

fn hpke(seed: u8) -> DeviceHpkeKey {
    DeviceHpkeKey::from_secret(Secret32::from_array([seed; 32]))
}

fn new_root() -> RootSecretV1 {
    RootSecretV1::new(VAULT_ID, NEW_EPOCH, Secret32::from_array([0x7A; 32]))
}

/// 与 `rotation_slots::generate_candidate_slots` 内部完全同口径的槽体 AAD。
fn slot_aad(
    device_id: DeviceId,
) -> impl Fn(&SlotDescriptorV1, &[u8; 24], usize) -> Result<Vec<u8>, menghunote_sync::SyncError> {
    move |descriptor, nonce, plaintext_len| {
        let mut message_id = [0u8; 16];
        message_id.copy_from_slice(&descriptor.slot_id[..16]);
        let header = EnvelopeHeader {
            protocol_version: PROTOCOL_VERSION,
            header_len: HEADER_LEN as u16,
            object_type: ObjectType::KeySlot,
            cipher_suite: CIPHER_SUITE_XCHACHA20_POLY1305,
            flags: 0,
            vault_id: descriptor.vault_id,
            object_id: descriptor.slot_id,
            message_id,
            producer_device_id: device_id,
            generation: 0,
            key_epoch: descriptor.key_epoch,
            sender_seq: 0,
            part_index: 0,
            part_count: 1,
            total_plaintext_len: plaintext_len as u64,
            this_plaintext_len: u32::try_from(plaintext_len)
                .map_err(|_| menghunote_sync::SyncError::Descriptor("槽根明文长度超出 u32"))?,
            ciphertext_len: u32::try_from(plaintext_len + 16)
                .map_err(|_| menghunote_sync::SyncError::Descriptor("槽密文长度超出 u32"))?,
            context_hash: menghunote_sync::slot::context_hash(descriptor)?,
            nonce: *nonce,
        };
        Ok(header.aad(&descriptor.encode()?)?)
    }
}

#[test]
fn active_hpke_opens_its_own_grant_and_revoked_key_cannot() {
    let active_id: DeviceId = [0x41; 16];
    let revoked_id: DeviceId = [0x42; 16];
    let active = hpke(0x31);
    let revoked = hpke(0x32);
    let root = new_root();
    let manager = ManagementKey::from_seed(Secret32::from_array([0x33; 32]));

    // 只给未吊销设备封包；被吊销设备不在 recipient 列表里。
    let leaves =
        plan_grants(&root, &manager, 2, &[(active_id, active.public_key())]).expect("plan grants");
    assert_eq!(leaves.len(), 1);
    let leaf = &leaves[0];
    assert_eq!(leaf.device_id, active_id);
    assert_eq!(leaf.epoch, NEW_EPOCH);
    let aad_len = leaf.signed_header.len() - 64;
    let aad = GrantAadV1::decode(&leaf.signed_header[..aad_len]).expect("grant aad");
    assert_eq!(aad.recipient_device_id, active_id);
    assert_eq!(aad.recipient_x25519_pub, active.public_key());

    // 正例：未吊销设备用自己的 HPKE 私钥解开新 grant，得到 candidate root。
    let opened = open_epoch_grant(active.secret(), &leaf.hpke_enc, &aad, &leaf.ciphertext)
        .expect("active device opens own grant");
    assert_eq!(
        opened.encode().expect("opened root"),
        root.encode().expect("candidate root")
    );

    // 负例：被吊销设备的 HPKE 私钥即使拿到同一份 grant 也解不开。
    assert!(
        open_epoch_grant(revoked.secret(), &leaf.hpke_enc, &aad, &leaf.ciphertext).is_err(),
        "被吊销 HPKE 私钥不得解开别人的新 grant"
    );
    // 攻击者把 AAD 的 recipient 改成 revoked 也无效（AAD 参与 HPKE 认证）。
    let mut forged = aad.clone();
    forged.recipient_device_id = revoked_id;
    forged.recipient_x25519_pub = revoked.public_key();
    assert!(
        open_epoch_grant(revoked.secret(), &leaf.hpke_enc, &forged, &leaf.ciphertext).is_err(),
        "伪造 recipient AAD 不得让被吊销 key 解包"
    );

    // 被吊销设备没有自己的新 grant（不能靠旧 grant 重放进入新 epoch）。
    let no_grant = plan_grants(&root, &manager, 2, &[(active_id, active.public_key())])
        .expect("plan grants again");
    assert!(no_grant.iter().all(|leaf| leaf.device_id != revoked_id));
}

#[test]
fn old_root_password_and_recovery_cannot_open_new_epoch_ciphertext() {
    let old_root = RootSecretV1::new(VAULT_ID, 1, Secret32::from_array([0x70; 32]));
    let root = new_root();
    let object_id: ObjectId = [0x61; 32];
    let (parts, _digest) = encrypt_local_parts(
        &root,
        ObjectType::Update,
        &object_id,
        [0x62; 16],
        [0x63; 16],
        1,
        1,
        b"new epoch payload",
        &[],
    )
    .expect("encrypt new epoch");

    // 新 root 能解自己的新密文；旧 root 不能。
    let (_, plaintext) = decrypt_envelope(&root, ObjectType::Update, &object_id, &parts[0].1, &[])
        .expect("new root decrypt");
    assert_eq!(&plaintext[..], b"new epoch payload");
    assert!(
        decrypt_envelope(&old_root, ObjectType::Update, &object_id, &parts[0].1, &[]).is_err(),
        "旧 root 不得解开新 epoch 密文"
    );

    // 新 password / 新 R 封装的候选根：只有新秘密能解，旧 password/R 都不行。
    let device_id: DeviceId = [0x64; 16];
    let mut rng = TestRandom::new(0x15_0001);
    let slots = generate_candidate_slots(
        VAULT_ID,
        NEW_EPOCH,
        "new-password",
        2,
        1_700_000_000,
        device_id,
        &root,
        &mut rng,
    )
    .expect("candidate slots");
    let aad = slot_aad(device_id);
    let via_new_password = slots
        .password
        .unwrap_password(b"new-password", &aad)
        .expect("新口令必须能解新槽");
    assert_eq!(
        via_new_password.encode().expect("root"),
        root.encode().expect("root")
    );
    assert!(
        slots
            .password
            .unwrap_password(b"old-password", &aad)
            .is_err(),
        "旧口令不得解出新 epoch root"
    );

    let via_new_recovery = slots
        .recovery
        .unwrap_recovery(&slots.recovery_seed, &aad)
        .expect("新 R 必须能解新恢复槽");
    assert_eq!(
        via_new_recovery.encode().expect("root"),
        root.encode().expect("root")
    );
    let old_recovery = RecoverySeed::from_array([0x99; 32]);
    assert!(
        slots.recovery.unwrap_recovery(&old_recovery, &aad).is_err(),
        "旧恢复码不得解出新 epoch root"
    );
}
