//! MNE1 信封的**结构化**部分（ADR 0006 §2 / §16）。
//!
//! relay 只验证格式与"固定切片"规则，**不**做 AEAD 解密、不解析密文载荷（那是客户端的活）。
//! 本模块的 196 字节 header 布局是冻结的 wire 契约；`sync-proto` 落地后应改为引用该 crate 的
//! 解析结果，`relay` 侧只保留 §16 的写入前一致性校验（结构相同，替换成本低）。
//!
//! 关键不变量：
//! - 所有整数大端；u64 字段必须 `<= i64::MAX`（ADR §2，写入 SQLite 前）。
//! - `envelope.len() == 196 + ciphertext_len`，`ciphertext_len == this_plaintext_len + 16`。
//! - §16.1：UPDATE/SNAPSHOT 的 `generation > 0`；MANIFEST/ASSET/KEY_SLOT 的 `generation == 0`。
//! - §16.2：只有 UPDATE 可以有非零 `sender_seq`（且 complete 时还要与载荷 `origin_seq` 相等）。
//! - §16.3：固定切片 `cap_plain = ASSET ? 4MiB-212 : 1MiB-212`；非末片满，末片为正剩余；
//!   `part_count == ceil(total/cap)`；KEY_SLOT 明文 ≤4KiB 且单片。

use crate::error::{Error, ErrorCode, Result};
use crate::ids::{be32, be64, Hash32, Id16};
use menghunote_sync_proto as proto;

/// 固定 header 长度。
pub const HEADER_LEN: usize = 196;
/// AEAD tag 长度（XChaCha20-Poly1305）。
pub const TAG_LEN: usize = 16;
/// 最小 envelope 字节数（空明文也有 16 字节 tag）。
pub const MIN_ENVELOPE_LEN: usize = HEADER_LEN + TAG_LEN;
/// 非 asset 单片 envelope 上限（ADR 0006 §5：非 asset 的 MNE1 片总长 ≤1MiB）。
pub const MAX_NON_ASSET_ENVELOPE: usize = 1024 * 1024;
/// asset 单片 envelope 上限（ADR 0006 §5：asset 片 ≤4MiB）。
pub const MAX_ASSET_ENVELOPE: usize = 4 * 1024 * 1024;
/// KEY_SLOT 明文上限（ADR 0006 §5）。
pub const MAX_KEY_SLOT_PLAINTEXT: u64 = 4096;
/// 信封固定开销（header + tag），用于计算每片明文容量。
pub const ENVELOPE_OVERHEAD: u64 = (HEADER_LEN + TAG_LEN) as u64;
/// asset 每片明文容量 `4MiB - 212`。
pub const ASSET_CAP_PLAIN: u64 = (MAX_ASSET_ENVELOPE as u64) - ENVELOPE_OVERHEAD;
/// 非 asset 每片明文容量 `1MiB - 212`。
pub const NON_ASSET_CAP_PLAIN: u64 = (MAX_NON_ASSET_ENVELOPE as u64) - ENVELOPE_OVERHEAD;

/// `MNE1` magic。
pub const MAGIC: [u8; 4] = *b"MNE1";
/// 协议版本。
pub const PROTOCOL_VERSION: u16 = 1;
/// 唯一支持的 cipher suite（XChaCha20-Poly1305）。
pub const CIPHER_SUITE_XCHACHA20_POLY1305: u8 = 1;

/// 对象类型（ADR 0006 §2 header.type）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ObjectType {
    Update = 1,
    Snapshot = 2,
    Manifest = 3,
    Asset = 4,
    KeySlot = 5,
}

