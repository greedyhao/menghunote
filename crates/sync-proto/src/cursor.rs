//! 不可伪造游标（ADR 0006 §5）：`base64url(version || instance || vault ||
//! log_incarnation || last_seq_BE64 || HMAC32)`。
//!
//! 本 crate 不做 HMAC；`CursorMacVerifier` 由 relay 注入。

use crate::error::ProtoError;

pub const CURSOR_VERSION: u8 = 1;
pub const CURSOR_RAW_LEN: usize = 1 + 16 + 16 + 16 + 8 + 32;
pub const CURSOR_MAC_INPUT_LEN: usize = 57;
pub const CURSOR_MAC_LEN: usize = 32;
pub const CURSOR_BASE64_LEN: usize = 119;

/// 游标；`mac` 由 relay 的实例密钥验证。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub instance_id: [u8; 16],
    pub vault_id: [u8; 16],
    pub log_incarnation: [u8; 16],
    pub last_seq: u64,
    pub mac: [u8; CURSOR_MAC_LEN],
}

/// relay 提供的 HMAC 校验接口；sync-proto 不持有密钥。
pub trait CursorMacVerifier {
    fn verify(&self, input: &[u8], mac: &[u8; CURSOR_MAC_LEN]) -> bool;
}

impl Cursor {
    pub fn new(
        instance_id: [u8; 16],
        vault_id: [u8; 16],
        log_incarnation: [u8; 16],
        last_seq: u64,
        mac: [u8; CURSOR_MAC_LEN],
    ) -> Result<Self, ProtoError> {
        if last_seq > i64::MAX as u64 {
            return Err(ProtoError::Invalid(
                "cursor.last_seq 超过 i64::MAX".to_owned(),
            ));
        }
        Ok(Self {
            instance_id,
            vault_id,
            log_incarnation,
            last_seq,
            mac,
        })
    }

    /// 89B 原始游标（含 MAC）。
    pub fn encode_bytes(&self) -> Result<[u8; CURSOR_RAW_LEN], ProtoError> {
        if self.last_seq > i64::MAX as u64 {
            return Err(ProtoError::Invalid(
                "cursor.last_seq 超过 i64::MAX".to_owned(),
            ));
        }
        let mut out = [0u8; CURSOR_RAW_LEN];
        out[0] = CURSOR_VERSION;
        out[1..17].copy_from_slice(&self.instance_id);
        out[17..33].copy_from_slice(&self.vault_id);
        out[33..49].copy_from_slice(&self.log_incarnation);
        out[49..57].copy_from_slice(&self.last_seq.to_be_bytes());
        out[57..89].copy_from_slice(&self.mac);
        Ok(out)
    }

    /// 前 57B，作为 HMAC 的 input（不含 mac）。
    pub fn mac_input(&self) -> Result<[u8; CURSOR_MAC_INPUT_LEN], ProtoError> {
        let raw = self.encode_bytes()?;
        raw[..CURSOR_MAC_INPUT_LEN]
            .try_into()
            .map_err(|_| ProtoError::Truncated)
    }

    /// `MN/v1/cursor\0 || 前 57B`，relay 计算 HMAC 时使用。
    pub fn mac_input_with_domain(&self) -> Result<Vec<u8>, ProtoError> {
        let mut out = Vec::with_capacity(crate::constants::CURSOR_HMAC_DOMAIN.len() + 57);
        out.extend_from_slice(crate::constants::CURSOR_HMAC_DOMAIN);
        out.extend_from_slice(&self.mac_input()?);
        Ok(out)
    }

    /// 解码结构，不验 MAC；`last_seq`/版本仍严格校验。
    pub fn decode_bytes(bytes: &[u8]) -> Result<Self, ProtoError> {
        if bytes.len() != CURSOR_RAW_LEN {
            return Err(ProtoError::Truncated);
        }
        if bytes[0] != CURSOR_VERSION {
            return Err(ProtoError::UnsupportedVersion(u16::from(bytes[0])));
        }
        let last_seq = u64::from_be_bytes(bytes[49..57].try_into().expect("长度8"));
        if last_seq > i64::MAX as u64 {
            return Err(ProtoError::Invalid(
                "cursor.last_seq 超过 i64::MAX".to_owned(),
            ));
        }
        Ok(Self {
            instance_id: bytes[1..17].try_into().expect("长度16"),
            vault_id: bytes[17..33].try_into().expect("长度16"),
            log_incarnation: bytes[33..49].try_into().expect("长度16"),
            last_seq,
            mac: bytes[57..89].try_into().expect("长度32"),
        })
    }

    /// 结构解码 + relay 注入的 MAC 校验。
    pub fn decode_verified(
        bytes: &[u8],
        verifier: &dyn CursorMacVerifier,
    ) -> Result<Self, ProtoError> {
        let cursor = Self::decode_bytes(bytes)?;
        let input = cursor.mac_input()?;
        if !verifier.verify(&input, &cursor.mac) {
            return Err(ProtoError::Invalid("cursor MAC 校验失败".to_owned()));
        }
        Ok(cursor)
    }

    pub fn to_base64url(&self) -> Result<String, ProtoError> {
        let raw = self.encode_bytes()?;
        Ok(base64url_encode(&raw))
    }

    pub fn from_base64url(value: &str) -> Result<Self, ProtoError> {
        let raw = base64url_decode(value)?;
        Self::decode_bytes(&raw)
    }

    pub fn from_base64url_verified(
        value: &str,
        verifier: &dyn CursorMacVerifier,
    ) -> Result<Self, ProtoError> {
        let raw = base64url_decode(value)?;
        Self::decode_verified(&raw, verifier)
    }
}

fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() >= 2 {
            out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() == 3 {
            out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        }
    }
    out
}

fn base64url_decode(value: &str) -> Result<Vec<u8>, ProtoError> {
    if value.len() % 4 == 1 {
        return Err(ProtoError::Invalid("base64url 长度非法".to_owned()));
    }
    let mut out = Vec::with_capacity(value.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for (index, byte) in value.bytes().enumerate() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => {
                return Err(ProtoError::Invalid(
                    "cursor base64url 不带 padding".to_owned(),
                ))
            }
            _ => {
                return Err(ProtoError::Invalid(format!(
                    "base64url 第 {index} 字节非法"
                )))
            }
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    if bits >= 6 || (buffer != 0 && bits > 0) {
        return Err(ProtoError::Invalid("base64url 尾部有非零填充".to_owned()));
    }
    Ok(out)
}
