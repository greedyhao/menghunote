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
    part_count_for, Envelope, EnvelopeHeader, ObjectCommit, ObjectType, MAX_ASSET_RAW_BYTES,
    MAX_KEY_SLOT_PLAINTEXT, MAX_LORO_SNAPSHOT_BYTES, MAX_MANIFEST_CDE_BYTES, MAX_UPDATE_CDE_BYTES,
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
            // ADR §31：ASSET 的 AAD 不得包含 producer_device_id（否则两台设备上传
            // 同一资产会得到不同 AAD/密文字节）。header 字段固定为 0；其他类型保持真实值。
            producer_device_id: asset_producer(object_type, producer_device_id),
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
        None,
    )
}

/// ASSET 真实验证的**完整期望声明**（全部字段来自本机事实/已绑定声明，不是读回后自证）。
///
/// 这是 checkpoint writer 与 engine pull 共用的唯一 ASSET 来源认证入口：任何一条
/// 字段不符都不得产出"已验证资产"事实。
#[derive(Debug, Clone, Copy)]
pub(crate) struct AssetExpectation<'a> {
    /// 声明对象 id（必须是完整明文头的 canonical HMAC）。
    pub object_id: &'a ObjectId,
    /// 声明对象类型（只接受 ASSET）。
    pub object_type: ObjectType,
    /// 声明 generation。
    pub generation: u64,
    /// 声明 message_id；`None` 表示"只要求等于本根确定性派生值"（refs 路径）。
    pub message_id: Option<[u8; 16]>,
    /// 声明 sender_seq。
    pub sender_seq: u64,
    /// 声明分片数。
    pub part_count: u32,
    /// 声明明文总长。
    pub total_len: u64,
    /// 声明密文对象摘要。
    pub object_digest: &'a [u8; 32],
    /// 声明 key epoch（必须等于验证根的 epoch）。
    pub key_epoch: u64,
}

/// 一次 ASSET 真实验证通过后的**最小事实集**：不含明文、不含密文。
///
/// 只由 [`fetch_verified_asset`] 产出；调用方可以把它们写进本地记录/缓存，
/// 但不得据此重放任何明文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedAssetFacts {
    /// 资产数据真实 SHA256（来自解密后的 canonical 明文头）。
    pub real_sha256: [u8; 32],
    /// 资产数据真实字节数。
    pub byte_len: u64,
    /// 资产 MIME（canonical 头的一部分）。
    pub media_type: String,
    /// 由实际密文分片重算并与声明核对的 object_digest。
    pub object_digest: [u8; 32],
}

/// 每片 MNE1 header 的期望上下文（ASSET 来源认证用）。
///
/// `fetch_object` 只校验 object_type/object_id/part_index/part_count；本结构把
/// **每一片**的 vault/epoch/generation/sender/message/producer 也固定下来，防止
/// relay 用同一对象的不同声明混片。
#[derive(Debug, Clone, Copy)]
struct ExpectedEnvelopeContext {
    vault_id: VaultId,
    key_epoch: u64,
    generation: u64,
    sender_seq: u64,
    message_id: [u8; 16],
    producer_device_id: DeviceId,
}

