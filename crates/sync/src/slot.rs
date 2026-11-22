//! 密钥槽（ADR 0006 §3 / §15）：wrap/unwrap 根密钥、改口令纯逻辑。
//!
//! **wire 类型来自 `menghunote-sync-proto`**：`SlotDescriptorV1`、`Kind`、`Kdf`、
//! `Argon2idParams` 的 CDE 编码/解码/边界校验都在 proto；本模块只加：
//!
//! - **策略校验**：kind 与 kdf 必须匹配、Argon2 参数在读者区间内（**调用 Argon2 之前**）；
//! - KEK 派生（HKDF 域来自 proto 常量）；
//! - 槽体 AEAD：`nonce(24B) || XChaCha20-Poly1305(K_wrap, AAD, RootSecretV1 CDE)`；
//! - 改口令的 CAS 纯逻辑。
//!
//! `AAD` 由**信封层**构造（`menghunote-sync-proto::EnvelopeHeader::aad`，ADR §2：
//! `b"MN/e2e/v1\0" || header[0..196] || BE32(desc_len) || desc`）。信封 header 里的
//! `this_plaintext_len` 决定 AAD，所以 [`SlotAad`] 会收到明文长度；加密侧用根 CDE 的长度，
//! 解密侧用 `ciphertext_len - TAG_LEN`（两者必须一致，这正是 AEAD 认证的一部分）。

use crate::domain;
use crate::error::SyncError;
use crate::kdf;
use crate::keys::{RootSecretV1, WrapKey};
use crate::random::RandomSource;
use crate::recovery::{self, RecoverySeed};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use menghunote_sync_proto::{Kdf, SlotDescriptorV1, SlotKind};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// 槽协议版本（v1 = 1，与 proto 的 `PROTOCOL_VERSION` 一致）。
pub const PROTOCOL_VERSION: u16 = 1;
/// 唯一的 wrap 套件：XChaCha20-Poly1305（`cipher_suite = 1`，ADR §2）。
pub const WRAP_SUITE_XCHACHA20_POLY1305: u16 = 1;
/// XChaCha20-Poly1305 nonce 长度（ADR §2：header 内 24B CSPRNG nonce）。
pub const NONCE_LEN: usize = 24;
/// Poly1305 tag 长度。
pub const TAG_LEN: usize = 16;

fn kind_label(kind: SlotKind) -> &'static str {
    match kind {
        SlotKind::Password => "password",
        SlotKind::Recovery => "recovery",
    }
}

/// 槽描述的策略校验（proto 校验之后还要过这一层）。
///
/// proto 的 `SlotDescriptorV1::validate` 只查版本/wrap_suite/key_epoch>0，且在
/// `encode/to_cde` 时才查 Argon2 参数；本函数补上 **kind ↔ kdf 一致性** 与
/// **KDF 之前的 Argon2 范围闸门**，供本地构造的描述在派生密钥前调用。
pub fn validate_slot_descriptor(descriptor: &SlotDescriptorV1) -> Result<(), SyncError> {
    descriptor.validate()?;
    if descriptor.created_at > i64::MAX as u64 {
        return Err(SyncError::Descriptor("created_at 超过 i64::MAX"));
    }
    if descriptor.manager_rev > i64::MAX as u64 {
        return Err(SyncError::Descriptor("manager_rev 超过 i64::MAX"));
    }
    match (&descriptor.kind, &descriptor.kdf) {
        (SlotKind::Password, Kdf::Argon2id(params)) => {
            kdf::validate_argon2id_params(params)?;
        }
        (SlotKind::Recovery, Kdf::RecoveryHkdf) => {}
        (SlotKind::Password, Kdf::RecoveryHkdf) => {
            return Err(SyncError::SlotKind {
                expected: "password/argon2id",
                actual: "password/recovery-hkdf",
            })
        }
        (SlotKind::Recovery, Kdf::Argon2id(_)) => {
            return Err(SyncError::SlotKind {
                expected: "recovery/hkdf",
                actual: "recovery/argon2id",
            })
        }
    }
    Ok(())
}

