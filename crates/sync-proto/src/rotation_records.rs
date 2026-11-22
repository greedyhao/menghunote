//! ADR 0006 §30.2：relay 轮换新增的共享 CDE 记录。
//!
//! 字段编号与 `crates/relay/src/{control,rotations,api}.rs` / ADR §30 逐字节一致；
//! 本模块只做 canonical 编解码与 digest 的**字节输入**，不做 SHA-256、不碰 SQLite。
//!
//! `SignedControlRecord` 的 `event` 保存 relay 事件 CDE 原字节；客户端按 kind 用
//! [`EpochActivated::decode`]、[`RotationRestarted::decode`]、[`RevocationsAdded::decode`]、
//! [`RotationAborted::decode`] 解码。

use std::collections::BTreeMap;

use crate::activation_t::ActivationTV1;
use crate::cde::{
    decode, encode, expect_array, expect_bool, expect_bytes, expect_bytes_n, expect_i64_max,
    expect_text, expect_u8, into_record_fields, CdeValue,
};
use crate::error::ProtoError;
use crate::records::{DeviceId, HttpMethod};

pub type Sha256 = [u8; 32];
pub type CheckpointId = [u8; 16];
pub type RotationId = [u8; 16];

fn record_map(fields: Vec<(u64, CdeValue)>) -> CdeValue {
    CdeValue::Map(fields)
}

fn optional_fields(
    value: CdeValue,
    allowed: &[u64],
    required: &[u64],
) -> Result<BTreeMap<u64, CdeValue>, ProtoError> {
    let CdeValue::Map(entries) = value else {
        return Err(ProtoError::Invalid("record 必须是 map".to_owned()));
    };
    let mut fields = BTreeMap::new();
    for (key, value) in entries {
        if !allowed.contains(&key) {
            return Err(ProtoError::UnknownField(key));
        }
        fields.insert(key, value);
    }
    for key in required {
        if !fields.contains_key(key) {
            return Err(ProtoError::MissingField(*key));
        }
    }
    Ok(fields)
}

fn take(fields: &mut BTreeMap<u64, CdeValue>, key: u64) -> Result<CdeValue, ProtoError> {
    fields.remove(&key).ok_or(ProtoError::MissingField(key))
}

fn take_uint(
    fields: &mut BTreeMap<u64, CdeValue>,
    key: u64,
    name: &str,
) -> Result<u64, ProtoError> {
    expect_i64_max(take(fields, key)?, name)
}

fn take_id16(
    fields: &mut BTreeMap<u64, CdeValue>,
    key: u64,
    name: &str,
) -> Result<[u8; 16], ProtoError> {
    expect_bytes_n::<16>(take(fields, key)?, name)
}

fn take_hash32(
    fields: &mut BTreeMap<u64, CdeValue>,
    key: u64,
    name: &str,
) -> Result<[u8; 32], ProtoError> {
    expect_bytes_n::<32>(take(fields, key)?, name)
}

/// 控制链 kind 的 wire 文本（与 relay `ControlKind::as_str` 一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKind {
    RotationBegin,
    RotationAbort,
    EpochActivated,
    RotationRestarted,
    RevocationsAdded,
}

impl ControlKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RotationBegin => "rotation.begin",
            Self::RotationAbort => "rotation.abort",
            Self::EpochActivated => "epoch.activated",
            Self::RotationRestarted => "rotation.restarted",
            Self::RevocationsAdded => "rotation.revoked-more",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ProtoError> {
        match value {
            "rotation.begin" => Ok(Self::RotationBegin),
            "rotation.abort" => Ok(Self::RotationAbort),
            "epoch.activated" => Ok(Self::EpochActivated),
            "rotation.restarted" => Ok(Self::RotationRestarted),
            "rotation.revoked-more" => Ok(Self::RevocationsAdded),
            _ => Err(ProtoError::Invalid(format!("未知控制记录 kind {value}"))),
        }
    }
}

/// `SignedControlRecord`（relay `control.rs` 的 0..6 字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedControlRecord {
    pub kind: ControlKind,
    pub request_id: DeviceId,
    pub manager_rev: u64,
    pub expected_control_rev: u64,
    pub signed_body: Vec<u8>,
    pub signature: Vec<u8>,
    /// 可选 typed 事件 CDE 原字节（kind=epoch.activated / rotation.abort /
    /// rotation.restarted / rotation.revoked-more 时存在）。
    pub event: Option<Vec<u8>>,
}

