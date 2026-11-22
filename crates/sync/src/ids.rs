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

    /// 映射到 Loro 的 `u64` peer id（**确定性**，同一条 16B 身份每次都得同一个值）。
    ///
    /// 规则（ADR 0009 切片 2a；`genesis::GenesisMaterials::peer_id_u64` 与
    /// `identity::LocalIdentityV1::loro_peer_id` 都委托到这里，**唯一实现**）：
    ///
    /// ```text
    /// u64 = BE(peer_id[0..8]) & LORO_PEER_ID_MASK
    /// u64 == 0 → 1                      // 0 不是合法的 Loro peer
    /// ```
    ///
    /// - **前 8 字节大端**：字段本身就是随机 16B，取哪一半都一样；固定取前半段，
    ///   保证"同一身份 → 同一 u64"可复现（重启/重装/重放都不变）；
    /// - **清最高位**（[`LORO_PEER_ID_MASK`]）：Loro 的内部计数器是 `i64`，peer 的高位
    ///   会在极端计数下把 `counter` 推到负区间；因此只保留 63 位。这条与 genesis 的
    ///   既有口径一致（`docs/adr/0006-sync-v1.md` 的 peer 生成即如此），不能两边不同；
    /// - **0 → 1**：Loro 不接受 peer 0；映射到 1 意味着"映射值 0 与 1 的两类身份共用
    ///   peer 1"（两类各自的概率都是 2^-63）；
    /// - **碰撞边界**：128 → 63 位截断，不同身份**可能**撞到同一 u64（生日界约 2^32 个
    ///   身份才到 50%）。dev-sync 每个实例一个独立库、同库不混身份，这个边界可接受；
    ///   但跨设备同步一旦使用同一库，撞 peer 会让 CRDT 的 op id 冲突——届时必须用 16B
    ///   [`PeerId`] 做权威标识并拒绝复用，不能只靠这个 u64。
    #[must_use]
    pub fn loro_u64(&self) -> u64 {
        let mut head = [0u8; 8];
        head.copy_from_slice(&self.0[..8]);
        let masked = u64::from_be_bytes(head) & LORO_PEER_ID_MASK;
        // 0 不是合法 peer；折到 1（见方法文档的碰撞边界）。
        if masked == 0 {
            1
        } else {
            masked
        }
    }
}

/// Loro peer id 的可用位宽掩码：63 位（最高位清零，避免越过 `i64` 计数器域）。
///
/// 见 [`PeerId::loro_u64`]；genesis 与 app 的 dev identity 共用这一个常量。
pub const LORO_PEER_ID_MASK: u64 = 0x7fff_ffff_ffff_ffff;

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
    fn loro_u64_is_big_endian_head_and_clears_high_bit() {
        // 前 8 字节大端 + 最高位清零。
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        assert_eq!(PeerId::from_bytes(bytes).loro_u64(), 0x0123_4567_89ab_cdef);
        // 最高位置位的身份：清掉最高位，落在 i64 计数器域内。
        assert_eq!(PeerId::from_bytes([0xff; 16]).loro_u64(), LORO_PEER_ID_MASK);
        // 只有后 8 字节不同 → u64 相同（映射只看前半段）；这也是碰撞边界的来源。
        let mut tail_only = [0u8; 16];
        tail_only[8..].copy_from_slice(&[0xaa; 8]);
        assert_eq!(PeerId::from_bytes(tail_only).loro_u64(), 1);
        // 确定性：同一字节永远同一结果。
        let peer = PeerId::from_bytes([0x5a; 16]);
        assert_eq!(peer.loro_u64(), peer.loro_u64());
    }

    #[test]
    fn loro_u64_never_returns_zero() {
        // 全 0 身份：0 不是合法 Loro peer，按文档折到 1。
        assert_eq!(PeerId::from_bytes([0u8; 16]).loro_u64(), 1);
        // 只有最高位（会被清掉）：同样折到 1。
        let mut high_only = [0u8; 16];
        high_only[0] = 0x80;
        assert_eq!(PeerId::from_bytes(high_only).loro_u64(), 1);
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
