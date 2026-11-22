//! MNE1 固定 196B header、AAD 与信封级规范化（ADR 0006 §2/§14/§16）。

use crate::error::ProtoError;
use crate::limits::{
    part_count_for, validate_slicing, CIPHER_SUITE_XCHACHA20_POLY1305, CONTEXT_HASH_LEN,
    HEADER_LEN, MAGIC, NONCE_LEN, PROTOCOL_VERSION,
};
use crate::records::{
    AssetPrefix, DeviceId, ManifestV1, MessageId, ObjectId, ObjectType, SnapshotV1, UpdateV1,
    VaultId,
};

/// AAD 域：`MN/e2e/v1` 后接**一个** 0x00 字节（§14）。
pub const AAD_DOMAIN: &[u8] = b"MN/e2e/v1\0";

/// MNE1 固定 header。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvelopeHeader {
    pub protocol_version: u16,
    pub header_len: u16,
    pub object_type: ObjectType,
    pub cipher_suite: u8,
    pub flags: u16,
    pub vault_id: VaultId,
    pub object_id: ObjectId,
    pub message_id: MessageId,
    pub producer_device_id: DeviceId,
    pub generation: u64,
    pub key_epoch: u64,
    pub sender_seq: u64,
    pub part_index: u32,
    pub part_count: u32,
    pub total_plaintext_len: u64,
    pub this_plaintext_len: u32,
    pub ciphertext_len: u32,
    pub context_hash: [u8; CONTEXT_HASH_LEN],
    pub nonce: [u8; NONCE_LEN],
}

impl Default for EnvelopeHeader {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            header_len: HEADER_LEN as u16,
            object_type: ObjectType::Update,
            cipher_suite: CIPHER_SUITE_XCHACHA20_POLY1305,
            flags: 0,
            vault_id: [0; 16],
            object_id: [0; 32],
            message_id: [0; 16],
            producer_device_id: [0; 16],
            generation: 1,
            key_epoch: 1,
            sender_seq: 1,
            part_index: 0,
            part_count: 1,
            total_plaintext_len: 1,
            this_plaintext_len: 1,
            ciphertext_len: 1 + 16,
            context_hash: [0; 32],
            nonce: [0; 24],
        }
    }
}