impl SignedControlRecord {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut fields = vec![
            (0, CdeValue::Text(self.kind.as_str().to_owned())),
            (1, CdeValue::Bytes(self.request_id.to_vec())),
            (2, CdeValue::UInt(self.manager_rev)),
            (3, CdeValue::UInt(self.expected_control_rev)),
            (4, CdeValue::Bytes(self.signed_body.clone())),
            (5, CdeValue::Bytes(self.signature.clone())),
        ];
        if let Some(event) = &self.event {
            fields.push((6, CdeValue::Bytes(event.clone())));
        }
        encode(&record_map(fields))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let value = decode(bytes)?;
        let mut fields = optional_fields(value, &[0, 1, 2, 3, 4, 5, 6], &[0, 1, 2, 3, 4, 5])?;
        let kind = ControlKind::parse(&expect_text(
            take(&mut fields, 0)?,
            "SignedControlRecord.kind",
        )?)?;
        let record = Self {
            kind,
            request_id: take_id16(&mut fields, 1, "SignedControlRecord.request_id")?,
            manager_rev: take_uint(&mut fields, 2, "SignedControlRecord.manager_rev")?,
            expected_control_rev: take_uint(
                &mut fields,
                3,
                "SignedControlRecord.expected_control_rev",
            )?,
            signed_body: expect_bytes(take(&mut fields, 4)?, "SignedControlRecord.signed_body")?,
            signature: expect_bytes(take(&mut fields, 5)?, "SignedControlRecord.signature")?,
            event: fields
                .remove(&6)
                .map(|value| expect_bytes(value, "SignedControlRecord.event"))
                .transpose()?,
        };
        Ok(record)
    }

    /// 按 kind 解码 event；kind 为 begin 或 event 缺失时返回 `Ok(None)`。
    pub fn decoded_event(&self) -> Result<Option<ControlEvent>, ProtoError> {
        let Some(bytes) = &self.event else {
            return Ok(None);
        };
        let event = match self.kind {
            ControlKind::EpochActivated => {
                ControlEvent::EpochActivated(EpochActivated::decode(bytes)?)
            }
            ControlKind::RotationAbort => {
                ControlEvent::RotationAborted(RotationAborted::decode(bytes)?)
            }
            ControlKind::RotationRestarted => {
                ControlEvent::RotationRestarted(RotationRestarted::decode(bytes)?)
            }
            ControlKind::RevocationsAdded => {
                ControlEvent::RevocationsAdded(RevocationsAdded::decode(bytes)?)
            }
            ControlKind::RotationBegin => {
                return Err(ProtoError::Invalid(
                    "rotation.begin 不应带 typed event".to_owned(),
                ));
            }
        };
        Ok(Some(event))
    }
}

/// `SignedControlRecord` 携带的 typed 事件。
///
/// `EpochActivated` 现在携带 T 与双签（ADR §34），比其它变体大；这是低频控制
/// 事件的解码结果，不值得为 288B 引入 Box 间接层与 API 变更。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    EpochActivated(EpochActivated),
    RotationAborted(RotationAborted),
    RotationRestarted(RotationRestarted),
    RevocationsAdded(RevocationsAdded),
}

/// `rotation.abort` 的 typed 事件（键 0..3）：重建管理签名输入所需的**完整被签事实**。
///
/// 背景（ADR 0006 §35.3）：abort 的请求 path 含 rotation id，而 canonical body 是
/// 空 map；relay 只存 body + 签名时，客户端无法重建
/// `management_signing_input(instance, vault, request_id, expected_rev,
/// SHA256(CDE{method, path, body}))`。本事件把 path/method/body digest 一起写进控制记录，
/// 使 abort 变成**可验签事实**而不是"结构上像签名"的记录。
///
/// 字段：
/// - 0 rotation_id(bytes16)：被废弃的轮换 id（必须与 path 中的 id 一致）；
/// - 1 method(uint)：管理签名标签（GET=1..DELETE=5；abort 固定 POST=2）；
/// - 2 path(text)：被签名的请求 path（`/v1/vaults/{v}/rotations/{r}/abort`）；
/// - 3 body_digest(bytes32)：`SHA256(record.signed_body)`，用于交叉校验 body 未被替换。
///
/// **安全边界**：事件本身不单独被签名；它只是把签名输入所需的公开事实显式化。
/// 客户端必须用 `record.signed_body` 复用同一 digest 重建签名输入再验管理签名，
/// 事件字段任一被改都会让验签失败（或被 path/digest 一致性检查拒绝）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationAborted {
    /// 被废弃的轮换 id。
    pub rotation_id: RotationId,
    /// 管理签名覆盖的 HTTP method（abort 固定 [`HttpMethod::Post`]）。
    pub method: HttpMethod,
    /// 管理签名覆盖的请求 path。
    pub path: String,
    /// `SHA256(record.signed_body)`。
    pub body_digest: Sha256,
}

impl RotationAborted {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(vec![
            (0, CdeValue::Bytes(self.rotation_id.to_vec())),
            (1, CdeValue::UInt(u64::from(u8::from(self.method)))),
            (2, CdeValue::Text(self.path.clone())),
            (3, CdeValue::Bytes(self.body_digest.to_vec())),
        ]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 4)?;
        let mut fields = fields
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as u64, value))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            rotation_id: take_id16(&mut fields, 0, "RotationAborted.rotation_id")?,
            method: HttpMethod::try_from(expect_u8(
                take(&mut fields, 1)?,
                "RotationAborted.method",
            )?)?,
            path: expect_text(take(&mut fields, 2)?, "RotationAborted.path")?,
            body_digest: take_hash32(&mut fields, 3, "RotationAborted.body_digest")?,
        })
    }
}

