//! 控制链的 **typed** 记录（`SignedControlRecord` + 事件）。
//!
//! ADR 0006 §28.1 条目 4/10、评审 0001 §三 5/10：控制链此前只追加"裸 signed payload"，
//! 客户端无法知道事件种类、也拿不到管理签名（signature 只进了 log）。这里定义 relay 侧的
//! CDE 记录，并在 `/control` 输出里带 `kind`：
//!
//! `SignedControlRecord`（整数键，键从 0 连续；optional 省略）：
//! - 0 kind(text)：`rotation.begin` / `rotation.abort` / `epoch.activated` /
//!   `rotation.restarted` / `rotation.revoked-more`
//! - 1 request_id(bytes16)  2 manager_rev(uint)  3 expected_control_rev(uint)
//! - 4 signed_body(bytes)（客户端被签名的 canonical body 原字节）
//! - 5 signature(bytes64)（管理 key 对 `management_signing_input` 的签名）
//! - 6 event(bytes, optional)：typed 事件（见下）
//!
//! 事件：
//! - [`EpochActivated`]：`0 vault,1 instance,2 rotation,3 from,4 to,5 S,
//!   6 primary_cid,7 fallback_cid,8 activation_commit,9 member_set_hash,
//!   10 grants_digest,11 control_rev,12 auth_rev`
//! - [`RotationRestarted`]：`0 old_rotation,1 new_rotation,2 from_epoch,3 old_to_epoch,
//!   4 new_to_epoch,5 freeze_seq`
//! - [`RevocationsAdded`]：`0 rotation,1 devices(array<bytes16>),2 restarted(bool),
//!   3 new_rotation?(bytes16)`
//!
//! **协议化说明**：这些记录目前定义在 relay（`crates/sync-proto` 的改动需 Lead 另行分派，
//! 本任务禁止私改）；客户端 decode/验证需按本节字段编号实现，后续再收敛进 `sync-proto`。

use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::ids::Id16;
use crate::vault;
use crate::wire;
use menghunote_sync_proto as proto;

/// 控制记录种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKind {
    RotationBegin,
    RotationAbort,
    EpochActivated,
    RotationRestarted,
    RotationRevokedMore,
}

impl ControlKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RotationBegin => "rotation.begin",
            Self::RotationAbort => "rotation.abort",
            Self::EpochActivated => "epoch.activated",
            Self::RotationRestarted => "rotation.restarted",
            Self::RotationRevokedMore => "rotation.revoked-more",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "rotation.begin" => Some(Self::RotationBegin),
            "rotation.abort" => Some(Self::RotationAbort),
            "epoch.activated" => Some(Self::EpochActivated),
            "rotation.restarted" => Some(Self::RotationRestarted),
            "rotation.revoked-more" => Some(Self::RotationRevokedMore),
            _ => None,
        }
    }
}

/// 一条 signed 控制记录（含管理签名与可选 typed 事件）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedControlRecord {
    pub kind: ControlKind,
    pub request_id: Id16,
    pub manager_rev: u64,
    pub expected_control_rev: u64,
    pub signed_body: Vec<u8>,
    pub signature: Vec<u8>,
    pub event: Option<Vec<u8>>,
}

impl SignedControlRecord {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut fields = vec![
            (0, wire::text(self.kind.as_str())),
            (1, wire::bytes(self.request_id)),
            (2, wire::uint(self.manager_rev)),
            (3, wire::uint(self.expected_control_rev)),
            (4, wire::bytes(self.signed_body.clone())),
            (5, wire::bytes(self.signature.clone())),
        ];
        if let Some(event) = &self.event {
            fields.push((6, wire::bytes(event.clone())));
        }
        proto::encode_cde(&wire::map(fields))
            .map_err(|_| Error::Invariant("signed control record encode"))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let value =
            proto::decode_cde(bytes).map_err(|_| Error::api(crate::ErrorCode::InvalidRequest))?;
        let fields = wire::value_map(&value)?;
        Ok(Self {
            kind: ControlKind::parse(&wire::text_field(&fields, 0)?)
                .ok_or_else(|| Error::api(crate::ErrorCode::InvalidRequest))?,
            request_id: wire::id16_field(&fields, 1)?,
            manager_rev: wire::uint_field(&fields, 2)?,
            expected_control_rev: wire::uint_field(&fields, 3)?,
            signed_body: wire::bytes_field(&fields, 4)?,
            signature: wire::bytes_field(&fields, 5)?,
            event: wire::opt_bytes_field(&fields, 6)?,
        })
    }
}

