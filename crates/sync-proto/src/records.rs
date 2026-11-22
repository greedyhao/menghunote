//! ADR 0006 §2 / §15 / §16 的 CDE record 类型与手写编解码。
//!
//! 所有 record 都是整数键 map；字段按本文件/ADR 的声明顺序从 0 连续编号。
//! optional v1 不省略未定义字段；未知键/未知 tag 直接拒绝。

use crate::cde::{
    decode, encode, expect_array, expect_bool, expect_bytes, expect_bytes_n, expect_i64_max,
    expect_text, expect_u16, expect_uint, into_record_fields, CdeValue, MAX_CDE_BYTES,
};
use crate::error::ProtoError;
use crate::limits::{MAX_ASSET_MIME_BYTES, MAX_ASSET_RAW_BYTES, MAX_DEVICES, MAX_REFS};

pub type VaultId = [u8; 16];
pub type ObjectId = [u8; 32];
pub type MessageId = [u8; 16];
pub type DeviceId = [u8; 16];
pub type CheckpointId = [u8; 16];
pub type ChallengeId = [u8; 16];
pub type RelayInstanceId = [u8; 16];
pub type Sha256 = [u8; 32];
pub type Ed25519PublicKey = [u8; 32];
pub type X25519PublicKey = [u8; 32];

/// 对象类型（MNE1 header.type）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum ObjectType {
    Update = 1,
    Snapshot = 2,
    Manifest = 3,
    Asset = 4,
    KeySlot = 5,
}

impl ObjectType {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Update => "UPDATE",
            Self::Snapshot => "SNAPSHOT",
            Self::Manifest => "MANIFEST",
            Self::Asset => "ASSET",
            Self::KeySlot => "KEY_SLOT",
        }
    }

    pub fn is_data(self) -> bool {
        !matches!(self, Self::KeySlot)
    }

    /// UPDATE/SNAPSHOT 的 generation 必须 >0；其他类型固定为 0（§16.1）。
    pub fn requires_generation(self) -> bool {
        matches!(self, Self::Update | Self::Snapshot)
    }
}

impl TryFrom<u8> for ObjectType {
    type Error = ProtoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Update),
            2 => Ok(Self::Snapshot),
            3 => Ok(Self::Manifest),
            4 => Ok(Self::Asset),
            5 => Ok(Self::KeySlot),
            other => Err(ProtoError::Invalid(format!("未知对象类型 {other}"))),
        }
    }
}

impl From<ObjectType> for u8 {
    fn from(value: ObjectType) -> Self {
        value.as_u8()
    }
}

/// 密钥槽种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SlotKind {
    Password = 1,
    Recovery = 2,
}

impl TryFrom<u8> for SlotKind {
    type Error = ProtoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Password),
            2 => Ok(Self::Recovery),
            other => Err(ProtoError::Invalid(format!("未知槽类型 {other}"))),
        }
    }
}

impl From<SlotKind> for u8 {
    fn from(value: SlotKind) -> Self {
        value as u8
    }
}

/// 鉴权 realm。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AuthRealm {
    Device = 1,
    Management = 2,
}

impl TryFrom<u8> for AuthRealm {
    type Error = ProtoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Device),
            2 => Ok(Self::Management),
            other => Err(ProtoError::Invalid(format!("未知 auth realm {other}"))),
        }
    }
}

impl From<AuthRealm> for u8 {
    fn from(value: AuthRealm) -> Self {
        value as u8
    }
}

/// HTTP method 的管理签名标签（GET=1..DELETE=5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HttpMethod {
    Get = 1,
    Post = 2,
    Put = 3,
    Patch = 4,
    Delete = 5,
}

impl TryFrom<u8> for HttpMethod {
    type Error = ProtoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Get),
            2 => Ok(Self::Post),
            3 => Ok(Self::Put),
            4 => Ok(Self::Patch),
            5 => Ok(Self::Delete),
            other => Err(ProtoError::Invalid(format!(
                "未知 HTTP method 标签 {other}"
            ))),
        }
    }
}

impl From<HttpMethod> for u8 {
    fn from(value: HttpMethod) -> Self {
        value as u8
    }
}

/// 设备权限位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rights(pub u32);

impl Rights {
    pub const READ: u32 = 1;
    pub const WRITE: u32 = 2;
    pub const ALL: u32 = Self::READ | Self::WRITE;

    pub fn new(bits: u32) -> Result<Self, ProtoError> {
        if bits == 0 || bits & !Self::ALL != 0 {
            return Err(ProtoError::Invalid(format!("非法 rights 位 {bits:#x}")));
        }
        Ok(Self(bits))
    }

    pub fn bits(self) -> u32 {
        self.0
    }

    pub fn can_read(self) -> bool {
        self.0 & Self::READ != 0
    }

    pub fn can_write(self) -> bool {
        self.0 & Self::WRITE != 0
    }
}

/// Argon2id 槽参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Argon2idParams {
    pub version: u16,
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
    pub salt: [u8; 16],
    pub output_len: u32,
}

/// 槽 KDF union；tag=1 Argon2id、tag=2 RecoveryHKDF。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kdf {
    Argon2id(Argon2idParams),
    RecoveryHkdf,
}

