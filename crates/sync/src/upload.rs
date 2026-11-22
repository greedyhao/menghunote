//! 通用对象上传器（S7-14）：KEY_SLOT / SNAPSHOT / MANIFEST / ASSET。
//!
//! 引擎的 UPDATE 走 storage outbox（可重试/持久化）；checkpoint 与初始槽是一次性导出，
//! 因此这里直接构造 MNE1 对象并上传，复用同一套固定切片/AAD/K_object/ObjectCommit 规则。
//!
//! 每个对象流程：
//! 1. `part_count_for` / `expected_part_plain_len` 切固定片；
//! 2. 每片随机 nonce（KEY_SLOT 复用槽体 nonce，见 [`upload_object`]），
//!    `header.aad(descriptor)` 作 AAD，XChaCha20-Poly1305 加密；
//! 3. `PUT part` → `ObjectCommit` + 设备签名 → `submit_object`（relay 侧 complete）；
//! 4. `object_digest` 按 §2 重算并随 commit 提交。

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};

use crate::device::DeviceSigningKey;
use crate::error::SyncError;
use crate::keys::{derive_object_key, RootSecretV1};
use crate::relay::{ObjectSubmission, RelayClient, UpdateStatus};
use crate::{DeviceId, ObjectId, VaultId};
use menghunote_sync_proto::{
    cap_plain, expected_part_plain_len, object_commit_signing_input, object_digest_input,
    part_count_for, Envelope, EnvelopeHeader, ObjectCommit, ObjectType,
};

/// 已上传对象的最小事实集（构造 manifest/refs/读回校验用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadedObject {
    /// 对象 id。
    pub object_id: ObjectId,
    /// 分片数。
    pub part_count: u32,
    /// 明文总长。
    pub total_plain_len: u64,
    /// 密文对象摘要（§2）。
    pub object_digest: [u8; 32],
}

/// 上传一个对象并 complete。
///
/// - `descriptor`：KEY_SLOT 的 `CDE(SlotDescriptorV1)`；其他类型传空切片；
/// - `nonce_override`：KEY_SLOT 必须复用槽体 nonce（包封时 AAD 已绑定它）；
///   其他类型传 `None`（每片随机）。
#[allow(clippy::too_many_arguments)]
pub fn upload_object<C: RelayClient>(
    client: &mut C,
    vault_id: VaultId,
    producer_device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: ObjectId,
    message_id: [u8; 16],
    generation: u64,
    sender_seq: u64,
    plaintext: &[u8],
    descriptor: &[u8],
    nonce_override: Option<[u8; 24]>,
) -> Result<UploadedObject, SyncError> {
    upload_object_inner(
        client,
        vault_id,
        producer_device_id,
        signing_key,
        root,
        object_type,
        object_id,
        message_id,
        generation,
        sender_seq,
        plaintext,
        descriptor,
        nonce_override,
        None,
        None,
    )
}

/// 候选 epoch 上传：携带 relay 维护 reservation id（§28.1）。
#[allow(clippy::too_many_arguments)]
pub fn upload_object_with_reservation<C: RelayClient>(
    client: &mut C,
    vault_id: VaultId,
    producer_device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: ObjectId,
    message_id: [u8; 16],
    generation: u64,
    sender_seq: u64,
    plaintext: &[u8],
    descriptor: &[u8],
    nonce_override: Option<[u8; 24]>,
    reservation: &[u8; 16],
) -> Result<UploadedObject, SyncError> {
    upload_object_inner(
        client,
        vault_id,
        producer_device_id,
        signing_key,
        root,
        object_type,
        object_id,
        message_id,
        generation,
        sender_seq,
        plaintext,
        descriptor,
        nonce_override,
        None,
        Some(reservation),
    )
}

