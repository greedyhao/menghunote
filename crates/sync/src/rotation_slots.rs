//! 轮换 P1：候选 epoch 的新 KEY_SLOT、slot_set_digest 与全 active 设备 grants。
//!
//! 槽必须用**新秘密**（新 password / 新 `R'`），AAD 闭包与最终上传 header 逐字段一致；
//! 槽体 AAD 的 `message_id = slot_id[..16]`、长度取根 CDE 明文长度（与 genesis /
//! `recovery_enroll` 标准读取路径同口径）；KEY_SLOT 走 `K_slot_mne` 且带 `?reservation=`。

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::device::{DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use crate::error::SyncError;
use crate::kdf;
use crate::keys::RootSecretV1;
use crate::recovery::RecoverySeed;
use crate::relay::{FrozenAssetRef, RelayClient, SlotUpload};
use crate::rotation::ManagerTransitionV1;
use crate::rotation_journal::JournalBaseline;
use crate::rotation_stage::ReencryptedAsset;
use crate::slot::{
    context_hash, validate_slot_descriptor, KeySlotV1, PROTOCOL_VERSION,
    WRAP_SUITE_XCHACHA20_POLY1305,
};
use crate::{DeviceId, ObjectId, VaultId};
use menghunote_sync_proto::{
    activation_t_signing_input, part_count_for, ActivationTV1, Kdf, ObjectRefV1, ObjectType,
    SlotDescriptorV1, SlotKind,
};

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

/// 生成候选 password/recovery 槽并自检（恢复种子 `R'` 由本次随机生成）。
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
    generate_candidate_slots_with_recovery_seed(
        vault_id,
        epoch,
        new_password,
        manager_rev,
        created_at,
        device_id,
        candidate_root,
        None,
        rng,
    )
}

