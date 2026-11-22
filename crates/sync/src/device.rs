//! 设备身份与证书（ADR 0006 §4）。
//!
//! - **签名身份**：Ed25519（RFC 8032），种子 32B，私钥不 Debug/不日志；
//! - **HPKE 身份**：X25519，32B 私钥材料经 RFC 9180 `DeriveKeyPair` 得到 keypair；
//! - **`DeviceCertV1`**：wire 类型来自 `menghunote-sync-proto`，由**管理 key** 签名
//!   （不是 vault root 派生——ADR §4：管理权不是 rights 里的 flag）；
//! - **管理 key 独立**：`management_ed25519_seed = HKDF(R, vault_id, b"MN/v1/management-ed25519\0")`，
//!   只有恢复码持有者能派生；普通根持有者不能据此注册/吊销设备；
//! - **邀请令牌**：`T = random32`，relay 只存 `SHA256(T)`，24h 一次性；`redeem` 是纯函数
//!   （不改 IO，只返回新状态）；
//! - **设备指纹**：`SHA256(vault_id || device_id || sign_pk || hpke_pk)` 前 128 bit，
//!   双端显示为 32 位小写 hex，用于人工比对（ADR §4）。
//!
//! 证书签名输入的域前缀见 [`crate::domain::DEVICE_CERT`]（ADR 未冻结，已在实现回报标注）。

use crate::error::SyncError;
use crate::hpke;
use crate::ids::hex_of;
use crate::keys::Secret32;
use crate::random::RandomSource;
use crate::recovery::{self, RecoverySeed};
use crate::{DeviceId, VaultId};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use menghunote_sync_proto::{DeviceCertV1, Ed25519PublicKey, Rights, X25519PublicKey};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Ed25519 签名私钥（32 字节种子）。
#[derive(Clone)]
pub struct DeviceSigningKey(Secret32);

impl DeviceSigningKey {
    /// 生成新的随机签名密钥。
    pub fn generate(rng: &mut impl RandomSource) -> Result<Self, SyncError> {
        Ok(Self(Secret32::generate(rng)?))
    }

    /// 从已有种子构造。
    #[must_use]
    pub fn from_seed(seed: Secret32) -> Self {
        Self(seed)
    }

    /// 取种子（DPAPI 身份 BLOB 需要持久化它）。
    #[must_use]
    pub fn seed(&self) -> &Secret32 {
        &self.0
    }

    /// Ed25519 公钥（32B）。
    #[must_use]
    pub fn public_key(&self) -> Ed25519PublicKey {
        signing_key(&self.0).verifying_key().to_bytes()
    }

    /// 签名（64B）。
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        signing_key(&self.0).sign(message).to_bytes()
    }
}

fn signing_key(seed: &Secret32) -> SigningKey {
    SigningKey::from_bytes(seed.expose())
}

/// 验证 Ed25519 签名；成功返回 `Ok(())`。
pub fn verify_signature(
    public_key: &[u8; 32],
    message: &[u8],
    signature: &[u8; 64],
) -> Result<(), SyncError> {
    let vk = VerifyingKey::from_bytes(public_key).map_err(|_| SyncError::Signature)?;
    let sig = Signature::from_bytes(signature);
    // verify_strict：拒绝非规范/小阶公钥等边界情况（ADR 要求"篡改全拒绝"）。
    vk.verify_strict(message, &sig)
        .map_err(|_| SyncError::Signature)
}

/// 管理私钥（独立于 vault root，由恢复码派生）。
#[derive(Clone)]
pub struct ManagementKey(Secret32);

impl ManagementKey {
    /// 从已有种子构造（DPAPI 身份里的 `manager_seed`）。
    #[must_use]
    pub fn from_seed(seed: Secret32) -> Self {
        Self(seed)
    }

    /// 从恢复码派生（ADR §3：`management_ed25519_seed = HKDF(R, vault_id, ...)`）。
    pub fn from_recovery(seed: &RecoverySeed, vault_id: &VaultId) -> Result<Self, SyncError> {
        Ok(Self(recovery::management_seed(seed, vault_id)?))
    }