impl ObjectType {
    /// 从 wire 字节解析；未知值拒绝（§16.4：v1 不接受未知 tag）。
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Update),
            2 => Some(Self::Snapshot),
            3 => Some(Self::Manifest),
            4 => Some(Self::Asset),
            5 => Some(Self::KeySlot),
            _ => None,
        }
    }

    /// 从 SQLite 的 INTEGER 列还原（列上有 CHECK 1..5）。
    pub fn from_i64(value: i64) -> Option<Self> {
        u8::try_from(value).ok().and_then(Self::from_u8)
    }

    /// wire 字节。
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// SQLite INTEGER 形态。
    pub const fn as_i64(self) -> i64 {
        self as i64
    }

    /// 是否资产（决定分片上限）。
    pub const fn is_asset(self) -> bool {
        matches!(self, Self::Asset)
    }

    /// 是否进 log（ADR 0006 §5：update/asset 完成会追加事件；checkpoint 有自己的 log kind）。
    pub const fn logs_on_complete(self) -> bool {
        matches!(self, Self::Update | Self::Asset)
    }
}

/// 解析后的固定 header。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub protocol_version: u16,
    pub object_type: ObjectType,
    pub cipher_suite: u8,
    pub flags: u16,
    pub vault_id: Id16,
    pub object_id: Hash32,
    pub message_id: Id16,
    pub producer_device_id: Id16,
    pub generation: u64,
    pub key_epoch: u64,
    pub sender_seq: u64,
    pub part_index: u32,
    pub part_count: u32,
    pub total_plaintext_len: u64,
    pub this_plaintext_len: u32,
    pub ciphertext_len: u32,
    pub context_hash: Hash32,
    pub nonce: [u8; 24],
}

fn bad(reason: &'static str) -> Error {
    let _ = reason; // 具体原因只进 tracing/诊断，不出 wire（避免给攻击者 oracle）。
    Error::Api(crate::error::ApiError::new(ErrorCode::InvalidRequest))
}

fn to_i64_checked(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| bad("u64 field exceeds i64::MAX"))
}

impl Header {
    /// 每个分片的明文容量（§16.3）。
    pub const fn cap_plain(&self) -> u64 {
        if self.object_type.is_asset() {
            ASSET_CAP_PLAIN
        } else {
            NON_ASSET_CAP_PLAIN
        }
    }

    /// 单片 envelope 的字节上限。
    pub const fn max_envelope(&self) -> usize {
        if self.object_type.is_asset() {
            MAX_ASSET_ENVELOPE
        } else {
            MAX_NON_ASSET_ENVELOPE
        }
    }

    /// 本地防御性校验：结构一致性 + §16 规则（proto 已做一遍，这里独立复核）。
    ///
    /// 保留它是因为 relay 的写入路径不能依赖"另一个 crate 的校验永远正确"；两处实现都以
    /// ADR 为准，出现分歧时宁可拒绝（fail-closed）。
    pub fn validate_against(&self, envelope: &[u8]) -> Result<()> {
        // ADR §2：写 SQLite 前 u64 必须 <= i64::MAX。
        to_i64_checked(self.generation)?;
        to_i64_checked(self.key_epoch)?;
        to_i64_checked(self.sender_seq)?;
        to_i64_checked(self.total_plaintext_len)?;

        if envelope.len() > self.max_envelope() {
            return Err(Error::Api(crate::error::ApiError::new(
                ErrorCode::PayloadTooLarge,
            )));
        }
        if self.part_count == 0 {
            return Err(bad("part_count must be >= 1"));
        }
        if self.part_index >= self.part_count {
            return Err(bad("part_index out of range"));
        }
        if self.this_plaintext_len == 0 {
            return Err(bad("empty part payload"));
        }
        if u64::from(self.this_plaintext_len) > self.cap_plain() {
            return Err(bad("part plaintext exceeds cap"));
        }
        if self.ciphertext_len != self.this_plaintext_len + TAG_LEN as u32 {
            return Err(bad("ciphertext_len mismatch"));
        }
        if envelope.len() != HEADER_LEN + self.ciphertext_len as usize {
            return Err(bad("envelope length mismatch"));
        }

        // §16.1 generation 口径。
        match self.object_type {
            ObjectType::Update | ObjectType::Snapshot => {
                if self.generation == 0 {
                    return Err(bad("generation must be > 0 for update/snapshot"));
                }
            }
            ObjectType::Manifest | ObjectType::Asset | ObjectType::KeySlot => {
                if self.generation != 0 {
                    return Err(bad("generation must be 0 for manifest/asset/key_slot"));
                }
            }
        }
        // §16.2 sender_seq 口径（UPDATE 的值本身在 complete 时与载荷 origin_seq 比对）。
        if self.object_type != ObjectType::Update && self.sender_seq != 0 {
            return Err(bad("sender_seq must be 0 for non-update"));
        }
        // context_hash：只有 KEY_SLOT 允许（且必须）非零。
        let zero_context = self.context_hash == [0u8; 32];
        if self.object_type == ObjectType::KeySlot {
            if zero_context {
                return Err(bad("key_slot requires context_hash"));
            }
        } else if !zero_context {
            return Err(bad("context_hash must be zero for non key_slot"));
        }

        // §16.3 固定切片。
        if self.total_plaintext_len == 0 {
            return Err(bad("total_plaintext_len must be >= 1"));
        }
        let cap = self.cap_plain();
        let expected_parts = self.total_plaintext_len.div_ceil(cap);
        if u64::from(self.part_count) != expected_parts {
            return Err(bad("part_count does not match fixed slicing"));
        }
        let index = u64::from(self.part_index);
        let this_len = u64::from(self.this_plaintext_len);
        if index + 1 < expected_parts {
            if this_len != cap {
                return Err(bad("non-final part must be full"));
            }
        } else {
            let remaining = self.total_plaintext_len - (expected_parts - 1) * cap;
            if this_len != remaining {
                return Err(bad("final part length mismatch"));
            }
        }
        if self.object_type == ObjectType::KeySlot
            && (self.total_plaintext_len > MAX_KEY_SLOT_PLAINTEXT || self.part_count != 1)
        {
            return Err(bad("key_slot must be single part <= 4KiB"));
        }
        Ok(())
    }