impl Kdf {
    fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        match self {
            Self::Argon2id(params) => {
                if params.version != 0x13 {
                    return Err(ProtoError::UnsupportedVersion(params.version));
                }
                if !(65_536..=262_144).contains(&params.m_kib)
                    || !(3..=6).contains(&params.t)
                    || !(1..=4).contains(&params.p)
                    || params.output_len != 32
                {
                    return Err(ProtoError::Invalid("Argon2id 参数越界".to_owned()));
                }
                Ok(CdeValue::Array(vec![
                    CdeValue::UInt(1),
                    CdeValue::Map(vec![
                        (0, CdeValue::UInt(u64::from(params.version))),
                        (1, CdeValue::UInt(u64::from(params.m_kib))),
                        (2, CdeValue::UInt(u64::from(params.t))),
                        (3, CdeValue::UInt(u64::from(params.p))),
                        (4, CdeValue::Bytes(params.salt.to_vec())),
                        (5, CdeValue::UInt(u64::from(params.output_len))),
                    ]),
                ]))
            }
            Self::RecoveryHkdf => Ok(CdeValue::Array(vec![
                CdeValue::UInt(2),
                CdeValue::Map(Vec::new()),
            ])),
        }
    }

    fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let CdeValue::Array(items) = value else {
            return Err(ProtoError::Invalid("Kdf 必须是 [tag, params]".to_owned()));
        };
        if items.len() != 2 {
            return Err(ProtoError::Invalid("Kdf 长度必须为 2".to_owned()));
        }
        let mut items = items.into_iter();
        let tag = expect_uint(items.next().expect("长度2"), "Kdf.tag")?;
        let params = items.next().expect("长度2");
        match tag {
            1 => {
                let CdeValue::Map(entries) = params else {
                    return Err(ProtoError::Invalid("Argon2id params 必须是 map".to_owned()));
                };
                let fields = into_record_fields(CdeValue::Map(entries), 6)?;
                let version = expect_u16(fields[0].clone(), "argon2.version")?;
                let m_kib = crate::cde::expect_u32(fields[1].clone(), "argon2.m_kib")?;
                let t = crate::cde::expect_u32(fields[2].clone(), "argon2.t")?;
                let p = crate::cde::expect_u32(fields[3].clone(), "argon2.p")?;
                let salt = expect_bytes_n::<16>(fields[4].clone(), "argon2.salt")?;
                let output_len = crate::cde::expect_u32(fields[5].clone(), "argon2.output_len")?;
                let params = Argon2idParams {
                    version,
                    m_kib,
                    t,
                    p,
                    salt,
                    output_len,
                };
                Self::Argon2id(params).to_cde()?;
                Ok(Self::Argon2id(Argon2idParams {
                    version,
                    m_kib,
                    t,
                    p,
                    salt,
                    output_len,
                }))
            }
            2 => {
                let CdeValue::Map(entries) = params else {
                    return Err(ProtoError::Invalid(
                        "RecoveryHKDF params 必须是空 map".to_owned(),
                    ));
                };
                if !entries.is_empty() {
                    return Err(ProtoError::Invalid(
                        "RecoveryHKDF params 必须为空".to_owned(),
                    ));
                }
                Ok(Self::RecoveryHkdf)
            }
            other => Err(ProtoError::Invalid(format!("未知 Kdf tag {other}"))),
        }
    }
}

/// 资产明文固定前缀：sha256[32] || BE64(byte_len) || BE16(mime_len) || mime。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetPrefix {
    pub sha256: Sha256,
    pub byte_len: u64,
    pub media_type: String,
}

impl AssetPrefix {
    pub fn new(
        sha256: Sha256,
        byte_len: u64,
        media_type: impl Into<String>,
    ) -> Result<Self, ProtoError> {
        let value = Self {
            sha256,
            byte_len,
            media_type: media_type.into(),
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.byte_len > MAX_ASSET_RAW_BYTES as u64 {
            return Err(ProtoError::TooLarge);
        }
        if self.media_type.len() > MAX_ASSET_MIME_BYTES {
            return Err(ProtoError::TooLarge);
        }
        if self.media_type.is_empty() {
            return Err(ProtoError::Invalid("asset media_type 不能为空".to_owned()));
        }
        Ok(())
    }

    /// 只编码前缀（不含 data）。
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        self.validate()?;
        let mut out = Vec::with_capacity(32 + 8 + 2 + self.media_type.len());
        out.extend_from_slice(&self.sha256);
        out.extend_from_slice(&self.byte_len.to_be_bytes());
        out.extend_from_slice(&(self.media_type.len() as u16).to_be_bytes());
        out.extend_from_slice(self.media_type.as_bytes());
        Ok(out)
    }

    /// 从前缀 + data 里解析前缀并返回数据偏移。
    pub fn parse(bytes: &[u8]) -> Result<(Self, usize), ProtoError> {
        if bytes.len() < 32 + 8 + 2 {
            return Err(ProtoError::Truncated);
        }
        let sha256 = bytes[0..32].try_into().expect("长度32");
        let byte_len = u64::from_be_bytes(bytes[32..40].try_into().expect("长度8"));
        let mime_len = usize::from(u16::from_be_bytes(bytes[40..42].try_into().expect("长度2")));
        if mime_len > MAX_ASSET_MIME_BYTES {
            return Err(ProtoError::TooLarge);
        }
        let prefix_len = 42 + mime_len;
        if bytes.len() < prefix_len {
            return Err(ProtoError::Truncated);
        }
        let media_type = std::str::from_utf8(&bytes[42..prefix_len])
            .map_err(|_| ProtoError::Invalid("asset mime 不是合法 UTF-8".to_owned()))?
            .to_owned();
        let value = Self {
            sha256,
            byte_len,
            media_type,
        };
        value.validate()?;
        Ok((value, prefix_len))
    }

    /// 解析完整资产明文，返回前缀与 data 切片。
    pub fn split_payload(bytes: &[u8]) -> Result<(Self, &[u8]), ProtoError> {
        if bytes.len() < 42 {
            return Err(ProtoError::Truncated);
        }
        let mime_len = usize::from(u16::from_be_bytes(bytes[40..42].try_into().expect("长度2")));
        if mime_len > MAX_ASSET_MIME_BYTES {
            return Err(ProtoError::TooLarge);
        }
        let prefix_len = 42 + mime_len;
        if bytes.len() < prefix_len {
            return Err(ProtoError::Truncated);
        }
        let media_type = std::str::from_utf8(&bytes[42..prefix_len])
            .map_err(|_| ProtoError::Invalid("asset mime 不是合法 UTF-8".to_owned()))?
            .to_owned();
        let value = Self {
            sha256: bytes[0..32].try_into().expect("长度32"),
            byte_len: u64::from_be_bytes(bytes[32..40].try_into().expect("长度8")),
            media_type,
        };
        value.validate()?;
        let data = &bytes[prefix_len..];
        if data.len() as u64 != value.byte_len {
            return Err(ProtoError::Invalid(format!(
                "asset data {}B 与前缀 byte_len {} 不符",
                data.len(),
                value.byte_len
            )));
        }
        Ok((value, data))
    }
}

/// UpdateV1 密文载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateV1 {
    pub doc_id: String,
    pub generation: u64,
    pub codec_version: String,
    pub app_doc_schema: u32,
    pub min_reader: u32,
    pub update_id: String,
    pub batch_id: String,
    pub origin_device_id: DeviceId,
    pub origin_seq: u64,
    pub base_vv: Vec<u8>,
    pub end_vv: Vec<u8>,
    pub end_frontier: Vec<u8>,
    pub loro_update_bytes: Vec<u8>,
}