/// ADR §20：KEY_SLOT 使用调用方给定的 `K_slot_mne`（而不是 root 派生的 K_object）。
#[allow(clippy::too_many_arguments)]
pub fn upload_object_with_key<C: RelayClient>(
    client: &mut C,
    vault_id: VaultId,
    producer_device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: ObjectId,
    message_id: [u8; 16],
    generation: u64,
    sender_seq: u64,
    plaintext: &[u8],
    descriptor: &[u8],
    nonce_override: Option<[u8; 24]>,
    key: &crate::keys::Secret32,
) -> Result<UploadedObject, SyncError> {
    upload_object_inner(
        client,
        vault_id,
        producer_device_id,
        signing_key,
        root,
        object_type,
        object_id,
        message_id,
        generation,
        sender_seq,
        plaintext,
        descriptor,
        nonce_override,
        Some(key),
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn upload_object_with_key_and_reservation<C: RelayClient>(
    client: &mut C,
    vault_id: VaultId,
    producer_device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: ObjectId,
    message_id: [u8; 16],
    generation: u64,
    sender_seq: u64,
    plaintext: &[u8],
    descriptor: &[u8],
    nonce_override: Option<[u8; 24]>,
    key: &crate::keys::Secret32,
    reservation: &[u8; 16],
) -> Result<UploadedObject, SyncError> {
    upload_object_inner(
        client,
        vault_id,
        producer_device_id,
        signing_key,
        root,
        object_type,
        object_id,
        message_id,
        generation,
        sender_seq,
        plaintext,
        descriptor,
        nonce_override,
        Some(key),
        Some(reservation),
    )
}

#[allow(clippy::too_many_arguments)]
fn upload_object_inner<C: RelayClient>(
    client: &mut C,
    vault_id: VaultId,
    producer_device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: ObjectId,
    message_id: [u8; 16],
    generation: u64,
    sender_seq: u64,
    plaintext: &[u8],
    descriptor: &[u8],
    nonce_override: Option<[u8; 24]>,
    key_override: Option<&crate::keys::Secret32>,
    reservation: Option<&[u8; 16]>,
) -> Result<UploadedObject, SyncError> {
    if plaintext.is_empty() {
        return Err(SyncError::LocalCorrupt("对象明文为空"));
    }
    let total = plaintext.len() as u64;
    let part_count = u32::try_from(part_count_for(total, object_type))
        .map_err(|_| SyncError::LocalCorrupt("分片数超出 u32"))?;
    let key = match key_override {
        Some(key) => key.clone(),
        None => derive_object_key(root, object_type, &object_id)?,
    };
    let cipher = XChaCha20Poly1305::new_from_slice(key.expose_slice())
        .map_err(|_| SyncError::Object("数据密钥长度非法"))?;
    let cap = cap_plain(object_type) as usize;
    let mut rng = crate::random::OsRandom;
    let mut digests = Vec::with_capacity(part_count as usize);
    for index in 0..part_count {
        let this_len = expected_part_plain_len(object_type, total, index)?;
        let start = index as usize * cap;
        let end = start
            .checked_add(this_len)
            .ok_or(SyncError::LocalCorrupt("分片偏移溢出"))?;
        let chunk = plaintext
            .get(start..end)
            .ok_or(SyncError::LocalCorrupt("分片超出明文长度"))?;
        let nonce = if let Some(nonce) = nonce_override {
            if index == 0 {
                nonce
            } else {
                crate::random::array::<24>(&mut rng)?
            }
        } else if object_type == ObjectType::Asset {
            // ADR §19：ASSET 用确定性 nonce，保证跨进程重传密文字节一致、relay 幂等。
            asset_nonce(&key, &vault_id, &object_id, index)?
        } else {
            crate::random::array::<24>(&mut rng)?
        };
        let header = EnvelopeHeader {
            protocol_version: 1,
            header_len: 196,
            object_type,
            cipher_suite: 1,
            flags: 0,
            vault_id,
            object_id,
            message_id,
            producer_device_id,
            generation,
            key_epoch: root.key_epoch,
            sender_seq,
            part_index: index,
            part_count,
            total_plaintext_len: total,
            this_plaintext_len: u32::try_from(chunk.len())
                .map_err(|_| SyncError::LocalCorrupt("分片明文超出 u32"))?,
            ciphertext_len: u32::try_from(chunk.len() + 16)
                .map_err(|_| SyncError::LocalCorrupt("分片密文超出 u32"))?,
            context_hash: context_hash_for(object_type, descriptor)?,
            nonce,
        };
        let aad = header.aad(descriptor)?;
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: chunk,
                    aad: &aad,
                },
            )
            .map_err(|_| SyncError::Object("对象加密失败"))?;
        let envelope = Envelope::new(header, ciphertext)?.encode()?;
        let digest: [u8; 32] = Sha256::digest(&envelope).into();
        client.put_part_with_reservation(&object_id, index, &envelope, reservation)?;
        digests.push(digest);
    }
    let digest_input = object_digest_input(part_count, &digests)?;
    let object_digest: [u8; 32] = Sha256::digest(&digest_input).into();
    let commit = ObjectCommit {
        vault_id,
        object_id,
        message_id,
        object_type,
        generation,
        key_epoch: root.key_epoch,
        sender_seq,
        part_count,
        total_len: total,
        object_digest,
    };
    let signed_commit = commit.encode()?;
    let signature = signing_key.sign(&object_commit_signing_input(&commit)?);
    let outcome = client.submit_object(ObjectSubmission {
        object_id,
        commit,
        signed_commit,
        signature,
    })?;
    match outcome.status {
        UpdateStatus::Accepted { .. } | UpdateStatus::Duplicate => {}
        UpdateStatus::Failed { code, retryable } => {
            return Err(SyncError::RelayApi {
                code: code.unwrap_or_else(|| "OBJECT_FAILED".to_owned()),
                retryable,
                current_epoch: None,
                current_control_rev: None,
                bootstrap_hint: None,
            })
        }
    }
    Ok(UploadedObject {
        object_id,
        part_count,
        total_plain_len: total,
        object_digest,
    })
}

