//! ADR 0006 §31：ASSET 确定性 nonce 的完整资产头修正。
//!
//! 覆盖：同字节不同 MIME → 不同 object_id/nonce；两设备上传同一资产 → 信封逐字节相同
//! （AAD 不含 producer）；同资产重传 → 字节相同。旧口径（只按 real_sha）的 nonce 复用
//! 反例在 `upload.rs` 的单元测试里（需要访问私有的 nonce 派生）。

mod support;

use support::*;

use menghunote_sync::checkpoint::asset_object_id;
use menghunote_sync::genesis;
use menghunote_sync::upload::{asset_message_id, upload_object};
use menghunote_sync::DeviceId;
use menghunote_sync_proto::{Envelope, ObjectType};
use sha2::{Digest, Sha256};

const DEVICE_A: DeviceId = [0xA1; 16];

fn setup() -> (tempfile::TempDir, genesis::GenesisMaterials, FakeRelay) {
    let (dir, _vault) = fresh_vault();
    let materials = genesis::build_materials(
        VAULT_ID,
        DEVICE_A,
        "pw",
        1_700_000_000,
        &mut TestRandom::new(0x31_31),
    )
    .expect("genesis 材料");
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    (dir, materials, relay)
}

/// 按 `asset_plaintext` 的冻结口径构造完整资产明文头 + 数据。
fn asset_plaintext(data: &[u8], media_type: &str) -> ([u8; 32], Vec<u8>) {
    let real: [u8; 32] = Sha256::digest(data).into();
    let mut out = Vec::with_capacity(32 + 8 + 2 + media_type.len() + data.len());
    out.extend_from_slice(&real);
    out.extend_from_slice(&(data.len() as u64).to_be_bytes());
    out.extend_from_slice(&(media_type.len() as u16).to_be_bytes());
    out.extend_from_slice(media_type.as_bytes());
    out.extend_from_slice(data);
    (real, out)
}

fn upload_asset(
    relay: &mut FakeRelay,
    root: &menghunote_sync::keys::RootSecretV1,
    producer: DeviceId,
    signing_key: &menghunote_sync::device::DeviceSigningKey,
    object_id: [u8; 32],
    message_id: [u8; 16],
    plaintext: &[u8],
) {
    upload_object(
        relay,
        VAULT_ID,
        producer,
        signing_key,
        root,
        ObjectType::Asset,
        object_id,
        message_id,
        0,
        0,
        plaintext,
        &[],
        None,
    )
    .expect("ASSET 上传");
}

fn part(relay: &FakeRelay, object_id: [u8; 32]) -> Vec<u8> {
    relay
        .parts
        .get(&(object_id, 0))
        .cloned()
        .expect("资产分片存在")
}

#[test]
fn same_bytes_different_mime_gets_distinct_object_ids_and_nonces() {
    let (_dir, materials, mut relay) = setup();
    let data = b"same asset bytes, different MIME";
    let (real, plain_a) = asset_plaintext(data, "text/plain");
    let (_, plain_b) = asset_plaintext(data, "application/octet-stream");
    assert_eq!(real, <[u8; 32]>::from(Sha256::digest(data)));
    assert_ne!(plain_a, plain_b, "同字节不同 MIME 的完整明文头必须不同");

    let len = data.len() as u64;
    let id_a = asset_object_id(&materials.root, &real, len, "text/plain").expect("object_id a");
    let id_b = asset_object_id(&materials.root, &real, len, "application/octet-stream")
        .expect("object_id b");
    assert_ne!(id_a, id_b, "§31：object_id 必须提交 MIME");

    let msg_a = asset_message_id(&materials.root, &id_a).expect("message_id a");
    let msg_b = asset_message_id(&materials.root, &id_b).expect("message_id b");
    upload_asset(
        &mut relay,
        &materials.root,
        materials.device_id,
        &materials.signing_key,
        id_a,
        msg_a,
        &plain_a,
    );
    upload_asset(
        &mut relay,
        &materials.root,
        materials.device_id,
        &materials.signing_key,
        id_b,
        msg_b,
        &plain_b,
    );

    let env_a = Envelope::decode(&part(&relay, id_a)).expect("envelope a");
    let env_b = Envelope::decode(&part(&relay, id_b)).expect("envelope b");
    assert_ne!(
        env_a.header.nonce, env_b.header.nonce,
        "不同明文头必须使用不同 nonce（否则 nonce 复用）"
    );
    assert_ne!(env_a.ciphertext, env_b.ciphertext);
}

#[test]
fn same_asset_from_two_devices_is_byte_identical() {
    let (_dir, materials, mut relay) = setup();
    let second = Device::new(0x62);
    relay.certs.insert(second.device_id, second.cert());
    assert_ne!(second.device_id, materials.device_id);

    let data = b"cross-device asset";
    let (real, plaintext) = asset_plaintext(data, "image/png");
    let object_id =
        asset_object_id(&materials.root, &real, data.len() as u64, "image/png").expect("object_id");
    let message_id = asset_message_id(&materials.root, &object_id).expect("message_id");

    upload_asset(
        &mut relay,
        &materials.root,
        materials.device_id,
        &materials.signing_key,
        object_id,
        message_id,
        &plaintext,
    );
    let first = part(&relay, object_id);
    upload_asset(
        &mut relay,
        &materials.root,
        second.device_id,
        &second.identity.signing_key,
        object_id,
        message_id,
        &plaintext,
    );
    let second_bytes = part(&relay, object_id);

    assert_eq!(
        first, second_bytes,
        "两设备上传同一资产必须逐字节相同（ASSET AAD 不含 producer_device_id）"
    );
    let envelope = Envelope::decode(&first).expect("envelope");
    assert_eq!(
        envelope.header.producer_device_id, [0u8; 16],
        "§31：ASSET header 的 producer 固定为 0，AAD 因而与设备无关"
    );
    assert_eq!(relay.put_history.len(), 2);
    assert_eq!(relay.put_history[0].2, relay.put_history[1].2);
}

#[test]
fn same_asset_retransmission_is_byte_identical() {
    let (_dir, materials, mut relay) = setup();
    let data = b"retransmitted asset";
    let (real, plaintext) = asset_plaintext(data, "text/markdown");
    let object_id = asset_object_id(&materials.root, &real, data.len() as u64, "text/markdown")
        .expect("object_id");
    let message_id = asset_message_id(&materials.root, &object_id).expect("message_id");

    upload_asset(
        &mut relay,
        &materials.root,
        materials.device_id,
        &materials.signing_key,
        object_id,
        message_id,
        &plaintext,
    );
    let first = part(&relay, object_id);
    upload_asset(
        &mut relay,
        &materials.root,
        materials.device_id,
        &materials.signing_key,
        object_id,
        message_id,
        &plaintext,
    );
    assert_eq!(first, part(&relay, object_id));
    assert_eq!(relay.put_history.len(), 2);
    assert_eq!(
        relay.put_history[0].2, relay.put_history[1].2,
        "重传必须复用同一密文字节（ADR §19）"
    );
}
