//! 恢复码自批新设备（ADR §3.7/§4/§20.3）。
//!
//! 只有恢复码 + relay、没有任何在线旧设备时：
//! 1. 恢复码 → 恢复种子 R → 管理私钥（`ManagementKey::from_recovery`）；
//! 2. management realm challenge/token（proof 用管理私钥签）；
//! 3. 用 management token `GET key-slots`（可见 staging）→ 取 recovery 槽 →
//!    `KeySlotV1::mne_key`（§20）解封 root（**不需要 glue 设备在线**）；
//! 4. 本地生成新设备 sign/HPKE key；管理签名建 invite → 新设备 redeem（proof 自证）→
//!    approve（管理签 §17.1 证书 + §17.2 HPKE grant，grant 只封给本次本地 HPKE 公钥）；
//! 5. 返回新设备身份 + root；调用方再用设备 token 走 `bootstrap::run`。
//!
//! 安全边界：不信任 relay 返回的任何公钥；grant 的 recipient 就是本地新生成的
//! `hpke_key.public_key()`，approve 前逐步校验响应字段。

use sha2::{Digest, Sha256};

use crate::api_version::RelayApi;
use crate::cbor;
use crate::device::{verify_signature, DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use crate::error::SyncError;
use crate::hpke::{open_epoch_grant, seal_epoch_grant};
use crate::http_relay::{error_from_response, HttpRequest, HttpResponse, HttpTransport};
use crate::keys::RootSecretV1;
use crate::recovery::RecoverySeed;
use crate::relay::SlotRecord;
use crate::slot::{KeySlotV1, WrappedRoot};
use crate::DeviceId;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use menghunote_sync_proto::{
    auth_signing_input, device_cert_signed_bytes, management_request_cde, management_signing_input,
    AuthRealm, Challenge, DeviceCertV1, Envelope, GrantAadV1, HttpMethod, ObjectType, Rights,
    SlotDescriptorV1, SlotKind as ProtoSlotKind,
};

/// 自批成功后的新设备材料。
pub struct EnrolledDevice {
    /// vault id。
    pub vault_id: [u8; 16],
    /// 新设备 id。
    pub device_id: DeviceId,
    /// Loro peer id。
    pub peer_id: u64,
    /// 设备签名私钥。
    pub signing_key: DeviceSigningKey,
    /// 设备 HPKE 私钥。
    pub hpke_key: DeviceHpkeKey,
    /// 解出的 epoch 根。
    pub root: RootSecretV1,
    /// relay 当前 control_rev（approve/建 invite 用）。
    pub control_rev: u64,
    /// 自批步骤日志（e2e 打印用）。
    pub steps: Vec<String>,
}

const CONTENT_TYPE: &str = "application/cbor";

/// 执行恢复码自批；返回新设备身份和 root。
pub fn self_enroll<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    recovery_code: &str,
    now: u64,
) -> Result<EnrolledDevice, SyncError> {
    let seed = RecoverySeed::from_code(recovery_code)?;
    let manager = ManagementKey::from_recovery(&seed, &vault_id)?;
    let mut steps = Vec::new();

    // 1. 管理 realm token。
    let (token, instance_id) = management_token(transport, api, vault_id, &manager, now)?;
    steps.push("management token ok".to_owned());

    // 2. 读 staging recovery 槽并解 root（§20 KEK 派生）。
    let slots = list_slots(transport, api, vault_id, &token)?;
    let recovery_slot = slots
        .iter()
        .find(|slot| slot.kind == ProtoSlotKind::Recovery)
        .ok_or(SyncError::Object(
            "relay 没有 recovery 槽（管理 realm 应可见 staging）",
        ))?
        .clone();
    let descriptor = recovery_slot_descriptor(&recovery_slot)?;
    // ADR §22：管理 realm 可只读 KEY_SLOT 对象；先解出真实 root，再封真实 grant。
    let root = unwrap_recovery_slot(transport, api, vault_id, &token, &recovery_slot, &seed)?;
    steps.push("recovery slot unwrapped (management realm)".to_owned());

    // 3. 本地生成新设备密钥（不信任 relay 的任何公钥）。
    let signing_key = DeviceSigningKey::generate(&mut crate::random::OsRandom)?;
    let hpke_key = DeviceHpkeKey::generate(&mut crate::random::OsRandom)?;
    let device_id: DeviceId = crate::random::array(&mut crate::random::OsRandom)?;
    let peer_bytes: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
    let mut peer_head = [0u8; 8];
    peer_head.copy_from_slice(&peer_bytes[..8]);
    let peer_id = u64::from_be_bytes(peer_head) & 0x7fff_ffff_ffff_ffff;
    let manager_rev = descriptor.manager_rev;

    // 4. 读取 state（control_rev/epoch）。
    let state = get_state(transport, api, vault_id, &token)?;
    let invitation = create_invite(
        transport,
        api,
        vault_id,
        &manager,
        &token,
        state.0,
        instance_id,
        now,
        None,
    )?;
    steps.push(format!("invite created id={}", hex16(&invitation.0)));
    redeem_invite(
        transport,
        api,
        vault_id,
        invitation.0,
        &invitation.1,
        device_id,
        &signing_key,
        &hpke_key,
    )?;
    steps.push("invite redeemed (device pending)".to_owned());
    // ADR §22：grant 必须封真实 root（占位 grant 是协议错误）。
    approve_device(
        transport,
        api,
        vault_id,
        &manager,
        &token,
        state.0,
        instance_id,
        device_id,
        signing_key.public_key(),
        hpke_key.public_key(),
        manager_rev,
        &root,
    )?;
    steps.push("recovery self-approval: device active (real root grant)".to_owned());
    let device_token = device_token(transport, api, vault_id, device_id, &signing_key, now)?;
    verify_latest_grant(
        transport,
        api,
        vault_id,
        device_id,
        &device_token,
        &hpke_key,
        &root,
    )?;
    steps.push("grant root verified".to_owned());

    Ok(EnrolledDevice {
        vault_id,
        device_id,
        peer_id,
        signing_key,
        hpke_key,
        root,
        control_rev: state.0,
        steps,
    })
}