impl UpdateV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 13)?;
        let value = Self {
            doc_id: expect_text(fields[0].clone(), "UpdateV1.doc_id")?,
            generation: expect_i64_max(fields[1].clone(), "UpdateV1.generation")?,
            codec_version: expect_text(fields[2].clone(), "UpdateV1.codec_version")?,
            app_doc_schema: crate::cde::expect_u32(fields[3].clone(), "UpdateV1.app_doc_schema")?,
            min_reader: crate::cde::expect_u32(fields[4].clone(), "UpdateV1.min_reader")?,
            update_id: expect_text(fields[5].clone(), "UpdateV1.update_id")?,
            batch_id: expect_text(fields[6].clone(), "UpdateV1.batch_id")?,
            origin_device_id: expect_bytes_n::<16>(fields[7].clone(), "UpdateV1.origin_device_id")?,
            origin_seq: expect_i64_max(fields[8].clone(), "UpdateV1.origin_seq")?,
            base_vv: expect_bytes(fields[9].clone(), "UpdateV1.base_vv")?,
            end_vv: expect_bytes(fields[10].clone(), "UpdateV1.end_vv")?,
            end_frontier: expect_bytes(fields[11].clone(), "UpdateV1.end_frontier")?,
            loro_update_bytes: expect_bytes(fields[12].clone(), "UpdateV1.loro_update_bytes")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Text(self.doc_id.clone())),
            (1, CdeValue::UInt(self.generation)),
            (2, CdeValue::Text(self.codec_version.clone())),
            (3, CdeValue::UInt(u64::from(self.app_doc_schema))),
            (4, CdeValue::UInt(u64::from(self.min_reader))),
            (5, CdeValue::Text(self.update_id.clone())),
            (6, CdeValue::Text(self.batch_id.clone())),
            (7, CdeValue::Bytes(self.origin_device_id.to_vec())),
            (8, CdeValue::UInt(self.origin_seq)),
            (9, CdeValue::Bytes(self.base_vv.clone())),
            (10, CdeValue::Bytes(self.end_vv.clone())),
            (11, CdeValue::Bytes(self.end_frontier.clone())),
            (12, CdeValue::Bytes(self.loro_update_bytes.clone())),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.generation == 0 {
            return Err(ProtoError::Invalid(
                "UpdateV1.generation 必须 >0".to_owned(),
            ));
        }
        if self.doc_id.is_empty() || self.codec_version.is_empty() {
            return Err(ProtoError::Invalid(
                "UpdateV1 doc/codec 不能为空".to_owned(),
            ));
        }
        if self.update_id.is_empty() || self.batch_id.is_empty() {
            return Err(ProtoError::Invalid(
                "UpdateV1 update_id/batch_id 不能为空".to_owned(),
            ));
        }
        if self.loro_update_bytes.is_empty() {
            return Err(ProtoError::Invalid("UpdateV1 更新不能为空".to_owned()));
        }
        if self.encoded_size_upper_bound() > MAX_CDE_BYTES {
            return Err(ProtoError::TooLarge);
        }
        Ok(())
    }

    fn encoded_size_upper_bound(&self) -> usize {
        self.loro_update_bytes.len()
            + self.base_vv.len()
            + self.end_vv.len()
            + self.end_frontier.len()
            + self.doc_id.len()
            + self.codec_version.len()
            + self.update_id.len()
            + self.batch_id.len()
            + 128
    }
}

/// SnapshotV1 密文载荷；v1 只允许 full_history=true。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotV1 {
    pub doc_id: String,
    pub kind: u64,
    pub generation: u64,
    pub codec_version: String,
    pub app_doc_schema: u32,
    pub min_reader: u32,
    pub full_history: bool,
    pub vv: Vec<u8>,
    pub frontier: Vec<u8>,
    pub loro_snapshot_bytes: Vec<u8>,
}

impl SnapshotV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 10)?;
        let value = Self {
            doc_id: expect_text(fields[0].clone(), "SnapshotV1.doc_id")?,
            kind: expect_i64_max(fields[1].clone(), "SnapshotV1.kind")?,
            generation: expect_i64_max(fields[2].clone(), "SnapshotV1.generation")?,
            codec_version: expect_text(fields[3].clone(), "SnapshotV1.codec_version")?,
            app_doc_schema: crate::cde::expect_u32(fields[4].clone(), "SnapshotV1.app_doc_schema")?,
            min_reader: crate::cde::expect_u32(fields[5].clone(), "SnapshotV1.min_reader")?,
            full_history: expect_bool(fields[6].clone(), "SnapshotV1.full_history")?,
            vv: expect_bytes(fields[7].clone(), "SnapshotV1.vv")?,
            frontier: expect_bytes(fields[8].clone(), "SnapshotV1.frontier")?,
            loro_snapshot_bytes: expect_bytes(fields[9].clone(), "SnapshotV1.loro_snapshot_bytes")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Text(self.doc_id.clone())),
            (1, CdeValue::UInt(self.kind)),
            (2, CdeValue::UInt(self.generation)),
            (3, CdeValue::Text(self.codec_version.clone())),
            (4, CdeValue::UInt(u64::from(self.app_doc_schema))),
            (5, CdeValue::UInt(u64::from(self.min_reader))),
            (6, CdeValue::Bool(self.full_history)),
            (7, CdeValue::Bytes(self.vv.clone())),
            (8, CdeValue::Bytes(self.frontier.clone())),
            (9, CdeValue::Bytes(self.loro_snapshot_bytes.clone())),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.generation == 0 {
            return Err(ProtoError::Invalid(
                "SnapshotV1.generation 必须 >0".to_owned(),
            ));
        }
        if !self.full_history {
            return Err(ProtoError::Invalid(
                "v1 禁止 shallow/state-only snapshot".to_owned(),
            ));
        }
        if self.doc_id.is_empty() || self.codec_version.is_empty() {
            return Err(ProtoError::Invalid(
                "SnapshotV1 doc/codec 不能为空".to_owned(),
            ));
        }
        if self.loro_snapshot_bytes.is_empty() {
            return Err(ProtoError::Invalid("SnapshotV1 快照不能为空".to_owned()));
        }
        if self.loro_snapshot_bytes.len() > crate::limits::MAX_LORO_SNAPSHOT_BYTES {
            return Err(ProtoError::TooLarge);
        }
        Ok(())
    }
}