/// `epoch.activated`（relay `EpochActivated`，键 0..15，13–15 可省略）。
///
/// ADR 0006 §34：新记录必须携带本次激活的完整 activation T（`ActivationTV1` 的
/// canonical CDE 原字节）与旧/新管理 key 对 `activation_t_signing_input(T)` 的双签，
/// 客户端据此从 `/control` 独立复算并验签。13–15 全体省略只用于兼容升级前的历史
/// 控制记录（它们不带 T，客户端应视为"无法复算"而不是"复算通过"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochActivated {
    pub vault_id: [u8; 16],
    pub instance_id: [u8; 16],
    pub rotation_id: RotationId,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub covered_seq: u64,
    pub primary_checkpoint: CheckpointId,
    pub fallback_checkpoint: CheckpointId,
    /// `ActivationCommitV1` 的 SHA-256。
    pub activation_commit: Sha256,
    pub member_set_hash: Sha256,
    pub grants_digest: Sha256,
    pub control_rev: u64,
    pub auth_rev: u64,
    /// 13：complete activation T 的 canonical CDE 原字节。
    pub activation_t: Option<Vec<u8>>,
    /// 14：旧管理 key 对 `activation_t_signing_input(T)` 的 64B 签名。
    pub old_t_signature: Option<Vec<u8>>,
    /// 15：新管理 key 对同一输入的 64B 签名。
    pub new_t_signature: Option<Vec<u8>>,
}

impl EpochActivated {
    /// `activation_commit` 字段的别名（语义为 ActivationCommitV1 的 SHA-256）。
    #[must_use]
    pub fn activation_commit_hash(&self) -> Sha256 {
        self.activation_commit
    }

    /// T 与双签必须同时存在；存在时签名各 64B，且 T 内的重复事实必须与本事件一致。
    fn check_activation_t_attachment(&self) -> Result<(), ProtoError> {
        let (Some(bytes), Some(old), Some(new)) = (
            &self.activation_t,
            &self.old_t_signature,
            &self.new_t_signature,
        ) else {
            if self.activation_t.is_none()
                && self.old_t_signature.is_none()
                && self.new_t_signature.is_none()
            {
                return Ok(());
            }
            return Err(ProtoError::Invalid(
                "EpochActivated 的 activation_t 与双签必须同时存在".to_owned(),
            ));
        };
        if old.len() != 64 || new.len() != 64 {
            return Err(ProtoError::Invalid(
                "EpochActivated 的 T 双签必须各 64 字节".to_owned(),
            ));
        }
        let activation_t = ActivationTV1::decode(bytes)?;
        let matches = activation_t.vault_id == self.vault_id
            && activation_t.instance_id == self.instance_id
            && activation_t.rotation_id == self.rotation_id
            && activation_t.from_epoch == self.from_epoch
            && activation_t.to_epoch == self.to_epoch
            && activation_t.freeze_seq == self.covered_seq
            && activation_t.primary_cid == self.primary_checkpoint
            && activation_t.fallback_cid == self.fallback_checkpoint
            && activation_t.grants_digest == self.grants_digest
            && activation_t.expected_control_rev == self.control_rev;
        if !matches {
            return Err(ProtoError::Invalid(
                "EpochActivated 与 activation_t 的重复事实不一致".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        self.check_activation_t_attachment()?;
        let mut fields = vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.rotation_id.to_vec())),
            (3, CdeValue::UInt(self.from_epoch)),
            (4, CdeValue::UInt(self.to_epoch)),
            (5, CdeValue::UInt(self.covered_seq)),
            (6, CdeValue::Bytes(self.primary_checkpoint.to_vec())),
            (7, CdeValue::Bytes(self.fallback_checkpoint.to_vec())),
            (8, CdeValue::Bytes(self.activation_commit.to_vec())),
            (9, CdeValue::Bytes(self.member_set_hash.to_vec())),
            (10, CdeValue::Bytes(self.grants_digest.to_vec())),
            (11, CdeValue::UInt(self.control_rev)),
            (12, CdeValue::UInt(self.auth_rev)),
        ];
        if let Some(activation_t) = &self.activation_t {
            // `check_activation_t_attachment` 已确保 14/15 同时存在。
            fields.push((13, CdeValue::Bytes(activation_t.clone())));
            fields.push((
                14,
                CdeValue::Bytes(
                    self.old_t_signature.clone().ok_or_else(|| {
                        ProtoError::Invalid("EpochActivated.old_t_signature".into())
                    })?,
                ),
            ));
            fields.push((
                15,
                CdeValue::Bytes(
                    self.new_t_signature.clone().ok_or_else(|| {
                        ProtoError::Invalid("EpochActivated.new_t_signature".into())
                    })?,
                ),
            ));
        }
        encode(&record_map(fields))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut fields = optional_fields(
            decode(bytes)?,
            &(0..=15).collect::<Vec<_>>(),
            &(0..=12).collect::<Vec<_>>(),
        )?;
        let record = Self {
            vault_id: take_id16(&mut fields, 0, "EpochActivated.vault_id")?,
            instance_id: take_id16(&mut fields, 1, "EpochActivated.instance_id")?,
            rotation_id: take_id16(&mut fields, 2, "EpochActivated.rotation_id")?,
            from_epoch: take_uint(&mut fields, 3, "EpochActivated.from_epoch")?,
            to_epoch: take_uint(&mut fields, 4, "EpochActivated.to_epoch")?,
            covered_seq: take_uint(&mut fields, 5, "EpochActivated.covered_seq")?,
            primary_checkpoint: take_id16(&mut fields, 6, "EpochActivated.primary_checkpoint")?,
            fallback_checkpoint: take_id16(&mut fields, 7, "EpochActivated.fallback_checkpoint")?,
            activation_commit: take_hash32(&mut fields, 8, "EpochActivated.activation_commit")?,
            member_set_hash: take_hash32(&mut fields, 9, "EpochActivated.member_set_hash")?,
            grants_digest: take_hash32(&mut fields, 10, "EpochActivated.grants_digest")?,
            control_rev: take_uint(&mut fields, 11, "EpochActivated.control_rev")?,
            auth_rev: take_uint(&mut fields, 12, "EpochActivated.auth_rev")?,
            activation_t: fields
                .remove(&13)
                .map(|value| expect_bytes(value, "EpochActivated.activation_t"))
                .transpose()?,
            old_t_signature: fields
                .remove(&14)
                .map(|value| expect_bytes(value, "EpochActivated.old_t_signature"))
                .transpose()?,
            new_t_signature: fields
                .remove(&15)
                .map(|value| expect_bytes(value, "EpochActivated.new_t_signature"))
                .transpose()?,
        };
        record.check_activation_t_attachment()?;
        Ok(record)
    }
}