/// 真实下载并验证一个 ASSET：分片上限/分配保护、固定切片、每片 header 上下文、
/// AEAD（AAD）、密文 digest、明文长度、data SHA256、MIME、根派生 OID 重算。
///
/// `max_total_len` 是调用方允许的最大声明总长（与协议 `MAX_ASSET_RAW_BYTES` 取小）；
/// 声明超限在**任何分配之前**拒绝。返回事实集，不返回持久明文。
///
/// checkpoint writer 的 refs/coverage 复核与 engine pull 的 skipped-asset 落盘都必须
/// 走这里，保证"同一份声明"只有一种验证口径。
pub(crate) fn fetch_verified_asset<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    vault_id: VaultId,
    max_total_len: u64,
    expected: &AssetExpectation<'_>,
) -> Result<VerifiedAssetFacts, SyncError> {
    if expected.object_type != ObjectType::Asset {
        return Err(SyncError::Object("共享 ASSET 验证器只接受 ASSET 声明"));
    }
    if root.vault_id != vault_id || root.key_epoch != expected.key_epoch {
        return Err(SyncError::Object("ASSET 验证根与声明的 vault/epoch 不一致"));
    }
    let cap = max_total_len.min(MAX_ASSET_RAW_BYTES as u64);
    if expected.total_len == 0 || expected.total_len > cap {
        return Err(SyncError::Object("ASSET 声明总长越界"));
    }
    if expected.part_count == 0 {
        return Err(SyncError::Object("ASSET 声明分片数为 0"));
    }
    // ASSET message_id 必须由根/object_id 确定性派生（ADR §19：重传密文字节一致）。
    let derived_message_id = asset_message_id(root, expected.object_id)?;
    if let Some(declared) = expected.message_id {
        if declared != derived_message_id {
            return Err(SyncError::Object(
                "ASSET 声明的 message_id 不是本根确定性派生值",
            ));
        }
    }
    let context = ExpectedEnvelopeContext {
        vault_id,
        key_epoch: expected.key_epoch,
        generation: expected.generation,
        sender_seq: expected.sender_seq,
        message_id: derived_message_id,
        producer_device_id: [0u8; 16],
    };
    let (header, plaintext) = fetch_object_inner(
        client,
        root,
        ObjectType::Asset,
        expected.object_id,
        expected.part_count,
        expected.total_len,
        expected.object_digest,
        &[],
        None,
        Some(&context),
    )
    .map_err(|err| match err {
        // 信封结构错误语义上是"对象无效"，与 UPDATE 路径保持一致（不把
        // Proto 细节当成 relay 协议错误）。
        SyncError::Proto(_) => SyncError::Object("ASSET 密文不符合 MNE1 契约"),
        other => other,
    })?;
    // 首片 header 的最终校验（逐片上下文已在上面的循环里校验）。
    if header.object_type != ObjectType::Asset
        || header.vault_id != vault_id
        || header.object_id != *expected.object_id
        || header.part_index != 0
        || header.part_count != expected.part_count
        || header.total_plaintext_len != expected.total_len
        || header.key_epoch != expected.key_epoch
        || header.generation != expected.generation
        || header.sender_seq != expected.sender_seq
        || header.message_id != derived_message_id
        || header.producer_device_id != [0u8; 16]
    {
        return Err(SyncError::Object("ASSET 信封 header 与完整声明不一致"));
    }
    // 明文头 = 32B real_sha256 + BE64 byte_len + BE16 mime_len + MIME + data；
    // 这里会校验 data.len()==byte_len 且 SHA256(data)==real_sha256。
    let (real_sha256, byte_len, media_type) =
        crate::rotation_stage::decode_asset_plaintext(&plaintext)?;
    // 根派生 object_id 重算：MIME 非空/≤255 在这里 fail-closed。不一致说明声明的
    // 对象不是这份明文（典型：伪造 ASSET / 被替换资产）。
    let canonical = crate::checkpoint::asset_object_id(root, &real_sha256, byte_len, &media_type)?;
    if canonical != *expected.object_id {
        return Err(SyncError::Object(
            "ASSET 明文头重算的 object_id 与声明不一致",
        ));
    }
    Ok(VerifiedAssetFacts {
        real_sha256,
        byte_len,
        media_type,
        object_digest: *expected.object_digest,
    })
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
        None,
    )
}

/// 本地重打包产物：`(part_no, envelope, part_digest)`。
pub type EncryptedLocalParts = Vec<(u32, Vec<u8>, [u8; 32])>;

/// 用新 epoch 根把同一明文重新加密成 MNE1 parts（阶段 J 重打包用）。
///
/// 返回 `(part_no, envelope, part_digest)` 与对象 digest；不接触 relay。
#[allow(clippy::too_many_arguments)]
pub fn encrypt_local_parts(
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: &ObjectId,
    message_id: [u8; 16],
    producer_device_id: DeviceId,
    generation: u64,
    sender_seq: u64,
    plaintext: &[u8],
    descriptor: &[u8],
) -> Result<(EncryptedLocalParts, [u8; 32]), SyncError> {
    if plaintext.is_empty() {
        return Err(SyncError::LocalCorrupt("重打包明文为空"));
    }
    let total = plaintext.len() as u64;
    let part_count = u32::try_from(part_count_for(total, object_type))
        .map_err(|_| SyncError::LocalCorrupt("分片数超出 u32"))?;
    let key = derive_object_key(root, object_type, object_id)?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.expose_slice())
        .map_err(|_| SyncError::Object("数据密钥长度非法"))?;
    let cap = cap_plain(object_type) as usize;
    let mut rng = crate::random::OsRandom;
    let mut parts = Vec::with_capacity(part_count as usize);
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
        let nonce = crate::random::array::<24>(&mut rng)?;
        let header = EnvelopeHeader {
            protocol_version: 1,
            header_len: 196,
            object_type,
            cipher_suite: 1,
            flags: 0,
            vault_id: root.vault_id,
            object_id: *object_id,
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
            .map_err(|_| SyncError::Object("重打包加密失败"))?;
        let envelope = Envelope::new(header, ciphertext)?.encode()?;
        digests.push(Sha256::digest(&envelope).into());
        parts.push((index, envelope, digests[index as usize]));
    }
    let digest_input = object_digest_input(part_count, &digests)?;
    let object_digest: [u8; 32] = Sha256::digest(&digest_input).into();
    Ok((parts, object_digest))
}