    /// 管理公钥（32B）。服务端只存这个。
    #[must_use]
    pub fn public_key(&self) -> Ed25519PublicKey {
        signing_key(&self.0).verifying_key().to_bytes()
    }

    /// 管理签名（64B）。
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        signing_key(&self.0).sign(message).to_bytes()
    }
}

/// X25519 HPKE 私钥（32 字节种子，经 RFC 9180 DeriveKeyPair 使用）。
#[derive(Clone)]
pub struct DeviceHpkeKey(Secret32);

impl DeviceHpkeKey {
    /// 生成新的随机 HPKE 身份。
    pub fn generate(rng: &mut impl RandomSource) -> Result<Self, SyncError> {
        Ok(Self(Secret32::generate(rng)?))
    }

    /// 从已有私钥材料构造。
    #[must_use]
    pub fn from_secret(secret: Secret32) -> Self {
        Self(secret)
    }

    /// 取私钥材料（DPAPI 身份 BLOB 需要持久化它）。
    #[must_use]
    pub fn secret(&self) -> &Secret32 {
        &self.0
    }

    /// X25519 公钥（HPKE `pkR`）。
    #[must_use]
    pub fn public_key(&self) -> X25519PublicKey {
        hpke::x25519_public_key(&self.0)
    }

    /// 解封给自己的 epoch grant（错 recipient 会返回 [`SyncError::RecipientMismatch`]）。
    pub fn open_grant(
        &self,
        enc: &[u8; 32],
        aad: &menghunote_sync_proto::GrantAadV1,
        ciphertext: &[u8],
    ) -> Result<crate::keys::RootSecretV1, SyncError> {
        hpke::open_epoch_grant(&self.0, enc, aad, ciphertext)
    }
}

/// 设备指纹（128 bit，显示为 32 位小写 hex）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DeviceFingerprint([u8; 16]);

impl DeviceFingerprint {
    /// 原始字节。
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// 32 位小写十六进制（双端人工比对用）。
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex_of(&self.0)
    }
}

impl core::fmt::Debug for DeviceFingerprint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// `SHA256(vault_id || device_id || sign_pk || hpke_pk)` 前 128 bit（ADR §4）。
#[must_use]
pub fn device_fingerprint(
    vault_id: &VaultId,
    device_id: &DeviceId,
    sign_pub: &Ed25519PublicKey,
    hpke_pub: &X25519PublicKey,
) -> DeviceFingerprint {
    let mut hasher = Sha256::new();
    hasher.update(vault_id);
    hasher.update(device_id);
    hasher.update(sign_pub);
    hasher.update(hpke_pub);
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    DeviceFingerprint(out)
}

/// 带签名的设备证书（relay `devices.certificate` 的载荷）。
#[derive(Clone, PartialEq, Eq)]
pub struct SignedDeviceCert {
    /// 证书本体（proto 的 `DeviceCertV1`）。
    pub cert: DeviceCertV1,
    /// 管理 key 的 Ed25519 签名（64B）。
    pub signature: [u8; 64],
}

impl SignedDeviceCert {
    /// 用管理公钥验签（签名输入 = `b"MN/v1/device-cert\0" || CDE(cert)`）。
    pub fn verify(&self, manager_pub: &Ed25519PublicKey) -> Result<(), SyncError> {
        let input = device_cert_signing_input(&self.cert)?;
        verify_signature(manager_pub, &input, &self.signature)
    }

    /// 证书 CDE（不含签名）。
    pub fn cert_bytes(&self) -> Result<Vec<u8>, SyncError> {
        Ok(self.cert.encode()?)
    }

    /// 本证书绑定的设备指纹。
    #[must_use]
    pub fn fingerprint(&self) -> DeviceFingerprint {
        device_fingerprint(
            &self.cert.vault_id,
            &self.cert.device_id,
            &self.cert.sign_ed25519_pub,
            &self.cert.hpke_x25519_pub,
        )
    }
}