/// `rotation.restarted`（relay `RotationRestarted`，键 0..5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationRestarted {
    pub old_rotation: RotationId,
    pub new_rotation: RotationId,
    pub from_epoch: u64,
    pub old_to_epoch: u64,
    pub new_to_epoch: u64,
    pub freeze_seq: u64,
}

impl RotationRestarted {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(vec![
            (0, CdeValue::Bytes(self.old_rotation.to_vec())),
            (1, CdeValue::Bytes(self.new_rotation.to_vec())),
            (2, CdeValue::UInt(self.from_epoch)),
            (3, CdeValue::UInt(self.old_to_epoch)),
            (4, CdeValue::UInt(self.new_to_epoch)),
            (5, CdeValue::UInt(self.freeze_seq)),
        ]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 6)?;
        let mut fields = fields
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as u64, value))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            old_rotation: take_id16(&mut fields, 0, "RotationRestarted.old_rotation")?,
            new_rotation: take_id16(&mut fields, 1, "RotationRestarted.new_rotation")?,
            from_epoch: take_uint(&mut fields, 2, "RotationRestarted.from_epoch")?,
            old_to_epoch: take_uint(&mut fields, 3, "RotationRestarted.old_to_epoch")?,
            new_to_epoch: take_uint(&mut fields, 4, "RotationRestarted.new_to_epoch")?,
            freeze_seq: take_uint(&mut fields, 5, "RotationRestarted.freeze_seq")?,
        })
    }
}

/// `rotation.revoked-more`（relay `RevocationsAdded`，键 0..3，3 可省略）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevocationsAdded {
    pub rotation_id: RotationId,
    pub devices: Vec<DeviceId>,
    pub restarted: bool,
    pub new_rotation: Option<RotationId>,
}

impl RevocationsAdded {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut fields = vec![
            (0, CdeValue::Bytes(self.rotation_id.to_vec())),
            (
                1,
                CdeValue::Array(
                    self.devices
                        .iter()
                        .map(|id| CdeValue::Bytes(id.to_vec()))
                        .collect(),
                ),
            ),
            (2, CdeValue::Bool(self.restarted)),
        ];
        if let Some(id) = self.new_rotation {
            fields.push((3, CdeValue::Bytes(id.to_vec())));
        }
        encode(&record_map(fields))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut fields = optional_fields(decode(bytes)?, &[0, 1, 2, 3], &[0, 1, 2])?;
        let rotation_id = take_id16(&mut fields, 0, "RevocationsAdded.rotation_id")?;
        let devices = expect_array(take(&mut fields, 1)?, "RevocationsAdded.devices")?
            .into_iter()
            .map(|value| expect_bytes_n::<16>(value, "RevocationsAdded.devices[]"))
            .collect::<Result<Vec<_>, _>>()?;
        let restarted = expect_bool(take(&mut fields, 2)?, "RevocationsAdded.restarted")?;
        let new_rotation = fields
            .remove(&3)
            .map(|value| expect_bytes_n::<16>(value, "RevocationsAdded.new_rotation"))
            .transpose()?;
        Ok(Self {
            rotation_id,
            devices,
            restarted,
            new_rotation,
        })
    }
}

/// `ActivationCommitV1`（relay `rotations::ActivationCommit`，键 0..15，14 可省略）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationCommitV1 {
    pub vault_id: [u8; 16],
    pub instance_id: [u8; 16],
    pub rotation_id: RotationId,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub covered_seq: u64,
    pub primary_checkpoint: CheckpointId,
    pub fallback_checkpoint: CheckpointId,
    pub primary_covered_seq: u64,
    pub fallback_covered_seq: u64,
    pub primary_manifest: Sha256,
    pub fallback_manifest: Sha256,
    pub member_set_hash: Sha256,
    pub grants_digest: Sha256,
    pub asset_set_digest: Option<Sha256>,
    pub control_rev: u64,
}