/// 追加一条 signed 控制记录，返回控制链新 rev。
pub fn append(conn: &Connection, now: i64, record: &SignedControlRecord) -> Result<u64> {
    vault::append_control(conn, now, &record.encode()?)
}

/// `/control` 输出用：从记录字节里识别 kind（旧记录是裸 CDE，返回 None = `raw`）。
pub fn kind_of(bytes: &[u8]) -> Option<String> {
    let value = proto::decode_cde(bytes).ok()?;
    let proto::CdeValue::Map(fields) = value else {
        return None;
    };
    let kind = wire::opt_field(&fields, 0)?;
    match kind {
        proto::CdeValue::Text(text) => Some(text.clone()),
        _ => None,
    }
}

/// `epoch.activated` 的 typed 事件（rid/E/S/commit hash/control_rev）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochActivated {
    pub vault_id: Id16,
    pub instance_id: Id16,
    pub rotation_id: Id16,
    pub from_epoch: u64,
    pub to_epoch: u64,
    /// 冻结截止 S。
    pub covered_seq: u64,
    pub primary_checkpoint: Id16,
    pub fallback_checkpoint: Id16,
    /// 激活 commit hash（`ActivationCommitV1` 的 SHA256）。
    pub activation_commit: [u8; 32],
    pub member_set_hash: [u8; 32],
    pub grants_digest: [u8; 32],
    pub control_rev: u64,
    pub auth_rev: u64,
}

impl EpochActivated {
    pub fn encode(&self) -> Result<Vec<u8>> {
        proto::encode_cde(&wire::map(vec![
            (0, wire::bytes(self.vault_id)),
            (1, wire::bytes(self.instance_id)),
            (2, wire::bytes(self.rotation_id)),
            (3, wire::uint(self.from_epoch)),
            (4, wire::uint(self.to_epoch)),
            (5, wire::uint(self.covered_seq)),
            (6, wire::bytes(self.primary_checkpoint)),
            (7, wire::bytes(self.fallback_checkpoint)),
            (8, wire::bytes(self.activation_commit)),
            (9, wire::bytes(self.member_set_hash)),
            (10, wire::bytes(self.grants_digest)),
            (11, wire::uint(self.control_rev)),
            (12, wire::uint(self.auth_rev)),
        ]))
        .map_err(|_| Error::Invariant("epoch activated event encode"))
    }

    /// 解码 `epoch.activated` 事件（客户端/测试按字段编号解析）。
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let value =
            proto::decode_cde(bytes).map_err(|_| Error::api(crate::ErrorCode::InvalidRequest))?;
        let fields = wire::value_map(&value)?;
        Ok(Self {
            vault_id: wire::id16_field(&fields, 0)?,
            instance_id: wire::id16_field(&fields, 1)?,
            rotation_id: wire::id16_field(&fields, 2)?,
            from_epoch: wire::uint_field(&fields, 3)?,
            to_epoch: wire::uint_field(&fields, 4)?,
            covered_seq: wire::uint_field(&fields, 5)?,
            primary_checkpoint: wire::id16_field(&fields, 6)?,
            fallback_checkpoint: wire::id16_field(&fields, 7)?,
            activation_commit: wire::hash32_field(&fields, 8)?,
            member_set_hash: wire::hash32_field(&fields, 9)?,
            grants_digest: wire::hash32_field(&fields, 10)?,
            control_rev: wire::uint_field(&fields, 11)?,
            auth_rev: wire::uint_field(&fields, 12)?,
        })
    }
}