/// 证书的管理签名输入：`b"MN/v1/device-cert\0" || CDE(DeviceCertV1)`（ADR §17 冻结）。
///
/// 只做转发：真正的字节由 `sync-proto` 生成，relay 与客户端因此共用同一套。
pub fn device_cert_signing_input(cert: &DeviceCertV1) -> Result<Vec<u8>, SyncError> {
    Ok(menghunote_sync_proto::device_cert_signed_bytes(cert)?)
}

/// 由管理 key 签发设备证书。
pub fn sign_device_cert(
    cert: &DeviceCertV1,
    manager: &ManagementKey,
) -> Result<SignedDeviceCert, SyncError> {
    let input = device_cert_signing_input(cert)?;
    Ok(SignedDeviceCert {
        cert: cert.clone(),
        signature: manager.sign(&input),
    })
}

/// 从恢复码出发验证整条证书链：
///
/// `R` → `management_ed25519_seed` → 管理公钥 → 验证证书签名，并校验
/// vault 归属、`issued_manager_rev`（管理权版本）与 rights 合法。
///
/// 这条链表达的是 ADR §4 的"恢复码具备独立管理能力"：没有 R 的一方（即使持有 vault
/// root）无法构造出通过校验的证书。
pub fn verify_cert_chain(
    signed: &SignedDeviceCert,
    recovery_seed: &RecoverySeed,
    expected_vault: &VaultId,
    expected_manager_rev: u64,
) -> Result<(), SyncError> {
    if signed.cert.vault_id != *expected_vault {
        return Err(SyncError::Certificate("证书 vault 归属不符"));
    }
    if signed.cert.issued_manager_rev != expected_manager_rev {
        return Err(SyncError::Certificate("证书管理权版本不符"));
    }
    // proto 的 validate() 会检查 cert_serial>0 与 rights 合法。
    signed.cert.validate()?;
    let manager = ManagementKey::from_recovery(recovery_seed, expected_vault)?;
    signed.verify(&manager.public_key())
}

/// 设备默认权限：READ|WRITE（ADR §4）。
#[must_use]
pub fn default_rights() -> Rights {
    Rights(Rights::ALL)
}

/// 邀请令牌 `T`（32B 随机）。**不 Debug/不 Display**。
#[derive(Clone)]
pub struct InviteToken(Secret32);

impl InviteToken {
    /// 生成 `T = random32`（ADR §4）。
    pub fn generate(rng: &mut impl RandomSource) -> Result<Self, SyncError> {
        Ok(Self(Secret32::generate(rng)?))
    }

    /// 从已有字节构造（配对 URI 里携带的就是它）。
    #[must_use]
    pub fn from_array(bytes: [u8; 32]) -> Self {
        Self(Secret32::from_array(bytes))
    }

    /// 取明文引用。
    #[must_use]
    pub fn expose(&self) -> &[u8; 32] {
        self.0.expose()
    }

    /// 常量时间相等。
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0)
    }
}

/// 邀请令牌的哈希：`SHA256(T)`（ADR §4：relay 只存这个，无额外域前缀）。
#[must_use]
pub fn invite_token_hash(token: &InviteToken) -> [u8; 32] {
    let digest = Sha256::digest(token.expose());
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// 邀请有效期：24h（ADR §4）。
pub const INVITE_TTL_SECS: u64 = 24 * 60 * 60;

/// 邀请记录（relay `invites` 行的客户端侧对应物）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InviteRecord {
    /// `SHA256(T)`。
    pub token_hash: [u8; 32],
    /// 过期时刻（Unix 秒）。
    pub expires_at: u64,
    /// 消费时刻；`Some` 后不可再用（一次性）。
    pub used_at: Option<u64>,
    /// 消费后绑定到的 PENDING 设备。
    pub pending_device: Option<DeviceId>,
}