impl ActivationCommitV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut fields = vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.rotation_id.to_vec())),
            (3, CdeValue::UInt(self.from_epoch)),
            (4, CdeValue::UInt(self.to_epoch)),
            (5, CdeValue::UInt(self.covered_seq)),
            (6, CdeValue::Bytes(self.primary_checkpoint.to_vec())),
            (7, CdeValue::Bytes(self.fallback_checkpoint.to_vec())),
            (8, CdeValue::UInt(self.primary_covered_seq)),
            (9, CdeValue::UInt(self.fallback_covered_seq)),
            (10, CdeValue::Bytes(self.primary_manifest.to_vec())),
            (11, CdeValue::Bytes(self.fallback_manifest.to_vec())),
            (12, CdeValue::Bytes(self.member_set_hash.to_vec())),
            (13, CdeValue::Bytes(self.grants_digest.to_vec())),
            (15, CdeValue::UInt(self.control_rev)),
        ];
        if let Some(digest) = self.asset_set_digest {
            fields.push((14, CdeValue::Bytes(digest.to_vec())));
        }
        encode(&record_map(fields))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut fields = optional_fields(
            decode(bytes)?,
            &(0..=15).collect::<Vec<_>>(),
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15],
        )?;
        Ok(Self {
            vault_id: take_id16(&mut fields, 0, "ActivationCommitV1.vault_id")?,
            instance_id: take_id16(&mut fields, 1, "ActivationCommitV1.instance_id")?,
            rotation_id: take_id16(&mut fields, 2, "ActivationCommitV1.rotation_id")?,
            from_epoch: take_uint(&mut fields, 3, "ActivationCommitV1.from_epoch")?,
            to_epoch: take_uint(&mut fields, 4, "ActivationCommitV1.to_epoch")?,
            covered_seq: take_uint(&mut fields, 5, "ActivationCommitV1.covered_seq")?,
            primary_checkpoint: take_id16(&mut fields, 6, "ActivationCommitV1.primary_checkpoint")?,
            fallback_checkpoint: take_id16(
                &mut fields,
                7,
                "ActivationCommitV1.fallback_checkpoint",
            )?,
            primary_covered_seq: take_uint(
                &mut fields,
                8,
                "ActivationCommitV1.primary_covered_seq",
            )?,
            fallback_covered_seq: take_uint(
                &mut fields,
                9,
                "ActivationCommitV1.fallback_covered_seq",
            )?,
            primary_manifest: take_hash32(&mut fields, 10, "ActivationCommitV1.primary_manifest")?,
            fallback_manifest: take_hash32(
                &mut fields,
                11,
                "ActivationCommitV1.fallback_manifest",
            )?,
            member_set_hash: take_hash32(&mut fields, 12, "ActivationCommitV1.member_set_hash")?,
            grants_digest: take_hash32(&mut fields, 13, "ActivationCommitV1.grants_digest")?,
            asset_set_digest: fields
                .remove(&14)
                .map(|value| expect_bytes_n::<32>(value, "ActivationCommitV1.asset_set_digest"))
                .transpose()?,
            control_rev: take_uint(&mut fields, 15, "ActivationCommitV1.control_rev")?,
        })
    }
}

/// `POST /rotations` 的 canonical body（键 0..3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationBeginBodyV1 {
    pub expected_epoch: u64,
    pub expected_control_rev: u64,
    pub request_id: DeviceId,
    pub revoke: Vec<DeviceId>,
}

impl RotationBeginBodyV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(vec![
            (0, CdeValue::UInt(self.expected_epoch)),
            (1, CdeValue::UInt(self.expected_control_rev)),
            (2, CdeValue::Bytes(self.request_id.to_vec())),
            (
                3,
                CdeValue::Array(
                    self.revoke
                        .iter()
                        .map(|id| CdeValue::Bytes(id.to_vec()))
                        .collect(),
                ),
            ),
        ]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 4)?;
        let mut fields = fields
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as u64, value))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            expected_epoch: take_uint(&mut fields, 0, "RotationBeginBodyV1.expected_epoch")?,
            expected_control_rev: take_uint(
                &mut fields,
                1,
                "RotationBeginBodyV1.expected_control_rev",
            )?,
            request_id: take_id16(&mut fields, 2, "RotationBeginBodyV1.request_id")?,
            revoke: expect_array(take(&mut fields, 3)?, "RotationBeginBodyV1.revoke")?
                .into_iter()
                .map(|value| expect_bytes_n::<16>(value, "RotationBeginBodyV1.revoke[]"))
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