pub(crate) fn management_token<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    manager: &ManagementKey,
    now: u64,
) -> Result<(String, [u8; 16]), SyncError> {
    let subject = management_subject(&manager.public_key());
    let body = cbor::encode(vec![
        (0, cbor::uint(u64::from(AuthRealm::Management as u8))),
        (1, cbor::bytes(subject.to_vec())),
    ])?;
    let response = send(
        transport,
        "POST",
        api.url(&api.auth_path(&vault_id, "challenges")),
        None,
        body,
    )?;
    let fields = decode(&response)?;
    let challenge_id: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
    let realm = cbor::uint_field(&fields, 1)?;
    if realm != u64::from(AuthRealm::Management as u8) {
        return Err(SyncError::RelayWire("management challenge realm 不符"));
    }
    let subject: [u8; 16] = cbor::bytes_fixed(&fields, 2)?;
    if subject != management_subject(&manager.public_key()) {
        return Err(SyncError::RelayWire("management challenge subject 不符"));
    }
    let nonce: [u8; 32] = cbor::bytes_fixed(&fields, 3)?;
    let expires_at = cbor::uint_field(&fields, 4)?;
    let response_vault: [u8; 16] = cbor::bytes_fixed(&fields, 5)?;
    let instance: [u8; 16] = cbor::bytes_fixed(&fields, 6)?;
    if response_vault != vault_id || expires_at <= now {
        return Err(SyncError::RelayWire("management challenge 过期/vault 不符"));
    }
    let origin_sha256: [u8; 32] = Sha256::digest(api.origin().as_bytes()).into();
    let challenge = Challenge {
        pv: 1,
        relay_instance_id: instance,
        origin_sha256,
        vault_id,
        realm: AuthRealm::Management,
        subject,
        challenge_id,
        nonce,
        expires_at,
    };
    let message = challenge.encode()?;
    let signature = manager.sign(&auth_signing_input(&challenge)?);
    let body = cbor::encode(vec![
        (0, cbor::bytes(challenge_id.to_vec())),
        (1, cbor::bytes(message)),
        (2, cbor::bytes(signature)),
    ])?;
    let response = send(
        transport,
        "POST",
        api.url(&api.auth_path(&vault_id, "tokens")),
        None,
        body,
    )?;
    let fields = decode(&response)?;
    let token = cbor::text_field(&fields, 0)?;
    let realm = cbor::uint_field(&fields, 3)?;
    if realm != u64::from(AuthRealm::Management as u8) {
        return Err(SyncError::RelayWire("token realm 不是 management"));
    }
    Ok((token, instance))
}