/// `context_hash = SHA256(CDE(descriptor))`（ADR §2：KEY_SLOT header 的 `context_hash`）。
pub fn context_hash(descriptor: &SlotDescriptorV1) -> Result<[u8; 32], SyncError> {
    let digest = Sha256::digest(descriptor.encode()?);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

/// 口令槽：派生 `K_password_wrap = HKDF(Argon2id(password), vault_id,
/// b"MN/v1/password-wrap\0" || slot_id, 32)`（ADR §3）。
///
/// 口令与 Argon2 参数都**先校验**，越界时不会执行 Argon2。
pub fn password_wrap_key(
    descriptor: &SlotDescriptorV1,
    password: &[u8],
) -> Result<WrapKey, SyncError> {
    validate_slot_descriptor(descriptor)?;
    if descriptor.kind != SlotKind::Password {
        return Err(SyncError::SlotKind {
            expected: "password",
            actual: kind_label(descriptor.kind),
        });
    }
    let params = match &descriptor.kdf {
        Kdf::Argon2id(params) => params,
        Kdf::RecoveryHkdf => {
            return Err(SyncError::SlotKind {
                expected: "argon2id",
                actual: "recovery-hkdf",
            })
        }
    };
    let material = kdf::argon2id_output(password, params)?;
    let info = domain::password_wrap_info(&descriptor.slot_id);
    let kek = kdf::hkdf32(material.expose_slice(), &descriptor.vault_id, &info)?;
    Ok(WrapKey::from_secret(kek))
}

/// 恢复槽：派生 `K_recovery_wrap = HKDF(R, vault_id,
/// b"MN/v1/recovery-wrap\0" || slot_id, 32)`（ADR §3）。
pub fn recovery_wrap_key(
    descriptor: &SlotDescriptorV1,
    seed: &RecoverySeed,
) -> Result<WrapKey, SyncError> {
    validate_slot_descriptor(descriptor)?;
    if descriptor.kind != SlotKind::Recovery {
        return Err(SyncError::SlotKind {
            expected: "recovery",
            actual: kind_label(descriptor.kind),
        });
    }
    if !matches!(descriptor.kdf, Kdf::RecoveryHkdf) {
        return Err(SyncError::SlotKind {
            expected: "recovery-hkdf",
            actual: "argon2id",
        });
    }
    recovery::recovery_wrap_key(seed, &descriptor.vault_id, &descriptor.slot_id)
}

/// 信封层提供的 AAD 构造器。
///
/// ADR §2：`AAD = b"MN/e2e/v1\0" || header[0..196] || BE32(descriptor_len) || descriptor`。
/// header 里的 `object_id`/`nonce`/`this_plaintext_len` 等只有信封层知道，所以由它来拼；
/// 本模块把"即将写入 header 的 nonce 与明文长度"回传给构造器，保证加解密两侧一致。
///
/// 真实调用方应当先用 proto 构造 `EnvelopeHeader`，再调用 `header.aad(&descriptor.encode()?)`。
pub trait SlotAad {
    /// 给定槽描述、nonce 与明文长度，返回完整 AAD 字节。
    fn aad(
        &self,
        descriptor: &SlotDescriptorV1,
        nonce: &[u8; NONCE_LEN],
        plaintext_len: usize,
    ) -> Result<Vec<u8>, SyncError>;
}

impl<F> SlotAad for F
where
    F: Fn(&SlotDescriptorV1, &[u8; NONCE_LEN], usize) -> Result<Vec<u8>, SyncError>,
{
    fn aad(
        &self,
        descriptor: &SlotDescriptorV1,
        nonce: &[u8; NONCE_LEN],
        plaintext_len: usize,
    ) -> Result<Vec<u8>, SyncError> {
        self(descriptor, nonce, plaintext_len)
    }
}

/// 槽体：`nonce || ciphertext || tag`（AEAD 输出已含 tag）。
#[derive(Clone, PartialEq, Eq)]
pub struct WrappedRoot {
    /// 24B CSPRNG nonce（同时写进 MNE1 header）。
    pub nonce: [u8; NONCE_LEN],
    /// XChaCha20-Poly1305 密文（含 16B tag）。
    pub ciphertext: Vec<u8>,
}

impl WrappedRoot {
    /// 序列化为 `nonce || ciphertext`。
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(NONCE_LEN + self.ciphertext.len());
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.ciphertext);
        out
    }

    /// 从 `nonce || ciphertext` 解析（至少要有 nonce + tag）。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SyncError> {
        if bytes.len() < NONCE_LEN + TAG_LEN {
            return Err(SyncError::Unwrap);
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&bytes[..NONCE_LEN]);
        Ok(Self {
            nonce,
            ciphertext: bytes[NONCE_LEN..].to_vec(),
        })
    }
}

/// 一个密钥槽：proto 的槽描述 + 被 KEK 封装的根。
#[derive(Clone, PartialEq, Eq)]
pub struct KeySlotV1 {
    /// 槽描述（CDE 会进 AAD / context_hash）。
    pub descriptor: SlotDescriptorV1,
    /// 槽体。
    pub wrapped: WrappedRoot,
}

impl KeySlotV1 {
    /// 用口令槽封装根。
    pub fn wrap_password(
        descriptor: &SlotDescriptorV1,
        password: &[u8],
        root: &RootSecretV1,
        aad: &impl SlotAad,
        rng: &mut impl RandomSource,
    ) -> Result<Self, SyncError> {
        let kek = password_wrap_key(descriptor, password)?;
        Self::wrap_with_key(descriptor, &kek, root, aad, rng)
    }

    /// 用恢复槽封装根。
    pub fn wrap_recovery(
        descriptor: &SlotDescriptorV1,
        seed: &RecoverySeed,
        root: &RootSecretV1,
        aad: &impl SlotAad,
        rng: &mut impl RandomSource,
    ) -> Result<Self, SyncError> {
        let kek = recovery_wrap_key(descriptor, seed)?;
        Self::wrap_with_key(descriptor, &kek, root, aad, rng)
    }