/// activate body 里的管理权转换包装
/// （键 0 transition / 1 old_sig? / 2 new_sig? / 3 activation_t? / 4 old_t_sig? / 5 new_t_sig?）。
///
/// ADR 0006 §34：完整 activation T（canonical CDE）与旧/新管理 key 对该 T 的双签随
/// `new_manager` 一起传输。3–5 在 wire 上**必须同时存在或全缺**（全缺只用于渐进接入
/// 的客户端与历史 body；relay 执行 activate 时三者必填，缺任一 → 409）。1/2 保留既有
/// 可选语义（管理权转换双签，relay 自行校验）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewManagerV1 {
    pub transition: Vec<u8>,
    pub old_signature: Option<Vec<u8>>,
    pub new_signature: Option<Vec<u8>>,
    /// 3：complete activation T 的 canonical CDE 原字节。
    pub activation_t: Option<Vec<u8>>,
    /// 4：旧管理 key 对 `activation_t_signing_input(T)` 的 64B 签名。
    pub old_t_signature: Option<Vec<u8>>,
    /// 5：新管理 key 对同一输入的 64B 签名。
    pub new_t_signature: Option<Vec<u8>>,
}

impl NewManagerV1 {
    /// 3–5 同时存在或全缺；存在时两份 T 签名各 64B、T 必须是可解码的 `ActivationTV1`。
    fn validate(&self) -> Result<(), ProtoError> {
        let present = [
            self.activation_t.is_some(),
            self.old_t_signature.is_some(),
            self.new_t_signature.is_some(),
        ];
        if present.iter().all(|value| !value) {
            return Ok(());
        }
        if !present.iter().all(|value| *value) {
            return Err(ProtoError::Invalid(
                "NewManagerV1 的 activation_t 与旧/新 T 签名必须同时存在".to_owned(),
            ));
        }
        let activation_t = self
            .activation_t
            .as_deref()
            .ok_or_else(|| ProtoError::Invalid("NewManagerV1.activation_t".to_owned()))?;
        ActivationTV1::decode(activation_t)?;
        for (name, signature) in [
            ("NewManagerV1.old_t_signature", &self.old_t_signature),
            ("NewManagerV1.new_t_signature", &self.new_t_signature),
        ] {
            let signature = signature
                .as_deref()
                .ok_or_else(|| ProtoError::Invalid(name.to_owned()))?;
            if signature.len() != 64 {
                return Err(ProtoError::Invalid(format!(
                    "{name} 必须是 64 字节，实际 {}",
                    signature.len()
                )));
            }
        }
        Ok(())
    }

    fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        let mut fields = vec![(0, CdeValue::Bytes(self.transition.clone()))];
        for (key, signature) in [
            (1u64, &self.old_signature),
            (2, &self.new_signature),
            (3, &self.activation_t),
            (4, &self.old_t_signature),
            (5, &self.new_t_signature),
        ] {
            if let Some(value) = signature {
                fields.push((key, CdeValue::Bytes(value.clone())));
            }
        }
        Ok(record_map(fields))
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_fields(optional_fields(decode(bytes)?, &[0, 1, 2, 3, 4, 5], &[0])?)
    }

    fn from_fields(mut fields: BTreeMap<u64, CdeValue>) -> Result<Self, ProtoError> {
        let record = Self {
            transition: expect_bytes(take(&mut fields, 0)?, "NewManagerV1.transition")?,
            old_signature: fields
                .remove(&1)
                .map(|value| expect_bytes(value, "NewManagerV1.old_signature"))
                .transpose()?,
            new_signature: fields
                .remove(&2)
                .map(|value| expect_bytes(value, "NewManagerV1.new_signature"))
                .transpose()?,
            activation_t: fields
                .remove(&3)
                .map(|value| expect_bytes(value, "NewManagerV1.activation_t"))
                .transpose()?,
            old_t_signature: fields
                .remove(&4)
                .map(|value| expect_bytes(value, "NewManagerV1.old_t_signature"))
                .transpose()?,
            new_t_signature: fields
                .remove(&5)
                .map(|value| expect_bytes(value, "NewManagerV1.new_t_signature"))
                .transpose()?,
        };
        record.validate()?;
        Ok(record)
    }
}

/// `POST /rotations/{r}/activate` 的 canonical body（键 0..6，均可省略）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationActivateBodyV1 {
    pub new_manager: Option<NewManagerV1>,
    pub primary_checkpoint: Option<CheckpointId>,
    pub fallback_checkpoint: Option<CheckpointId>,
    pub member_cert_digest: Option<Sha256>,
    pub grants_digest: Option<Sha256>,
    pub slot_set_digest: Option<Sha256>,
    pub asset_set_digest: Option<Sha256>,
}

