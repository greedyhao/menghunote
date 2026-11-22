//! 轮换 P1：候选 epoch 的新 KEY_SLOT、slot_set_digest 与全 active 设备 grants。
//!
//! 槽必须用**新秘密**（新 password / 新 `R'`），AAD 闭包与最终上传 header 逐字段一致；
//! KEY_SLOT 走 `K_slot_mne` 且带 `?reservation=`。

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::device::{DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use crate::error::SyncError;
use crate::kdf;
use crate::keys::RootSecretV1;
use crate::recovery::RecoverySeed;
use crate::relay::{RelayClient, SlotUpload};
use crate::rotation::ManagerTransitionV1;
use crate::rotation_journal::JournalBaseline;
use crate::slot::{
    context_hash, validate_slot_descriptor, KeySlotV1, PROTOCOL_VERSION,
    WRAP_SUITE_XCHACHA20_POLY1305,
};
use crate::{DeviceId, ObjectId, VaultId};
use menghunote_sync_proto::ObjectType;
use menghunote_sync_proto::{Kdf, SlotDescriptorV1, SlotKind};

/// 候选槽集合（新秘密）。
pub struct CandidateSlots {
    pub password: KeySlotV1,
    pub recovery: KeySlotV1,
    /// 新恢复种子 R'（**不是**旧 R）。
    pub recovery_seed: RecoverySeed,
    /// 展示给用户的恢复码（由 R' 编码）。
    pub recovery_code: Zeroizing<String>,
    /// 由 R' 派生的新管理 key。
    pub new_manager: ManagementKey,
    pub new_manager_public: [u8; 32],
}

/// 生成候选 password/recovery 槽并自检。
#[allow(clippy::too_many_arguments)]
pub fn generate_candidate_slots(
    vault_id: VaultId,
    epoch: u64,
    new_password: &str,
    manager_rev: u64,
    created_at: u64,
    device_id: DeviceId,
    candidate_root: &RootSecretV1,
    rng: &mut impl crate::random::RandomSource,
) -> Result<CandidateSlots, SyncError> {
    if new_password.is_empty() {
        return Err(SyncError::PasswordLength {
            actual: 0,
            min: 1,
            max: 1024,
        });
    }
    let password_slot_id: ObjectId = crate::random::array(rng)?;
    let recovery_slot_id: ObjectId = crate::random::array(rng)?;
    let password_descriptor = SlotDescriptorV1 {
        protocol_version: PROTOCOL_VERSION,
        slot_id: password_slot_id,
        vault_id,
        key_epoch: epoch,
        kind: SlotKind::Password,
        wrap_suite: WRAP_SUITE_XCHACHA20_POLY1305,
        kdf: Kdf::Argon2id(kdf::new_writer_params(rng)?),
        created_at,
        manager_rev,
    };
    let recovery_descriptor = SlotDescriptorV1 {
        protocol_version: PROTOCOL_VERSION,
        slot_id: recovery_slot_id,
        vault_id,
        key_epoch: epoch,
        kind: SlotKind::Recovery,
        wrap_suite: WRAP_SUITE_XCHACHA20_POLY1305,
        kdf: Kdf::RecoveryHkdf,
        created_at,
        manager_rev,
    };
    validate_slot_descriptor(&password_descriptor)?;
    validate_slot_descriptor(&recovery_descriptor)?;
    let recovery_seed = RecoverySeed::generate(rng)?;
    let recovery_code = recovery_seed.to_code()?;
    let root = candidate_root.clone();
    let password_message = crate::random::array::<16>(rng)?;
    let password = KeySlotV1::wrap_password(
        &password_descriptor,
        new_password.as_bytes(),
        &root,
        &slot_aad(vault_id, device_id, password_message),
        rng,
    )?;
    let recovery_message = crate::random::array::<16>(rng)?;
    let recovery = KeySlotV1::wrap_recovery(
        &recovery_descriptor,
        &recovery_seed,
        &root,
        &slot_aad(vault_id, device_id, recovery_message),
        rng,
    )?;
    // 自检：新秘密必须各自解回同一候选根。
    if !password
        .unwrap_password(
            new_password.as_bytes(),
            &slot_aad(vault_id, device_id, password_message),
        )?
        .ct_eq(&root)
        || !recovery
            .unwrap_recovery(
                &recovery_seed,
                &slot_aad(vault_id, device_id, recovery_message),
            )?
            .ct_eq(&root)
    {
        return Err(SyncError::Unwrap);
    }
    let new_manager = ManagementKey::from_recovery(&recovery_seed, &vault_id)?;
    let new_manager_public = new_manager.public_key();
    Ok(CandidateSlots {
        password,
        recovery,
        recovery_seed,
        recovery_code,
        new_manager,
        new_manager_public,
    })
}

/// 与 `upload_slot_with_reservation` 写入 header 完全一致的 AAD 构造器。
fn slot_aad(
    vault_id: VaultId,
    device_id: DeviceId,
    message_id: [u8; 16],
) -> impl Fn(&SlotDescriptorV1, &[u8; 24], usize) -> Result<Vec<u8>, SyncError> {
    move |descriptor, nonce, plaintext_len| {
        let header = menghunote_sync_proto::EnvelopeHeader {
            protocol_version: menghunote_sync_proto::PROTOCOL_VERSION,
            header_len: menghunote_sync_proto::HEADER_LEN as u16,
            object_type: ObjectType::KeySlot,
            cipher_suite: menghunote_sync_proto::CIPHER_SUITE_XCHACHA20_POLY1305,
            flags: 0,
            vault_id,
            object_id: descriptor.slot_id,
            message_id,
            producer_device_id: device_id,
            generation: 0,
            key_epoch: descriptor.key_epoch,
            sender_seq: 0,
            part_index: 0,
            part_count: 1,
            total_plaintext_len: plaintext_len as u64,
            this_plaintext_len: plaintext_len as u32,
            ciphertext_len: (plaintext_len + 16) as u32,
            context_hash: context_hash(descriptor)?,
            nonce: *nonce,
        };
        Ok(header.aad(&descriptor.encode()?)?)
    }
}

/// 上传一个候选 KEY_SLOT（K_slot_mne + 维护 reservation）。
#[allow(clippy::too_many_arguments)]
pub fn upload_slot_with_reservation<C: RelayClient>(
    client: &mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    root: &RootSecretV1,
    slot: &KeySlotV1,
    password: Option<&str>,
    recovery_seed: Option<&RecoverySeed>,
    message_id: [u8; 16],
    reservation: &[u8; 16],
) -> Result<(), SyncError> {
    let descriptor = slot.descriptor.encode()?;
    let key = slot.mne_key(password.map(str::as_bytes), recovery_seed)?;
    crate::upload::upload_object_with_key_and_reservation(
        client,
        vault_id,
        device_id,
        signing_key,
        root,
        ObjectType::KeySlot,
        slot.descriptor.slot_id,
        message_id,
        0,
        0,
        &slot.wrapped_bytes(),
        &descriptor,
        Some(slot.wrapped.nonce),
        &key,
        reservation,
    )?;
    let signature = signing_key.sign(&descriptor);
    client.put_key_slot(SlotUpload {
        slot_id: slot.descriptor.slot_id,
        epoch: slot.descriptor.key_epoch,
        kind: slot.descriptor.kind,
        descriptor: descriptor.clone(),
        signed_message: descriptor,
        signature,
    })?;
    Ok(())
}

/// 上传候选 password/recovery 两个槽（各自独立 message_id）。
#[allow(clippy::too_many_arguments)]
pub fn upload_candidate_slots<C: RelayClient>(
    client: &mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    root: &RootSecretV1,
    slots: &CandidateSlots,
    password: &str,
    reservation: &[u8; 16],
) -> Result<[u8; 32], SyncError> {
    let password_message: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
    upload_slot_with_reservation(
        client,
        vault_id,
        device_id,
        signing_key,
        root,
        &slots.password,
        Some(password),
        None,
        password_message,
        reservation,
    )?;
    let recovery_message: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
    upload_slot_with_reservation(
        client,
        vault_id,
        device_id,
        signing_key,
        root,
        &slots.recovery,
        None,
        Some(&slots.recovery_seed),
        recovery_message,
        reservation,
    )?;
    slot_set_digest(&[
        slots.password.descriptor.clone(),
        slots.recovery.descriptor.clone(),
    ])
}

/// `SHA256(concat(sorted(slot_id32 || SHA256(descriptor))))`。
pub fn slot_set_digest(descriptors: &[SlotDescriptorV1]) -> Result<[u8; 32], SyncError> {
    let mut ordered = descriptors
        .iter()
        .map(|descriptor| {
            let descriptor_hash: [u8; 32] = Sha256::digest(descriptor.encode()?).into();
            Ok((descriptor.slot_id, descriptor_hash))
        })
        .collect::<Result<Vec<_>, SyncError>>()?;
    ordered.sort_by_key(|(slot_id, _)| *slot_id);
    let mut input = Vec::with_capacity(ordered.len() * 64);
    for (slot_id, descriptor_hash) in ordered {
        input.extend_from_slice(&slot_id);
        input.extend_from_slice(&descriptor_hash);
    }
    Ok(Sha256::digest(&input).into())
}

/// 阶段 G：构造 9 字段 manager transition 并让旧/新管理 key 双签同一 CDE。
#[allow(clippy::too_many_arguments)]
pub fn build_manager_transition(
    vault_id: VaultId,
    instance_id: [u8; 16],
    rotation_id: [u8; 16],
    from_epoch: u64,
    to_epoch: u64,
    old_manager_rev: u64,
    new_manager_rev: u64,
    new_manager_public: [u8; 32],
    freeze_seq: u64,
    old_manager: &ManagementKey,
    new_manager: &ManagementKey,
) -> Result<menghunote_sync_proto::NewManagerV1, SyncError> {
    let transition = ManagerTransitionV1 {
        vault_id,
        instance_id,
        rotation_id,
        from_epoch,
        to_epoch,
        old_rev: old_manager_rev,
        new_rev: new_manager_rev,
        new_pubkey: new_manager_public,
        covered_seq: freeze_seq,
    }
    .encode()?;
    Ok(menghunote_sync_proto::NewManagerV1 {
        old_signature: Some(old_manager.sign(&transition).to_vec()),
        new_signature: Some(new_manager.sign(&transition).to_vec()),
        transition,
    })
}

/// 候选基线/槽绑定记录（journal 持久化用）。
#[derive(Debug, Clone)]
pub struct CandidateSlotRecord {
    pub slot_id: ObjectId,
    pub kind: SlotKind,
    pub descriptor: Vec<u8>,
    pub baseline: Option<JournalBaseline>,
}

/// relay 口径的 grants_digest：`SHA256(concat(sorted(device_id || SHA256(signed_header))))`。
pub fn grants_digest(leaves: &[crate::rotation_stage::GrantLeaf]) -> Result<[u8; 32], SyncError> {
    let mut ordered = leaves
        .iter()
        .map(|leaf| {
            let header_hash: [u8; 32] = Sha256::digest(&leaf.signed_header).into();
            (leaf.device_id, header_hash)
        })
        .collect::<Vec<_>>();
    ordered.sort_by_key(|(device_id, _)| *device_id);
    let mut input = Vec::with_capacity(ordered.len() * 48);
    for (device_id, header_hash) in ordered {
        input.extend_from_slice(&device_id);
        input.extend_from_slice(&header_hash);
    }
    Ok(Sha256::digest(&input).into())
}

/// 从已验证的 `/control` 缓存取全部 active 设备 HPKE 公钥（不信任裸 relay 列表）。
pub fn active_member_hpke_keys<C: RelayClient>(
    client: &mut C,
) -> Result<Vec<(DeviceId, [u8; 32])>, SyncError> {
    client.active_member_hpke_keys()
}

/// 对所有 active 设备（含本机）封 candidate root，生成 grant leaves。
pub fn plan_all_active_grants<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    manager: &ManagementKey,
    manager_rev: u64,
) -> Result<Vec<crate::rotation_stage::GrantLeaf>, SyncError> {
    let recipients = active_member_hpke_keys(client)?;
    if recipients.is_empty() {
        return Err(SyncError::Unsupported("控制链没有 active 设备证书"));
    }
    crate::rotation_stage::plan_grants(root, manager, manager_rev, &recipients)
}

/// 自检：恢复码必须能从 R' 重新解析。
pub fn verify_recovery_code(slots: &CandidateSlots) -> Result<(), SyncError> {
    let parsed = RecoverySeed::from_code(slots.recovery_code.as_str())?;
    if parsed.expose() != slots.recovery_seed.expose() {
        return Err(SyncError::Object("新恢复码与 R' 不一致"));
    }
    Ok(())
}

/// 自检：新管理 key 必须由 R' 派生。
pub fn verify_new_manager(slots: &CandidateSlots, vault_id: VaultId) -> Result<(), SyncError> {
    let derived = ManagementKey::from_recovery(&slots.recovery_seed, &vault_id)?;
    if derived.public_key() != slots.new_manager_public {
        return Err(SyncError::Object("新管理公钥不是由 R' 派生"));
    }
    Ok(())
}

/// 兼容：让调用方拿到 HPKE 公钥（本机）。
pub fn local_hpke(hpke: &DeviceHpkeKey) -> [u8; 32] {
    hpke.public_key()
}
