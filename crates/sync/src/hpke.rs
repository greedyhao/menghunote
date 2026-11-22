//! HPKE（RFC 9180）base mode：把 epoch 根封装给指定设备的 X25519 公钥（ADR 0006 §4/§17）。
//!
//! 冻结参数：
//!
//! | 项 | 值 |
//! | --- | --- |
//! | mode | base (0x00) |
//! | KEM | DHKEM(X25519, HKDF-SHA256) = 0x0020 |
//! | KDF | HKDF-SHA256 = 0x0001 |
//! | AEAD | ChaCha20Poly1305 = 0x0003 |
//! | info | `b"MN/v1/epoch-grant\0"`（`sync-proto::HPKE_INFO`，末尾真正的 0x00） |
//! | AAD | `CDE(GrantAadV1)`（`sync-proto`，§17 冻结） |
//!
//! **不自造 nonce**：AEAD nonce 由 HPKE 按 RFC 9180 §5.2 从 `base_nonce` 与序列号派生，
//! 本文件不接触 nonce 字节。实现用成熟 crate `hpke` 0.15.1（RustCrypto），并附 RFC 9180
//! 附录 A.2 的 known-answer 向量测试。
//!
//! 外层管理签名覆盖 `AAD || enc32 || SHA256(hpke_ciphertext)`（ADR §17），待签字节由
//! `sync-proto` 的 `GrantAadV1::grant_signed_bytes` 生成；签名本身由 [`crate::device`] 做。

use crate::domain;
use crate::error::SyncError;
use crate::keys::{RootSecretV1, Secret32};
use hpke::kem::X25519HkdfSha256;
use hpke::{
    aead::ChaCha20Poly1305, kdf::HkdfSha256, Deserializable, Kem as KemTrait, OpModeR, OpModeS,
    Serializable,
};
use menghunote_sync_proto::GrantAadV1;
use sha2::{Digest, Sha256};

/// HPKE KEM：DHKEM(X25519, HKDF-SHA256)（RFC 9180 值 0x0020）。
pub type GrantKem = X25519HkdfSha256;
/// HPKE KDF：HKDF-SHA256（0x0001）。
pub type GrantKdf = HkdfSha256;
/// HPKE AEAD：ChaCha20Poly1305（0x0003）。
pub type GrantAead = ChaCha20Poly1305;

/// HPKE info 域（ADR §4；`sync-proto` 的常量，末尾是一个 0x00 字节）。
pub const HPKE_INFO: &[u8] = domain::HPKE_INFO;

/// 一次封装的结果：`enc`（32B 临时公钥）+ HPKE 密文。
///
/// `enc` 与密文都**不是秘密**（密文只有收件设备能解）。
#[derive(Clone, PartialEq, Eq)]
pub struct SealedEpochGrant {
    /// HPKE 封装出的临时 X25519 公钥（RFC 9180 `enc`）。
    pub enc: [u8; 32],
    /// HPKE 密文（含 AEAD tag）。
    pub ciphertext: Vec<u8>,
}

impl SealedEpochGrant {
    /// 管理签名的待签字节：`AAD || enc32 || SHA256(hpke_ciphertext)`（ADR §17）。
    pub fn signature_input(&self, aad: &GrantAadV1) -> Result<Vec<u8>, SyncError> {
        grant_signature_input(aad, &self.enc, &self.ciphertext)
    }
}

/// ADR §17 的管理签名覆盖字节：`AAD || enc32 || SHA256(hpke_ciphertext)`。
///
/// AAD 是 `CDE(GrantAadV1)`；签名算法（Ed25519）与调用点由 [`crate::device`] 负责。
pub fn grant_signature_input(
    aad: &GrantAadV1,
    enc: &[u8; 32],
    ciphertext: &[u8],
) -> Result<Vec<u8>, SyncError> {
    let digest: [u8; 32] = Sha256::digest(ciphertext).into();
    Ok(aad.grant_signed_bytes(enc, &digest)?)
}

/// 由 32 字节私钥材料派生 X25519 公钥（RFC 9180 §7.1 `DeriveKeyPair`）。
#[must_use]
pub fn x25519_public_key(secret: &Secret32) -> [u8; 32] {
    let (_sk, pk) = GrantKem::derive_keypair(secret.expose_slice());
    let bytes = pk.to_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(bytes.as_slice());
    out
}

fn hpke_public_key(bytes: &[u8; 32]) -> Result<<GrantKem as KemTrait>::PublicKey, SyncError> {
    <GrantKem as KemTrait>::PublicKey::from_bytes(bytes)
        .map_err(|_| SyncError::Hpke("收件人 X25519 公钥无效"))
}

