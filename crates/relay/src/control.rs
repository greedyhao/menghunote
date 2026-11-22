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
//!   10 grants_digest,11 control_rev,12 auth_rev,13 activation_t?,
//!   14 old_t_signature?,15 new_t_signature?`（ADR §34：13–15 同时存在或全缺）
//! - [`RotationAborted`]：`0 rotation,1 method,2 path,3 body_digest`（ADR §35.3：
//!   abort 的完整被签事实，客户端据此重建 `management_signing_input` 并验签）
//! - [`RotationRestarted`]：`0 old_rotation,1 new_rotation,2 from_epoch,3 old_to_epoch,
//!   4 new_to_epoch,5 freeze_seq`
//! - [`RevocationsAdded`]：`0 rotation,1 devices(array<bytes16>),2 restarted(bool),
//!   3 new_rotation?(bytes16)`
//!
//! **协议化说明**：record/event 的字段编号与 canonical 编解码由 `sync-proto` 冻结
//! （`proto::SignedControlRecord` / `proto::RotationAborted`）；本模块只保留 relay
//! 语义类型并委托给 proto，避免两处字段编号漂移。

use crate::error::{Error, Result};
use crate::ids::Id16;
use crate::scope::ScopeConn;
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
pub fn append(conn: &ScopeConn, now: i64, record: &SignedControlRecord) -> Result<u64> {
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

/// `epoch.activated` 的 typed 事件（rid/E/S/commit hash/control_rev + activation T 双签）。
///
/// 13–15 为 ADR §34 新增：完整 activation T 的 canonical CDE 原字节，以及旧/新管理
/// key 对 `activation_t_signing_input(T)` 的双签。三者必须同时存在（新记录）或同时
/// 省略（升级前的历史记录）；wire 编解码统一交给 `sync-proto`，避免两处字段编号漂移。
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
    /// 完整 activation T 的 canonical CDE 原字节。
    pub activation_t: Option<Vec<u8>>,
    /// 旧管理 key 对 `activation_t_signing_input(T)` 的 64B 签名。
    pub old_t_signature: Option<Vec<u8>>,
    /// 新管理 key 对同一输入的 64B 签名。
    pub new_t_signature: Option<Vec<u8>>,
}

impl EpochActivated {
    fn to_proto(&self) -> proto::EpochActivated {
        proto::EpochActivated {
            vault_id: self.vault_id,
            instance_id: self.instance_id,
            rotation_id: self.rotation_id,
            from_epoch: self.from_epoch,
            to_epoch: self.to_epoch,
            covered_seq: self.covered_seq,
            primary_checkpoint: self.primary_checkpoint,
            fallback_checkpoint: self.fallback_checkpoint,
            activation_commit: self.activation_commit,
            member_set_hash: self.member_set_hash,
            grants_digest: self.grants_digest,
            control_rev: self.control_rev,
            auth_rev: self.auth_rev,
            activation_t: self.activation_t.clone(),
            old_t_signature: self.old_t_signature.clone(),
            new_t_signature: self.new_t_signature.clone(),
        }
    }

    fn from_proto(event: proto::EpochActivated) -> Self {
        Self {
            vault_id: event.vault_id,
            instance_id: event.instance_id,
            rotation_id: event.rotation_id,
            from_epoch: event.from_epoch,
            to_epoch: event.to_epoch,
            covered_seq: event.covered_seq,
            primary_checkpoint: event.primary_checkpoint,
            fallback_checkpoint: event.fallback_checkpoint,
            activation_commit: event.activation_commit,
            member_set_hash: event.member_set_hash,
            grants_digest: event.grants_digest,
            control_rev: event.control_rev,
            auth_rev: event.auth_rev,
            activation_t: event.activation_t,
            old_t_signature: event.old_t_signature,
            new_t_signature: event.new_t_signature,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.to_proto()
            .encode()
            .map_err(|_| Error::Invariant("epoch activated event encode"))
    }

    /// 解码 `epoch.activated` 事件（客户端/测试按字段编号解析）。
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        proto::EpochActivated::decode(bytes)
            .map(Self::from_proto)
            .map_err(|_| Error::api(crate::ErrorCode::InvalidRequest))
    }
}

/// `rotation.abort` 的 typed 事件（ADR §35.3）。
///
/// abort 的 canonical body 是空 map，请求 path 含 rotation id；relay 此前只持久化
/// body + 签名，客户端无法重建管理签名输入。把被签 path/method 与 body digest
/// 原样写进事件后，`/control` 的消费者可以独立复算并验签。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationAborted {
    /// 被废弃的轮换 id。
    pub rotation_id: Id16,
    /// 管理签名覆盖的 method（abort 固定 POST）。
    pub method: proto::HttpMethod,
    /// 管理签名覆盖的请求 path。
    pub path: String,
    /// `SHA256(signed_body)`。
    pub body_digest: [u8; 32],
}

impl RotationAborted {
    fn to_proto(&self) -> proto::RotationAborted {
        proto::RotationAborted {
            rotation_id: self.rotation_id,
            method: self.method,
            path: self.path.clone(),
            body_digest: self.body_digest,
        }
    }

    fn from_proto(event: proto::RotationAborted) -> Self {
        Self {
            rotation_id: event.rotation_id,
            method: event.method,
            path: event.path,
            body_digest: event.body_digest,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.to_proto()
            .encode()
            .map_err(|_| Error::Invariant("rotation aborted event encode"))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        proto::RotationAborted::decode(bytes)
            .map(Self::from_proto)
            .map_err(|_| Error::api(crate::ErrorCode::InvalidRequest))
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
            activation_t: None,
            old_t_signature: None,
            new_t_signature: None,
        };
        let encoded = event.encode().expect("encode");
        // 同一语义重编码必须逐字节一致（CDE 唯一性）。
        assert_eq!(encoded, event.encode().expect("re-encode"));
        assert_eq!(EpochActivated::decode(&encoded).expect("decode"), event);
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
        let aborted = RotationAborted {
            rotation_id: [3u8; 16],
            method: proto::HttpMethod::Post,
            path: "/v1/vaults/1/rotations/1/abort".to_owned(),
            body_digest: [0xD0u8; 32],
        };
        let encoded = aborted.encode().expect("abort event");
        assert_eq!(encoded, aborted.encode().expect("abort re-encode"));
        assert_eq!(RotationAborted::decode(&encoded).expect("decode"), aborted);
    }
}