impl RotationActivateBodyV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut fields = Vec::new();
        if let Some(manager) = &self.new_manager {
            fields.push((0, manager.to_cde()?));
        }
        if let Some(id) = self.primary_checkpoint {
            fields.push((1, CdeValue::Bytes(id.to_vec())));
        }
        if let Some(id) = self.fallback_checkpoint {
            fields.push((2, CdeValue::Bytes(id.to_vec())));
        }
        if let Some(digest) = self.member_cert_digest {
            fields.push((3, CdeValue::Bytes(digest.to_vec())));
        }
        if let Some(digest) = self.grants_digest {
            fields.push((4, CdeValue::Bytes(digest.to_vec())));
        }
        if let Some(digest) = self.slot_set_digest {
            fields.push((5, CdeValue::Bytes(digest.to_vec())));
        }
        if let Some(digest) = self.asset_set_digest {
            fields.push((6, CdeValue::Bytes(digest.to_vec())));
        }
        encode(&record_map(fields))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut fields = optional_fields(decode(bytes)?, &[0, 1, 2, 3, 4, 5, 6], &[])?;
        let new_manager = fields
            .remove(&0)
            .map(|value| {
                NewManagerV1::from_fields(optional_fields(
                    value,
                    &[0, 1, 2, 3, 4, 5],
                    &[0, 3, 4, 5],
                )?)
            })
            .transpose()?;
        Ok(Self {
            new_manager,
            primary_checkpoint: fields
                .remove(&1)
                .map(|value| expect_bytes_n::<16>(value, "RotationActivateBodyV1.primary"))
                .transpose()?,
            fallback_checkpoint: fields
                .remove(&2)
                .map(|value| expect_bytes_n::<16>(value, "RotationActivateBodyV1.fallback"))
                .transpose()?,
            member_cert_digest: fields
                .remove(&3)
                .map(|value| expect_bytes_n::<32>(value, "RotationActivateBodyV1.member_digest"))
                .transpose()?,
            grants_digest: fields
                .remove(&4)
                .map(|value| expect_bytes_n::<32>(value, "RotationActivateBodyV1.grants_digest"))
                .transpose()?,
            slot_set_digest: fields
                .remove(&5)
                .map(|value| expect_bytes_n::<32>(value, "RotationActivateBodyV1.slot_digest"))
                .transpose()?,
            asset_set_digest: fields
                .remove(&6)
                .map(|value| expect_bytes_n::<32>(value, "RotationActivateBodyV1.asset_digest"))
                .transpose()?,
        })
    }
}

/// `POST /rotations/{r}/grants/{device}` 的 canonical body（键 0..5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantStageBodyV1 {
    pub grant_id: DeviceId,
    pub hpke_enc: [u8; 32],
    pub hpke_ciphertext: Vec<u8>,
    pub signed_header: Vec<u8>,
    pub signature: Vec<u8>,
    pub manager_rev: u64,
}

impl GrantStageBodyV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(vec![
            (0, CdeValue::Bytes(self.grant_id.to_vec())),
            (1, CdeValue::Bytes(self.hpke_enc.to_vec())),
            (2, CdeValue::Bytes(self.hpke_ciphertext.clone())),
            (3, CdeValue::Bytes(self.signed_header.clone())),
            (4, CdeValue::Bytes(self.signature.clone())),
            (5, CdeValue::UInt(self.manager_rev)),
        ]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 6)?;
        let mut fields = fields
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as u64, value))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            grant_id: take_id16(&mut fields, 0, "GrantStageBodyV1.grant_id")?,
            hpke_enc: take_hash32(&mut fields, 1, "GrantStageBodyV1.hpke_enc")?,
            hpke_ciphertext: expect_bytes(
                take(&mut fields, 2)?,
                "GrantStageBodyV1.hpke_ciphertext",
            )?,
            signed_header: expect_bytes(take(&mut fields, 3)?, "GrantStageBodyV1.signed_header")?,
            signature: expect_bytes(take(&mut fields, 4)?, "GrantStageBodyV1.signature")?,
            manager_rev: take_uint(&mut fields, 5, "GrantStageBodyV1.manager_rev")?,
        })
    }
}

/// `POST /rotations/{r}/abort` 的 canonical body：空 map。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RotationAbortBodyV1;

impl RotationAbortBodyV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(Vec::new()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = optional_fields(decode(bytes)?, &[], &[])?;
        debug_assert!(fields.is_empty());
        Ok(Self)
    }
}

/// `POST /key-slots/password/activate` 的 canonical body（键 0/1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotActivateBodyV1 {
    pub expected_slot_set_rev: u64,
    pub new_slot_id: Sha256,
}

impl SlotActivateBodyV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(vec![
            (0, CdeValue::UInt(self.expected_slot_set_rev)),
            (1, CdeValue::Bytes(self.new_slot_id.to_vec())),
        ]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 2)?;
        let mut fields = fields
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as u64, value))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            expected_slot_set_rev: take_uint(
                &mut fields,
                0,
                "SlotActivateBodyV1.expected_slot_set_rev",
            )?,
            new_slot_id: take_hash32(&mut fields, 1, "SlotActivateBodyV1.new_slot_id")?,
        })
    }
}

/// `POST /invites` 的 canonical body（键 0 token_hash / 1 expires_at?）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteCreateBodyV1 {
    pub token_hash: Sha256,
    pub expires_at: Option<u64>,
}

impl InviteCreateBodyV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut fields = vec![(0, CdeValue::Bytes(self.token_hash.to_vec()))];
        if let Some(expires_at) = self.expires_at {
            fields.push((1, CdeValue::UInt(expires_at)));
        }
        encode(&record_map(fields))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut fields = optional_fields(decode(bytes)?, &[0, 1], &[0])?;
        Ok(Self {
            token_hash: take_hash32(&mut fields, 0, "InviteCreateBodyV1.token_hash")?,
            expires_at: fields
                .remove(&1)
                .map(|value| expect_i64_max(value, "InviteCreateBodyV1.expires_at"))
                .transpose()?,
        })
    }
}

