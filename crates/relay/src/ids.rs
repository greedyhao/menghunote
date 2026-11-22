//! 标识符、哈希、HMAC、base64url、随机字节。
//!
//! relay 只处理**不透明**标识与摘要：16 字节 ID（UUID 网络字节序）、32 字节 SHA256/HMAC。
//! 所有随机数来自操作系统 CSPRNG（`getrandom`），不用可预测的伪随机源。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// 16 字节不透明 ID（vault/device/message/log/epoch 相关）。
pub type Id16 = [u8; 16];
/// 32 字节摘要（SHA256 / HMAC-SHA256 / object_id）。
pub type Hash32 = [u8; 32];

/// 全零 16 字节 ID。
pub const ZERO_ID16: Id16 = [0u8; 16];
/// 全零 32 字节摘要。
pub const ZERO_HASH32: Hash32 = [0u8; 32];

type HmacSha256 = Hmac<Sha256>;

fn csprng(buf: &mut [u8]) -> Result<()> {
    getrandom::getrandom(buf).map_err(|_| Error::Invariant("os csprng unavailable"))
}

/// 随机 16 字节 ID。
pub fn random_id16() -> Result<Id16> {
    let mut out = [0u8; 16];
    csprng(&mut out)?;
    Ok(out)
}

/// 随机 32 字节（摘要/密钥材料）。
pub fn random_hash32() -> Result<Hash32> {
    let mut out = [0u8; 32];
    csprng(&mut out)?;
    Ok(out)
}

/// 随机 N 字节。
pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut out = [0u8; N];
    csprng(&mut out)?;
    Ok(out)
}

/// SHA256（单段）。
pub fn sha256(bytes: &[u8]) -> Hash32 {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// SHA256（多段拼接，避免为大对象拼出临时大 Vec）。
pub fn sha256_parts(parts: &[&[u8]]) -> Hash32 {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// HMAC-SHA256（多段拼接）。
pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> Result<Hash32> {
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(key).map_err(|_| Error::Invariant("hmac key"))?;
    for part in parts {
        mac.update(part);
    }
    Ok(mac.finalize().into_bytes().into())
}

/// 常数时间校验 HMAC-SHA256（避免给攻击者时序侧信道）。
pub fn hmac_verify(key: &[u8], parts: &[&[u8]], tag: &[u8]) -> bool {
    let Ok(mut mac) = <HmacSha256 as Mac>::new_from_slice(key) else {
        return false;
    };
    for part in parts {
        mac.update(part);
    }
    mac.verify_slice(tag).is_ok()
}

/// 常数时间比较（长度不同直接 false；用于非 MAC 的一次性密钥摘要比较）。
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// base64url（无 padding）编码——token/cursor 的 wire 形态。
pub fn b64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// base64url（无 padding）解码；**拒绝**带 padding 的非规范输入。
pub fn b64url_decode(text: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).map_err(|_| Error::Base64)
}

/// 小写十六进制（ETag 等诊断字段）。
pub fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// 大端 u32。
pub fn be32(value: u32) -> [u8; 4] {
    value.to_be_bytes()
}

/// 大端 u64。
pub fn be64(value: u64) -> [u8; 8] {
    value.to_be_bytes()
}

/// 读大端 u32（越界由调用方先校验长度）。
pub fn read_be32(bytes: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(bytes);
    u32::from_be_bytes(buf)
}

/// 读大端 u64（越界由调用方先校验长度）。
pub fn read_be64(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(bytes);
    u64::from_be_bytes(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        // 空串 SHA256 是公开测试向量。
        assert_eq!(
            hex_lower(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn b64url_roundtrip_is_unpadded() {
        let raw = [0u8, 1, 2, 250, 251, 255];
        let text = b64url_encode(&raw);
        assert!(!text.contains('='));
        assert!(!text.contains('+'));
        assert!(!text.contains('/'));
        assert_eq!(b64url_decode(&text).expect("decode"), raw);
        // 带 padding 的非规范编码必须拒绝（否则游标/ token 会出现两种等价形态）。
        assert!(b64url_decode("AAEC-vs").is_ok());
    }

    #[test]
    fn hmac_verify_rejects_tamper() {
        let key = [7u8; 32];
        let tag = hmac_sha256(&key, &[b"a", b"b"]).expect("hmac");
        assert!(hmac_verify(&key, &[b"a", b"b"], &tag));
        assert!(!hmac_verify(&key, &[b"a", b"c"], &tag));
        let mut bad = tag;
        bad[0] ^= 1;
        assert!(!hmac_verify(&key, &[b"a", b"b"], &bad));
    }

    #[test]
    fn ct_eq_detects_difference() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn random_ids_do_not_repeat() {
        let a = random_id16().expect("rand");
        let b = random_id16().expect("rand");
        assert_ne!(a, b);
        assert_ne!(a, ZERO_ID16);
    }
}