/// Manifest 里的 doc 引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestDoc {
    pub doc_id: String,
    pub kind: u64,
    pub generation: u64,
    pub vv: Vec<u8>,
    pub frontier: Vec<u8>,
    pub snapshot_object_id: ObjectId,
    pub part_count: u32,
    pub object_digest: Sha256,
}

impl ManifestDoc {
    fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Text(self.doc_id.clone())),
            (1, CdeValue::UInt(self.kind)),
            (2, CdeValue::UInt(self.generation)),
            (3, CdeValue::Bytes(self.vv.clone())),
            (4, CdeValue::Bytes(self.frontier.clone())),
            (5, CdeValue::Bytes(self.snapshot_object_id.to_vec())),
            (6, CdeValue::UInt(u64::from(self.part_count))),
            (7, CdeValue::Bytes(self.object_digest.to_vec())),
        ]))
    }

    fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, 8)?;
        let value = Self {
            doc_id: expect_text(fields[0].clone(), "ManifestDoc.doc_id")?,
            kind: expect_i64_max(fields[1].clone(), "ManifestDoc.kind")?,
            generation: expect_i64_max(fields[2].clone(), "ManifestDoc.generation")?,
            vv: expect_bytes(fields[3].clone(), "ManifestDoc.vv")?,
            frontier: expect_bytes(fields[4].clone(), "ManifestDoc.frontier")?,
            snapshot_object_id: expect_bytes_n::<32>(
                fields[5].clone(),
                "ManifestDoc.snapshot_object_id",
            )?,
            part_count: crate::cde::expect_u32(fields[6].clone(), "ManifestDoc.part_count")?,
            object_digest: expect_bytes_n::<32>(fields[7].clone(), "ManifestDoc.object_digest")?,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ProtoError> {
        if self.doc_id.is_empty() {
            return Err(ProtoError::Invalid(
                "ManifestDoc.doc_id 不能为空".to_owned(),
            ));
        }
        if self.generation == 0 {
            return Err(ProtoError::Invalid(
                "ManifestDoc.generation 必须 >0".to_owned(),
            ));
        }
        if self.part_count == 0 {
            return Err(ProtoError::Invalid(
                "ManifestDoc.part_count 必须 ≥1".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Manifest 里的 asset 引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestAsset {
    pub real_sha256: Sha256,
    pub byte_len: u64,
    pub media_type: String,
    pub object_id: ObjectId,
    pub part_count: u32,
    pub object_digest: Sha256,
}

impl ManifestAsset {
    fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.real_sha256.to_vec())),
            (1, CdeValue::UInt(self.byte_len)),
            (2, CdeValue::Text(self.media_type.clone())),
            (3, CdeValue::Bytes(self.object_id.to_vec())),
            (4, CdeValue::UInt(u64::from(self.part_count))),
            (5, CdeValue::Bytes(self.object_digest.to_vec())),
        ]))
    }

    fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, 6)?;
        let value = Self {
            real_sha256: expect_bytes_n::<32>(fields[0].clone(), "ManifestAsset.real_sha256")?,
            byte_len: expect_i64_max(fields[1].clone(), "ManifestAsset.byte_len")?,
            media_type: expect_text(fields[2].clone(), "ManifestAsset.media_type")?,
            object_id: expect_bytes_n::<32>(fields[3].clone(), "ManifestAsset.object_id")?,
            part_count: crate::cde::expect_u32(fields[4].clone(), "ManifestAsset.part_count")?,
            object_digest: expect_bytes_n::<32>(fields[5].clone(), "ManifestAsset.object_digest")?,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ProtoError> {
        if self.byte_len > MAX_ASSET_RAW_BYTES as u64 {
            return Err(ProtoError::TooLarge);
        }
        if self.media_type.is_empty() || self.media_type.len() > MAX_ASSET_MIME_BYTES {
            return Err(ProtoError::Invalid(
                "ManifestAsset.media_type 非法".to_owned(),
            ));
        }
        if self.part_count == 0 {
            return Err(ProtoError::Invalid(
                "ManifestAsset.part_count 必须 ≥1".to_owned(),
            ));
        }
        Ok(())
    }
}

/// ManifestV1 密文载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestV1 {
    pub vault_id: VaultId,
    pub key_epoch: u64,
    pub checkpoint_id: CheckpointId,
    pub covered_relay_seq: u64,
    pub min_reader: u32,
    pub structure_doc_id: String,
    pub docs: Vec<ManifestDoc>,
    pub assets: Vec<ManifestAsset>,
}