fn hpke_encapped_key(bytes: &[u8; 32]) -> Result<<GrantKem as KemTrait>::EncappedKey, SyncError> {
    <GrantKem as KemTrait>::EncappedKey::from_bytes(bytes)
        .map_err(|_| SyncError::Hpke("HPKE enc 长度/格式无效"))
}

fn hpke_private_key(secret: &Secret32) -> <GrantKem as KemTrait>::PrivateKey {
    let (sk, _pk) = GrantKem::derive_keypair(secret.expose_slice());
    sk
}

/// 内部封装：`info` 可注入，只用于 RFC 9180 known-answer 测试。
fn seal_raw(
    recipient_x25519_pub: &[u8; 32],
    info: &[u8],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<SealedEpochGrant, SyncError> {
    let pk = hpke_public_key(recipient_x25519_pub)?;
    let (enc, ciphertext) = hpke::single_shot_seal::<GrantAead, GrantKdf, GrantKem>(
        &OpModeS::Base,
        &pk,
        info,
        plaintext,
        aad,
    )
    .map_err(|_| SyncError::Hpke("HPKE 封装失败"))?;
    let enc_bytes = enc.to_bytes();
    let mut enc_out = [0u8; 32];
    enc_out.copy_from_slice(enc_bytes.as_slice());
    Ok(SealedEpochGrant {
        enc: enc_out,
        ciphertext,
    })
}

/// 内部解封：`info` 可注入，只用于 RFC 9180 known-answer 测试。
fn open_raw(
    recipient_secret: &Secret32,
    enc: &[u8; 32],
    info: &[u8],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, SyncError> {
    let sk = hpke_private_key(recipient_secret);
    let encapped = hpke_encapped_key(enc)?;
    hpke::single_shot_open::<GrantAead, GrantKdf, GrantKem>(
        &OpModeR::Base,
        &sk,
        &encapped,
        info,
        ciphertext,
        aad,
    )
    .map_err(|_| SyncError::Hpke("HPKE 解封失败"))
}

/// 把 epoch 根封装给收件设备（ADR §4/§17）。
///
/// 明文是 `RootSecretV1` 的 CDE；AAD 必须与收件设备公钥一致，否则拒绝。
pub fn seal_epoch_grant(
    recipient_x25519_pub: &[u8; 32],
    root: &RootSecretV1,
    aad: &GrantAadV1,
) -> Result<SealedEpochGrant, SyncError> {
    aad.validate()?;
    if &aad.recipient_x25519_pub != recipient_x25519_pub {
        return Err(SyncError::RecipientMismatch);
    }
    if aad.vault_id != root.vault_id || aad.epoch != root.key_epoch {
        return Err(SyncError::Hpke("grant AAD 与根归属不一致"));
    }
    let aad_bytes = aad.aad_bytes()?;
    // 明文根只在这层存在一瞬间：封装后立即零化。
    let plaintext = zeroize::Zeroizing::new(root.encode()?);
    seal_raw(recipient_x25519_pub, HPKE_INFO, &plaintext, &aad_bytes)
}

/// 解封 epoch grant（收件设备侧）。
///
/// 校验顺序：AAD 的收件人公钥 == 本机派生公钥（错 recipient 立即失败）→ HPKE 解封 →
/// 解析 `RootSecretV1` → 根归属必须与 AAD 一致。
pub fn open_epoch_grant(
    recipient_secret: &Secret32,
    enc: &[u8; 32],
    aad: &GrantAadV1,
    ciphertext: &[u8],
) -> Result<RootSecretV1, SyncError> {
    aad.validate()?;
    if x25519_public_key(recipient_secret) != aad.recipient_x25519_pub {
        return Err(SyncError::RecipientMismatch);
    }
    let aad_bytes = aad.aad_bytes()?;
    let plaintext = zeroize::Zeroizing::new(open_raw(
        recipient_secret,
        enc,
        HPKE_INFO,
        &aad_bytes,
        ciphertext,
    )?);
    let root = RootSecretV1::decode(&plaintext)?;
    if root.vault_id != aad.vault_id || root.key_epoch != aad.epoch {
        return Err(SyncError::Hpke("grant 载荷与 AAD 归属不一致"));
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRng;

    fn hex<const N: usize>(s: &str) -> [u8; N] {
        crate::ids::parse_hex::<N>(s).unwrap()
    }

    /// RFC 9180 附录 A.2：DHKEM(X25519, HKDF-SHA256) + HKDF-SHA256 + ChaCha20Poly1305，
    /// base mode，sequence number 0。
    ///
    /// 这证明我们选用的 ciphersuite（0x0020/0x0001/0x0003）与 AAD/明文处理是 RFC 一致的；
    /// 我们自己的 `info` 域不同，所以这里走 `open_raw`。
    ///
    /// 注意用 RFC 的 **ikmR**（而不是 skRm）作为输入：生产的 X25519 私钥就是从 32B ikm
    /// 经 `DeriveKeyPair` 得到的，这样 KAT 覆盖的正是真实代码路径（pk 必须等于 RFC 的 pkRm）。
    #[test]
    fn rfc9180_a2_known_answer() {
        let ikm_r = hex::<32>("1ac01f181fdf9f352797655161c58b75c656a6cc2716dcb66372da835542e1df");
        let enc = hex::<32>("1afa08d3dec047a643885163f1180476fa7ddb54c6a8029ea33f95796bf2ac4a");
        let info = hex::<20>("4f6465206f6e2061204772656369616e2055726e");
        let ct = hex::<45>(
            "1c5250d8034ec2b784ba2cfd69dbdb8af406cfe3ff938e131f0def8c8b60b4db21993c62ce81883d2dd1b51a28",
        );
        let secret = Secret32::from_array(ikm_r);
        let opened = open_raw(&secret, &enc, &info, b"Count-0", &ct).unwrap();
        assert_eq!(
            opened, b"Beauty is truth, truth beauty",
            "必须与 RFC 9180 A.2.1.1 的明文一致"
        );
    }

    fn sample_aad(rng: &mut TestRng) -> (Secret32, GrantAadV1) {
        let secret = Secret32::generate(rng).unwrap();
        let pubkey = x25519_public_key(&secret);
        (
            secret,
            GrantAadV1 {
                pv: 1,
                vault_id: [1u8; 16],
                epoch: 7,
                recipient_device_id: [2u8; 16],
                recipient_x25519_pub: pubkey,
                manager_rev: 3,
                grant_id: [9u8; 16],
            },
        )
    }

    #[test]
    fn seal_open_roundtrip() {
        let mut rng = TestRng::new(21);
        let (secret, aad) = sample_aad(&mut rng);
        let root = RootSecretV1::generate(aad.vault_id, aad.epoch, &mut rng).unwrap();

        let sealed = seal_epoch_grant(&aad.recipient_x25519_pub, &root, &aad).unwrap();
        assert_eq!(sealed.enc.len(), 32);
        assert!(sealed.ciphertext.len() > 32);
        assert_ne!(sealed.ciphertext, root.root.expose_slice());

        let opened = open_epoch_grant(&secret, &sealed.enc, &aad, &sealed.ciphertext).unwrap();
        assert!(root.ct_eq(&opened));
    }

    #[test]
    fn tampered_ciphertext_or_aad_or_enc_rejected() {
        let mut rng = TestRng::new(22);
        let (secret, aad) = sample_aad(&mut rng);
        let root = RootSecretV1::generate(aad.vault_id, aad.epoch, &mut rng).unwrap();
        let sealed = seal_epoch_grant(&aad.recipient_x25519_pub, &root, &aad).unwrap();

        // 密文任意一位翻转
        let mut ct = sealed.ciphertext.clone();
        ct[0] ^= 0x01;
        assert!(open_epoch_grant(&secret, &sealed.enc, &aad, &ct).is_err());

        // enc 翻转
        let mut enc = sealed.enc;
        enc[0] ^= 0x01;
        assert!(open_epoch_grant(&secret, &enc, &aad, &sealed.ciphertext).is_err());

        // AAD 任一字段变化
        let mut bad = aad.clone();
        bad.manager_rev += 1;
        assert!(open_epoch_grant(&secret, &sealed.enc, &bad, &sealed.ciphertext).is_err());
        let mut bad = aad.clone();
        bad.grant_id[0] ^= 0x01;
        assert!(open_epoch_grant(&secret, &sealed.enc, &bad, &sealed.ciphertext).is_err());
        let mut bad = aad.clone();
        bad.epoch += 1;
        assert!(open_epoch_grant(&secret, &sealed.enc, &bad, &sealed.ciphertext).is_err());
        let mut bad = aad.clone();
        bad.recipient_device_id = [3u8; 16];
        assert!(open_epoch_grant(&secret, &sealed.enc, &bad, &sealed.ciphertext).is_err());
        let mut bad = aad.clone();
        bad.pv = 2;
        assert!(open_epoch_grant(&secret, &sealed.enc, &bad, &sealed.ciphertext).is_err());

        // 截断
        assert!(open_epoch_grant(&secret, &sealed.enc, &aad, &sealed.ciphertext[..10]).is_err());
    }

    #[test]
    fn wrong_recipient_rejected() {
        let mut rng = TestRng::new(23);
        let (_secret, aad) = sample_aad(&mut rng);
        let root = RootSecretV1::generate(aad.vault_id, aad.epoch, &mut rng).unwrap();
        let sealed = seal_epoch_grant(&aad.recipient_x25519_pub, &root, &aad).unwrap();

        // 另一台设备的私钥：AAD 里的公钥对不上 → RecipientMismatch
        let other_secret = Secret32::generate(&mut rng).unwrap();
        assert!(matches!(
            open_epoch_grant(&other_secret, &sealed.enc, &aad, &sealed.ciphertext),
            Err(SyncError::RecipientMismatch)
        ));

        // 如果 AAD 被改成另一台设备的公钥，HPKE 解封本身失败
        let other_pub = x25519_public_key(&other_secret);
        let mut forged = aad.clone();
        forged.recipient_x25519_pub = other_pub;
        assert!(open_epoch_grant(&other_secret, &sealed.enc, &forged, &sealed.ciphertext).is_err());
    }

    #[test]
    fn seal_rejects_inconsistent_aad() {
        let mut rng = TestRng::new(24);
        let (_secret, aad) = sample_aad(&mut rng);
        let root = RootSecretV1::generate(aad.vault_id, aad.epoch, &mut rng).unwrap();
        // 收件人公钥与 AAD 不一致
        let mut wrong_aad = aad.clone();
        wrong_aad.recipient_x25519_pub = [0x77; 32];
        assert!(matches!(
            seal_epoch_grant(&aad.recipient_x25519_pub, &root, &wrong_aad),
            Err(SyncError::RecipientMismatch)
        ));
        // 根归属与 AAD 不一致
        let other_root = RootSecretV1::generate([8u8; 16], 7, &mut rng).unwrap();
        assert!(seal_epoch_grant(&aad.recipient_x25519_pub, &other_root, &aad).is_err());
    }

    #[test]
    fn aad_cde_roundtrip_and_field_bound() {
        let mut rng = TestRng::new(25);
        let (_secret, aad) = sample_aad(&mut rng);
        let bytes = aad.encode().unwrap();
        let back = GrantAadV1::decode(&bytes).unwrap();
        assert_eq!(aad, back);
        assert_eq!(bytes, back.encode().unwrap());
        assert_eq!(bytes[0], 0xa7, "7 项 map 头（0xa7）");
        // 末尾是 grant_id（16B）
        assert_eq!(&bytes[bytes.len() - 16..], &[9u8; 16]);
        // 尾部多余
        let mut long = bytes;
        long.push(0);
        assert!(GrantAadV1::decode(&long).is_err());
    }

    #[test]
    fn aad_unknown_field_rejected() {
        // 8 项 map → 字段数不符
        let value = menghunote_sync_proto::CdeValue::Map(vec![
            (0, menghunote_sync_proto::CdeValue::UInt(1)),
            (1, menghunote_sync_proto::CdeValue::Bytes(vec![0; 16])),
            (2, menghunote_sync_proto::CdeValue::UInt(7)),
            (3, menghunote_sync_proto::CdeValue::Bytes(vec![0; 16])),
            (4, menghunote_sync_proto::CdeValue::Bytes(vec![0; 32])),
            (5, menghunote_sync_proto::CdeValue::UInt(3)),
            (6, menghunote_sync_proto::CdeValue::Bytes(vec![0; 16])),
            (7, menghunote_sync_proto::CdeValue::UInt(0)),
        ]);
        let bytes = menghunote_sync_proto::encode_cde(&value).unwrap();
        assert!(GrantAadV1::decode(&bytes).is_err());
    }

    #[test]
    fn signature_input_matches_adr_coverage() {
        let enc = [0x11u8; 32];
        let ct = b"ciphertext";
        let (_secret, aad) = sample_aad(&mut TestRng::new(1));
        let input = grant_signature_input(&aad, &enc, ct).unwrap();
        let aad_bytes = aad.aad_bytes().unwrap();
        assert_eq!(&input[..aad_bytes.len()], aad_bytes.as_slice());
        assert_eq!(&input[aad_bytes.len()..aad_bytes.len() + 32], &enc);
        assert_eq!(
            &input[aad_bytes.len() + 32..],
            Sha256::digest(ct).as_slice()
        );
        // 覆盖范围变化必须改变待签字节
        let mut ct2 = ct.to_vec();
        ct2.push(0);
        assert_ne!(input, grant_signature_input(&aad, &enc, &ct2).unwrap());

        // `SealedEpochGrant::signature_input` 与自由函数一致
        let sealed = SealedEpochGrant {
            enc,
            ciphertext: ct.to_vec(),
        };
        assert_eq!(
            sealed.signature_input(&aad).unwrap(),
            grant_signature_input(&aad, &enc, ct).unwrap()
        );
    }

    #[test]
    fn x25519_public_key_is_derived_deterministically() {
        let mut rng = TestRng::new(26);
        let secret = Secret32::generate(&mut rng).unwrap();
        let pub_a = x25519_public_key(&secret);
        let pub_b = x25519_public_key(&Secret32::from_array(*secret.expose()));
        assert_eq!(pub_a, pub_b);
        assert_ne!(pub_a, [0u8; 32]);
    }
}