/// 解密**本地持久化**的单个 MNE1 part（阶段 J 重打包用）。
///
/// 明文仅放在 `Zeroizing` 缓冲中返回；调用方必须立即重新加密，不得落库。
pub fn decrypt_envelope(
    root: &RootSecretV1,
    expected_type: ObjectType,
    expected_object_id: &ObjectId,
    envelope_bytes: &[u8],
    descriptor: &[u8],
) -> Result<(EnvelopeHeader, zeroize::Zeroizing<Vec<u8>>), SyncError> {
    let envelope = Envelope::decode(envelope_bytes)?;
    if envelope.header.object_type != expected_type
        || envelope.header.object_id != *expected_object_id
    {
        return Err(SyncError::Object("本地 part 的 header 与 outbox 记录不符"));
    }
    let key = derive_object_key(root, expected_type, expected_object_id)?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.expose_slice())
        .map_err(|_| SyncError::Object("数据密钥长度非法"))?;
    let aad = envelope.header.aad(descriptor)?;
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&envelope.header.nonce),
            Payload {
                msg: &envelope.ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| SyncError::Object("本地 part 解密失败（旧 epoch/根不匹配）"))?;
    Ok((envelope.header, zeroize::Zeroizing::new(plaintext)))
}

/// 读回路径的明文上限：与 `EnvelopeHeader::validate_lengths` 的每类型上限保持一致。
///
/// `fetch_object_inner` 收到的 `total_plain_len` 可能是 relay/其它记录里未按对象类型
/// 校验过的声明，分配前必须按类型收口（ASSET 用 `MAX_ASSET_RAW_BYTES`，其它沿用各自
/// 既有 limits）。
fn max_plaintext_len(object_type: ObjectType) -> u64 {
    match object_type {
        ObjectType::Update => MAX_UPDATE_CDE_BYTES as u64,
        ObjectType::Snapshot => MAX_LORO_SNAPSHOT_BYTES as u64,
        ObjectType::Manifest => MAX_MANIFEST_CDE_BYTES as u64,
        ObjectType::Asset => MAX_ASSET_RAW_BYTES as u64,
        ObjectType::KeySlot => MAX_KEY_SLOT_PLAINTEXT as u64,
    }
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
    expected_context: Option<&ExpectedEnvelopeContext>,
) -> Result<(EnvelopeHeader, Vec<u8>), SyncError> {
    // 分配之前先按对象类型与固定切片规则把声明收口。调用方的 `part_count` /
    // `total_plain_len` 可能直接来自 relay 声明（例如 ASSET 日志事件的
    // `ObjectCommit`），而 `Vec::with_capacity(u32::MAX as usize)` 在分配失败时是
    // **abort**（不可捕获），读回层无从 fail-closed。因此这里先验后配：
    // 1) 总长不得超过该类型的明文上限（与 `EnvelopeHeader::validate_lengths` 同口径）；
    // 2) 片数必须等于固定切片规则算出的片数（关系自洽）；
    // 3) 容量申请用 `try_reserve*`，失败映射成可捕获的 `SyncError::Object`。
    if total_plain_len > max_plaintext_len(object_type) {
        return Err(SyncError::Object("读回对象明文超出类型上限"));
    }
    if u64::from(part_count) != part_count_for(total_plain_len, object_type) {
        return Err(SyncError::Object("读回对象分片数与固定切片不符"));
    }
    let mut envelopes = Vec::new();
    envelopes
        .try_reserve_exact(part_count as usize)
        .map_err(|_| SyncError::Object("读回对象分片数超出可分配容量"))?;
    let mut digests = Vec::new();
    digests
        .try_reserve_exact(part_count as usize)
        .map_err(|_| SyncError::Object("读回对象分片数超出可分配容量"))?;
    for index in 0..part_count {
        let bytes = client.part(object_id, index)?;
        let envelope = Envelope::decode(&bytes)?;
        if envelope.header.object_type != object_type
            || envelope.header.object_id != *object_id
            || envelope.header.part_index != index
            || envelope.header.part_count != part_count
            || envelope.header.total_plaintext_len != total_plain_len
            || envelope.header.generation == 0
                && matches!(object_type, ObjectType::Update | ObjectType::Snapshot)
        {
            return Err(SyncError::Object("读回对象 header 不符"));
        }
        // ASSET 来源认证：每一片的 epoch/generation/sender/message/producer 上下文
        // 都必须等于声明，防止 relay 把同 key 的不同声明（或不同代）混片。
        if let Some(expected) = expected_context {
            if envelope.header.vault_id != expected.vault_id
                || envelope.header.key_epoch != expected.key_epoch
                || envelope.header.generation != expected.generation
                || envelope.header.sender_seq != expected.sender_seq
                || envelope.header.message_id != expected.message_id
                || envelope.header.producer_device_id != expected.producer_device_id
            {
                return Err(SyncError::Object("读回对象分片 header 上下文不符"));
            }
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
    let plaintext_len = usize::try_from(total_plain_len)
        .map_err(|_| SyncError::Object("读回对象明文超出地址空间"))?;
    let mut plaintext = Vec::new();
    plaintext
        .try_reserve(plaintext_len)
        .map_err(|_| SyncError::Object("读回对象明文超出可分配容量"))?;
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

/// ADR §31：ASSET 的 header `producer_device_id` 固定为 0（AAD 不含 producer）。
///
/// 其他对象类型保留真实设备 id（它们用随机 nonce，不存在跨设备确定性要求）。
fn asset_producer(object_type: ObjectType, producer_device_id: DeviceId) -> DeviceId {
    if object_type == ObjectType::Asset {
        [0u8; 16]
    } else {
        producer_device_id
    }
}

/// ADR §19：ASSET 的确定性 nonce = `HKDF-SHA256(K_object, salt=vault_id,
/// info = "MN/v1/asset-nonce\\0" || object_id || BE32(part_index), 24)`。
///
/// §31 起 `object_id` 提交完整资产明文头（MIME/长度/数据摘要），所以同一
/// `(object_id, part_index)` 只可能对应同一明文头与同一 AAD。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::derive_asset_id_key;
    use crate::test_support::TestRng;
    use hmac::{Hmac, Mac};

    /// §31 之前的旧口径：`object_id = HMAC(K_asset_id, real_sha256)`。
    fn old_object_id(root: &RootSecretV1, real_sha256: &[u8; 32]) -> ObjectId {
        let key = derive_asset_id_key(root).expect("asset id key");
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.expose_slice()).expect("HMAC key");
        mac.update(real_sha256);
        let digest = mac.finalize().into_bytes();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }

    #[test]
    fn old_real_sha_only_id_would_reuse_nonce_for_different_mime() {
        let root = RootSecretV1::generate([0x5a; 16], 1, &mut TestRng::new(7)).expect("root");
        let data = b"same asset bytes";
        let real: [u8; 32] = Sha256::digest(data).into();

        // 旧口径：同字节不同 MIME 撞同一个 object_id → 同一个 K_object → 同一个 nonce。
        let old_a = old_object_id(&root, &real);
        let old_b = old_object_id(&root, &real);
        assert_eq!(old_a, old_b, "旧口径只按 real_sha 必然撞 id");
        let key_old_a = derive_object_key(&root, ObjectType::Asset, &old_a).expect("old key a");
        let key_old_b = derive_object_key(&root, ObjectType::Asset, &old_b).expect("old key b");
        assert_eq!(
            asset_nonce(&key_old_a, &root.vault_id, &old_a, 0).expect("old nonce a"),
            asset_nonce(&key_old_b, &root.vault_id, &old_b, 0).expect("old nonce b"),
            "旧口径下不同 MIME 会复用同一 nonce（灾难性）"
        );
        // 但两个 MIME 的完整资产明文头不同——正是 §31 要修的 nonce 复用。
        let plain_a =
            crate::checkpoint::asset_plaintext(&real, data.len() as u64, "text/plain", data)
                .expect("plaintext a");
        let plain_b = crate::checkpoint::asset_plaintext(
            &real,
            data.len() as u64,
            "application/octet-stream",
            data,
        )
        .expect("plaintext b");
        assert_ne!(plain_a, plain_b);

        // §31 新口径：object_id 提交完整明文头 → 不同 id → 不同 nonce。
        let new_a =
            crate::checkpoint::asset_object_id(&root, &real, data.len() as u64, "text/plain")
                .expect("new id a");
        let new_b = crate::checkpoint::asset_object_id(
            &root,
            &real,
            data.len() as u64,
            "application/octet-stream",
        )
        .expect("new id b");
        assert_ne!(new_a, new_b, "§31：MIME 必须进入 object_id");
        let key_new_a = derive_object_key(&root, ObjectType::Asset, &new_a).expect("new key a");
        let key_new_b = derive_object_key(&root, ObjectType::Asset, &new_b).expect("new key b");
        assert_ne!(
            asset_nonce(&key_new_a, &root.vault_id, &new_a, 0).expect("new nonce a"),
            asset_nonce(&key_new_b, &root.vault_id, &new_b, 0).expect("new nonce b"),
            "不同明文头必须得到不同 nonce"
        );
    }
}