fn verify_latest_grant<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    device_id: DeviceId,
    token: &str,
    hpke_key: &DeviceHpkeKey,
    expected_root: &RootSecretV1,
) -> Result<(), SyncError> {
    let response = send(
        transport,
        "GET",
        api.url(&api.vault_path(&vault_id, "/devices/self/grants/latest")),
        Some(token),
        Vec::new(),
    )?;
    let fields = decode(&response)?;
    let epoch = cbor::uint_field(&fields, 0)?;
    let grant_device: [u8; 16] = cbor::bytes_fixed(&fields, 1)?;
    let manager_rev = cbor::uint_field(&fields, 2)?;
    let enc: [u8; 32] = cbor::bytes_fixed(&fields, 3)?;
    let ciphertext = cbor::bytes_field(&fields, 4)?;
    let signed_header = cbor::bytes_field(&fields, 5)?;
    let signature = cbor::opt_bytes_field(&fields, 6)?.unwrap_or_default();
    let grant_id = cbor::opt_bytes_field(&fields, 7)?;
    let activation_ref = cbor::opt_bytes_field(&fields, 8)?;
    let header_hash = cbor::opt_bytes_field(&fields, 9)?;
    let package_digest = cbor::opt_bytes_field(&fields, 10)?;
    if grant_device != device_id || signed_header.len() < 64 {
        return Err(SyncError::RelayWire("self grant 字段不符"));
    }
    if let Some(grant_id) = &grant_id {
        if grant_id.len() != 16 {
            return Err(SyncError::RelayWire("self grant grant_id 长度非法"));
        }
    }
    if let Some(activation_ref) = &activation_ref {
        if activation_ref.len() != 16 {
            return Err(SyncError::RelayWire("self grant activation_ref 长度非法"));
        }
    }
    if let Some(header_hash) = &header_hash {
        let expected: [u8; 32] = Sha256::digest(&signed_header).into();
        if header_hash.as_slice() != expected {
            return Err(SyncError::RelayWire("self grant signed_header digest 不符"));
        }
    }
    if let Some(package_digest) = &package_digest {
        if signature.len() != 64 {
            return Err(SyncError::RelayWire("self grant signature 长度非法"));
        }
        let mut input = Vec::new();
        input.extend_from_slice(&enc);
        input.extend_from_slice(&Sha256::digest(&ciphertext));
        input.extend_from_slice(&Sha256::digest(&signed_header));
        input.extend_from_slice(&signature);
        let expected: [u8; 32] = Sha256::digest(&input).into();
        if package_digest.as_slice() != expected {
            return Err(SyncError::RelayWire("self grant package digest 不符"));
        }
    }
    let aad_len = signed_header.len() - 64;
    let aad = GrantAadV1::decode(&signed_header[..aad_len])?;
    let signed_enc: [u8; 32] = signed_header[aad_len..aad_len + 32]
        .try_into()
        .map_err(|_| SyncError::RelayWire("self grant enc 长度非法"))?;
    let signed_hash: [u8; 32] = signed_header[aad_len + 32..]
        .try_into()
        .map_err(|_| SyncError::RelayWire("self grant hash 长度非法"))?;
    let ct_hash: [u8; 32] = Sha256::digest(&ciphertext).into();
    if signed_enc != enc
        || signed_hash != ct_hash
        || aad.epoch != epoch
        || aad.manager_rev != manager_rev
        || aad.recipient_device_id != device_id
        || aad.recipient_x25519_pub != hpke_key.public_key()
    {
        return Err(SyncError::RelayWire("self grant 与本地设备/槽不一致"));
    }
    let opened = open_epoch_grant(hpke_key.secret(), &enc, &aad, &ciphertext)?;
    if opened.encode()? != expected_root.encode()? {
        return Err(SyncError::Object("grant root 与槽里解出的 root 不一致"));
    }
    Ok(())
}