    /// 从完整 envelope 字节解析并做**结构 + §16 一致性**校验。
    ///
    /// 解析与规范校验委托给 `sync-proto`（wire 单一真源，ADR §15/§16），relay 再跑一遍本地
    /// `validate_against` 作为防御（版本漂移时宁可拒绝，不静默接受）。
    pub fn parse(envelope: &[u8]) -> Result<Self> {
        // 先按 header 的 type 字节判上限：非 asset 片 >1MiB 必须是 413（而不是"格式错误"）。
        if envelope.len() >= 9 {
            let type_byte = envelope[8];
            let limit = if type_byte == ObjectType::Asset.as_u8() {
                MAX_ASSET_ENVELOPE
            } else {
                MAX_NON_ASSET_ENVELOPE
            };
            if envelope.len() > limit {
                return Err(Error::Api(crate::error::ApiError::new(
                    ErrorCode::PayloadTooLarge,
                )));
            }
        }
        let decoded = proto::Envelope::decode(envelope).map_err(map_proto_error)?;
        let h = decoded.header;
        let object_type = ObjectType::from_u8(h.object_type.as_u8())
            .ok_or_else(|| Error::api(ErrorCode::InvalidRequest))?;
        let header = Self {
            protocol_version: h.protocol_version,
            object_type,
            cipher_suite: h.cipher_suite,
            flags: h.flags,
            vault_id: h.vault_id,
            object_id: h.object_id,
            message_id: h.message_id,
            producer_device_id: h.producer_device_id,
            generation: h.generation,
            key_epoch: h.key_epoch,
            sender_seq: h.sender_seq,
            part_index: h.part_index,
            part_count: h.part_count,
            total_plaintext_len: h.total_plaintext_len,
            this_plaintext_len: h.this_plaintext_len,
            ciphertext_len: h.ciphertext_len,
            context_hash: h.context_hash,
            nonce: h.nonce,
        };
        header.validate_against(envelope)?;
        Ok(header)
    }