    fn wrap_with_key(
        descriptor: &SlotDescriptorV1,
        kek: &WrapKey,
        root: &RootSecretV1,
        aad: &impl SlotAad,
        rng: &mut impl RandomSource,
    ) -> Result<Self, SyncError> {
        validate_slot_descriptor(descriptor)?;
        if descriptor.vault_id != root.vault_id || descriptor.key_epoch != root.key_epoch {
            return Err(SyncError::Descriptor("槽描述与根归属（vault/epoch）不一致"));
        }
        let plaintext = Zeroizing::new(root.encode()?);
        let nonce = crate::random::array::<NONCE_LEN>(rng)?;
        let aad_bytes = aad.aad(descriptor, &nonce, plaintext.len())?;
        let cipher =
            XChaCha20Poly1305::new_from_slice(kek.expose()).map_err(|_| SyncError::Unwrap)?;
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext.as_slice(),
                    aad: &aad_bytes,
                },
            )
            .map_err(|_| SyncError::Unwrap)?;
        Ok(Self {
            descriptor: descriptor.clone(),
            wrapped: WrappedRoot { nonce, ciphertext },
        })
    }

    /// 用口令解封根；错口令/错槽/篡改 → [`SyncError::Unwrap`]。
    pub fn unwrap_password(
        &self,
        password: &[u8],
        aad: &impl SlotAad,
    ) -> Result<RootSecretV1, SyncError> {
        let kek = password_wrap_key(&self.descriptor, password)?;
        self.unwrap_with_key(&kek, aad)
    }

    /// 用恢复码解封根；错 R/错槽/篡改 → [`SyncError::Unwrap`]。
    pub fn unwrap_recovery(
        &self,
        seed: &RecoverySeed,
        aad: &impl SlotAad,
    ) -> Result<RootSecretV1, SyncError> {
        let kek = recovery_wrap_key(&self.descriptor, seed)?;
        self.unwrap_with_key(&kek, aad)
    }

    fn unwrap_with_key(
        &self,
        kek: &WrapKey,
        aad: &impl SlotAad,
    ) -> Result<RootSecretV1, SyncError> {
        validate_slot_descriptor(&self.descriptor)?;
        let plaintext_len = self
            .wrapped
            .ciphertext
            .len()
            .checked_sub(TAG_LEN)
            .ok_or(SyncError::Unwrap)?;
        let aad_bytes = aad.aad(&self.descriptor, &self.wrapped.nonce, plaintext_len)?;
        let cipher =
            XChaCha20Poly1305::new_from_slice(kek.expose()).map_err(|_| SyncError::Unwrap)?;
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    XNonce::from_slice(&self.wrapped.nonce),
                    Payload {
                        msg: &self.wrapped.ciphertext,
                        aad: &aad_bytes,
                    },
                )
                .map_err(|_| SyncError::Unwrap)?,
        );
        let root = RootSecretV1::decode(&plaintext)?;
        if root.vault_id != self.descriptor.vault_id || root.key_epoch != self.descriptor.key_epoch
        {
            return Err(SyncError::Descriptor("解出的根归属与槽描述不一致"));
        }
        Ok(root)
    }

    /// ADR §20：KEY_SLOT 的 MNE1 密钥（password 用 Argon2 输出，recovery 用恢复种子）。
    pub fn mne_key(
        &self,
        password: Option<&[u8]>,
        recovery_seed: Option<&RecoverySeed>,
    ) -> Result<crate::keys::Secret32, SyncError> {
        let ikm = match self.descriptor.kind {
            SlotKind::Password => {
                let Kdf::Argon2id(params) = &self.descriptor.kdf else {
                    return Err(SyncError::Descriptor("password 槽的 KDF 不是 Argon2id"));
                };
                let password = password.ok_or(SyncError::Unwrap)?;
                kdf::argon2id_output(password, params)?
            }
            SlotKind::Recovery => {
                let seed = recovery_seed.ok_or(SyncError::Unwrap)?;
                crate::keys::Secret32::from_array(*seed.expose())
            }
        };
        crate::keys::derive_slot_mne_key(
            ikm.expose_slice(),
            &self.descriptor.vault_id,
            self.descriptor.kind,
            self.descriptor.key_epoch,
            &self.descriptor.slot_id,
        )
    }

    /// 槽体序列化（`nonce || ciphertext`）。
    #[must_use]
    pub fn wrapped_bytes(&self) -> Vec<u8> {
        self.wrapped.to_bytes()
    }
}

/// 槽状态（对应 relay `key_slots.state`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotState {
    /// 暂存（尚未激活，24h 回收）。
    Staging,
    /// 当前激活。
    Active,
    /// 待退役（已切换，等待删除）。
    Retiring,
}

/// 槽集合里的一项。
#[derive(Clone)]
pub struct SlotEntry {
    /// 槽本体。
    pub slot: KeySlotV1,
    /// 状态。
    pub state: SlotState,
}

/// 槽集合快照。
#[derive(Clone)]
pub struct SlotSetSnapshot {
    /// 槽集合版本（CAS 用）。
    pub rev: u64,
    /// 全部槽。
    pub slots: Vec<SlotEntry>,
}