/// 设备 realm 认证的完整回执（邀请加入端要核对 vault/instance pin）。
pub(crate) struct DeviceAuth {
    /// bearer token。
    pub token: String,
    /// challenge 响应的 vault（必须与请求一致）。
    pub vault_id: [u8; 16],
    /// relay instance（必须与邀请链接的 pin 一致，**不自动换 pin**）。
    pub instance_id: [u8; 16],
}

/// 设备 realm challenge/token；返回完整回执（vault/instance 供调用方核对）。
pub(crate) fn device_auth<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    now: u64,
) -> Result<DeviceAuth, SyncError> {
    let body = cbor::encode(vec![
        (0, cbor::uint(u64::from(AuthRealm::Device as u8))),
        (1, cbor::bytes(device_id.to_vec())),
    ])?;
    let response = send(
        transport,
        "POST",
        api.url(&api.auth_path(&vault_id, "challenges")),
        None,
        body,
    )?;
    let fields = decode(&response)?;
    let challenge_id: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
    let nonce: [u8; 32] = cbor::bytes_fixed(&fields, 3)?;
    let expires_at = cbor::uint_field(&fields, 4)?;
    let response_vault: [u8; 16] = cbor::bytes_fixed(&fields, 5)?;
    let instance: [u8; 16] = cbor::bytes_fixed(&fields, 6)?;
    if response_vault != vault_id || expires_at <= now {
        return Err(SyncError::RelayWire("device challenge 过期/vault 不符"));
    }
    let origin_sha256: [u8; 32] = Sha256::digest(api.origin().as_bytes()).into();
    let challenge = Challenge {
        pv: 1,
        relay_instance_id: instance,
        origin_sha256,
        vault_id,
        realm: AuthRealm::Device,
        subject: device_id,
        challenge_id,
        nonce,
        expires_at,
    };
    let message = challenge.encode()?;
    let signature = signing_key.sign(&auth_signing_input(&challenge)?);
    let body = cbor::encode(vec![
        (0, cbor::bytes(challenge_id.to_vec())),
        (1, cbor::bytes(message)),
        (2, cbor::bytes(signature.to_vec())),
    ])?;
    let response = send(
        transport,
        "POST",
        api.url(&api.auth_path(&vault_id, "tokens")),
        None,
        body,
    )?;
    let fields = decode(&response)?;
    Ok(DeviceAuth {
        token: cbor::text_field(&fields, 0)?,
        vault_id: response_vault,
        instance_id: instance,
    })
}

/// 只要 bearer token 的薄封装（恢复码自批路径；instance pin 由调用方另外核对）。
pub(crate) fn device_token<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    now: u64,
) -> Result<String, SyncError> {
    Ok(device_auth(transport, api, vault_id, device_id, signing_key, now)?.token)
}

fn list_slots<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    token: &str,
) -> Result<Vec<SlotRecord>, SyncError> {
    let response = send(
        transport,
        "GET",
        api.url(&api.vault_path(&vault_id, "/key-slots")),
        Some(token),
        Vec::new(),
    )?;
    let fields = decode(&response)?;
    let mut slots = Vec::new();
    for value in cbor::array_field(&fields, 0)? {
        let fields = cbor::value_map(&value)?;
        slots.push(SlotRecord {
            slot_id: cbor::bytes_fixed(&fields, 0)?,
            epoch: cbor::uint_field(&fields, 1)?,
            kind: parse_kind(&cbor::text_field(&fields, 2)?)?,
            descriptor: cbor::bytes_field(&fields, 3)?,
            state: cbor::text_field(&fields, 4)?,
            slot_set_rev: cbor::uint_field(&fields, 5)?,
            retire_at: cbor::opt_uint_field(&fields, 6)?,
        });
    }
    Ok(slots)
}