impl ManifestV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 8)?;
        let docs = expect_array(fields[6].clone(), "ManifestV1.docs")?
            .into_iter()
            .map(ManifestDoc::from_cde)
            .collect::<Result<Vec<_>, _>>()?;
        let assets = expect_array(fields[7].clone(), "ManifestV1.assets")?
            .into_iter()
            .map(ManifestAsset::from_cde)
            .collect::<Result<Vec<_>, _>>()?;
        let value = Self {
            vault_id: expect_bytes_n::<16>(fields[0].clone(), "ManifestV1.vault_id")?,
            key_epoch: expect_i64_max(fields[1].clone(), "ManifestV1.key_epoch")?,
            checkpoint_id: expect_bytes_n::<16>(fields[2].clone(), "ManifestV1.checkpoint_id")?,
            covered_relay_seq: expect_i64_max(fields[3].clone(), "ManifestV1.covered_relay_seq")?,
            min_reader: crate::cde::expect_u32(fields[4].clone(), "ManifestV1.min_reader")?,
            structure_doc_id: expect_text(fields[5].clone(), "ManifestV1.structure_doc_id")?,
            docs,
            assets,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        let docs = self
            .docs
            .iter()
            .map(ManifestDoc::to_cde)
            .collect::<Result<Vec<_>, _>>()?;
        let assets = self
            .assets
            .iter()
            .map(ManifestAsset::to_cde)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::UInt(self.key_epoch)),
            (2, CdeValue::Bytes(self.checkpoint_id.to_vec())),
            (3, CdeValue::UInt(self.covered_relay_seq)),
            (4, CdeValue::UInt(u64::from(self.min_reader))),
            (5, CdeValue::Text(self.structure_doc_id.clone())),
            (6, CdeValue::Array(docs)),
            (7, CdeValue::Array(assets)),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.key_epoch == 0 {
            return Err(ProtoError::Invalid(
                "ManifestV1.key_epoch 必须 >0".to_owned(),
            ));
        }
        if self.structure_doc_id.is_empty() {
            return Err(ProtoError::Invalid(
                "ManifestV1.structure_doc_id 不能为空".to_owned(),
            ));
        }
        if self.docs.len() + self.assets.len() > MAX_REFS {
            return Err(ProtoError::TooLarge);
        }
        // docs 按 doc_id 的 UTF-8 字节严格升序；assets 按 real_sha256 严格升序。
        for pair in self.docs.windows(2) {
            if pair[0].doc_id.as_bytes() >= pair[1].doc_id.as_bytes() {
                return Err(ProtoError::NonCanonical(
                    "ManifestV1.docs 未按 doc_id 升序".to_owned(),
                ));
            }
        }
        for pair in self.assets.windows(2) {
            if pair[0].real_sha256 >= pair[1].real_sha256 {
                return Err(ProtoError::NonCanonical(
                    "ManifestV1.assets 未按 real_sha256 升序".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Key slot 的 CDE descriptor；作为 KEY_SLOT AAD 的 descriptor。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotDescriptorV1 {
    pub protocol_version: u16,
    pub slot_id: ObjectId,
    pub vault_id: VaultId,
    pub key_epoch: u64,
    pub kind: SlotKind,
    pub wrap_suite: u16,
    pub kdf: Kdf,
    pub created_at: u64,
    pub manager_rev: u64,
}

impl SlotDescriptorV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 9)?;
        let value = Self {
            protocol_version: expect_u16(fields[0].clone(), "SlotDescriptorV1.protocol_version")?,
            slot_id: expect_bytes_n::<32>(fields[1].clone(), "SlotDescriptorV1.slot_id")?,
            vault_id: expect_bytes_n::<16>(fields[2].clone(), "SlotDescriptorV1.vault_id")?,
            key_epoch: expect_i64_max(fields[3].clone(), "SlotDescriptorV1.key_epoch")?,
            kind: SlotKind::try_from(crate::cde::expect_u8(
                fields[4].clone(),
                "SlotDescriptorV1.kind",
            )?)?,
            wrap_suite: expect_u16(fields[5].clone(), "SlotDescriptorV1.wrap_suite")?,
            kdf: Kdf::from_cde(fields[6].clone())?,
            created_at: expect_i64_max(fields[7].clone(), "SlotDescriptorV1.created_at")?,
            manager_rev: expect_i64_max(fields[8].clone(), "SlotDescriptorV1.manager_rev")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::UInt(u64::from(self.protocol_version))),
            (1, CdeValue::Bytes(self.slot_id.to_vec())),
            (2, CdeValue::Bytes(self.vault_id.to_vec())),
            (3, CdeValue::UInt(self.key_epoch)),
            (4, CdeValue::UInt(u64::from(u8::from(self.kind)))),
            (5, CdeValue::UInt(u64::from(self.wrap_suite))),
            (6, self.kdf.to_cde()?),
            (7, CdeValue::UInt(self.created_at)),
            (8, CdeValue::UInt(self.manager_rev)),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.protocol_version != 1 {
            return Err(ProtoError::UnsupportedVersion(self.protocol_version));
        }
        if self.key_epoch == 0 {
            return Err(ProtoError::Invalid(
                "SlotDescriptorV1.key_epoch 必须 >0".to_owned(),
            ));
        }
        if self.wrap_suite != 1 {
            return Err(ProtoError::Invalid(
                "SlotDescriptorV1.wrap_suite 必须为 1".to_owned(),
            ));
        }
        Ok(())
    }
}

/// RootSecretV1；只在槽/HPKE 内部使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootSecretV1 {
    pub vault_id: VaultId,
    pub key_epoch: u64,
    pub root: Sha256,
}

impl RootSecretV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 3)?;
        let value = Self {
            vault_id: expect_bytes_n::<16>(fields[0].clone(), "RootSecretV1.vault_id")?,
            key_epoch: expect_i64_max(fields[1].clone(), "RootSecretV1.key_epoch")?,
            root: expect_bytes_n::<32>(fields[2].clone(), "RootSecretV1.root")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::UInt(self.key_epoch)),
            (2, CdeValue::Bytes(self.root.to_vec())),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.key_epoch == 0 {
            return Err(ProtoError::Invalid(
                "RootSecretV1.key_epoch 必须 >0".to_owned(),
            ));
        }
        Ok(())
    }
}

/// DeviceCertV1；由管理根/control 链签名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCertV1 {
    pub vault_id: VaultId,
    pub device_id: DeviceId,
    pub sign_ed25519_pub: Ed25519PublicKey,
    pub hpke_x25519_pub: X25519PublicKey,
    pub cert_serial: u64,
    pub issued_manager_rev: u64,
    pub rights: Rights,
}

impl DeviceCertV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 7)?;
        let value = Self {
            vault_id: expect_bytes_n::<16>(fields[0].clone(), "DeviceCertV1.vault_id")?,
            device_id: expect_bytes_n::<16>(fields[1].clone(), "DeviceCertV1.device_id")?,
            sign_ed25519_pub: expect_bytes_n::<32>(fields[2].clone(), "DeviceCertV1.sign_pub")?,
            hpke_x25519_pub: expect_bytes_n::<32>(fields[3].clone(), "DeviceCertV1.hpke_pub")?,
            cert_serial: expect_i64_max(fields[4].clone(), "DeviceCertV1.cert_serial")?,
            issued_manager_rev: expect_i64_max(
                fields[5].clone(),
                "DeviceCertV1.issued_manager_rev",
            )?,
            rights: Rights::new(crate::cde::expect_u32(
                fields[6].clone(),
                "DeviceCertV1.rights",
            )?)?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.device_id.to_vec())),
            (2, CdeValue::Bytes(self.sign_ed25519_pub.to_vec())),
            (3, CdeValue::Bytes(self.hpke_x25519_pub.to_vec())),
            (4, CdeValue::UInt(self.cert_serial)),
            (5, CdeValue::UInt(self.issued_manager_rev)),
            (6, CdeValue::UInt(u64::from(self.rights.bits()))),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.cert_serial == 0 {
            return Err(ProtoError::Invalid(
                "DeviceCertV1.cert_serial 必须 >0".to_owned(),
            ));
        }
        Rights::new(self.rights.bits())?;
        Ok(())
    }
}

/// 鉴权 challenge。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub pv: u16,
    pub relay_instance_id: RelayInstanceId,
    pub origin_sha256: Sha256,
    pub vault_id: VaultId,
    pub realm: AuthRealm,
    pub subject: DeviceId,
    pub challenge_id: ChallengeId,
    pub nonce: Sha256,
    pub expires_at: u64,
}