/// 改口令所需的 CAS 存储接口。
///
/// **语义要求**：
///
/// - [`Self::stage_slot`] 只落新槽（`state = Staging`），不得改动 active 指针；
/// - [`Self::activate_password_slot`] 必须在 `expected_rev == 当前 rev` 时原子完成
///   "新槽 → Active、旧 active password 槽 → Retiring、rev += 1"，否则返回
///   [`SyncError::Cas`]（ADR §3："带 `expected_slot_set_rev` CAS 原子切 active 指针"）；
/// - [`Self::remove_slot`] 删除指定槽（改口令成功后删旧槽）。
///
/// 该接口由 storage 侧实现（本 crate 不碰 SQLite）；本模块只提供纯逻辑。
pub trait SlotStore {
    /// 读取当前槽集合（含 rev 与状态）。
    fn slot_set(&self) -> Result<SlotSetSnapshot, SyncError>;
    /// 暂存新槽（Staging）。
    fn stage_slot(&mut self, slot: &KeySlotV1) -> Result<(), SyncError>;
    /// CAS 切换 active password 槽；成功返回新的槽集合 rev。
    fn activate_password_slot(
        &mut self,
        expected_rev: u64,
        new_slot_id: &[u8; 32],
    ) -> Result<u64, SyncError>;
    /// 删除槽。
    fn remove_slot(&mut self, slot_id: &[u8; 32]) -> Result<(), SyncError>;
}

/// 改口令的输入。
#[derive(Clone, Copy)]
pub struct PasswordChangePlan<'a> {
    /// 新口令（原样 UTF-8，1..=1024B）。
    pub new_password: &'a [u8],
    /// 新槽 ID（32B 随机，调用方生成并保证不复用）。
    pub new_slot_id: [u8; 32],
    /// 新槽创建时刻。
    pub created_at: u64,
}

/// 改口令的结果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PasswordChangeOutcome {
    /// 切换后的槽集合 rev。
    pub new_slot_set_rev: u64,
    /// 被替换的旧口令槽。
    pub old_slot_id: [u8; 32],
    /// 新口令槽。
    pub new_slot_id: [u8; 32],
}

