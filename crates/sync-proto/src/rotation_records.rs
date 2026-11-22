//! ADR 0006 §30.2：relay 轮换新增的共享 CDE 记录。
//!
//! 字段编号与 `crates/relay/src/{control,rotations,api}.rs` / ADR §30 逐字节一致；
//! 本模块只做 canonical 编解码与 digest 的**字节输入**，不做 SHA-256、不碰 SQLite。
//!
//! `SignedControlRecord` 的 `event` 保存 relay 事件 CDE 原字节；客户端按 kind 用
//! [`EpochActivated::decode`]、[`RotationRestarted::decode`]、[`RevocationsAdded::decode`] 解码。

use std::collections::BTreeMap;

use crate::cde::{
    decode, encode, expect_array, expect_bool, expect_bytes, expect_bytes_n, expect_i64_max,
    expect_text, into_record_fields, CdeValue,
};
use crate::error::ProtoError;
use crate::records::DeviceId;

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
    /// 可选 typed 事件 CDE 原字节（kind=epoch.activated / rotation.restarted /
    /// rotation.revoked-more 时存在）。
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

    /// 按 kind 解码 event；kind 为 begin/abort 或 event 缺失时返回 `Ok(None)`。
    pub fn decoded_event(&self) -> Result<Option<ControlEvent>, ProtoError> {
        let Some(bytes) = &self.event else {
            return Ok(None);
        };
        let event = match self.kind {
            ControlKind::EpochActivated => {
                ControlEvent::EpochActivated(EpochActivated::decode(bytes)?)
            }
            ControlKind::RotationRestarted => {
                ControlEvent::RotationRestarted(RotationRestarted::decode(bytes)?)
            }
            ControlKind::RevocationsAdded => {
                ControlEvent::RevocationsAdded(RevocationsAdded::decode(bytes)?)
            }
            ControlKind::RotationBegin | ControlKind::RotationAbort => {
                return Err(ProtoError::Invalid(
                    "rotation.begin/abort 不应带 typed event".to_owned(),
                ));
            }
        };
        Ok(Some(event))
    }
}

/// `SignedControlRecord` 携带的 typed 事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    EpochActivated(EpochActivated),
    RotationRestarted(RotationRestarted),
    RevocationsAdded(RevocationsAdded),
}

/// `epoch.activated`（relay `EpochActivated`，键 0..12）。
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
}

impl EpochActivated {
    /// `activation_commit` 字段的别名（语义为 ActivationCommitV1 的 SHA-256）。
    #[must_use]
    pub fn activation_commit_hash(&self) -> Sha256 {
        self.activation_commit
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(vec![
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
        ]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 13)?;
        let mut fields = fields
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as u64, value))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
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
        })
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

/// activate body 里的管理权转换包装（键 0 transition / 1 old_sig? / 2 new_sig?）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewManagerV1 {
    pub transition: Vec<u8>,
    pub old_signature: Option<Vec<u8>>,
    pub new_signature: Option<Vec<u8>>,
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
            let mut manager_fields = vec![(0, CdeValue::Bytes(manager.transition.clone()))];
            if let Some(signature) = &manager.old_signature {
                manager_fields.push((1, CdeValue::Bytes(signature.clone())));
            }
            if let Some(signature) = &manager.new_signature {
                manager_fields.push((2, CdeValue::Bytes(signature.clone())));
            }
            fields.push((0, record_map(manager_fields)));
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
                let mut manager = optional_fields(value, &[0, 1, 2], &[0])?;
                Ok::<_, ProtoError>(NewManagerV1 {
                    transition: expect_bytes(
                        take(&mut manager, 0)?,
                        "RotationActivateBodyV1.new_manager.transition",
                    )?,
                    old_signature: manager
                        .remove(&1)
                        .map(|value| expect_bytes(value, "RotationActivateBodyV1.old_signature"))
                        .transpose()?,
                    new_signature: manager
                        .remove(&2)
                        .map(|value| expect_bytes(value, "RotationActivateBodyV1.new_signature"))
                        .transpose()?,
                })
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