    /// 序列化 196 字节 header（测试与客户端 fixture 用；`body` 追加在其后）。
    pub fn encode(&self, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&self.protocol_version.to_be_bytes());
        out.extend_from_slice(&(HEADER_LEN as u16).to_be_bytes());
        out.push(self.object_type.as_u8());
        out.push(self.cipher_suite);
        out.extend_from_slice(&self.flags.to_be_bytes());
        out.extend_from_slice(&self.vault_id);
        out.extend_from_slice(&self.object_id);
        out.extend_from_slice(&self.message_id);
        out.extend_from_slice(&self.producer_device_id);
        out.extend_from_slice(&be64(self.generation));
        out.extend_from_slice(&be64(self.key_epoch));
        out.extend_from_slice(&be64(self.sender_seq));
        out.extend_from_slice(&be32(self.part_index));
        out.extend_from_slice(&be32(self.part_count));
        out.extend_from_slice(&be64(self.total_plaintext_len));
        out.extend_from_slice(&be32(self.this_plaintext_len));
        out.extend_from_slice(&be32(self.ciphertext_len));
        out.extend_from_slice(&self.context_hash);
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(body);
        out
    }
}

/// proto → relay 错误映射（结构错误一律 400，超限 413）。
fn map_proto_error(err: proto::ProtoError) -> Error {
    match err {
        proto::ProtoError::TooLarge => Error::api(ErrorCode::PayloadTooLarge),
        _ => Error::api(ErrorCode::InvalidRequest),
    }
}