impl InviteRecord {
    /// 由令牌与当前时刻创建（有效期 [`INVITE_TTL_SECS`]）。
    #[must_use]
    pub fn new(token: &InviteToken, now: u64) -> Self {
        Self {
            token_hash: invite_token_hash(token),
            expires_at: now.saturating_add(INVITE_TTL_SECS),
            used_at: None,
            pending_device: None,
        }
    }

    /// 纯函数消费：校验"未用过 + 未过期 + 哈希匹配"，返回新状态与 PENDING 设备。
    ///
    /// 不做任何 IO；持久化/CAS 由调用方（relay 或 storage）负责。哈希比较是常量时间。
    pub fn redeem(
        self,
        offered: &InviteToken,
        now: u64,
        pending_device: DeviceId,
    ) -> Result<(Self, DeviceId), SyncError> {
        if self.used_at.is_some() {
            return Err(SyncError::Invite("令牌已被消费"));
        }
        if now > self.expires_at {
            return Err(SyncError::Invite("令牌已过期"));
        }
        let offered_hash = invite_token_hash(offered);
        if !bool::from(offered_hash.ct_eq(&self.token_hash)) {
            return Err(SyncError::Invite("令牌哈希不匹配"));
        }
        let record = Self {
            used_at: Some(now),
            pending_device: Some(pending_device),
            ..self
        };
        Ok((record, pending_device))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRng;

    fn fixture(
        seed: u64,
    ) -> (
        TestRng,
        VaultId,
        DeviceId,
        DeviceSigningKey,
        DeviceHpkeKey,
        ManagementKey,
    ) {
        let mut rng = TestRng::new(seed);
        let vault_id: VaultId = [0x11; 16];
        let device_id: DeviceId = [0x22; 16];
        let sign = DeviceSigningKey::generate(&mut rng).unwrap();
        let hpke_key = DeviceHpkeKey::generate(&mut rng).unwrap();
        // 管理 key 从恢复码派生（与设备 key 完全独立）
        let recovery = RecoverySeed::generate(&mut rng).unwrap();
        let manager = ManagementKey::from_recovery(&recovery, &vault_id).unwrap();
        (rng, vault_id, device_id, sign, hpke_key, manager)
    }

    fn cert(
        vault_id: VaultId,
        device_id: DeviceId,
        sign: &DeviceSigningKey,
        hpke_key: &DeviceHpkeKey,
    ) -> DeviceCertV1 {
        DeviceCertV1 {
            vault_id,
            device_id,
            sign_ed25519_pub: sign.public_key(),
            hpke_x25519_pub: hpke_key.public_key(),
            cert_serial: 7,
            issued_manager_rev: 3,
            rights: default_rights(),
        }
    }

    #[test]
    fn sign_verify_roundtrip_and_tamper() {
        let (_rng, _v, _d, sign, _hpke_key, _manager) = fixture(31);
        let msg = b"MN/v1/device-cert\0payload";
        let sig = sign.sign(msg);
        verify_signature(&sign.public_key(), msg, &sig).unwrap();

        // 改消息 / 改签名 / 改公钥都失败
        assert!(verify_signature(&sign.public_key(), b"other", &sig).is_err());
        let mut bad_sig = sig;
        bad_sig[0] ^= 0x01;
        assert!(verify_signature(&sign.public_key(), msg, &bad_sig).is_err());
        let mut bad_pub = sign.public_key();
        bad_pub[0] ^= 0x01;
        assert!(verify_signature(&bad_pub, msg, &sig).is_err());
    }

    #[test]
    fn cert_cde_roundtrip_and_tamper_rejected() {
        let (_rng, vault_id, device_id, sign, hpke_key, manager) = fixture(32);
        let cert = cert(vault_id, device_id, &sign, &hpke_key);
        let bytes = cert.encode().unwrap();
        let parsed = DeviceCertV1::decode(&bytes).unwrap();
        assert_eq!(cert, parsed);
        assert_eq!(bytes, parsed.encode().unwrap());
        assert_eq!(bytes[0], 0xa7, "7 项 map 头");

        let signed = sign_device_cert(&cert, &manager).unwrap();
        signed.verify(&manager.public_key()).unwrap();
        assert_eq!(signed.cert_bytes().unwrap(), bytes);

        // 每个字段篡改都必须失败
        let manager_pub = manager.public_key();
        let mut tampered_sig = signed.clone();
        tampered_sig.cert.cert_serial += 1;
        assert!(tampered_sig.verify(&manager_pub).is_err());
        let mut tampered_sig = signed.clone();
        tampered_sig.cert.rights = Rights(Rights::READ);
        assert!(tampered_sig.verify(&manager_pub).is_err());
        let mut tampered_sig = signed.clone();
        tampered_sig.cert.hpke_x25519_pub[0] ^= 1;
        assert!(tampered_sig.verify(&manager_pub).is_err());
        let mut tampered_sig = signed.clone();
        tampered_sig.cert.device_id = [0x33; 16];
        assert!(tampered_sig.verify(&manager_pub).is_err());
        let mut tampered_sig = signed;
        tampered_sig.signature[63] ^= 1;
        assert!(tampered_sig.verify(&manager_pub).is_err());

        // 换一个管理 key 验签失败（"管理权不能来自 root"）
        let mut other_rng = TestRng::new(99);
        let other_manager = ManagementKey::from_recovery(
            &RecoverySeed::generate(&mut other_rng).unwrap(),
            &vault_id,
        )
        .unwrap();
        assert!(tampered_sig.verify(&other_manager.public_key()).is_err());
    }

    #[test]
    fn cert_chain_from_recovery_seed() {
        let mut rng = TestRng::new(33);
        let vault_id: VaultId = [0x44; 16];
        let device_id: DeviceId = [0x55; 16];
        let sign = DeviceSigningKey::generate(&mut rng).unwrap();
        let hpke_key = DeviceHpkeKey::generate(&mut rng).unwrap();
        let recovery = RecoverySeed::generate(&mut rng).unwrap();
        let manager = ManagementKey::from_recovery(&recovery, &vault_id).unwrap();
        let signed =
            sign_device_cert(&cert(vault_id, device_id, &sign, &hpke_key), &manager).unwrap();

        verify_cert_chain(&signed, &recovery, &vault_id, 3).unwrap();
        // 管理权版本不符
        assert!(verify_cert_chain(&signed, &recovery, &vault_id, 4).is_err());
        // 另一个 vault
        assert!(verify_cert_chain(&signed, &recovery, &[0x46; 16], 3).is_err());
        // 另一个恢复码（不是签发者）
        let other_recovery = RecoverySeed::generate(&mut rng).unwrap();
        assert!(verify_cert_chain(&signed, &other_recovery, &vault_id, 3).is_err());
        // 篡改证书后链校验失败
        let mut tampered = signed.clone();
        tampered.cert.sign_ed25519_pub[0] ^= 1;
        assert!(verify_cert_chain(&tampered, &recovery, &vault_id, 3).is_err());
    }

    #[test]
    fn management_key_is_independent_from_device_keys() {
        let (_rng, _vault_id, _device_id, sign, _hpke_key, manager) = fixture(34);
        assert_ne!(manager.public_key(), sign.public_key());
        // 设备 key 签的东西不能当管理签名通过
        let msg = b"management-only";
        assert!(verify_signature(&manager.public_key(), msg, &sign.sign(msg)).is_err());
    }

    #[test]
    fn device_fingerprint_golden_and_hex() {
        let fp = device_fingerprint(&[0x11; 16], &[0x22; 16], &[0x33; 32], &[0x44; 32]);
        // 独立实现（Python hashlib）算出的 golden：
        // SHA256(0x11*16 || 0x22*16 || 0x33*32 || 0x44*32) 前 16 字节
        assert_eq!(fp.to_hex(), "ed6a59d351a1502d49c04a3cb2838540");
        assert_eq!(fp.to_hex().len(), 32);
        assert_eq!(fp.as_bytes().len(), 16);
        // 任一输入不同 → 指纹不同
        assert_ne!(
            fp.to_hex(),
            device_fingerprint(&[0x11; 16], &[0x23; 16], &[0x33; 32], &[0x44; 32]).to_hex()
        );
        // 证书自带同一指纹
        let (_rng, vault_id, device_id, sign, hpke_key, _manager) = fixture(35);
        let c = cert(vault_id, device_id, &sign, &hpke_key);
        let signed = sign_device_cert(&c, &_manager).unwrap();
        assert_eq!(
            signed.fingerprint(),
            device_fingerprint(
                &vault_id,
                &device_id,
                &c.sign_ed25519_pub,
                &c.hpke_x25519_pub
            )
        );
    }

    #[test]
    fn rights_are_strict() {
        assert_eq!(Rights(Rights::ALL).bits(), 0b11);
        assert!(Rights(Rights::ALL).can_read());
        assert!(Rights(Rights::ALL).can_write());
        assert!(!Rights(Rights::READ).can_write());
        assert!(Rights::new(0b11).is_ok());
        assert!(Rights::new(0b01).is_ok());
        assert!(Rights::new(0).is_err());
        assert!(Rights::new(0b100).is_err());
        assert!(Rights::new(0xff).is_err());
        // proto 的 DeviceCertV1::validate 要求 cert_serial > 0
        let (_rng, vault_id, device_id, sign, hpke_key, _manager) = fixture(36);
        let mut c = cert(vault_id, device_id, &sign, &hpke_key);
        c.cert_serial = 0;
        assert!(c.encode().is_err());
    }

    #[test]
    fn invite_token_golden_and_single_use() {
        // T = 0x00..0x1f 的 SHA256（独立 Python hashlib 计算）
        let token = InviteToken::from_array([
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ]);
        let hash = invite_token_hash(&token);
        assert_eq!(
            hex_of(&hash),
            "630dcd2966c4336691125448bbb25b4ff412a49c732db2c8abc1b8581bd710dd"
        );
        assert_ne!(&hash, token.expose());

        let device: DeviceId = [0x77; 16];
        let record = InviteRecord::new(&token, 1_000);
        assert_eq!(record.expires_at, 1_000 + INVITE_TTL_SECS);
        assert_eq!(record.token_hash, hash);

        // 成功消费一次
        let (record, pending) = record.redeem(&token, 1_100, device).unwrap();
        assert_eq!(pending, device);
        assert_eq!(record.used_at, Some(1_100));
        assert_eq!(record.pending_device, Some(device));
        // 第二次消费失败（一次性）
        assert!(matches!(
            record.clone().redeem(&token, 1_200, device),
            Err(SyncError::Invite(_))
        ));
        // 过期
        let record = InviteRecord::new(&token, 1_000);
        assert!(record
            .clone()
            .redeem(&token, 1_000 + INVITE_TTL_SECS + 1, device)
            .is_err());
        // 边界：恰好在过期时刻仍可用（now == expires_at）
        assert!(record
            .clone()
            .redeem(&token, 1_000 + INVITE_TTL_SECS, device)
            .is_ok());
        // 错令牌
        let record = InviteRecord::new(&token, 1_000);
        let mut other = [0u8; 32];
        other[0] = 1;
        assert!(record
            .clone()
            .redeem(&InviteToken::from_array(other), 1_000, device)
            .is_err());
    }

    #[test]
    fn cert_unknown_field_rejected() {
        let (_rng, vault_id, device_id, sign, hpke_key, _manager) = fixture(37);
        let cert = cert(vault_id, device_id, &sign, &hpke_key);
        let bytes = cert.encode().unwrap();
        // 7 项 map 头改成 8 项 → 字段数不符
        let mut bad = bytes.clone();
        bad[0] = 0xa8;
        assert!(DeviceCertV1::decode(&bad).is_err());
        // rights 位非法（最后一项值改为 4）
        let mut bad = bytes;
        let last = bad.len() - 1;
        bad[last] = 0x04;
        assert!(DeviceCertV1::decode(&bad).is_err());
    }
}