/// 读回一个对象：拉全部片、校验摘要/类型/长度、解密并返回 `(header, plaintext)`。
///
/// 用于 checkpoint/slot 的"从 relay 读回解密/导入"验证；KEY_SLOT 需要传 descriptor。
#[allow(clippy::too_many_arguments)]
pub fn fetch_object<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: &ObjectId,
    part_count: u32,
    total_plain_len: u64,
    expected_digest: &[u8; 32],
    descriptor: &[u8],
) -> Result<(EnvelopeHeader, Vec<u8>), SyncError> {
    fetch_object_inner(
        client,
        root,
        object_type,
        object_id,
        part_count,
        total_plain_len,
        expected_digest,
        descriptor,
        None,
    )
}

/// ADR §20：用调用方给定的 `K_slot_mne` 读回 KEY_SLOT。
#[allow(clippy::too_many_arguments)]
pub fn fetch_object_with_key<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: &ObjectId,
    part_count: u32,
    total_plain_len: u64,
    expected_digest: &[u8; 32],
    descriptor: &[u8],
    key: &crate::keys::Secret32,
) -> Result<(EnvelopeHeader, Vec<u8>), SyncError> {
    fetch_object_inner(
        client,
        root,
        object_type,
        object_id,
        part_count,
        total_plain_len,
        expected_digest,
        descriptor,
        Some(key),
    )
}