/// `POST /devices/{device}/approve` 的 canonical body（键 0..7）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceApproveBodyV1 {
    pub certificate: Vec<u8>,
    pub certificate_signature: Vec<u8>,
    pub grant_id: DeviceId,
    pub hpke_enc: [u8; 32],
    pub hpke_ciphertext: Vec<u8>,
    pub grant_signature: Vec<u8>,
    pub request_id: DeviceId,
    pub expected_control_rev: u64,
}

impl DeviceApproveBodyV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(vec![
            (0, CdeValue::Bytes(self.certificate.clone())),
            (1, CdeValue::Bytes(self.certificate_signature.clone())),
            (2, CdeValue::Bytes(self.grant_id.to_vec())),
            (3, CdeValue::Bytes(self.hpke_enc.to_vec())),
            (4, CdeValue::Bytes(self.hpke_ciphertext.clone())),
            (5, CdeValue::Bytes(self.grant_signature.clone())),
            (6, CdeValue::Bytes(self.request_id.to_vec())),
            (7, CdeValue::UInt(self.expected_control_rev)),
        ]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 8)?;
        let mut fields = fields
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as u64, value))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            certificate: expect_bytes(take(&mut fields, 0)?, "DeviceApproveBodyV1.certificate")?,
            certificate_signature: expect_bytes(
                take(&mut fields, 1)?,
                "DeviceApproveBodyV1.certificate_signature",
            )?,
            grant_id: take_id16(&mut fields, 2, "DeviceApproveBodyV1.grant_id")?,
            hpke_enc: take_hash32(&mut fields, 3, "DeviceApproveBodyV1.hpke_enc")?,
            hpke_ciphertext: expect_bytes(
                take(&mut fields, 4)?,
                "DeviceApproveBodyV1.hpke_ciphertext",
            )?,
            grant_signature: expect_bytes(
                take(&mut fields, 5)?,
                "DeviceApproveBodyV1.grant_signature",
            )?,
            request_id: take_id16(&mut fields, 6, "DeviceApproveBodyV1.request_id")?,
            expected_control_rev: take_uint(
                &mut fields,
                7,
                "DeviceApproveBodyV1.expected_control_rev",
            )?,
        })
    }
}

/// `member_cert_digest` 的字节输入项（relay 按 device_id 升序拼接）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberCertDigestInputV1 {
    pub device_id: DeviceId,
    pub certificate: Vec<u8>,
    pub certificate_signature: Option<Vec<u8>>,
}

/// 成员证书 digest 的**字节输入**（不做 SHA-256）；按 `device_id` 升序。
#[must_use]
pub fn member_cert_digest(entries: &[MemberCertDigestInputV1]) -> Vec<u8> {
    let mut ordered = entries.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|entry| entry.device_id);
    let mut out = Vec::new();
    for entry in ordered {
        out.extend_from_slice(&entry.device_id);
        out.extend_from_slice(&entry.certificate);
        if let Some(signature) = &entry.certificate_signature {
            out.extend_from_slice(signature);
        }
    }
    out
}

/// `slot_set_digest` 的字节输入项；`descriptor_sha256` 由调用方先算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotSetDigestInputV1 {
    pub slot_id: Sha256,
    pub descriptor_sha256: Sha256,
}

/// 槽集合 digest 的**字节输入**（不做 SHA-256）；按 `slot_id` 升序。
#[must_use]
pub fn slot_set_digest(entries: &[SlotSetDigestInputV1]) -> Vec<u8> {
    let mut ordered = entries.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|entry| entry.slot_id);
    let mut out = Vec::new();
    for entry in ordered {
        out.extend_from_slice(&entry.slot_id);
        out.extend_from_slice(&entry.descriptor_sha256);
    }
    out
}

/// grant 包 digest 的**字节输入**（不做 SHA-256）：
/// `hpke_enc || ciphertext_sha256 || signed_header_sha256 || signature`。
#[must_use]
pub fn grant_package_digest(
    hpke_enc: &Sha256,
    ciphertext_sha256: &Sha256,
    signed_header_sha256: &Sha256,
    signature: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + 32 + 32 + signature.len());
    out.extend_from_slice(hpke_enc);
    out.extend_from_slice(ciphertext_sha256);
    out.extend_from_slice(signed_header_sha256);
    out.extend_from_slice(signature);
    out
}

/// 管理 canonical body 的简名别名（begin/activate/grants/abort/slots-activate/invites/approve）。
pub type BeginBodyV1 = RotationBeginBodyV1;
pub type ActivateBodyV1 = RotationActivateBodyV1;
pub type GrantsBodyV1 = GrantStageBodyV1;
pub type AbortBodyV1 = RotationAbortBodyV1;
pub type SlotsActivateBodyV1 = SlotActivateBodyV1;
pub type InvitesBodyV1 = InviteCreateBodyV1;
pub type ApproveBodyV1 = DeviceApproveBodyV1;