impl EnvelopeHeader {
    /// 固定 encode 为 196B，并做完整校验。
    pub fn encode(&self) -> Result<[u8; HEADER_LEN], ProtoError> {
        self.validate()?;
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&MAGIC);
        out[4..6].copy_from_slice(&self.protocol_version.to_be_bytes());
        out[6..8].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes());
        out[8] = self.object_type.as_u8();
        out[9] = self.cipher_suite;
        out[10..12].copy_from_slice(&self.flags.to_be_bytes());
        out[12..28].copy_from_slice(&self.vault_id);
        out[28..60].copy_from_slice(&self.object_id);
        out[60..76].copy_from_slice(&self.message_id);
        out[76..92].copy_from_slice(&self.producer_device_id);
        out[92..100].copy_from_slice(&self.generation.to_be_bytes());
        out[100..108].copy_from_slice(&self.key_epoch.to_be_bytes());
        out[108..116].copy_from_slice(&self.sender_seq.to_be_bytes());
        out[116..120].copy_from_slice(&self.part_index.to_be_bytes());
        out[120..124].copy_from_slice(&self.part_count.to_be_bytes());
        out[124..132].copy_from_slice(&self.total_plaintext_len.to_be_bytes());
        out[132..136].copy_from_slice(&self.this_plaintext_len.to_be_bytes());
        out[136..140].copy_from_slice(&self.ciphertext_len.to_be_bytes());
        out[140..172].copy_from_slice(&self.context_hash);
        out[172..196].copy_from_slice(&self.nonce);
        Ok(out)
    }

    /// 解析前 196B；更长的输入只取 header（给信封解析用）。
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        if bytes.len() < HEADER_LEN {
            return Err(ProtoError::Truncated);
        }
        if bytes[0..4] != MAGIC {
            return Err(ProtoError::Invalid("MNE1 magic 不匹配".to_owned()));
        }
        let protocol_version = u16::from_be_bytes([bytes[4], bytes[5]]);
        if protocol_version != PROTOCOL_VERSION {
            return Err(ProtoError::UnsupportedVersion(protocol_version));
        }
        let header_len = u16::from_be_bytes([bytes[6], bytes[7]]);
        if usize::from(header_len) != HEADER_LEN {
            return Err(ProtoError::Invalid(format!(
                "header_len={header_len}，必须是 196"
            )));
        }
        let object_type = ObjectType::try_from(bytes[8])?;
        let cipher_suite = bytes[9];
        let flags = u16::from_be_bytes([bytes[10], bytes[11]]);
        let value = Self {
            protocol_version,
            header_len,
            object_type,
            cipher_suite,
            flags,
            vault_id: bytes[12..28].try_into().expect("长度16"),
            object_id: bytes[28..60].try_into().expect("长度32"),
            message_id: bytes[60..76].try_into().expect("长度16"),
            producer_device_id: bytes[76..92].try_into().expect("长度16"),
            generation: read_u64(bytes, 92),
            key_epoch: read_u64(bytes, 100),
            sender_seq: read_u64(bytes, 108),
            part_index: read_u32(bytes, 116),
            part_count: read_u32(bytes, 120),
            total_plaintext_len: read_u64(bytes, 124),
            this_plaintext_len: read_u32(bytes, 132),
            ciphertext_len: read_u32(bytes, 136),
            context_hash: bytes[140..172].try_into().expect("长度32"),
            nonce: bytes[172..196].try_into().expect("长度24"),
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_bytes(&self) -> Result<[u8; HEADER_LEN], ProtoError> {
        self.encode()
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ProtoError::UnsupportedVersion(self.protocol_version));
        }
        if self.header_len as usize != HEADER_LEN {
            return Err(ProtoError::Invalid("header_len 必须为 196".to_owned()));
        }
        if self.cipher_suite != CIPHER_SUITE_XCHACHA20_POLY1305 {
            return Err(ProtoError::Invalid(format!(
                "未知 cipher suite {}",
                self.cipher_suite
            )));
        }
        if self.flags != 0 {
            return Err(ProtoError::Invalid("v1 flags 必须为 0".to_owned()));
        }
        self.validate_normalization()?;
        self.validate_lengths()?;
        Ok(())
    }

    /// §16.1/§16.2：不需要密文载荷就能校验的 generation/sender_seq 规则。
    pub fn validate_normalization(&self) -> Result<(), ProtoError> {
        match self.object_type {
            ObjectType::Update | ObjectType::Snapshot => {
                if self.generation == 0 {
                    return Err(ProtoError::Invalid(
                        "UPDATE/SNAPSHOT header.generation 必须 >0".to_owned(),
                    ));
                }
            }
            ObjectType::Manifest | ObjectType::Asset | ObjectType::KeySlot => {
                if self.generation != 0 {
                    return Err(ProtoError::Invalid(
                        "MANIFEST/ASSET/KEY_SLOT header.generation 必须为 0".to_owned(),
                    ));
                }
            }
        }
        if self.object_type != ObjectType::Update && self.sender_seq != 0 {
            return Err(ProtoError::Invalid(
                "非 UPDATE 的 header.sender_seq 必须为 0".to_owned(),
            ));
        }
        if self.generation > i64::MAX as u64
            || self.key_epoch > i64::MAX as u64
            || self.sender_seq > i64::MAX as u64
            || self.total_plaintext_len > i64::MAX as u64
        {
            return Err(ProtoError::Invalid(
                "header u64 字段超过 i64::MAX".to_owned(),
            ));
        }
        Ok(())
    }

    /// §16.3 固定切片和本类型的长度上限。
    pub fn validate_lengths(&self) -> Result<(), ProtoError> {
        let total = self.total_plaintext_len;
        if total == 0 {
            return Err(ProtoError::Invalid("载荷不能为空".to_owned()));
        }
        let expected_parts = part_count_for(total, self.object_type);
        if u64::from(self.part_count) != expected_parts {
            return Err(ProtoError::Invalid(format!(
                "part_count={}，固定切片应为 {expected_parts}",
                self.part_count
            )));
        }
        validate_slicing(
            self.object_type,
            total,
            self.part_index,
            self.part_count,
            self.this_plaintext_len,
        )?;
        let expected_ciphertext = self
            .this_plaintext_len
            .checked_add(16)
            .ok_or(ProtoError::TooLarge)?;
        if self.ciphertext_len != expected_ciphertext {
            return Err(ProtoError::Invalid(format!(
                "ciphertext_len={}，应为 this_plaintext_len+16={expected_ciphertext}",
                self.ciphertext_len
            )));
        }
        // 单片密文不得超过固定上限（header+tag 计入 envelope）。
        let max_ciphertext = match self.object_type {
            ObjectType::Asset => crate::limits::FOUR_MIB - HEADER_LEN,
            _ => crate::limits::ONE_MIB - HEADER_LEN,
        };
        if self.ciphertext_len as usize > max_ciphertext {
            return Err(ProtoError::TooLarge);
        }
        match self.object_type {
            ObjectType::Update => {
                if total > crate::limits::MAX_UPDATE_CDE_BYTES as u64 {
                    return Err(ProtoError::TooLarge);
                }
            }
            ObjectType::Manifest => {
                if total > crate::limits::MAX_MANIFEST_CDE_BYTES as u64 {
                    return Err(ProtoError::TooLarge);
                }
            }
            ObjectType::Snapshot => {
                if total > crate::limits::MAX_LORO_SNAPSHOT_BYTES as u64 {
                    return Err(ProtoError::TooLarge);
                }
            }
            ObjectType::Asset => {
                if total > crate::limits::MAX_ASSET_RAW_BYTES as u64 {
                    return Err(ProtoError::TooLarge);
                }
            }
            ObjectType::KeySlot => {
                if total > crate::limits::MAX_KEY_SLOT_PLAINTEXT as u64 {
                    return Err(ProtoError::TooLarge);
                }
            }
        }
        if self.object_type == ObjectType::KeySlot {
            if self.part_count != 1 {
                return Err(ProtoError::Invalid("KEY_SLOT 必须单片".to_owned()));
            }
            if self.part_index != 0 {
                return Err(ProtoError::Invalid(
                    "KEY_SLOT part_index 必须为 0".to_owned(),
                ));
            }
        }
        if self.object_type != ObjectType::KeySlot && self.context_hash != [0u8; 32] {
            return Err(ProtoError::Invalid(
                "非 KEY_SLOT 的 context_hash 必须全零".to_owned(),
            ));
        }
        Ok(())
    }

    /// §16.1：UPDATE 的 header.generation 必须等于载荷 doc generation。
    pub fn validate_update_payload(&self, update: &UpdateV1) -> Result<(), ProtoError> {
        self.validate()?;
        if self.object_type != ObjectType::Update {
            return Err(ProtoError::Invalid("header 不是 UPDATE".to_owned()));
        }
        if self.generation != update.generation {
            return Err(ProtoError::Invalid(
                "header.generation 与 UpdateV1.generation 不一致".to_owned(),
            ));
        }
        if self.sender_seq != update.origin_seq {
            return Err(ProtoError::Invalid(
                "header.sender_seq 与 UpdateV1.origin_seq 不一致".to_owned(),
            ));
        }
        Ok(())
    }

    /// §16.1：SNAPSHOT 的 generation 对齐，sender_seq 必须为 0。
    pub fn validate_snapshot_payload(&self, snapshot: &SnapshotV1) -> Result<(), ProtoError> {
        self.validate()?;
        if self.object_type != ObjectType::Snapshot {
            return Err(ProtoError::Invalid("header 不是 SNAPSHOT".to_owned()));
        }
        if self.generation != snapshot.generation {
            return Err(ProtoError::Invalid(
                "header.generation 与 SnapshotV1.generation 不一致".to_owned(),
            ));
        }
        if self.sender_seq != 0 {
            return Err(ProtoError::Invalid(
                "SNAPSHOT sender_seq 必须为 0".to_owned(),
            ));
        }
        Ok(())
    }

    /// §16.1/§16.2：MANIFEST 的 generation/sender_seq 均为 0，key_epoch 与载荷一致。
    pub fn validate_manifest_payload(&self, manifest: &ManifestV1) -> Result<(), ProtoError> {
        self.validate()?;
        if self.object_type != ObjectType::Manifest {
            return Err(ProtoError::Invalid("header 不是 MANIFEST".to_owned()));
        }
        if self.generation != 0 || self.sender_seq != 0 {
            return Err(ProtoError::Invalid(
                "MANIFEST generation/sender_seq 必须为 0".to_owned(),
            ));
        }
        if self.key_epoch != manifest.key_epoch {
            return Err(ProtoError::Invalid(
                "header.key_epoch 与 ManifestV1.key_epoch 不一致".to_owned(),
            ));
        }
        if self.vault_id != manifest.vault_id {
            return Err(ProtoError::Invalid(
                "header.vault_id 与 ManifestV1.vault_id 不一致".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn validate_asset_payload(&self, prefix: &AssetPrefix) -> Result<(), ProtoError> {
        self.validate()?;
        if self.object_type != ObjectType::Asset {
            return Err(ProtoError::Invalid("header 不是 ASSET".to_owned()));
        }
        if self.generation != 0 || self.sender_seq != 0 {
            return Err(ProtoError::Invalid(
                "ASSET generation/sender_seq 必须为 0".to_owned(),
            ));
        }
        if self.total_plaintext_len < 42 {
            return Err(ProtoError::Invalid("ASSET 明文不足以容纳前缀".to_owned()));
        }
        let _ = prefix;
        Ok(())
    }

    /// AAD = `MN/e2e/v1\0 || header || BE32(descriptor_len) || descriptor`。
    pub fn aad(&self, descriptor: &[u8]) -> Result<Vec<u8>, ProtoError> {
        self.validate()?;
        if self.object_type != ObjectType::KeySlot && !descriptor.is_empty() {
            return Err(ProtoError::Invalid(
                "非 KEY_SLOT 的 descriptor 必须为空".to_owned(),
            ));
        }
        if self.object_type == ObjectType::KeySlot && descriptor.is_empty() {
            return Err(ProtoError::Invalid("KEY_SLOT 必须带 descriptor".to_owned()));
        }
        let descriptor_len = u32::try_from(descriptor.len()).map_err(|_| ProtoError::TooLarge)?;
        let header = self.encode()?;
        let mut out = Vec::with_capacity(AAD_DOMAIN.len() + HEADER_LEN + 4 + descriptor.len());
        out.extend_from_slice(AAD_DOMAIN);
        out.extend_from_slice(&header);
        out.extend_from_slice(&descriptor_len.to_be_bytes());
        out.extend_from_slice(descriptor);
        Ok(out)
    }
}

/// 便捷构造 AAD（会做 header 校验与 descriptor 规则）。
pub fn aad(header: &EnvelopeHeader, descriptor: &[u8]) -> Result<Vec<u8>, ProtoError> {
    header.aad(descriptor)
}

/// 一个完整 MNE1 信封。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub header: EnvelopeHeader,
    pub ciphertext: Vec<u8>,
}

impl Envelope {
    pub fn new(header: EnvelopeHeader, ciphertext: Vec<u8>) -> Result<Self, ProtoError> {
        let value = Self { header, ciphertext };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        self.header.validate()?;
        let expected = self.header.ciphertext_len as usize;
        if self.ciphertext.len() != expected {
            return Err(ProtoError::Invalid(format!(
                "ciphertext {}B，header 声明 {expected}B",
                self.ciphertext.len()
            )));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        self.validate()?;
        let header = self.header.encode()?;
        let mut out = Vec::with_capacity(HEADER_LEN + self.ciphertext.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(&self.ciphertext);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        if bytes.len() < HEADER_LEN {
            return Err(ProtoError::Truncated);
        }
        let header = EnvelopeHeader::decode(&bytes[..HEADER_LEN])?;
        let expected = HEADER_LEN + header.ciphertext_len as usize;
        if bytes.len() != expected {
            return Err(ProtoError::Invalid(format!(
                "信封总长 {}B，应为 {expected}B",
                bytes.len()
            )));
        }
        Ok(Self {
            header,
            ciphertext: bytes[HEADER_LEN..].to_vec(),
        })
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("长度4"))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(bytes[offset..offset + 8].try_into().expect("长度8"))
}