impl Challenge {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 9)?;
        let value = Self {
            pv: expect_u16(fields[0].clone(), "Challenge.pv")?,
            relay_instance_id: expect_bytes_n::<16>(
                fields[1].clone(),
                "Challenge.relay_instance_id",
            )?,
            origin_sha256: expect_bytes_n::<32>(fields[2].clone(), "Challenge.origin_sha256")?,
            vault_id: expect_bytes_n::<16>(fields[3].clone(), "Challenge.vault_id")?,
            realm: AuthRealm::try_from(crate::cde::expect_u8(
                fields[4].clone(),
                "Challenge.realm",
            )?)?,
            subject: expect_bytes_n::<16>(fields[5].clone(), "Challenge.subject")?,
            challenge_id: expect_bytes_n::<16>(fields[6].clone(), "Challenge.challenge_id")?,
            nonce: expect_bytes_n::<32>(fields[7].clone(), "Challenge.nonce")?,
            expires_at: expect_i64_max(fields[8].clone(), "Challenge.expires_at")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::UInt(u64::from(self.pv))),
            (1, CdeValue::Bytes(self.relay_instance_id.to_vec())),
            (2, CdeValue::Bytes(self.origin_sha256.to_vec())),
            (3, CdeValue::Bytes(self.vault_id.to_vec())),
            (4, CdeValue::UInt(u64::from(u8::from(self.realm)))),
            (5, CdeValue::Bytes(self.subject.to_vec())),
            (6, CdeValue::Bytes(self.challenge_id.to_vec())),
            (7, CdeValue::Bytes(self.nonce.to_vec())),
            (8, CdeValue::UInt(self.expires_at)),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.pv != 1 {
            return Err(ProtoError::UnsupportedVersion(self.pv));
        }
        Ok(())
    }
}

/// HPKE grant 的 AAD（ADR 0006 §17.2）；字段按整数键 0..6 声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantAadV1 {
    pub pv: u16,
    pub vault_id: VaultId,
    pub epoch: u64,
    pub recipient_device_id: DeviceId,
    pub recipient_x25519_pub: X25519PublicKey,
    pub manager_rev: u64,
    pub grant_id: [u8; 16],
}

impl GrantAadV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 7)?;
        let value = Self {
            pv: expect_u16(fields[0].clone(), "GrantAadV1.pv")?,
            vault_id: expect_bytes_n::<16>(fields[1].clone(), "GrantAadV1.vault_id")?,
            epoch: expect_i64_max(fields[2].clone(), "GrantAadV1.epoch")?,
            recipient_device_id: expect_bytes_n::<16>(
                fields[3].clone(),
                "GrantAadV1.recipient_device_id",
            )?,
            recipient_x25519_pub: expect_bytes_n::<32>(
                fields[4].clone(),
                "GrantAadV1.recipient_x25519_pub",
            )?,
            manager_rev: expect_i64_max(fields[5].clone(), "GrantAadV1.manager_rev")?,
            grant_id: expect_bytes_n::<16>(fields[6].clone(), "GrantAadV1.grant_id")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::UInt(u64::from(self.pv))),
            (1, CdeValue::Bytes(self.vault_id.to_vec())),
            (2, CdeValue::UInt(self.epoch)),
            (3, CdeValue::Bytes(self.recipient_device_id.to_vec())),
            (4, CdeValue::Bytes(self.recipient_x25519_pub.to_vec())),
            (5, CdeValue::UInt(self.manager_rev)),
            (6, CdeValue::Bytes(self.grant_id.to_vec())),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.pv != 1 {
            return Err(ProtoError::UnsupportedVersion(self.pv));
        }
        if self.epoch == 0 {
            return Err(ProtoError::Invalid("GrantAadV1.epoch 必须 >0".to_owned()));
        }
        Ok(())
    }

    /// AAD 字节 = `CDE(GrantAadV1)`。
    pub fn aad_bytes(&self) -> Result<Vec<u8>, ProtoError> {
        self.encode()
    }

    /// HPKE 外层管理签名待签字节 = `AAD || enc32 || sha256(hpke_ciphertext)`。
    ///
    /// 本函数只拼字节，不计算 SHA256；`ciphertext_sha256` 由 sync 计算。
    pub fn grant_signed_bytes(
        &self,
        enc: &[u8; 32],
        ciphertext_sha256: &[u8; 32],
    ) -> Result<Vec<u8>, ProtoError> {
        let aad = self.aad_bytes()?;
        let mut out = Vec::with_capacity(aad.len() + 32 + 32);
        out.extend_from_slice(&aad);
        out.extend_from_slice(enc);
        out.extend_from_slice(ciphertext_sha256);
        Ok(out)
    }
}

/// `DeviceCertV1` 的管理签名待签字节：`MN/v1/device-cert\0 || CDE(cert)`。
pub fn device_cert_signed_bytes(cert: &DeviceCertV1) -> Result<Vec<u8>, ProtoError> {
    let mut out = Vec::with_capacity(crate::constants::SIGN_DEVICE_CERT_DOMAIN.len());
    out.extend_from_slice(crate::constants::SIGN_DEVICE_CERT_DOMAIN);
    out.extend_from_slice(&cert.encode()?);
    Ok(out)
}

impl DeviceCertV1 {
    /// 管理签名待签字节；与 [`device_cert_signed_bytes`] 相同。
    pub fn device_cert_signed_bytes(&self) -> Result<Vec<u8>, ProtoError> {
        device_cert_signed_bytes(self)
    }
}

/// `GrantAadV1` 外层管理签名待签字节的便捷函数。
pub fn grant_signed_bytes(
    grant: &GrantAadV1,
    enc: &[u8; 32],
    ciphertext_sha256: &[u8; 32],
) -> Result<Vec<u8>, ProtoError> {
    grant.grant_signed_bytes(enc, ciphertext_sha256)
}

/// ObjectCommit 签名载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectCommit {
    pub vault_id: VaultId,
    pub object_id: ObjectId,
    pub message_id: MessageId,
    pub object_type: ObjectType,
    pub generation: u64,
    pub key_epoch: u64,
    pub sender_seq: u64,
    pub part_count: u32,
    pub total_len: u64,
    pub object_digest: Sha256,
}

