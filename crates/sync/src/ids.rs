//! 本 crate 私有的本地标识符与小工具。
//!
//! `VaultId` / `DeviceId` / `ObjectId` 等 wire 类型由 `menghunote-sync-proto` 定义
//! （都是定长字节别名），这里只补一个 **Loro peer id** 的本地类型和十六进制显示工具。
//! 这些 ID 都**不是秘密**，可以进日志/UI（恢复码材料还要打印 vault UUID）。

use crate::error::SyncError;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// 任意字节的小写十六进制（非秘密值显示用，例如设备指纹）。
#[must_use]
pub fn hex_of(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// 从十六进制解析定长字节（测试/恢复材料输入用）。
pub fn parse_hex<const N: usize>(s: &str) -> Result<[u8; N], SyncError> {
    if s.len() != N * 2 {
        return Err(SyncError::Identity("十六进制长度不符"));
    }
    let bytes = s.as_bytes();
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = (bytes[i * 2] as char)
            .to_digit(16)
            .ok_or(SyncError::Identity("十六进制字符非法"))?;
        let lo = (bytes[i * 2 + 1] as char)
            .to_digit(16)
            .ok_or(SyncError::Identity("十六进制字符非法"))?;
        *slot = ((hi << 4) | lo) as u8;
    }
    Ok(out)
}

/// 标准 UUID 字符串（8-4-4-4-12，小写）。恢复码材料需要打印 vault UUID。
#[must_use]
pub fn vault_uuid_hyphenated(vault_id: &crate::VaultId) -> String {
    let h = hex_of(vault_id);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Loro peer id（16B）。ADR 0001 §3.2：克隆备份/恢复入口必须换新 peer_id。
///
/// 不是秘密；`Debug`/`Display` 打印小写 hex。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId([u8; 16]);

impl PeerId {
    /// 从 16 字节构造。
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// 原始字节。
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// 小写十六进制。
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex_of(&self.0)
    }
}

impl core::fmt::Debug for PeerId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PeerId({})", self.to_hex())
    }
}

impl core::fmt::Display for PeerId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let bytes: [u8; 4] = [0x00, 0x11, 0xab, 0xff];
        assert_eq!(hex_of(&bytes), "0011abff");
        assert_eq!(parse_hex::<4>("0011abff").unwrap(), bytes);
        assert!(parse_hex::<4>("0011abf").is_err());
        assert!(parse_hex::<4>("0011abfg").is_err());
    }

    #[test]
    fn peer_id_display_is_hex() {
        let peer = PeerId::from_bytes([0x12; 16]);
        assert_eq!(peer.to_hex().len(), 32);
        assert_eq!(
            format!("{peer}"),
            format!("{peer:?}").replace("PeerId(", "").replace(')', "")
        );
    }

    #[test]
    fn vault_uuid_layout() {
        let vault = [
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
            0x77, 0x88,
        ];
        assert_eq!(
            vault_uuid_hyphenated(&vault),
            "12345678-9abc-def0-1122-334455667788"
        );
    }
}