/// `rotation.restarted` 事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationRestarted {
    pub old_rotation: Id16,
    pub new_rotation: Id16,
    pub from_epoch: u64,
    pub old_to_epoch: u64,
    pub new_to_epoch: u64,
    pub freeze_seq: u64,
}

impl RotationRestarted {
    pub fn encode(&self) -> Result<Vec<u8>> {
        proto::encode_cde(&wire::map(vec![
            (0, wire::bytes(self.old_rotation)),
            (1, wire::bytes(self.new_rotation)),
            (2, wire::uint(self.from_epoch)),
            (3, wire::uint(self.old_to_epoch)),
            (4, wire::uint(self.new_to_epoch)),
            (5, wire::uint(self.freeze_seq)),
        ]))
        .map_err(|_| Error::Invariant("rotation restarted event encode"))
    }
}

/// `rotation.revoked-more` 事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevocationsAdded {
    pub rotation_id: Id16,
    pub devices: Vec<Id16>,
    pub restarted: bool,
    pub new_rotation: Option<Id16>,
}

impl RevocationsAdded {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut fields = vec![
            (0, wire::bytes(self.rotation_id)),
            (
                1,
                wire::array(self.devices.iter().map(|id| wire::bytes(*id)).collect()),
            ),
            (2, wire::boolean(self.restarted)),
        ];
        if let Some(id) = self.new_rotation {
            fields.push((3, wire::bytes(id)));
        }
        proto::encode_cde(&wire::map(fields))
            .map_err(|_| Error::Invariant("revocations added event encode"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> SignedControlRecord {
        SignedControlRecord {
            kind: ControlKind::EpochActivated,
            request_id: [1u8; 16],
            manager_rev: 2,
            expected_control_rev: 7,
            signed_body: b"body".to_vec(),
            signature: vec![0xAA; 64],
            event: Some(b"event".to_vec()),
        }
    }

    #[test]
    fn signed_control_record_roundtrips_and_exposes_kind() {
        let encoded = record().encode().expect("encode");
        assert_eq!(
            SignedControlRecord::decode(&encoded).expect("decode"),
            record()
        );
        assert_eq!(kind_of(&encoded).as_deref(), Some("epoch.activated"));
        // 裸 CDE（genesis 证书）不识别为 typed 记录。
        let raw = proto::encode_cde(&wire::map(vec![(0, wire::uint(1))])).expect("raw");
        assert_eq!(kind_of(&raw), None);
        assert!(SignedControlRecord::decode(&raw).is_err());
    }

    #[test]
    fn events_encode_as_canonical_cde() {
        let event = EpochActivated {
            vault_id: [1u8; 16],
            instance_id: [2u8; 16],
            rotation_id: [3u8; 16],
            from_epoch: 1,
            to_epoch: 2,
            covered_seq: 9,
            primary_checkpoint: [4u8; 16],
            fallback_checkpoint: [5u8; 16],
            activation_commit: [6u8; 32],
            member_set_hash: [7u8; 32],
            grants_digest: [8u8; 32],
            control_rev: 11,
            auth_rev: 12,
        };
        let encoded = event.encode().expect("encode");
        // 同一语义重编码必须逐字节一致（CDE 唯一性）。
        assert_eq!(encoded, event.encode().expect("re-encode"));
        let restarted = RotationRestarted {
            old_rotation: [1u8; 16],
            new_rotation: [2u8; 16],
            from_epoch: 1,
            old_to_epoch: 2,
            new_to_epoch: 3,
            freeze_seq: 5,
        };
        assert_eq!(
            restarted.encode().expect("restart"),
            restarted.encode().expect("restart re-encode")
        );
        let revoked = RevocationsAdded {
            rotation_id: [1u8; 16],
            devices: vec![[2u8; 16]],
            restarted: true,
            new_rotation: Some([3u8; 16]),
        };
        assert_eq!(
            revoked.encode().expect("revoke"),
            revoked.encode().expect("revoke re-encode")
        );
    }
}