impl ObjectCommit {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 10)?;
        let value = Self {
            vault_id: expect_bytes_n::<16>(fields[0].clone(), "ObjectCommit.vault_id")?,
            object_id: expect_bytes_n::<32>(fields[1].clone(), "ObjectCommit.object_id")?,
            message_id: expect_bytes_n::<16>(fields[2].clone(), "ObjectCommit.message_id")?,
            object_type: ObjectType::try_from(crate::cde::expect_u8(
                fields[3].clone(),
                "ObjectCommit.type",
            )?)?,
            generation: expect_i64_max(fields[4].clone(), "ObjectCommit.generation")?,
            key_epoch: expect_i64_max(fields[5].clone(), "ObjectCommit.key_epoch")?,
            sender_seq: expect_i64_max(fields[6].clone(), "ObjectCommit.sender_seq")?,
            part_count: crate::cde::expect_u32(fields[7].clone(), "ObjectCommit.part_count")?,
            total_len: expect_i64_max(fields[8].clone(), "ObjectCommit.total_len")?,
            object_digest: expect_bytes_n::<32>(fields[9].clone(), "ObjectCommit.object_digest")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.object_id.to_vec())),
            (2, CdeValue::Bytes(self.message_id.to_vec())),
            (3, CdeValue::UInt(u64::from(self.object_type.as_u8()))),
            (4, CdeValue::UInt(self.generation)),
            (5, CdeValue::UInt(self.key_epoch)),
            (6, CdeValue::UInt(self.sender_seq)),
            (7, CdeValue::UInt(u64::from(self.part_count))),
            (8, CdeValue::UInt(self.total_len)),
            (9, CdeValue::Bytes(self.object_digest.to_vec())),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.key_epoch == 0 {
            return Err(ProtoError::Invalid(
                "ObjectCommit.key_epoch 必须 >0".to_owned(),
            ));
        }
        if self.part_count == 0 || self.total_len == 0 {
            return Err(ProtoError::Invalid(
                "ObjectCommit part/total 必须 >0".to_owned(),
            ));
        }
        if self.object_type.requires_generation() {
            if self.generation == 0 {
                return Err(ProtoError::Invalid(
                    "UPDATE/SNAPSHOT ObjectCommit.generation 必须 >0".to_owned(),
                ));
            }
        } else if self.generation != 0 {
            return Err(ProtoError::Invalid(
                "其他对象 ObjectCommit.generation 必须为 0".to_owned(),
            ));
        }
        let expected_parts = crate::limits::part_count_for(self.total_len, self.object_type);
        if u64::from(self.part_count) != expected_parts {
            return Err(ProtoError::Invalid(format!(
                "ObjectCommit.part_count={} 与固定切片 {expected_parts} 不符",
                self.part_count
            )));
        }
        if self.total_len > crate::limits::MAX_UPDATE_CDE_BYTES as u64
            && self.object_type != ObjectType::Asset
            && self.object_type != ObjectType::Snapshot
        {
            return Err(ProtoError::TooLarge);
        }
        match self.object_type {
            ObjectType::KeySlot
                if self.total_len > crate::limits::MAX_KEY_SLOT_PLAINTEXT as u64 =>
            {
                return Err(ProtoError::TooLarge)
            }
            ObjectType::Manifest
                if self.total_len > crate::limits::MAX_MANIFEST_CDE_BYTES as u64 =>
            {
                return Err(ProtoError::TooLarge)
            }
            ObjectType::Snapshot
                if self.total_len > crate::limits::MAX_LORO_SNAPSHOT_BYTES as u64 =>
            {
                return Err(ProtoError::TooLarge)
            }
            _ => {}
        }
        Ok(())
    }
}

/// CheckpointCommit 签名载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointCommit {
    pub vault_id: VaultId,
    pub checkpoint_id: CheckpointId,
    pub epoch: u64,
    pub covered_relay_seq: u64,
    pub manifest_object_id: ObjectId,
    pub manifest_digest: Sha256,
    pub refs_digest: Sha256,
    pub producer_device_id: DeviceId,
    pub validation_version: u16,
}

impl CheckpointCommit {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 9)?;
        let value = Self {
            vault_id: expect_bytes_n::<16>(fields[0].clone(), "CheckpointCommit.vault_id")?,
            checkpoint_id: expect_bytes_n::<16>(
                fields[1].clone(),
                "CheckpointCommit.checkpoint_id",
            )?,
            epoch: expect_i64_max(fields[2].clone(), "CheckpointCommit.epoch")?,
            covered_relay_seq: expect_i64_max(fields[3].clone(), "CheckpointCommit.S")?,
            manifest_object_id: expect_bytes_n::<32>(
                fields[4].clone(),
                "CheckpointCommit.manifest_oid",
            )?,
            manifest_digest: expect_bytes_n::<32>(
                fields[5].clone(),
                "CheckpointCommit.manifest_digest",
            )?,
            refs_digest: expect_bytes_n::<32>(fields[6].clone(), "CheckpointCommit.refs_digest")?,
            producer_device_id: expect_bytes_n::<16>(
                fields[7].clone(),
                "CheckpointCommit.producer_device_id",
            )?,
            validation_version: expect_u16(
                fields[8].clone(),
                "CheckpointCommit.validation_version",
            )?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.checkpoint_id.to_vec())),
            (2, CdeValue::UInt(self.epoch)),
            (3, CdeValue::UInt(self.covered_relay_seq)),
            (4, CdeValue::Bytes(self.manifest_object_id.to_vec())),
            (5, CdeValue::Bytes(self.manifest_digest.to_vec())),
            (6, CdeValue::Bytes(self.refs_digest.to_vec())),
            (7, CdeValue::Bytes(self.producer_device_id.to_vec())),
            (8, CdeValue::UInt(u64::from(self.validation_version))),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.epoch == 0 {
            return Err(ProtoError::Invalid(
                "CheckpointCommit.epoch 必须 >0".to_owned(),
            ));
        }
        if self.validation_version != 1 {
            return Err(ProtoError::UnsupportedVersion(self.validation_version));
        }
        Ok(())
    }
}

/// 公开对象引用（manifest/GC 共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRefV1 {
    pub object_id: ObjectId,
    pub object_type: ObjectType,
    pub generation: u64,
    pub key_epoch: u64,
    pub part_count: u32,
    pub total_plain_len: u64,
    pub object_digest: Sha256,
}