/// 对象摘要（ADR 0006 §2）：
/// `SHA256(BE32(part_count) || concat(BE32(part_index) || SHA256(exact_envelope_part)))`，按片号排序。
///
/// 输入字节由 `sync-proto::object_digest_input` 构造（wire 单一真源），SHA 在本 crate 计算。
pub fn object_digest(part_count: u32, ordered_parts: &[(u32, Hash32)]) -> Result<Hash32> {
    if ordered_parts.len() != part_count as usize {
        return Err(Error::api(ErrorCode::Incomplete));
    }
    for (expected_index, (part_no, _)) in ordered_parts.iter().enumerate() {
        if *part_no != expected_index as u32 {
            return Err(Error::Invariant("object parts are not contiguous"));
        }
    }
    let hashes: Vec<Hash32> = ordered_parts.iter().map(|(_, digest)| *digest).collect();
    let input = proto::object_digest_input(part_count, &hashes).map_err(map_proto_error)?;
    Ok(crate::ids::sha256(&input))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(object_type: ObjectType, total: u64, part_count: u32) -> Header {
        let cap = if object_type.is_asset() {
            ASSET_CAP_PLAIN
        } else {
            NON_ASSET_CAP_PLAIN
        };
        let _ = cap;
        Header {
            protocol_version: PROTOCOL_VERSION,
            object_type,
            cipher_suite: CIPHER_SUITE_XCHACHA20_POLY1305,
            flags: 0,
            vault_id: [1u8; 16],
            object_id: [2u8; 32],
            message_id: [3u8; 16],
            producer_device_id: [4u8; 16],
            generation: if matches!(object_type, ObjectType::Update | ObjectType::Snapshot) {
                1
            } else {
                0
            },
            key_epoch: 1,
            sender_seq: if object_type == ObjectType::Update {
                1
            } else {
                0
            },
            part_index: 0,
            part_count,
            total_plaintext_len: total,
            this_plaintext_len: u32::try_from(total.min(cap)).expect("fits"),
            ciphertext_len: u32::try_from(total.min(cap)).expect("fits") + 16,
            context_hash: if object_type == ObjectType::KeySlot {
                [9u8; 32]
            } else {
                [0u8; 32]
            },
            nonce: [5u8; 24],
        }
    }

    fn body_for(h: &Header) -> Vec<u8> {
        vec![0xAAu8; h.ciphertext_len as usize]
    }

    fn envelope_bytes(h: &Header) -> Vec<u8> {
        h.encode(&body_for(h))
    }

    #[test]
    fn parses_and_roundtrips_single_part() {
        let h = header(ObjectType::Update, 100, 1);
        let bytes = envelope_bytes(&h);
        assert_eq!(bytes.len(), MIN_ENVELOPE_LEN + 100);
        let parsed = Header::parse(&bytes).expect("parse");
        assert_eq!(parsed, h);
    }

    #[test]
    fn multi_part_fixed_slicing_is_enforced() {
        let cap = ASSET_CAP_PLAIN;
        let total = cap * 2 + 10;
        let parts = 3u32;
        // 首片必须满。
        let mut first = header(ObjectType::Asset, total, parts);
        first.this_plaintext_len = u32::try_from(cap).expect("fits");
        first.ciphertext_len = first.this_plaintext_len + 16;
        let bytes = envelope_bytes(&first);
        Header::parse(&bytes).expect("full first part is valid");

        // 首片不满 → 拒绝。
        let mut bad = first.clone();
        bad.this_plaintext_len -= 1;
        bad.ciphertext_len -= 1;
        let bytes = envelope_bytes(&bad);
        assert!(matches!(
            Header::parse(&bytes),
            Err(Error::Api(e)) if e.code == ErrorCode::InvalidRequest
        ));
    }

    #[test]
    fn rejects_wrong_part_count() {
        let mut h = header(ObjectType::Update, 10, 1);
        h.part_count = 2;
        h.this_plaintext_len = 10;
        h.ciphertext_len = 26;
        let bytes = envelope_bytes(&h);
        assert!(Header::parse(&bytes).is_err());
    }

    #[test]
    fn rejects_generation_and_sender_seq_violations() {
        let mut h = header(ObjectType::Snapshot, 10, 1);
        h.generation = 0;
        assert!(Header::parse(&envelope_bytes(&h)).is_err());

        let mut h = header(ObjectType::Asset, 10, 1);
        h.generation = 3;
        assert!(Header::parse(&envelope_bytes(&h)).is_err());

        let mut h = header(ObjectType::Snapshot, 10, 1);
        h.sender_seq = 5;
        assert!(Header::parse(&envelope_bytes(&h)).is_err());
    }

    #[test]
    fn rejects_bad_magic_and_flags() {
        let h = header(ObjectType::Update, 10, 1);
        let mut bytes = envelope_bytes(&h);
        bytes[0] = b'X';
        assert!(Header::parse(&bytes).is_err());

        let h = header(ObjectType::Update, 10, 1);
        let mut bytes = envelope_bytes(&h);
        bytes[10] = 1; // flags
        assert!(Header::parse(&bytes).is_err());
    }

    #[test]
    fn rejects_oversized_non_asset_part() {
        // 非 asset 片 >1MiB 必须 413。
        let mut h = header(ObjectType::Update, NON_ASSET_CAP_PLAIN, 1);
        h.this_plaintext_len = u32::try_from(NON_ASSET_CAP_PLAIN).expect("fits");
        h.ciphertext_len = h.this_plaintext_len + 16;
        let mut bytes = envelope_bytes(&h);
        bytes.resize(MAX_ASSET_ENVELOPE, 0);
        assert!(matches!(
            Header::parse(&bytes),
            Err(Error::Api(e)) if e.code == ErrorCode::PayloadTooLarge
        ));
    }

    #[test]
    fn key_slot_must_be_single_part() {
        let mut h = header(ObjectType::KeySlot, 5000, 1);
        h.this_plaintext_len = 5000;
        h.ciphertext_len = 5016;
        assert!(Header::parse(&envelope_bytes(&h)).is_err());

        let h = header(ObjectType::KeySlot, 100, 1);
        Header::parse(&envelope_bytes(&h)).expect("single small key slot ok");
    }

    #[test]
    fn object_digest_depends_on_order_and_count() {
        let parts = [(0u32, [1u8; 32]), (1, [2u8; 32])];
        let d1 = object_digest(2, &parts).expect("digest");
        let mut swapped = [(0u32, [2u8; 32]), (1, [1u8; 32])];
        let d2 = object_digest(2, &swapped).expect("digest");
        assert_ne!(d1, d2);
        // 片数不同 → 摘要不同（前缀 BE32(part_count) 参与）。
        let d3 = object_digest(2, &[parts[0]]).err();
        assert!(d3.is_some());
        swapped.reverse();
        // 顺序被反转（片号不连续）→ 内部不变量错误。
        assert!(matches!(
            object_digest(2, &swapped),
            Err(Error::Invariant(_))
        ));
    }
}