/// 同 [`generate_candidate_slots`]，但允许调用方**指定**恢复种子 `R'`。
///
/// 用途（ADR 0009 Phase 2 切片 6a）：dev-local 轮换不更换管理 key/恢复码。调用方若持有
/// 当前管理恢复码，就把它的种子传进来，让候选 epoch 的 recovery 槽**能被同一恢复码解开**
/// ——否则新设备在轮换后无法再用恢复码加入（随机 `R'` 没人知道）。
///
/// 是否允许该种子由调用方（轮换编排）决定：这里只负责按同一套 AAD/KEK 规则包槽并自检。
/// `None` = 沿用旧的"一次性随机 `R'`"语义（槽集合完整，但旧恢复码打不开）。
#[allow(clippy::too_many_arguments)]
pub fn generate_candidate_slots_with_recovery_seed(
    vault_id: VaultId,
    epoch: u64,
    new_password: &str,
    manager_rev: u64,
    created_at: u64,
    device_id: DeviceId,
    candidate_root: &RootSecretV1,
    recovery_seed: Option<&RecoverySeed>,
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
    // `Some` = 调用方指定的恢复种子（同一恢复码仍能解开候选根）；`None` = 一次性随机。
    let recovery_seed = match recovery_seed {
        Some(seed) => seed.clone(),
        None => RecoverySeed::generate(rng)?,
    };
    let recovery_code = recovery_seed.to_code()?;
    let root = candidate_root.clone();
    let aad = slot_aad(device_id);
    let password = KeySlotV1::wrap_password(
        &password_descriptor,
        new_password.as_bytes(),
        &root,
        &aad,
        rng,
    )?;
    let recovery =
        KeySlotV1::wrap_recovery(&recovery_descriptor, &recovery_seed, &root, &aad, rng)?;
    // 自检：新秘密必须各自解回同一候选根（AAD 与标准 reader 同口径）。
    if !password
        .unwrap_password(new_password.as_bytes(), &aad)?
        .ct_eq(&root)
        || !recovery.unwrap_recovery(&recovery_seed, &aad)?.ct_eq(&root)
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

/// 与 genesis / 标准 reader 逐字节一致的槽体 AAD 构造器。
///
/// 口径（`genesis::build_materials` 包槽体、`recovery_enroll::unwrap_recovery_slot`
/// 读回重建，两边相同）：
/// - `message_id = slot_id[..16]`（**不是**随机值，也不是 MNE 层的 message_id）；
/// - 长度字段取根 CDE 明文长度（`plaintext_len` / `plaintext_len + 16`）；
/// - 其余字段与 `upload::upload_object` 写 KEY_SLOT header 时一致。
///
/// 轮换产出的新槽将来要能被标准 reader 用新口令/新恢复码解回；这里任何字段漂移都会
/// 让新槽"写得进、读不回"。
fn slot_aad(
    device_id: DeviceId,
) -> impl Fn(&SlotDescriptorV1, &[u8; 24], usize) -> Result<Vec<u8>, SyncError> {
    move |descriptor, nonce, plaintext_len| {
        let mut message_id = [0u8; 16];
        message_id.copy_from_slice(&descriptor.slot_id[..16]);
        let header = menghunote_sync_proto::EnvelopeHeader {
            protocol_version: menghunote_sync_proto::PROTOCOL_VERSION,
            header_len: menghunote_sync_proto::HEADER_LEN as u16,
            object_type: ObjectType::KeySlot,
            cipher_suite: menghunote_sync_proto::CIPHER_SUITE_XCHACHA20_POLY1305,
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
                .map_err(|_| SyncError::Descriptor("槽根明文长度超出 u32"))?,
            ciphertext_len: u32::try_from(plaintext_len + 16)
                .map_err(|_| SyncError::Descriptor("槽密文长度超出 u32"))?,
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

/// 完整 activation T（ADR 0006 §34）各字段在**客户端**的逐项来源。
///
/// 这里不保存任何秘密：全部是公开标识/摘要。字段与 `ActivationTV1` 的 20 个整数键
/// 一一对应，`build_activation_t` 只做类型收窄与结构校验，不做网络/存储 IO。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationTMaterial {
    pub vault_id: VaultId,
    pub instance_id: [u8; 16],
    pub rotation_id: [u8; 16],
    pub from_epoch: u64,
    pub to_epoch: u64,
    /// 冻结截止 S（= `rotation.freeze_seq`）。
    pub freeze_seq: u64,
    pub old_manager_rev: u64,
    pub new_manager_rev: u64,
    pub new_manager_public: [u8; 32],
    /// 激活前控制链链尾 `record_hash`（relay `/control` 已验证链的最后一跳）。
    pub prev_control_hash: [u8; 32],
    /// **激活后**控制链 rev（= 当前链尾 rev + 1；relay `bind_activation_t` 的 CAS 口径）。
    pub expected_control_rev: u64,
    pub primary_checkpoint: [u8; 16],
    /// `SHA256(CDE(CheckpointCommitV1))`，即 relay 的 `signed_commit` 摘要。
    pub primary_commit_digest: [u8; 32],
    pub fallback_checkpoint: [u8; 16],
    pub fallback_commit_digest: [u8; 32],
    /// 候选 epoch staging 槽集合摘要（`slot_set_digest` 口径）。
    pub slot_set_digest: [u8; 32],
    /// 控制链 active 成员证书集合摘要（`member_cert_digest` 口径）。
    pub member_cert_digest: [u8; 32],
    /// 候选 epoch 完整 grants 包摘要（`grant_package_digest` 口径）。
    pub grants_digest: [u8; 32],
    /// 旧 epoch 冻结资产集合摘要（relay frozen `asset_set_digest`）。
    pub old_asset_set_digest: [u8; 32],
    /// 候选 epoch 重加密资产集合摘要（relay `candidate_asset_set_digest` 口径）。
    pub new_asset_set_digest: [u8; 32],
}

/// ADR 0006 §34：从本地材料构造完整的 20 字段 activation T。
///
/// 只做字段装配与 `ActivationTV1::validate`（epoch/rev 单调、非全零、SQLite 上界）；
/// 与 relay 事实的逐项比对由 relay `bind_activation_t` 与客户端 `/control` 复算共同完成。
pub fn build_activation_t(material: &ActivationTMaterial) -> Result<ActivationTV1, SyncError> {
    let old_manager_rev = u32::try_from(material.old_manager_rev)
        .map_err(|_| SyncError::Object("activation T: old_manager_rev 超出 u32"))?;
    let new_manager_rev = u32::try_from(material.new_manager_rev)
        .map_err(|_| SyncError::Object("activation T: new_manager_rev 超出 u32"))?;
    let record = ActivationTV1 {
        vault_id: material.vault_id,
        instance_id: material.instance_id,
        rotation_id: material.rotation_id,
        from_epoch: material.from_epoch,
        to_epoch: material.to_epoch,
        freeze_seq: material.freeze_seq,
        old_manager_rev,
        new_manager_rev,
        new_manager_pubkey: material.new_manager_public,
        prev_control_hash: material.prev_control_hash,
        expected_control_rev: material.expected_control_rev,
        primary_cid: material.primary_checkpoint,
        primary_commit_digest: material.primary_commit_digest,
        fallback_cid: material.fallback_checkpoint,
        fallback_commit_digest: material.fallback_commit_digest,
        slot_set_digest: material.slot_set_digest,
        member_cert_digest: material.member_cert_digest,
        grants_digest: material.grants_digest,
        old_asset_set_digest: material.old_asset_set_digest,
        new_asset_set_digest: material.new_asset_set_digest,
    };
    record.validate()?;
    Ok(record)
}

/// 阶段 G：构造 9 字段 manager transition + 完整 activation T，并让旧/新管理 key
/// 分别对 `transition` 与 `MN/v1/activation-t\0 || CDE(T)` 各签一次。
///
/// `NewManagerV1` 的 3–5 字段（T + 旧/新 T 双签）永远一起填充；activate 路径不得再传
/// `None`（relay 对缺 T 的请求一律 409）。
#[allow(clippy::too_many_arguments)]
pub fn build_manager_transition(
    material: &ActivationTMaterial,
    old_manager: &ManagementKey,
    new_manager: &ManagementKey,
) -> Result<menghunote_sync_proto::NewManagerV1, SyncError> {
    let transition = ManagerTransitionV1 {
        vault_id: material.vault_id,
        instance_id: material.instance_id,
        rotation_id: material.rotation_id,
        from_epoch: material.from_epoch,
        to_epoch: material.to_epoch,
        old_rev: material.old_manager_rev,
        new_rev: material.new_manager_rev,
        new_pubkey: material.new_manager_public,
        covered_seq: material.freeze_seq,
    }
    .encode()?;
    let activation_t = build_activation_t(material)?;
    let t_signing_input = activation_t_signing_input(&activation_t)?;
    Ok(menghunote_sync_proto::NewManagerV1 {
        old_signature: Some(old_manager.sign(&transition).to_vec()),
        new_signature: Some(new_manager.sign(&transition).to_vec()),
        transition,
        activation_t: Some(activation_t.encode()?),
        old_t_signature: Some(old_manager.sign(&t_signing_input).to_vec()),
        new_t_signature: Some(new_manager.sign(&t_signing_input).to_vec()),
    })
}

/// ADR §34：`primary_commit_digest` / `fallback_commit_digest` 的口径。
///
/// `SHA256(CDE(CheckpointCommit))`，与 relay 持久化的 `signed_commit` 字节逐字节一致。
/// task-122 起 commit 为 v2（额外绑定 `validation_digest`），因此摘要**只能**取
/// promote 实际签名时落下的 `report.commit_digest`；不得再用公开字段另拼一份 commit。
pub fn checkpoint_commit_digest(
    report: &crate::checkpoint::CheckpointReport,
) -> Result<[u8; 32], SyncError> {
    report.commit_digest.ok_or(SyncError::Object(
        "checkpoint report 缺 promote 后的 commit 摘要（基线尚未发布）",
    ))
}

/// relay 存下的 `signed_commit` 字节的摘要（恢复路径拿不到 `PreparedCheckpoint` 时用）。
#[must_use]
pub fn signed_commit_digest(signed_commit: &[u8]) -> [u8; 32] {
    Sha256::digest(signed_commit).into()
}

/// relay `objects::candidate_asset_set_digest` 同口径：
/// `SHA256(CDE(sorted ObjectRefV1[type=ASSET]))`，取**候选 epoch**本次 rotation 已
/// complete 的重加密资产集合（`ReencryptedAsset` 的全部字段足以重建 `ObjectRefV1`）。
pub fn candidate_asset_set_digest(
    epoch: u64,
    assets: &[ReencryptedAsset],
) -> Result<[u8; 32], SyncError> {
    let mut refs = Vec::with_capacity(assets.len());
    for asset in assets {
        if asset.media_type.is_empty() || asset.media_type.len() > 255 {
            return Err(SyncError::Object("候选资产 MIME 长度非法"));
        }
        let total_plain_len = 42u64
            .checked_add(asset.media_type.len() as u64)
            .and_then(|value| value.checked_add(asset.byte_len))
            .ok_or(SyncError::Object("候选资产明文长度溢出"))?;
        let part_count = u32::try_from(part_count_for(total_plain_len, ObjectType::Asset))
            .map_err(|_| SyncError::Object("候选资产 part_count 超出 u32"))?;
        refs.push(ObjectRefV1 {
            object_id: asset.object_id,
            object_type: ObjectType::Asset,
            generation: 0,
            key_epoch: epoch,
            part_count,
            total_plain_len,
            object_digest: asset.object_digest,
        });
    }
    let input = menghunote_sync_proto::refs_digest_input(&refs)?;
    Ok(Sha256::digest(&input).into())
}

/// relay `objects::asset_set_digest` / 冻结源 `source_digest` 同口径：
/// `SHA256(CDE(sorted ObjectRefV1[type=ASSET]))`，取旧 epoch 冻结时点的完整 finalized 集合。
pub fn frozen_asset_set_digest(refs: &[FrozenAssetRef]) -> Result<[u8; 32], SyncError> {
    let mut object_refs = Vec::with_capacity(refs.len());
    for reference in refs {
        object_refs.push(ObjectRefV1 {
            object_id: reference.object_id,
            object_type: ObjectType::Asset,
            generation: reference.generation,
            key_epoch: reference.key_epoch,
            part_count: reference.part_count,
            total_plain_len: reference.total_plain_len,
            object_digest: reference.object_digest,
        });
    }
    let input = menghunote_sync_proto::refs_digest_input(&object_refs)?;
    Ok(Sha256::digest(&input).into())
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
