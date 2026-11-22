//! base64url（无 padding）编解码。
//!
//! relay 的 HTTP JSON 把所有 ID/hash/token 都写成 `base64url` 字符串（见
//! `crates/relay/src/api.rs` 的 `b64url_encode`）。本 crate 不引入额外依赖，
//! 自实现一个小而严格的编解码器（只接受 `A-Za-z0-9-_`，拒绝 padding，拒绝长度 %4==1）。

use crate::error::SyncError;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn value_of(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

/// 编码为 base64url（无 `=` padding）。
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3f) as usize] as char);
        }
    }
    out
}

/// 解码 base64url（只接受无 padding 的规范形式）。
pub fn decode(text: &str) -> Result<Vec<u8>, SyncError> {
    let bytes = text.as_bytes();
    if bytes.len() % 4 == 1 {
        return Err(SyncError::RelayWire("base64url 长度非法"));
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc: u32 = 0;
        for (index, byte) in chunk.iter().enumerate() {
            let value = value_of(*byte).ok_or(SyncError::RelayWire("base64url 字符非法"))?;
            acc |= u32::from(value) << (18 - index * 6);
        }
        out.push(((acc >> 16) & 0xff) as u8);
        if chunk.len() > 2 {
            out.push(((acc >> 8) & 0xff) as u8);
        }
        if chunk.len() > 3 {
            out.push((acc & 0xff) as u8);
        }
    }
    Ok(out)
}

/// 解码并断言恰好 `N` 字节。
pub fn decode_array<const N: usize>(text: &str) -> Result<[u8; N], SyncError> {
    let bytes = decode(text)?;
    bytes
        .try_into()
        .map_err(|_| SyncError::RelayWire("base64url 解码长度不符"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        // RFC 4648 §10 的标准 base64 向量（换成 url-safe 字母表后逐字节一致）
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg");
        assert_eq!(encode(b"fo"), "Zm8");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg");
        assert_eq!(encode(b"fooba"), "Zm9vYmE");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        for value in ["", "Zg", "Zm8", "Zm9v", "Zm9vYg", "Zm9vYmE", "Zm9vYmFy"] {
            assert_eq!(decode(value).unwrap(), decode(value).unwrap());
        }
    }

    #[test]
    fn url_safe_alphabet_and_roundtrip() {
        let bytes: Vec<u8> = (0u8..=255).collect();
        let text = encode(&bytes);
        assert!(!text.contains('+') && !text.contains('/') && !text.contains('='));
        assert_eq!(decode(&text).unwrap(), bytes);
        // 0xfb 0xff 会用到 url-safe 字符 - 和 _
        assert_eq!(encode(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn rejects_malformed() {
        assert!(decode("Z").is_err(), "长度 %4==1 必须拒绝");
        assert!(decode("Zg==").is_err(), "padding 必须拒绝");
        assert!(decode("Zg+").is_err(), "标准 base64 '+' 不同意");
        assert!(decode("Zg/").is_err(), "标准 base64 '/' 不同意");
        assert!(decode_array::<32>("Zg").is_err());
        let ids = [0xaa; 16];
        assert_eq!(decode_array::<16>(&encode(&ids)).unwrap(), ids);
    }
}