impl ObjectRefV1 {
    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.object_id.to_vec())),
            (1, CdeValue::UInt(u64::from(self.object_type.as_u8()))),
            (2, CdeValue::UInt(self.generation)),
            (3, CdeValue::UInt(self.key_epoch)),
            (4, CdeValue::UInt(u64::from(self.part_count))),
            (5, CdeValue::UInt(self.total_plain_len)),
            (6, CdeValue::Bytes(self.object_digest.to_vec())),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, 7)?;
        let value = Self {
            object_id: expect_bytes_n::<32>(fields[0].clone(), "ObjectRefV1.object_id")?,
            object_type: ObjectType::try_from(crate::cde::expect_u8(
                fields[1].clone(),
                "ObjectRefV1.type",
            )?)?,
            generation: expect_i64_max(fields[2].clone(), "ObjectRefV1.generation")?,
            key_epoch: expect_i64_max(fields[3].clone(), "ObjectRefV1.key_epoch")?,
            part_count: crate::cde::expect_u32(fields[4].clone(), "ObjectRefV1.part_count")?,
            total_plain_len: expect_i64_max(fields[5].clone(), "ObjectRefV1.total_plain_len")?,
            object_digest: expect_bytes_n::<32>(fields[6].clone(), "ObjectRefV1.object_digest")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.key_epoch == 0 {
            return Err(ProtoError::Invalid(
                "ObjectRefV1.key_epoch 必须 >0".to_owned(),
            ));
        }
        if self.part_count == 0 || self.total_plain_len == 0 {
            return Err(ProtoError::Invalid(
                "ObjectRefV1 part/total 必须 >0".to_owned(),
            ));
        }
        if self.object_type.requires_generation() {
            if self.generation == 0 {
                return Err(ProtoError::Invalid(
                    "UPDATE/SNAPSHOT ObjectRefV1.generation 必须 >0".to_owned(),
                ));
            }
        } else if self.generation != 0 {
            return Err(ProtoError::Invalid(
                "其他对象 ObjectRefV1.generation 必须为 0".to_owned(),
            ));
        }
        let expected_parts = crate::limits::part_count_for(self.total_plain_len, self.object_type);
        if u64::from(self.part_count) != expected_parts {
            return Err(ProtoError::Invalid(format!(
                "ObjectRefV1.part_count={} 与固定切片 {expected_parts} 不符",
                self.part_count
            )));
        }
        match self.object_type {
            ObjectType::Update | ObjectType::Manifest
                if self.total_plain_len > crate::limits::MAX_UPDATE_CDE_BYTES as u64 =>
            {
                return Err(ProtoError::TooLarge)
            }
            ObjectType::Asset
                if self.total_plain_len > crate::limits::MAX_ASSET_RAW_BYTES as u64 =>
            {
                return Err(ProtoError::TooLarge)
            }
            ObjectType::Snapshot
                if self.total_plain_len > crate::limits::MAX_LORO_SNAPSHOT_BYTES as u64 =>
            {
                return Err(ProtoError::TooLarge)
            }
            ObjectType::KeySlot
                if self.total_plain_len > crate::limits::MAX_KEY_SLOT_PLAINTEXT as u64 =>
            {
                return Err(ProtoError::TooLarge)
            }
            _ => {}
        }
        Ok(())
    }
}

/// `refs_digest` 的待编码输入：按 object_id 字节升序的 `CDE(array<ObjectRefV1>)`。
/// 本函数不计算 SHA256（sync-proto 不做 crypto）。
pub fn refs_digest_input(refs: &[ObjectRefV1]) -> Result<Vec<u8>, ProtoError> {
    if refs.len() > MAX_REFS {
        return Err(ProtoError::TooLarge);
    }
    let mut refs = refs.to_vec();
    refs.sort_by_key(|a| a.object_id);
    for reference in &refs {
        if !matches!(
            reference.object_type,
            ObjectType::Snapshot | ObjectType::Asset
        ) {
            return Err(ProtoError::Invalid(
                "refs_digest 只覆盖 snapshot/asset 数据对象".to_owned(),
            ));
        }
    }
    for pair in refs.windows(2) {
        if pair[0].object_id == pair[1].object_id {
            return Err(ProtoError::Invalid("refs 中 object_id 重复".to_owned()));
        }
    }
    let values = refs
        .iter()
        .map(ObjectRefV1::to_cde)
        .collect::<Result<Vec<_>, _>>()?;
    encode(&CdeValue::Array(values))
}

/// `object_digest` 的待哈希输入：
/// `BE32(part_count) || concat(BE32(part_index) || SHA256(exact_envelope_part))`。
pub fn object_digest_input(
    part_count: u32,
    part_hashes: &[[u8; 32]],
) -> Result<Vec<u8>, ProtoError> {
    if part_count == 0 {
        return Err(ProtoError::Invalid("part_count 必须 ≥1".to_owned()));
    }
    if part_hashes.len() != part_count as usize {
        return Err(ProtoError::Invalid(format!(
            "part_count={part_count} 但提供 {} 个 part hash",
            part_hashes.len()
        )));
    }
    if part_hashes.len() > u32::MAX as usize {
        return Err(ProtoError::TooLarge);
    }
    let mut out = Vec::with_capacity(4 + part_hashes.len() * 36);
    out.extend_from_slice(&part_count.to_be_bytes());
    for (index, hash) in part_hashes.iter().enumerate() {
        out.extend_from_slice(&(index as u32).to_be_bytes());
        out.extend_from_slice(hash);
    }
    Ok(out)
}

/// 管理请求签名的待编码输入。`path` 必须是规范化后的实际路径。
pub fn management_signing_input(
    method: HttpMethod,
    path: &str,
    unsigned_body: &[u8],
) -> Result<Vec<u8>, ProtoError> {
    if !path.starts_with('/') {
        return Err(ProtoError::Invalid(
            "management path 必须是绝对路径".to_owned(),
        ));
    }
    if path.len() > 4096 {
        return Err(ProtoError::TooLarge);
    }
    let mut out = Vec::with_capacity(1 + path.len() + 4 + unsigned_body.len());
    out.push(u8::from(method));
    out.extend_from_slice(path.as_bytes());
    out.extend_from_slice(&(unsigned_body.len() as u32).to_be_bytes());
    out.extend_from_slice(unsigned_body);
    Ok(out)
}

/// 校验设备数上限（供 sync/relay 使用）。
pub fn check_device_count(count: usize) -> Result<(), ProtoError> {
    if count > MAX_DEVICES {
        return Err(ProtoError::TooLarge);
    }
    Ok(())
}