fn unwrap_recovery_slot<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    token: &str,
    record: &SlotRecord,
    seed: &RecoverySeed,
) -> Result<RootSecretV1, SyncError> {
    let descriptor = SlotDescriptorV1::decode(&record.descriptor)?;
    if descriptor.kind != ProtoSlotKind::Recovery {
        return Err(SyncError::Object("rec 槽类型不符"));
    }
    let mne_key = crate::keys::derive_slot_mne_key(
        seed.expose(),
        &vault_id,
        descriptor.kind,
        descriptor.key_epoch,
        &record.slot_id,
    )?;
    let cipher = XChaCha20Poly1305::new_from_slice(mne_key.expose_slice())
        .map_err(|_| SyncError::Object("槽数据密钥长度非法"))?;
    let mut plaintext = Vec::new();
    let mut first_header = None;
    let mut part_count = 1u32;
    let mut index = 0u32;
    while index < part_count {
        let path =
            api.url(&api.object_path(&vault_id, &record.slot_id, &format!("/parts/{index}")));
        let response = send(transport, "GET", path, Some(token), Vec::new())?;
        let envelope = Envelope::decode(&response.body)?;
        if envelope.header.object_type != ObjectType::KeySlot
            || envelope.header.object_id != record.slot_id
            || envelope.header.part_index != index
        {
            return Err(SyncError::Object("KEY_SLOT header 不符"));
        }
        part_count = envelope.header.part_count;
        if first_header.is_none() {
            first_header = Some(envelope.header.clone());
        }
        let aad = envelope.header.aad(&record.descriptor)?;
        let chunk = cipher
            .decrypt(
                XNonce::from_slice(&envelope.header.nonce),
                Payload {
                    msg: &envelope.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| SyncError::Object("KEY_SLOT MNE 解密失败"))?;
        plaintext.extend_from_slice(&chunk);
        index += 1;
    }
    if plaintext.len() as u64
        != first_header
            .as_ref()
            .map_or(0, |header| header.total_plaintext_len)
    {
        return Err(SyncError::Object("KEY_SLOT 明文长度不符"));
    }
    let wrapped = WrappedRoot::from_bytes(&plaintext)?;
    let slot = KeySlotV1 {
        descriptor,
        wrapped,
    };
    let mut header_for_aad = first_header.ok_or(SyncError::Object("KEY_SLOT 没有分片"))?;
    // 槽体 AAD 是 genesis 包 root 时用的 AAD（明文长度 = root CDE 长度），不是 MNE 层
    // 包 wrapped bytes 的 AAD；按 ciphertext - TAG 反推并重算 header 字段。
    let root_plain_len = slot
        .wrapped
        .ciphertext
        .len()
        .checked_sub(16)
        .ok_or(SyncError::Object("KEY_SLOT 槽体过短"))?;
    header_for_aad.total_plaintext_len = root_plain_len as u64;
    header_for_aad.this_plaintext_len =
        u32::try_from(root_plain_len).map_err(|_| SyncError::Object("槽 root 长度溢出"))?;
    header_for_aad.ciphertext_len =
        u32::try_from(root_plain_len + 16).map_err(|_| SyncError::Object("槽密文长度溢出"))?;
    // genesis 包槽体时 message_id = slot_id[..16]（不是 MNE 层的随机 id）。
    header_for_aad
        .message_id
        .copy_from_slice(&record.slot_id[..16]);
    let aad = move |descriptor: &SlotDescriptorV1,
                    _nonce: &[u8; 24],
                    _plain_len: usize|
          -> Result<Vec<u8>, SyncError> {
        Ok(header_for_aad.aad(&descriptor.encode()?)?)
    };
    slot.unwrap_recovery(seed, &aad)
        .map_err(|_| SyncError::Object("KEY_SLOT KEK 解封失败"))
}

/// 创建邀请（管理签名 canonical body）。
///
/// `expires_at = Some(ts)` 时把 TTL 明确写进被签名的 body（ADR 0014 S1：默认 15 分钟，
/// 上限 relay 的 24h）；`None` 保持历史语义（relay 缺省 = now + 24h）。
///
/// 返回 `(invite_id, token, expires_at)`：`expires_at` 优先取 relay 回执（权威事实），
/// 旧 relay 不带该字段时回退到请求值/`now + 24h`。
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_invite<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    manager: &ManagementKey,
    token: &str,
    control_rev: u64,
    instance: [u8; 16],
    now: u64,
    expires_at: Option<u64>,
) -> Result<([u8; 16], [u8; 32], u64), SyncError> {
    let raw: [u8; 32] = crate::random::array(&mut crate::random::OsRandom)?;
    let token_hash = Sha256::digest(raw);
    let mut payload_fields = vec![(0, cbor::bytes(token_hash.to_vec()))];
    if let Some(expires_at) = expires_at {
        payload_fields.push((1, cbor::uint(expires_at)));
    }
    let payload = cbor::encode(payload_fields)?;
    let path = api.vault_path(&vault_id, "/invites");
    let request_id: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
    let body_digest: [u8; 32] =
        Sha256::digest(management_request_cde(HttpMethod::Post, &path, &payload)?).into();
    let signature = manager.sign(&management_signing_input(
        &instance,
        &vault_id,
        &request_id,
        control_rev,
        &body_digest,
    )?);
    let body = cbor::encode(vec![
        (
            0,
            cbor::CdeValue::Map(vec![
                (0, cbor::bytes(payload)),
                (1, cbor::bytes(signature.to_vec())),
            ]),
        ),
        (1, cbor::bytes(request_id.to_vec())),
        (2, cbor::uint(control_rev)),
    ])?;
    let response = send(transport, "POST", api.url(&path), Some(token), body)?;
    let fields = decode(&response)?;
    let invite_id = cbor::bytes_fixed(&fields, 0)?;
    // 回退值只用于**展示**（旧 relay 不回 TTL 时与 relay 缺省一致 = 24h）；
    // 真实一次性/过期语义始终由 relay 判定，本地从不据此放行。
    let confirmed_expires_at = cbor::opt_uint_field(&fields, 1)?
        .or(expires_at)
        .unwrap_or(now.saturating_add(24 * 60 * 60));
    Ok((invite_id, raw, confirmed_expires_at))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn redeem_invite<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    invite_id: [u8; 16],
    token: &[u8; 32],
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    hpke_key: &DeviceHpkeKey,
) -> Result<(), SyncError> {
    let proof: [u8; 32] = crate::random::array(&mut crate::random::OsRandom)?;
    let proof_signature = signing_key.sign(&proof);
    let body = cbor::encode(vec![
        (0, cbor::bytes(token.to_vec())),
        (1, cbor::bytes(device_id.to_vec())),
        (2, cbor::bytes(signing_key.public_key().to_vec())),
        (3, cbor::bytes(hpke_key.public_key().to_vec())),
        (4, cbor::bytes(proof.to_vec())),
        (5, cbor::bytes(proof_signature.to_vec())),
    ])?;
    let response = send(
        transport,
        "POST",
        api.vault_path(&vault_id, &format!("/invites/{}/redeem", b64(&invite_id))),
        None,
        body,
    )?;
    let fields = decode(&response)?;
    let pending: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
    if pending != device_id {
        return Err(SyncError::RelayWire("redeem pending_device 不符"));
    }
    Ok(())
}