#[allow(clippy::too_many_arguments)]
fn fetch_object_inner<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: &ObjectId,
    part_count: u32,
    total_plain_len: u64,
    expected_digest: &[u8; 32],
    descriptor: &[u8],
    key_override: Option<&crate::keys::Secret32>,
) -> Result<(EnvelopeHeader, Vec<u8>), SyncError> {
    let mut envelopes = Vec::with_capacity(part_count as usize);
    let mut digests = Vec::with_capacity(part_count as usize);
    for index in 0..part_count {
        let bytes = client.part(object_id, index)?;
        let envelope = Envelope::decode(&bytes)?;
        if envelope.header.object_type != object_type
            || envelope.header.object_id != *object_id
            || envelope.header.part_index != index
            || envelope.header.part_count != part_count
            || envelope.header.generation == 0
                && matches!(object_type, ObjectType::Update | ObjectType::Snapshot)
        {
            return Err(SyncError::Object("读回对象 header 不符"));
        }
        digests.push(Sha256::digest(&bytes).into());
        envelopes.push(envelope);
    }
    let digest_input = object_digest_input(part_count, &digests)?;
    let computed: [u8; 32] = Sha256::digest(&digest_input).into();
    if computed != *expected_digest {
        return Err(SyncError::Object("读回对象摘要不符"));
    }
    let key = match key_override {
        Some(key) => key.clone(),
        None => derive_object_key(root, object_type, object_id)?,
    };
    let cipher = XChaCha20Poly1305::new_from_slice(key.expose_slice())
        .map_err(|_| SyncError::Object("数据密钥长度非法"))?;
    let mut plaintext = Vec::with_capacity(total_plain_len as usize);
    for (index, envelope) in envelopes.iter().enumerate() {
        let expected = expected_part_plain_len(
            object_type,
            total_plain_len,
            u32::try_from(index).map_err(|_| SyncError::Object("分片下标溢出"))?,
        )?;
        if envelope.header.this_plaintext_len as usize != expected {
            return Err(SyncError::Object("读回分片长度不符"));
        }
        let aad = envelope.header.aad(descriptor)?;
        let chunk = cipher
            .decrypt(
                XNonce::from_slice(&envelope.header.nonce),
                Payload {
                    msg: &envelope.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| SyncError::Object("读回 AEAD 解密失败"))?;
        plaintext.extend_from_slice(&chunk);
    }
    if plaintext.len() as u64 != total_plain_len {
        return Err(SyncError::Object("读回明文长度不符"));
    }
    let header = envelopes
        .first()
        .map(|envelope| envelope.header.clone())
        .ok_or(SyncError::Object("读回对象没有分片"))?;
    Ok((header, plaintext))
}

/// ADR §19：ASSET 的确定性 nonce = `HKDF-SHA256(K_object, salt=vault_id,
/// info = "MN/v1/asset-nonce\\0" || object_id || BE32(part_index), 24)`。
fn asset_nonce(
    key: &crate::keys::Secret32,
    vault_id: &VaultId,
    object_id: &ObjectId,
    part_index: u32,
) -> Result<[u8; 24], SyncError> {
    let mut info = Vec::with_capacity(crate::domain::HKDF_ASSET_NONCE_INFO.len() + 36);
    info.extend_from_slice(crate::domain::HKDF_ASSET_NONCE_INFO);
    info.extend_from_slice(object_id);
    info.extend_from_slice(&part_index.to_be_bytes());
    let mut nonce = [0u8; 24];
    crate::kdf::hkdf_expand(key.expose_slice(), vault_id, &info, &mut nonce)?;
    Ok(nonce)
}

/// ASSET 的确定性 message_id（ADR §19 要求重传密文字节一致；header 里的 message_id
/// 也参与 AAD，因此必须确定）。取确定性 nonce 派生输入的一个保留 part 序号后前 16B。
pub fn asset_message_id(root: &RootSecretV1, object_id: &ObjectId) -> Result<[u8; 16], SyncError> {
    let key = derive_object_key(root, ObjectType::Asset, object_id)?;
    let nonce = asset_nonce(&key, &root.vault_id, object_id, u32::MAX)?;
    let mut message_id = [0u8; 16];
    message_id.copy_from_slice(&nonce[..16]);
    Ok(message_id)
}

/// KEY_SLOT 的 `context_hash` = `SHA256(CDE(descriptor))`；其他类型全零。
fn context_hash_for(object_type: ObjectType, descriptor: &[u8]) -> Result<[u8; 32], SyncError> {
    if object_type == ObjectType::KeySlot {
        if descriptor.is_empty() {
            return Err(SyncError::Descriptor("KEY_SLOT 缺少 descriptor"));
        }
        Ok(Sha256::digest(descriptor).into())
    } else {
        Ok([0u8; 32])
    }
}