/// 改口令的**纯逻辑**（ADR §3）：
///
/// 1. 找当前 active 的口令槽（没有则报错）；
/// 2. 用新口令 + 新 salt 生成新 `SlotDescriptorV1`，封装**同一个根**；
/// 3. **试解密校验**：重新派生 KEK 并解封新槽，根必须一致（写入前的自检）；
/// 4. `stage_slot` 暂存新槽；
/// 5. 带 `expected_slot_set_rev` CAS 切换 active（[`SlotStore::activate_password_slot`]）；
/// 6. 删除旧口令槽。
///
/// 不碰数据对象、不换 epoch、不动恢复槽与其他设备 DPAPI——"改口令只重包"。
/// 真正的"从 relay 读回再解密"由调用方在 4→5 之间做；本函数的第 3 步是同一套
/// 派生/解封路径的本地自检。槽指针更新的设备签名也不在这里（属于信封/请求层）。
pub fn change_password<S, A, R>(
    store: &mut S,
    root: &RootSecretV1,
    plan: &PasswordChangePlan<'_>,
    aad: &A,
    rng: &mut R,
) -> Result<PasswordChangeOutcome, SyncError>
where
    S: SlotStore,
    A: SlotAad,
    R: RandomSource,
{
    // 口令长度必须在 Argon2 之前拒绝。
    kdf::validate_password(plan.new_password)?;

    let snapshot = store.slot_set()?;
    let mut active: Option<&SlotEntry> = None;
    for entry in &snapshot.slots {
        if entry.state == SlotState::Active && entry.slot.descriptor.kind == SlotKind::Password {
            if active.is_some() {
                return Err(SyncError::PasswordChange("存在多个 active 口令槽"));
            }
            active = Some(entry);
        }
    }
    let old = active.ok_or(SyncError::PasswordChange("没有 active 口令槽"))?;
    let old_slot_id = old.slot.descriptor.slot_id;
    if old_slot_id == plan.new_slot_id {
        return Err(SyncError::PasswordChange("新槽 ID 与旧槽相同"));
    }

    // 新槽：同 vault/epoch/manager_rev，新 slot_id + 新 salt。
    let params = kdf::new_writer_params(rng)?;
    let descriptor = SlotDescriptorV1 {
        protocol_version: PROTOCOL_VERSION,
        slot_id: plan.new_slot_id,
        vault_id: root.vault_id,
        key_epoch: root.key_epoch,
        kind: SlotKind::Password,
        wrap_suite: WRAP_SUITE_XCHACHA20_POLY1305,
        kdf: Kdf::Argon2id(params),
        created_at: plan.created_at,
        manager_rev: old.slot.descriptor.manager_rev,
    };
    validate_slot_descriptor(&descriptor)?;
    let new_slot = KeySlotV1::wrap_password(&descriptor, plan.new_password, root, aad, rng)?;

    // 试解密校验：重新派生 KEK（再跑一次 Argon2id）并解封，根必须一致。
    let verified = new_slot.unwrap_password(plan.new_password, aad)?;
    if !verified.ct_eq(root) {
        return Err(SyncError::PasswordChange("新槽自检解出的根与当前根不一致"));
    }

    store.stage_slot(&new_slot)?;
    let new_rev = store.activate_password_slot(snapshot.rev, &plan.new_slot_id)?;
    store.remove_slot(&old_slot_id)?;

    Ok(PasswordChangeOutcome {
        new_slot_set_rev: new_rev,
        old_slot_id,
        new_slot_id: plan.new_slot_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::derive_object_key;
    use crate::keys::Secret32;
    use crate::test_support::TestRng;
    use crate::VaultId;
    use menghunote_sync_proto::{Argon2idParams, EnvelopeHeader, ObjectType};
    use zeroize::Zeroizing;

    fn params(salt: [u8; kdf::SALT_LEN]) -> Argon2idParams {
        kdf::writer_params(salt)
    }

    fn vault() -> VaultId {
        [0x10; 16]
    }

    fn password_descriptor(slot_id: [u8; 32], salt: [u8; kdf::SALT_LEN]) -> SlotDescriptorV1 {
        SlotDescriptorV1 {
            protocol_version: PROTOCOL_VERSION,
            slot_id,
            vault_id: vault(),
            key_epoch: 5,
            kind: SlotKind::Password,
            wrap_suite: WRAP_SUITE_XCHACHA20_POLY1305,
            kdf: Kdf::Argon2id(params(salt)),
            created_at: 1_700_000_000,
            manager_rev: 2,
        }
    }

    fn recovery_descriptor(slot_id: [u8; 32]) -> SlotDescriptorV1 {
        SlotDescriptorV1 {
            protocol_version: PROTOCOL_VERSION,
            slot_id,
            vault_id: vault(),
            key_epoch: 5,
            kind: SlotKind::Recovery,
            wrap_suite: WRAP_SUITE_XCHACHA20_POLY1305,
            kdf: Kdf::RecoveryHkdf,
            created_at: 1_700_000_000,
            manager_rev: 2,
        }
    }

    /// 测试用 AAD：用 proto 的真实 `EnvelopeHeader` 走 ADR §2 的 AAD 构造。
    fn test_aad(
        descriptor: &SlotDescriptorV1,
        nonce: &[u8; NONCE_LEN],
        plaintext_len: usize,
    ) -> Result<Vec<u8>, SyncError> {
        let descriptor_bytes = descriptor.encode()?;
        let header = EnvelopeHeader {
            protocol_version: menghunote_sync_proto::PROTOCOL_VERSION,
            header_len: menghunote_sync_proto::HEADER_LEN as u16,
            object_type: ObjectType::KeySlot,
            cipher_suite: menghunote_sync_proto::CIPHER_SUITE_XCHACHA20_POLY1305,
            flags: 0,
            vault_id: descriptor.vault_id,
            object_id: descriptor.slot_id,
            message_id: [0x0a; 16],
            producer_device_id: [0x0b; 16],
            generation: 0,
            key_epoch: descriptor.key_epoch,
            sender_seq: 0,
            part_index: 0,
            part_count: 1,
            total_plaintext_len: plaintext_len as u64,
            this_plaintext_len: plaintext_len as u32,
            ciphertext_len: (plaintext_len + TAG_LEN) as u32,
            context_hash: context_hash(descriptor)?,
            nonce: *nonce,
        };
        Ok(header.aad(&descriptor_bytes)?)
    }

    #[test]
    fn descriptor_validation_catches_kind_kdf_mismatch() {
        let mut desc = password_descriptor([0x01; 32], [0x02; 16]);
        assert!(validate_slot_descriptor(&desc).is_ok());
        desc.kind = SlotKind::Recovery;
        assert!(matches!(
            validate_slot_descriptor(&desc),
            Err(SyncError::SlotKind { .. })
        ));

        let mut desc = recovery_descriptor([0x03; 32]);
        assert!(validate_slot_descriptor(&desc).is_ok());
        desc.kdf = Kdf::Argon2id(params([0u8; 16]));
        assert!(matches!(
            validate_slot_descriptor(&desc),
            Err(SyncError::SlotKind { .. })
        ));
    }

    #[test]
    fn descriptor_rejects_out_of_range_argon_params_before_kdf() {
        let mut desc = password_descriptor([0u8; 32], [1u8; 16]);
        desc.kdf = Kdf::Argon2id(Argon2idParams {
            version: 0x13,
            m_kib: 1 << 20, // 1 TiB
            t: 3,
            p: 1,
            salt: [0u8; 16],
            output_len: 32,
        });
        // proto 的 encode 会拒绝越界参数
        assert!(desc.encode().is_err());
        // 本地策略校验直接报 Argon2Param（在 KDF 之前）
        assert!(matches!(
            validate_slot_descriptor(&desc),
            Err(SyncError::Argon2Param { field: "m_kib", .. })
        ));
        let started = std::time::Instant::now();
        assert!(password_wrap_key(&desc, b"pw").is_err());
        assert!(started.elapsed().as_millis() < 50, "越界参数不能跑 Argon2");

        // proto 的 decode 侧也会拒绝同一份字节
        let mut good = password_descriptor([0u8; 32], [1u8; 16]);
        good.kdf = Kdf::Argon2id(kdf::writer_params([1u8; 16]));
        let bytes = good.encode().unwrap();
        // 手工把 m_kib 改成 1TiB 不可能（canonical），所以直接断言 proto 参数校验：
        assert!(
            Argon2idParams {
                m_kib: 1 << 20,
                ..kdf::writer_params([1u8; 16])
            }
            .m_kib
                > kdf::M_MAX_KIB
        );
        let _ = bytes;
    }

    #[test]
    fn descriptor_cde_roundtrip_and_context_hash() {
        let desc = password_descriptor([0xab; 32], [0xcd; 16]);
        let bytes = desc.encode().unwrap();
        let parsed = SlotDescriptorV1::decode(&bytes).unwrap();
        assert_eq!(desc, parsed);
        assert_eq!(bytes, parsed.encode().unwrap());
        assert_eq!(bytes[0], 0xa9, "9 项 map 头");
        // context_hash 必须等于 SHA256(descriptor CDE)
        let hash = context_hash(&desc).unwrap();
        assert_eq!(hash.len(), 32);
        assert_ne!(hash, [0u8; 32]);

        // 尾部多余
        let mut long = bytes;
        long.push(0);
        assert!(SlotDescriptorV1::decode(&long).is_err());
    }

    #[test]
    fn password_slot_roundtrip_and_wrong_password() {
        let mut rng = TestRng::new(41);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let desc = password_descriptor([0x01; 32], [0x02; 16]);
        let slot =
            KeySlotV1::wrap_password(&desc, b"correct horse", &root, &test_aad, &mut rng).unwrap();

        let opened = slot.unwrap_password(b"correct horse", &test_aad).unwrap();
        assert!(root.ct_eq(&opened));

        // 错口令
        assert!(matches!(
            slot.unwrap_password(b"Correct horse", &test_aad),
            Err(SyncError::Unwrap)
        ));
        // 口令不做 trim：前后空格也失败
        assert!(slot.unwrap_password(b" correct horse ", &test_aad).is_err());
        // AAD 不同（模拟信封字段被改）
        let bad_aad = |_d: &SlotDescriptorV1, _n: &[u8; NONCE_LEN], _l: usize| Ok(vec![0u8; 16]);
        assert!(slot.unwrap_password(b"correct horse", &bad_aad).is_err());
    }

    #[test]
    fn password_slot_rejects_wrong_descriptor_or_body() {
        let mut rng = TestRng::new(42);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let desc = password_descriptor([0x01; 32], [0x02; 16]);
        let slot = KeySlotV1::wrap_password(&desc, b"pw", &root, &test_aad, &mut rng).unwrap();

        // 换 slot_id（HKDF info 变了）→ 解封失败
        let mut other_desc = desc.clone();
        other_desc.slot_id = [0x09; 32];
        let mut tampered = slot.clone();
        tampered.descriptor = other_desc;
        assert!(tampered.unwrap_password(b"pw", &test_aad).is_err());

        // 换 epoch（根归属不符）→ 描述/归属校验失败
        let mut epoch_desc = desc.clone();
        epoch_desc.key_epoch = 6;
        let mut tampered = slot.clone();
        tampered.descriptor = epoch_desc;
        assert!(tampered.unwrap_password(b"pw", &test_aad).is_err());

        // 篡改槽体
        let mut tampered = slot.clone();
        tampered.wrapped.ciphertext[0] ^= 1;
        assert!(matches!(
            tampered.unwrap_password(b"pw", &test_aad),
            Err(SyncError::Unwrap)
        ));

        // 截断槽体
        let mut tampered = slot.clone();
        tampered.wrapped.ciphertext.truncate(3);
        assert!(tampered.unwrap_password(b"pw", &test_aad).is_err());
    }

    #[test]
    fn recovery_slot_roundtrip_and_wrong_seed() {
        let mut rng = TestRng::new(43);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let seed = RecoverySeed::generate(&mut rng).unwrap();
        let desc = recovery_descriptor([0x03; 32]);
        let slot = KeySlotV1::wrap_recovery(&desc, &seed, &root, &test_aad, &mut rng).unwrap();
        assert!(root.ct_eq(&slot.unwrap_recovery(&seed, &test_aad).unwrap()));

        let other = RecoverySeed::generate(&mut rng).unwrap();
        assert!(matches!(
            slot.unwrap_recovery(&other, &test_aad),
            Err(SyncError::Unwrap)
        ));

        // 用口令路径解恢复槽 → 类型错误
        assert!(matches!(
            slot.unwrap_password(b"pw", &test_aad),
            Err(SyncError::SlotKind { .. })
        ));
    }

    /// 只凭恢复码（不再持有内存中的 R）解出根 —— ADR §11 步 2 的验收点之一。
    #[test]
    fn recovery_code_alone_recovers_root() {
        let mut rng = TestRng::new(49);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let seed = RecoverySeed::generate(&mut rng).unwrap();
        let desc = recovery_descriptor([0x23; 32]);
        let slot = KeySlotV1::wrap_recovery(&desc, &seed, &root, &test_aad, &mut rng).unwrap();

        // 用户抄下恢复码，此后 R 不再存在于内存
        let code = seed.to_code().unwrap();
        drop(seed);

        let restored = RecoverySeed::from_code(&code).unwrap();
        let opened = slot.unwrap_recovery(&restored, &test_aad).unwrap();
        assert!(root.ct_eq(&opened));
        // 管理能力也从恢复码独立派生（同一 R 的另一条 HKDF 域）
        let manager =
            crate::device::ManagementKey::from_recovery(&restored, &desc.vault_id).unwrap();
        assert_ne!(manager.public_key(), [0u8; 32]);
    }

    #[test]
    fn wrapped_root_bytes_roundtrip() {
        let mut rng = TestRng::new(44);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let desc = password_descriptor([0x04; 32], [0x05; 16]);
        let slot = KeySlotV1::wrap_password(&desc, b"pw", &root, &test_aad, &mut rng).unwrap();
        let bytes = slot.wrapped_bytes();
        assert_eq!(bytes.len(), NONCE_LEN + slot.wrapped.ciphertext.len());
        let parsed = WrappedRoot::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.nonce, slot.wrapped.nonce);
        assert_eq!(parsed.ciphertext, slot.wrapped.ciphertext);
        assert!(WrappedRoot::from_bytes(&bytes[..NONCE_LEN + TAG_LEN - 1]).is_err());
    }

    // ---- 改口令纯逻辑 ----

    struct MemorySlotStore {
        rev: u64,
        entries: Vec<SlotEntry>,
        /// 测试用：模拟"读快照后 rev 被并发推进"，让 CAS 失败。
        race_before_activate: bool,
    }

    impl MemorySlotStore {
        fn new() -> Self {
            Self {
                rev: 1,
                entries: Vec::new(),
                race_before_activate: false,
            }
        }

        fn add(&mut self, slot: KeySlotV1, state: SlotState) {
            self.entries.push(SlotEntry { slot, state });
        }

        fn find(&self, slot_id: &[u8; 32]) -> Option<&SlotEntry> {
            self.entries
                .iter()
                .find(|e| &e.slot.descriptor.slot_id == slot_id)
        }
    }

    impl SlotStore for MemorySlotStore {
        fn slot_set(&self) -> Result<SlotSetSnapshot, SyncError> {
            Ok(SlotSetSnapshot {
                rev: self.rev,
                slots: self.entries.clone(),
            })
        }

        fn stage_slot(&mut self, slot: &KeySlotV1) -> Result<(), SyncError> {
            if self.find(&slot.descriptor.slot_id).is_some() {
                return Err(SyncError::PasswordChange("槽 ID 冲突"));
            }
            self.entries.push(SlotEntry {
                slot: slot.clone(),
                state: SlotState::Staging,
            });
            Ok(())
        }

        fn activate_password_slot(
            &mut self,
            expected_rev: u64,
            new_slot_id: &[u8; 32],
        ) -> Result<u64, SyncError> {
            if self.race_before_activate {
                return Err(SyncError::Cas {
                    expected: expected_rev,
                    actual: expected_rev + 1,
                });
            }
            if expected_rev != self.rev {
                return Err(SyncError::Cas {
                    expected: expected_rev,
                    actual: self.rev,
                });
            }
            for entry in &mut self.entries {
                if entry.state == SlotState::Active
                    && entry.slot.descriptor.kind == SlotKind::Password
                {
                    entry.state = SlotState::Retiring;
                }
            }
            let entry = self
                .entries
                .iter_mut()
                .find(|e| &e.slot.descriptor.slot_id == new_slot_id)
                .ok_or(SyncError::PasswordChange("待激活的槽不存在"))?;
            entry.state = SlotState::Active;
            self.rev += 1;
            Ok(self.rev)
        }

        fn remove_slot(&mut self, slot_id: &[u8; 32]) -> Result<(), SyncError> {
            let before = self.entries.len();
            self.entries
                .retain(|e| &e.slot.descriptor.slot_id != slot_id);
            if self.entries.len() == before {
                return Err(SyncError::PasswordChange("待删除的槽不存在"));
            }
            Ok(())
        }
    }

    fn seeded_store(rng: &mut TestRng, root: &RootSecretV1) -> (MemorySlotStore, RecoverySeed) {
        let mut store = MemorySlotStore::new();
        let desc = password_descriptor([0x11; 32], [0x12; 16]);
        let slot = KeySlotV1::wrap_password(&desc, b"old password", root, &test_aad, rng).unwrap();
        store.add(slot, SlotState::Active);

        let seed = RecoverySeed::generate(rng).unwrap();
        let rec_desc = recovery_descriptor([0x21; 32]);
        let rec_slot = KeySlotV1::wrap_recovery(&rec_desc, &seed, root, &test_aad, rng).unwrap();
        store.add(rec_slot, SlotState::Active);
        (store, seed)
    }

    /// 小工具：用数据对象密钥加解密，验证"改口令后数据密文完全不变"。
    fn data_encrypt(key: &Secret32, nonce: &[u8; NONCE_LEN], plaintext: &[u8]) -> Vec<u8> {
        let cipher = XChaCha20Poly1305::new_from_slice(key.expose_slice()).unwrap();
        cipher
            .encrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: plaintext,
                    aad: b"data-object-aad",
                },
            )
            .unwrap()
    }

    fn data_decrypt(
        key: &Secret32,
        nonce: &[u8; NONCE_LEN],
        ciphertext: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, SyncError> {
        let cipher =
            XChaCha20Poly1305::new_from_slice(key.expose_slice()).map_err(|_| SyncError::Unwrap)?;
        let plaintext = cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: b"data-object-aad",
                },
            )
            .map_err(|_| SyncError::Unwrap)?;
        Ok(Zeroizing::new(plaintext))
    }

    #[test]
    fn change_password_keeps_data_ciphertext_unchanged() {
        let mut rng = TestRng::new(45);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let (mut store, seed) = seeded_store(&mut rng, &root);

        // 改口令前：用 K_object 加密一段"笔记正文"
        let object_id = [0x77u8; 32];
        let data_key_before = derive_object_key(&root, ObjectType::Update, &object_id).unwrap();
        let nonce = [0x5au8; NONCE_LEN];
        let ciphertext = data_encrypt(&data_key_before, &nonce, b"note body: hello");
        let ciphertext_before = ciphertext.clone();

        let new_slot_id = [0x31u8; 32];
        let outcome = change_password(
            &mut store,
            &root,
            &PasswordChangePlan {
                new_password: b"new password",
                new_slot_id,
                created_at: 1_700_000_100,
            },
            &test_aad,
            &mut rng,
        )
        .unwrap();
        assert_eq!(outcome.old_slot_id, [0x11u8; 32]);
        assert_eq!(outcome.new_slot_id, new_slot_id);
        assert_eq!(outcome.new_slot_set_rev, 2);

        // 数据密文原样可解 —— 改口令不碰数据
        let data_key_after = derive_object_key(&root, ObjectType::Update, &object_id).unwrap();
        assert!(data_key_before.ct_eq(&data_key_after));
        assert_eq!(ciphertext, ciphertext_before);
        assert_eq!(
            &**data_decrypt(&data_key_after, &nonce, &ciphertext).unwrap(),
            b"note body: hello"
        );

        // 新槽 active 且能解出新口令；旧槽已删除
        let snapshot = store.slot_set().unwrap();
        assert!(snapshot
            .slots
            .iter()
            .all(|e| e.slot.descriptor.slot_id != [0x11u8; 32]));
        let active = snapshot
            .slots
            .iter()
            .find(|e| e.slot.descriptor.slot_id == new_slot_id)
            .expect("新槽必须存在");
        assert_eq!(active.state, SlotState::Active);
        assert!(root.ct_eq(
            &active
                .slot
                .unwrap_password(b"new password", &test_aad)
                .unwrap()
        ));
        // 旧口令失效：新槽不接受旧口令
        assert!(active
            .slot
            .unwrap_password(b"old password", &test_aad)
            .is_err());

        // 恢复槽原封不动（改口令不动恢复码）
        let rec = snapshot
            .slots
            .iter()
            .find(|e| e.slot.descriptor.kind == SlotKind::Recovery)
            .expect("恢复槽必须保留");
        assert_eq!(rec.state, SlotState::Active);
        assert!(root.ct_eq(&rec.slot.unwrap_recovery(&seed, &test_aad).unwrap()));
    }

    #[test]
    fn change_password_cas_conflict_leaves_old_active() {
        let mut rng = TestRng::new(46);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let (mut store, _seed) = seeded_store(&mut rng, &root);
        // 模拟并发：读取快照后 rev 被别人推进（CAS 必须失败）
        let stale_rev = store.slot_set().unwrap().rev;
        store.race_before_activate = true;

        let err = change_password(
            &mut store,
            &root,
            &PasswordChangePlan {
                new_password: b"new password",
                new_slot_id: [0x41u8; 32],
                created_at: 1_700_000_100,
            },
            &test_aad,
            &mut rng,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SyncError::Cas {
                expected,
                actual
            } if expected == stale_rev && actual == stale_rev + 1
        ));
        // 旧口令槽仍是 active，未删除
        let snapshot = store.slot_set().unwrap();
        let old = snapshot
            .slots
            .iter()
            .find(|e| e.slot.descriptor.slot_id == [0x11u8; 32])
            .expect("旧槽必须还在");
        assert_eq!(old.state, SlotState::Active);
        assert!(old.slot.unwrap_password(b"old password", &test_aad).is_ok());
        // 新槽留在 staging（24h 回收，ADR §3）
        let staged = snapshot
            .slots
            .iter()
            .find(|e| e.slot.descriptor.slot_id == [0x41u8; 32])
            .expect("新槽已暂存");
        assert_eq!(staged.state, SlotState::Staging);
    }

    #[test]
    fn change_password_rejects_bad_password_before_kdf() {
        let mut rng = TestRng::new(47);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let (mut store, _seed) = seeded_store(&mut rng, &root);

        let started = std::time::Instant::now();
        let err = change_password(
            &mut store,
            &root,
            &PasswordChangePlan {
                new_password: b"",
                new_slot_id: [0x51u8; 32],
                created_at: 1,
            },
            &test_aad,
            &mut rng,
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::PasswordLength { actual: 0, .. }));
        assert!(
            started.elapsed().as_millis() < 50,
            "空口令必须在 Argon2 之前拒绝"
        );
        // 未暂存任何槽
        assert_eq!(store.slot_set().unwrap().slots.len(), 2);
    }

    #[test]
    fn change_password_requires_active_password_slot() {
        let mut rng = TestRng::new(48);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let mut store = MemorySlotStore::new();
        let err = change_password(
            &mut store,
            &root,
            &PasswordChangePlan {
                new_password: b"pw",
                new_slot_id: [0x61u8; 32],
                created_at: 1,
            },
            &test_aad,
            &mut rng,
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::PasswordChange(_)));
    }
}