/// 批准一台 PENDING 设备（管理签名证书 + 封给它的 HPKE grant）。
///
/// 只接收**公钥**：恢复码自批路径用本地刚生成的密钥，邀请路径用管理端从成员视图读到的
/// pending 设备公钥（ADR 0014 S1 §3）。两条路径共用同一份签名/canonical body 逻辑。
#[allow(clippy::too_many_arguments)]
pub(crate) fn approve_device<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    manager: &ManagementKey,
    token: &str,
    control_rev: u64,
    instance: [u8; 16],
    device_id: DeviceId,
    sign_pk: [u8; 32],
    hpke_pk: [u8; 32],
    manager_rev: u64,
    root: &RootSecretV1,
) -> Result<(), SyncError> {
    let cert = DeviceCertV1 {
        vault_id,
        device_id,
        sign_ed25519_pub: sign_pk,
        hpke_x25519_pub: hpke_pk,
        cert_serial: 1,
        issued_manager_rev: manager_rev,
        rights: Rights(Rights::ALL),
    };
    let cert_bytes = cert.encode()?;
    let certificate_signature = manager.sign(&device_cert_signed_bytes(&cert)?);
    let grant_id: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
    let aad = GrantAadV1 {
        pv: 1,
        vault_id,
        epoch: root.key_epoch,
        recipient_device_id: device_id,
        recipient_x25519_pub: hpke_pk,
        manager_rev,
        grant_id,
    };
    let sealed = seal_epoch_grant(&hpke_pk, root, &aad)?;
    let grant_signature = manager.sign(&sealed.signature_input(&aad)?);
    let request_id: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
    let payload = cbor::encode(vec![
        (0, cbor::bytes(cert_bytes.clone())),
        (1, cbor::bytes(certificate_signature.to_vec())),
        (2, cbor::bytes(grant_id.to_vec())),
        (3, cbor::bytes(sealed.enc.to_vec())),
        (4, cbor::bytes(sealed.ciphertext.clone())),
        (5, cbor::bytes(grant_signature.to_vec())),
        (6, cbor::bytes(request_id.to_vec())),
        (7, cbor::uint(control_rev)),
    ])?;
    let path = api.vault_path(&vault_id, &format!("/devices/{}/approve", b64(&device_id)));
    let body_digest: [u8; 32] =
        Sha256::digest(management_request_cde(HttpMethod::Post, &path, &payload)?).into();
    let management_signature = manager.sign(&management_signing_input(
        &instance,
        &vault_id,
        &request_id,
        control_rev,
        &body_digest,
    )?);
    let body = cbor::encode(vec![
        (
            0,
            cbor::CdeValue::Map(vec![
                (0, cbor::bytes(payload)),
                (1, cbor::bytes(management_signature.to_vec())),
            ]),
        ),
        (1, cbor::bytes(request_id.to_vec())),
        (2, cbor::uint(control_rev)),
        (3, cbor::bytes(cert_bytes)),
        (4, cbor::bytes(certificate_signature.to_vec())),
        (5, cbor::bytes(grant_id.to_vec())),
        (6, cbor::bytes(sealed.enc.to_vec())),
        (7, cbor::bytes(sealed.ciphertext)),
        (8, cbor::bytes(grant_signature.to_vec())),
    ])?;
    let response = send(transport, "POST", api.url(&path), Some(token), body)?;
    let fields = decode(&response)?;
    let approved: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
    if approved != device_id {
        return Err(SyncError::RelayWire("approve device_id 不符"));
    }
    let cert_sig: [u8; 64] = cbor::bytes_fixed(&fields, 4)?;
    verify_signature(
        &manager.public_key(),
        &device_cert_signed_bytes(&cert)?,
        &cert_sig,
    )?;
    Ok(())
}

pub(crate) fn get_state<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    token: &str,
) -> Result<(u64, u64), SyncError> {
    let response = send(
        transport,
        "GET",
        api.url(&api.vault_path(&vault_id, "/state")),
        Some(token),
        Vec::new(),
    )?;
    let fields = decode(&response)?;
    Ok((cbor::uint_field(&fields, 5)?, cbor::uint_field(&fields, 4)?))
}

pub(crate) fn send<T: HttpTransport>(
    transport: &mut T,
    method: &'static str,
    path: String,
    token: Option<&str>,
    body: Vec<u8>,
) -> Result<HttpResponse, SyncError> {
    let mut headers = Vec::new();
    if !body.is_empty() {
        headers.push(("content-type".to_owned(), CONTENT_TYPE.to_owned()));
    }
    if let Some(token) = token {
        headers.push(("authorization".to_owned(), format!("Bearer {token}")));
    }
    transport.send(&HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

pub(crate) fn decode(response: &HttpResponse) -> Result<cbor::Fields, SyncError> {
    if !(200..300).contains(&response.status) {
        return Err(error_from_response(response));
    }
    cbor::decode(&response.body)
}

fn management_subject(manager_pub: &[u8; 32]) -> [u8; 16] {
    let digest = Sha256::digest(manager_pub);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

fn hex16(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn recovery_slot_descriptor(slot: &SlotRecord) -> Result<SlotDescriptorV1, SyncError> {
    Ok(SlotDescriptorV1::decode(&slot.descriptor)?)
}

fn parse_kind(text: &str) -> Result<ProtoSlotKind, SyncError> {
    match text {
        "password" => Ok(ProtoSlotKind::Password),
        "recovery" => Ok(ProtoSlotKind::Recovery),
        _ => Err(SyncError::Unsupported("未知槽类型")),
    }
}

fn b64(bytes: &[u8]) -> String {
    crate::base64url::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedTransport(Vec<u8>);

    impl HttpTransport for FixedTransport {
        fn send(&mut self, _request: &HttpRequest) -> Result<HttpResponse, SyncError> {
            Ok(HttpResponse {
                status: 200,
                body: self.0.clone(),
            })
        }
    }

    fn grant_body(root: &RootSecretV1, hpke: &DeviceHpkeKey, device_id: DeviceId) -> Vec<u8> {
        let aad = GrantAadV1 {
            pv: 1,
            vault_id: root.vault_id,
            epoch: root.key_epoch,
            recipient_device_id: device_id,
            recipient_x25519_pub: hpke.public_key(),
            manager_rev: 1,
            grant_id: [9; 16],
        };
        let sealed = seal_epoch_grant(&hpke.public_key(), root, &aad).expect("seal");
        let hash: [u8; 32] = Sha256::digest(&sealed.ciphertext).into();
        let signed = aad
            .grant_signed_bytes(&sealed.enc, &hash)
            .expect("signed header");
        cbor::encode(vec![
            (0, cbor::uint(root.key_epoch)),
            (1, cbor::bytes(device_id.to_vec())),
            (2, cbor::uint(1)),
            (3, cbor::bytes(sealed.enc.to_vec())),
            (4, cbor::bytes(sealed.ciphertext)),
            (5, cbor::bytes(signed)),
        ])
        .expect("body")
    }

    fn sample(hpke: &DeviceHpkeKey) -> (RootSecretV1, DeviceId) {
        let root = RootSecretV1::new([7; 16], 1, crate::keys::Secret32::from_array([0x42; 32]));
        let device_id: DeviceId = [3; 16];
        let _ = hpke;
        (root, device_id)
    }

    #[test]
    fn grant_unseals_to_the_same_root_and_placeholder_is_rejected() {
        let hpke = DeviceHpkeKey::generate(&mut crate::random::OsRandom).expect("hpke");
        let (root, device_id) = sample(&hpke);
        let mut transport = FixedTransport(grant_body(&root, &hpke, device_id));
        verify_latest_grant(
            &mut transport,
            &RelayApi::new("http://relay", crate::api_version::RelayApiVersion::V1),
            root.vault_id,
            device_id,
            "token",
            &hpke,
            &root,
        )
        .expect("真实 grant 必须解出同一 root");

        let placeholder = RootSecretV1::new(
            root.vault_id,
            root.key_epoch,
            crate::keys::Secret32::from_array([0x5A; 32]),
        );
        let mut transport = FixedTransport(grant_body(&placeholder, &hpke, device_id));
        assert!(
            verify_latest_grant(
                &mut transport,
                &RelayApi::new("http://relay", crate::api_version::RelayApiVersion::V1),
                root.vault_id,
                device_id,
                "token",
                &hpke,
                &root,
            )
            .is_err(),
            "占位 root 的 grant 必须被拒绝"
        );
    }
}
