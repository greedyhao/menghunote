//! ADR 0016 **S4d2**：relay 审批收件箱（通道 B）。
//!
//! relay 在这里**只是运输/存储/验证者**：它不持管理私钥、不代签、不改控制链、不改门限。
//! 收件箱里的工作单元是一个**已冻结**的 `OperationV2`（完整字节）+ 已收到的签名：
//!
//! - 创建（`POST /v2/vaults/{V}/approvals`）由发起者 A 上传"A 已签"的 `ApprovalEnvelopeV1`
//!   （canonical CDE：完整 op、`h=SHA256(op)`、initiator_index、签名项、cap 证据、
//!   控制链证据）。relay 用**与 `/control/submit` 同源**的干跑校验（`control_v2::validate_partial`）
//!   验证结构/域/payload/路径/I/V/rev/signers/cap/scope/时间窗；未签槽位是"尚未收集"，
//!   不是零签名占位。收件箱只接**双签且两个 Device 槽**的 kind（单签邀请、Upgrade、
//!   RecoveryReset 仍走通道 A）。
//! - 列表（`GET .../approvals`）只返回**调用者自己的待办**（收件人视角，或 `role=initiator`
//!   的发起者视角），按 `(recipient_device,status,seq)` 索引取，分页 ≤50。
//! - 读取（`GET .../approvals/{R}`）返回摘要 + 完整 op/签名/cap 证据（B 据此加签）。
//! - 加签（`POST .../approvals/{R}/signatures`）只接受**冻结槽位**（realm3 主体/cap 必须
//!   就是这个 Device 槽的 device/cap）；事务内重验授权/时间/CAS，并重跑整包干跑校验。
//! - 取消（`POST .../approvals/{R}/cancel`）只停止协调：已外发的签名不可撤回，任何持有
//!   满签包的人仍可 submit。
//!
//! # 可见性（失败统一 404）
//!
//! list/read/sign/cancel 只对**该 job 的 A 与预选 B** 可见；两者都必须当前 ACTIVE、
//! realm3（持有效 cap）。A 用创建时的 `initiator_cap`，B 用 op 里冻结的槽位 cap
//! （换 cap 不认）。任何一项不符 → `404 NOT_FOUND`（不提供"不是你的待办"的细分 oracle）。
//!
//! # 生命周期
//!
//! `PENDING`（等 B 加签）→ `READY`（全槽已签）→ `EXECUTED`（`dedup_v2` 里出现**逐字节相同**
//! 的满签包回执）/ `STALE`（head/rev 变化）/ `EXPIRED`（`min(op.not_after, 参与 cap 到期)`）/
//! `CANCELLED`（显式取消）。终态**立即清空正文与签名**（墓碑只留摘要：request_id/op_digest/
//! kind/设备/rev/时间/状态/回执 rev），24h 后由 sweep 删除；正式 control 历史与 `dedup_v2`
//! 回执**永不删除**。
//!
//! 终结是**惰性**的：每次收件箱请求先跑一次有界 sweep（≤64 行），把到期/陈旧/已执行的
//! job 投影成终态并删除过期墓碑。HTTP 轮询是基线，丢通知不影响正确性。
//! 维护性变更与业务操作同事务：**失败请求会整体回滚**（含 sweep），下次成功请求会再清一次；
//! 这只影响清理时机，不影响正确性（终结判定始终按提交时刻的事实重算）。
//!
//! # 限流与限额
//!
//! 入口复用 v2 业务限流（实例级固定窗口，见 `crate::ratelimit`）；收件箱额外有：
//! 完整包 ≤1MiB、vault 内待办 ≤64、同一源设备待办 ≤8、待办正文总字节 ≤16MiB、
//! 分页 ≤50。超限 → 507 `QUOTA_EXCEEDED` / 413 `REQUEST_TOO_LARGE`。
//!
//! # 不存什么
//!
//! 不存笔记明文、root/seed/恢复私钥、邀请 token、私密备注；日志只允许摘要/状态（本模块
//! 不写任何业务日志）。`op_bytes`/cap 证据全部是客户端公开签名材料。

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::OptionalExtension;

use crate::auth::AuthContext;
use crate::control_v2;
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{sha256, Hash32, Id16};
use crate::scope::ScopeConn;
use crate::time::Clock;
use crate::vault::{self, Vault};
use crate::wire;
use menghunote_sync_proto as proto;

/// 完整审批包（`ApprovalEnvelopeV1` 的 canonical CDE 字节）上限：ADR 0016「≤1MiB 控制预算」。
pub const MAX_APPROVAL_ENVELOPE_BYTES: usize = 1024 * 1024;
/// 单个 vault 的待办上限（PENDING/READY）。
pub const MAX_APPROVAL_JOBS_PER_VAULT: i64 = 64;
/// 同一源设备（initiator）的待办上限。
pub const MAX_APPROVAL_JOBS_PER_DEVICE: i64 = 8;
/// 单个 vault 待办正文（`op_bytes`）总字节上限。
pub const MAX_APPROVAL_TOTAL_BYTES: i64 = 16 * 1024 * 1024;
/// 分页上限。
pub const MAX_APPROVAL_PAGE: i64 = 50;
/// 默认分页大小。
pub const DEFAULT_APPROVAL_PAGE: i64 = 20;
/// 终态摘要墓碑保留时长（秒）。
pub const APPROVAL_TOMBSTONE_SECS: i64 = 24 * 60 * 60;
/// 每次 sweep 处理的行数上限（有界维护，不阻塞请求）。
const SWEEP_BATCH: i64 = 64;

fn invalid() -> Error {
    Error::api(ErrorCode::InvalidRequest)
}

fn forbidden() -> Error {
    Error::api(ErrorCode::Forbidden)
}

fn not_found() -> Error {
    Error::api(ErrorCode::NotFound)
}

fn signature_invalid() -> Error {
    Error::api(ErrorCode::SignatureInvalid)
}

fn quota_exceeded() -> Error {
    Error::api(ErrorCode::QuotaExceeded)
}

fn i64_of(value: u64, field: &'static str) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Invariant(field))
}

// ---------------------------------------------------------------- 信封

/// `ApprovalEnvelopeV1.control`：客户端上传时绑定的控制链证据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlEvidenceV1 {
    pub control_rev: u64,
    pub head_hash: Hash32,
    pub authz_rev: u64,
    pub authority_rev: u64,
}

/// `ApprovalEnvelopeV1`（ADR 0016 §共同包；wire 见
/// `docs/notes/management-cap-v2-wire.md` §9 定义与 §11 relay 收件箱协议）。
///
/// relay 侧**独立实现**严格解码（不依赖 `crates/sync` 的客户端类型）：解码后重新编码逐字节
/// 相同、`h` 重算、签名槽严格升序去重、cap 证据恰好等于 role0 槽引用的 cap 集合。
#[derive(Debug, Clone)]
pub struct ApprovalEnvelopeV1 {
    pub operation: proto::OperationV2,
    /// canonical `CDE(OperationV2)`（= 信封键 1 的原始字节）。
    pub operation_bytes: Vec<u8>,
    pub initiator_index: usize,
    /// 已收到的签名（槽位 → sig64；缺签就是没有条目，**不用零占位**）。
    pub signatures: BTreeMap<usize, [u8; 64]>,
    pub caps: Vec<proto::ManagementCapV2>,
    pub control: ControlEvidenceV1,
}

impl ApprovalEnvelopeV1 {
    /// 严格解码 + 结构/证据自洽校验（不含任何 DB/授权语义）。
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_APPROVAL_ENVELOPE_BYTES {
            return Err(Error::api(ErrorCode::PayloadTooLarge));
        }
        // canonical：解码后重新编码必须逐字节相同（拒绝非最短整数/乱序键/尾随字节）。
        let value = proto::decode_cde(bytes).map_err(|_| invalid())?;
        if proto::encode_cde(&value).map_err(|_| invalid())? != bytes {
            return Err(invalid());
        }
        let fields = wire::value_map(&value)?;
        expect_record(&fields, 7)?;
        if wire::uint_field(&fields, 0)? != 1 {
            return Err(invalid());
        }
        let operation_bytes = wire::bytes_field(&fields, 1)?;
        let operation = proto::OperationV2::decode(&operation_bytes).map_err(|_| invalid())?;
        // 唯一 canonical 编码：换一字节/换编码形式都必须失败。
        if operation.encode().map_err(|_| invalid())? != operation_bytes {
            return Err(invalid());
        }
        let digest = sha256(&operation_bytes);
        let declared: Hash32 = wire::hash32_field(&fields, 2)?;
        if declared != digest {
            return Err(invalid());
        }
        let initiator_index =
            usize::try_from(wire::uint_field(&fields, 3)?).map_err(|_| invalid())?;
        if initiator_index >= operation.signers.len() {
            return Err(invalid());
        }
        let signatures = decode_signatures(&fields, 4, operation.signers.len())?;
        let caps = decode_caps(&fields, 5)?;
        let control = decode_control(&fields, 6)?;

        // cap 证据必须恰好等于 role0（Device）槽引用的 cap 集合；其余角色不得带 cap。
        let mut slot_caps: BTreeSet<[u8; 16]> = BTreeSet::new();
        for signer in &operation.signers {
            match signer.role {
                proto::SignerRole::Device => {
                    let Some(cap_id) = signer.cap_id else {
                        return Err(invalid());
                    };
                    slot_caps.insert(cap_id);
                }
                _ => {
                    if signer.cap_id.is_some() {
                        return Err(invalid());
                    }
                }
            }
        }
        let evidence: BTreeSet<[u8; 16]> = caps.iter().map(|cap| cap.cap_id).collect();
        if slot_caps != evidence {
            return Err(invalid());
        }

        // 控制链证据与 op 逐字段绑定（Upgrade 特例：升级前没有 V2 授权）。
        if control.control_rev != operation.expected_control_rev
            || control.head_hash != operation.prev_control_hash
        {
            return Err(invalid());
        }
        if operation.kind == proto::ControlKindV2::UpgradePolicy {
            if control.authz_rev != 0 || control.authority_rev != 0 {
                return Err(invalid());
            }
        } else if control.authz_rev != operation.expected_authz_rev
            || control.authority_rev != operation.authority_rev
        {
            return Err(invalid());
        }

        Ok(Self {
            operation,
            operation_bytes,
            initiator_index,
            signatures,
            caps,
            control,
        })
    }

    /// 已收到签名的槽位集合。
    pub fn present(&self) -> BTreeSet<usize> {
        self.signatures.keys().copied().collect()
    }

    /// 构造与 `control_v2` 校验同形的 `SignedControlV2`（缺签槽位用 [`control_v2::UNSIGNED_SLOT`] 占位）。
    ///
    /// **只用于干跑校验**；该字节串既不落库也不参与链 hash（链 hash 由提交方用真实满签包计算）。
    pub fn to_signed(&self) -> proto::SignedControlV2 {
        let signatures = (0..self.operation.signers.len())
            .map(|index| {
                self.signatures
                    .get(&index)
                    .copied()
                    .unwrap_or(control_v2::UNSIGNED_SLOT)
            })
            .collect();
        proto::SignedControlV2 {
            operation: self.operation.clone(),
            signatures,
        }
    }
}

/// 记录键必须恰好是 `0..count`（CDE 解码已保证升序去重，这里只校验"无空洞/无多余"）。
fn expect_record(fields: &wire::Fields, count: u64) -> Result<()> {
    if fields.len() as u64 != count
        || fields
            .iter()
            .enumerate()
            .any(|(index, (key, _))| *key != index as u64)
    {
        return Err(invalid());
    }
    Ok(())
}

fn decode_signatures(
    fields: &wire::Fields,
    key: u64,
    signers: usize,
) -> Result<BTreeMap<usize, [u8; 64]>> {
    let mut out = BTreeMap::new();
    let mut previous: Option<usize> = None;
    for value in wire::array_field(fields, key)? {
        let entry = wire::value_map(&value)?;
        expect_record(&entry, 2)?;
        let index = usize::try_from(wire::uint_field(&entry, 0)?).map_err(|_| invalid())?;
        let bytes = wire::bytes_field(&entry, 1)?;
        let signature: [u8; 64] = bytes.as_slice().try_into().map_err(|_| invalid())?;
        if index >= signers || signature.iter().all(|byte| *byte == 0) {
            return Err(invalid());
        }
        if previous.is_some_and(|previous| previous >= index) {
            return Err(invalid());
        }
        previous = Some(index);
        out.insert(index, signature);
    }
    Ok(out)
}

fn decode_caps(fields: &wire::Fields, key: u64) -> Result<Vec<proto::ManagementCapV2>> {
    let mut out: Vec<proto::ManagementCapV2> = Vec::new();
    for value in wire::array_field(fields, key)? {
        let proto::CdeValue::Bytes(bytes) = value else {
            return Err(invalid());
        };
        let cap = proto::ManagementCapV2::decode(&bytes).map_err(|_| invalid())?;
        if cap.encode().map_err(|_| invalid())? != bytes {
            return Err(invalid());
        }
        if let Some(previous) = out.last() {
            if previous.cap_id >= cap.cap_id {
                return Err(invalid());
            }
        }
        out.push(cap);
    }
    Ok(out)
}

fn decode_control(fields: &wire::Fields, key: u64) -> Result<ControlEvidenceV1> {
    let control = wire::map_field(fields, key)?;
    expect_record(&control, 4)?;
    Ok(ControlEvidenceV1 {
        control_rev: wire::uint_field(&control, 0)?,
        head_hash: wire::hash32_field(&control, 1)?,
        authz_rev: wire::uint_field(&control, 2)?,
        authority_rev: wire::uint_field(&control, 3)?,
    })
}

// ---------------------------------------------------------------- 状态与行

/// 收件箱 job 状态（DB CHECK 的同名取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalStatus {
    /// 等待其余槽位加签。
    Pending,
    /// 全部槽位已签（任何持有者都可以提交；未提交前仍是活状态）。
    Ready,
    /// `dedup_v2` 里已有**逐字节相同**的满签包回执（控制链真的前进过）。
    Executed,
    /// 显式取消（只停止协调；已外发签名不可撤回）。
    Cancelled,
    /// `min(op.not_after, 参与 cap 到期)` 已到。
    Expired,
    /// head/authz/authority rev 在等待期间变化（必须新建操作全部重签）。
    Stale,
}

impl ApprovalStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Ready => "READY",
            Self::Executed => "EXECUTED",
            Self::Cancelled => "CANCELLED",
            Self::Expired => "EXPIRED",
            Self::Stale => "STALE",
        }
    }

    /// 解析 wire/DB 状态串（未知值是不变量错误，不是客户端错误）。
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "PENDING" => Some(Self::Pending),
            "READY" => Some(Self::Ready),
            "EXECUTED" => Some(Self::Executed),
            "CANCELLED" => Some(Self::Cancelled),
            "EXPIRED" => Some(Self::Expired),
            "STALE" => Some(Self::Stale),
            _ => None,
        }
    }

    /// 活状态（还有正文/签名，允许加签或取消）。
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Pending | Self::Ready)
    }
}

/// 收件箱里的一行（+ 已收到的签名）。
#[derive(Debug, Clone)]
pub struct ApprovalJob {
    pub request_id: Id16,
    /// 终态/墓碑为 `None`（正文已清空，只留摘要）。
    pub op_bytes: Option<Vec<u8>>,
    pub op_digest: Hash32,
    pub kind: u8,
    pub initiator_device: Id16,
    pub initiator_cap: Id16,
    pub initiator_index: u32,
    pub recipient_device: Id16,
    pub expected_control_rev: u64,
    pub expected_head_hash: Hash32,
    pub expected_authz_rev: u64,
    pub authority_rev: u64,
    pub signed_at: i64,
    pub not_after: i64,
    pub created_at: i64,
    pub expires_at: i64,
    pub status: ApprovalStatus,
    pub seq: i64,
    pub terminal_at: Option<i64>,
    pub receipt_ref: Option<u64>,
    /// 已收到的签名（按槽位升序；墓碑为空）。
    pub signatures: Vec<(u32, [u8; 64])>,
}

impl ApprovalJob {
    fn signed_indices(&self) -> Vec<u32> {
        self.signatures.iter().map(|(index, _)| *index).collect()
    }

    /// 摘要（store 视图）的 wire 字段；不含 op/签名正文。
    pub fn summary_fields(&self) -> wire::Fields {
        let mut fields = vec![
            (0, wire::bytes(self.request_id)),
            (1, wire::uint(u64::from(self.kind))),
            (2, wire::text(self.status.as_str())),
            (3, wire::bytes(self.op_digest)),
            (4, wire::bytes(self.initiator_device)),
            (5, wire::bytes(self.initiator_cap)),
            (6, wire::bytes(self.recipient_device)),
            (7, wire::uint(u64::from(self.initiator_index))),
            (8, wire::uint(u64::try_from(self.signed_at).unwrap_or(0))),
            (9, wire::uint(u64::try_from(self.not_after).unwrap_or(0))),
            (10, wire::uint(u64::try_from(self.created_at).unwrap_or(0))),
            (11, wire::uint(u64::try_from(self.expires_at).unwrap_or(0))),
            (12, wire::uint(u64::try_from(self.seq).unwrap_or(0))),
            (13, wire::uint(self.expected_control_rev)),
            (14, wire::bytes(self.expected_head_hash)),
            (15, wire::uint(self.expected_authz_rev)),
            (16, wire::uint(self.authority_rev)),
            (
                17,
                wire::array(
                    self.signed_indices()
                        .into_iter()
                        .map(|index| wire::uint(u64::from(index)))
                        .collect(),
                ),
            ),
        ];
        if let Some(terminal_at) = self.terminal_at {
            fields.push((18, wire::uint(u64::try_from(terminal_at).unwrap_or(0))));
        }
        if let Some(receipt_ref) = self.receipt_ref {
            fields.push((19, wire::uint(receipt_ref)));
        }
        fields.sort_by_key(|(key, _)| *key);
        fields
    }

    /// 完整视图（read/create 响应用）：摘要 + op + 已有签名 + cap 证据（终态只有摘要）。
    pub fn detail_fields(&self, caps: &[proto::ManagementCapV2]) -> Result<wire::Fields> {
        let mut fields = vec![(0, wire::map(self.summary_fields()))];
        if let Some(op_bytes) = &self.op_bytes {
            fields.push((1, wire::bytes(op_bytes.clone())));
            let signatures = self
                .signatures
                .iter()
                .map(|(index, signature)| {
                    wire::map(vec![
                        (0, wire::uint(u64::from(*index))),
                        (1, wire::bytes(signature.to_vec())),
                    ])
                })
                .collect();
            fields.push((2, wire::array(signatures)));
            let caps = caps
                .iter()
                .map(|cap| cap.encode().map(wire::bytes))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|_| Error::Invariant("cap encode"))?;
            fields.push((3, wire::array(caps)));
        }
        Ok(fields)
    }
}

// ---------------------------------------------------------------- 鉴权/授权

/// realm3 管理设备主体：`(device_id, cap_id)`。
///
/// `auth::authenticate` 已经逐请求重验 cap（撤销/到期/authority 变更立即失效），这里只把
/// realm 收窄成"管理设备"，其它 realm 一律 403（`MANAGEMENT_REQUIRED`）。
fn actor(auth: &AuthContext) -> Result<(Id16, Id16)> {
    let cap_id = auth.require_management_device()?;
    let device_id = auth.device_id.ok_or_else(forbidden)?;
    Ok((device_id, cap_id))
}

/// kind → 每一位 Device 签名者必须具备的 scope（与 `control_v2` 的 kind 校验同源；
/// 条件性 scope（例如 RotationBegin 带 revoke 时的 REVOKE）由创建/加签的干跑校验兜住）。
fn required_scopes(kind: u8) -> Result<&'static [u32]> {
    let kind = proto::ControlKindV2::try_from(kind).map_err(|_| Error::Invariant("kind"))?;
    Ok(match kind {
        proto::ControlKindV2::UpgradePolicy
        | proto::ControlKindV2::PolicyReplace
        | proto::ControlKindV2::RecoveryReset => &[],
        proto::ControlKindV2::CapGrant | proto::ControlKindV2::CapRevoke => {
            &[proto::CapScope::CAP_ADMIN]
        }
        proto::ControlKindV2::DeviceApprove | proto::ControlKindV2::DeviceRightsChange => {
            &[proto::CapScope::APPROVE]
        }
        proto::ControlKindV2::DeviceRevoke => &[proto::CapScope::REVOKE],
        proto::ControlKindV2::RotationBegin
        | proto::ControlKindV2::RotationAbort
        | proto::ControlKindV2::RotationRestart
        | proto::ControlKindV2::EpochActivate => &[proto::CapScope::ROTATE],
        proto::ControlKindV2::RotationAddRevokes => {
            &[proto::CapScope::ROTATE, proto::CapScope::REVOKE]
        }
        proto::ControlKindV2::InviteCreate | proto::ControlKindV2::InviteCancel => {
            &[proto::CapScope::INVITE]
        }
        proto::ControlKindV2::RecoveryKeyChange => &[proto::CapScope::POLICY],
    })
}

/// 收件箱接的 kind：只接"两签 + 两个 Device 槽"的高危操作。
///
/// - 单签 `InviteCreate/Cancel`：通道 A 直接提交，没有第二签要协调；
/// - `UpgradePolicy`：升级前没有 V2 授权，realm3 无法建立（旧 creator/恢复钥证据沿 A 收集）；
/// - `RecoveryReset`：离线恢复根 + 新钥 PoP，没有 Device 槽（同样沿 A 收集）；
/// - `PolicyReplace`：relay 仍 501。
fn inbox_kind(kind: proto::ControlKindV2) -> bool {
    matches!(
        kind,
        proto::ControlKindV2::CapGrant
            | proto::ControlKindV2::CapRevoke
            | proto::ControlKindV2::DeviceApprove
            | proto::ControlKindV2::DeviceRevoke
            | proto::ControlKindV2::DeviceRightsChange
            | proto::ControlKindV2::RotationBegin
            | proto::ControlKindV2::RotationAbort
            | proto::ControlKindV2::RotationRestart
            | proto::ControlKindV2::RotationAddRevokes
            | proto::ControlKindV2::EpochActivate
            | proto::ControlKindV2::RecoveryKeyChange
    )
}

/// cap 此刻是否有效且覆盖 `scopes`（用于墓碑/列表的"持所需 scope"判定）。
fn cap_covers(conn: &ScopeConn, now: i64, cap_id: &Id16, scopes: &[u32]) -> Result<bool> {
    match control_v2::effective_cap(conn, now, cap_id) {
        Ok(cap) => Ok(scopes.iter().all(|scope| cap.scopes.bits() & scope != 0)),
        Err(Error::Api(api)) if api.code == ErrorCode::Forbidden => Ok(false),
        Err(err) => Err(err),
    }
}

/// job 可见性 + 授权：只允许 A（创建时的 initiator cap）与预选 B（op 里冻结的槽位 cap）。
///
/// 失败统一 404（不提供"存在但不是你的"的 oracle）。
fn authorize_job(
    conn: &ScopeConn,
    now: i64,
    job: &ApprovalJob,
    device: &Id16,
    cap_id: &Id16,
) -> Result<()> {
    if device == &job.initiator_device {
        if cap_id == &job.initiator_cap {
            return Ok(());
        }
        return Err(not_found());
    }
    if device != &job.recipient_device {
        return Err(not_found());
    }
    if let Some(op_bytes) = &job.op_bytes {
        let operation =
            proto::OperationV2::decode(op_bytes).map_err(|_| Error::Invariant("approval op"))?;
        let frozen = operation.signers.iter().any(|signer| {
            signer.role == proto::SignerRole::Device
                && signer.device_id == Some(*device)
                && signer.cap_id == Some(*cap_id)
        });
        if frozen {
            return Ok(());
        }
        return Err(not_found());
    }
    // 墓碑（正文已清空）：B 的冻结 cap 已不可考；只要求"本设备持一张覆盖该 kind
    // 基础 scope 的当前有效 cap"。摘要里没有任何正文，泄露面为零。
    let scopes = required_scopes(job.kind)?;
    if cap_covers(conn, now, cap_id, scopes)? {
        return Ok(());
    }
    Err(not_found())
}

// ---------------------------------------------------------------- DB 读写

const JOB_COLUMNS: &str = "request_id, op_bytes, op_digest, kind, initiator_device, initiator_cap,
     initiator_index, recipient_device, expected_control_rev, expected_head_hash,
     expected_authz_rev, authority_rev, signed_at, not_after, created_at, expires_at,
     status, seq, terminal_at, receipt_ref";

fn load_job(conn: &ScopeConn, request_id: &Id16) -> Result<Option<ApprovalJob>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {JOB_COLUMNS} FROM approval_jobs
         WHERE vault_id = ?1 AND request_id = ?2"
    ))?;
    let row = stmt
        .query_row(
            (conn.scope().as_slice(), request_id.as_slice()),
            read_job_row,
        )
        .optional()?;
    let Some(mut job) = row else {
        return Ok(None);
    };
    job.signatures = load_signatures(conn, request_id)?;
    Ok(Some(job))
}

fn column_blob16(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Id16> {
    let bytes: Vec<u8> = row.get(index)?;
    bytes.as_slice().try_into().map_err(|_| {
        rusqlite::Error::InvalidColumnType(index, "id16".to_owned(), rusqlite::types::Type::Blob)
    })
}

fn column_blob32(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Hash32> {
    let bytes: Vec<u8> = row.get(index)?;
    bytes.as_slice().try_into().map_err(|_| {
        rusqlite::Error::InvalidColumnType(index, "hash32".to_owned(), rusqlite::types::Type::Blob)
    })
}

/// `approval_jobs` 一行 → 结构体（长度/取值不符是 DB 不变量破坏，不静默兜底）。
fn read_job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalJob> {
    let kind = u8::try_from(row.get::<_, i64>(3)?).map_err(|_| {
        rusqlite::Error::InvalidColumnType(3, "kind".to_owned(), rusqlite::types::Type::Integer)
    })?;
    let initiator_index = u32::try_from(row.get::<_, i64>(6)?).map_err(|_| {
        rusqlite::Error::InvalidColumnType(6, "index".to_owned(), rusqlite::types::Type::Integer)
    })?;
    let status_text: String = row.get(16)?;
    let status = ApprovalStatus::parse(&status_text).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(16, "status".to_owned(), rusqlite::types::Type::Text)
    })?;
    let uint = |index: usize| -> rusqlite::Result<u64> {
        let value: i64 = row.get(index)?;
        u64::try_from(value).map_err(|_| {
            rusqlite::Error::InvalidColumnType(
                index,
                "u64".to_owned(),
                rusqlite::types::Type::Integer,
            )
        })
    };
    Ok(ApprovalJob {
        request_id: column_blob16(row, 0)?,
        op_bytes: row.get(1)?,
        op_digest: column_blob32(row, 2)?,
        kind,
        initiator_device: column_blob16(row, 4)?,
        initiator_cap: column_blob16(row, 5)?,
        initiator_index,
        recipient_device: column_blob16(row, 7)?,
        expected_control_rev: uint(8)?,
        expected_head_hash: column_blob32(row, 9)?,
        expected_authz_rev: uint(10)?,
        authority_rev: uint(11)?,
        signed_at: row.get(12)?,
        not_after: row.get(13)?,
        created_at: row.get(14)?,
        expires_at: row.get(15)?,
        status,
        seq: row.get(17)?,
        terminal_at: row.get(18)?,
        receipt_ref: row
            .get::<_, Option<i64>>(19)?
            .and_then(|value| u64::try_from(value).ok()),
        signatures: Vec::new(),
    })
}

fn load_signatures(conn: &ScopeConn, request_id: &Id16) -> Result<Vec<(u32, [u8; 64])>> {
    let mut stmt = conn.prepare(
        "SELECT signer_index, sig64 FROM approval_signatures
         WHERE vault_id = ?1 AND request_id = ?2 ORDER BY signer_index ASC",
    )?;
    let rows = stmt.query_map((conn.scope().as_slice(), request_id.as_slice()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (index, signature) = row?;
        let signature: [u8; 64] = signature
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("approval_signatures.sig64"))?;
        out.push((
            u32::try_from(index).map_err(|_| Error::Invariant("approval_signatures.index"))?,
            signature,
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------- 生命周期

/// 有界 sweep：终态墓碑到期删除 + 活 job 的惰性终结。
fn sweep(conn: &ScopeConn, now: i64) -> Result<()> {
    // 1) 终态 24h 后的墓碑：先删子表（FK），再删主表；分批 ≤64。
    let tombstone_before = now - APPROVAL_TOMBSTONE_SECS;
    conn.execute(
        "DELETE FROM approval_signatures
          WHERE vault_id = ?1 AND request_id IN (
            SELECT request_id FROM approval_jobs
             WHERE vault_id = ?1 AND terminal_at IS NOT NULL AND terminal_at <= ?2
             ORDER BY terminal_at ASC LIMIT ?3)",
        (conn.scope().as_slice(), tombstone_before, SWEEP_BATCH),
    )?;
    conn.execute(
        "DELETE FROM approval_jobs
          WHERE rowid IN (
            SELECT rowid FROM approval_jobs
             WHERE vault_id = ?1 AND terminal_at IS NOT NULL AND terminal_at <= ?2
             ORDER BY terminal_at ASC LIMIT ?3)",
        (conn.scope().as_slice(), tombstone_before, SWEEP_BATCH),
    )?;

    // 2) 活 job：到期/已执行/rev 变化 → 终态（正文/签名清空，摘要留 24h）。
    let vault = vault::load(conn)?;
    let (_, head_hash) = vault::control_head(conn)?;
    let mut jobs = live_jobs(conn, SWEEP_BATCH)?;
    for job in &mut jobs {
        refresh_live_job(conn, now, &vault, &head_hash, job)?;
    }
    Ok(())
}

fn live_jobs(conn: &ScopeConn, limit: i64) -> Result<Vec<ApprovalJob>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {JOB_COLUMNS} FROM approval_jobs
         WHERE vault_id = ?1 AND status IN ('PENDING','READY')
         ORDER BY seq ASC LIMIT ?2"
    ))?;
    let rows = stmt.query_map((conn.scope().as_slice(), limit), read_job_row)?;
    let mut out = Vec::new();
    for row in rows {
        let mut job = row?;
        job.signatures = load_signatures(conn, &job.request_id)?;
        out.push(job);
    }
    Ok(out)
}

/// 活 job 的惰性终结（幂等；每次收件箱请求都会跑）。
fn refresh_live_job(
    conn: &ScopeConn,
    now: i64,
    vault: &Vault,
    head_hash: &Hash32,
    job: &mut ApprovalJob,
) -> Result<()> {
    if !job.status.is_live() {
        return Ok(());
    }
    // (a) 满签且 dedup_v2 里已有**逐字节相同**的回执 → EXECUTED（"收件箱 approved != 已执行"）。
    if let Some(receipt_rev) = executed_receipt(conn, job)? {
        return terminalize(conn, job, ApprovalStatus::Executed, now, Some(receipt_rev));
    }
    // (b) 到期（min(op.not_after, 参与 cap 到期)）。
    if now >= job.expires_at {
        return terminalize(conn, job, ApprovalStatus::Expired, now, None);
    }
    // (c) head/rev 变化 → STALE（必须新建操作全部重签，禁止自动改 rev/延 deadline）。
    if job.expected_control_rev != vault.control_rev
        || job.expected_head_hash != *head_hash
        || job.expected_authz_rev != vault.authz_rev
        || job.authority_rev != vault.authority_rev
    {
        return terminalize(conn, job, ApprovalStatus::Stale, now, None);
    }
    Ok(())
}

/// 满签包是否真的进过控制链：用本地 op 字节 + 槽位签名重建 `SignedControlV2`，
/// 与 `dedup_v2.request_hash` 逐字节比较；命中返回回执的 control rev。
fn executed_receipt(conn: &ScopeConn, job: &ApprovalJob) -> Result<Option<u64>> {
    let Some(op_bytes) = &job.op_bytes else {
        return Ok(None);
    };
    let operation =
        proto::OperationV2::decode(op_bytes).map_err(|_| Error::Invariant("approval op"))?;
    if job.signatures.len() != operation.signers.len() {
        return Ok(None);
    }
    let mut signatures: Vec<[u8; 64]> = Vec::with_capacity(operation.signers.len());
    for (expected, (index, signature)) in job.signatures.iter().enumerate() {
        if *index as usize != expected {
            return Ok(None);
        }
        signatures.push(*signature);
    }
    let signed = proto::SignedControlV2 {
        operation,
        signatures,
    };
    let bytes = signed
        .encode()
        .map_err(|_| Error::Invariant("approval op encode"))?;
    let digest = sha256(&bytes);
    let row: Option<(Vec<u8>, Option<i64>)> = conn
        .query_row(
            "SELECT request_hash, control_rev FROM dedup_v2
             WHERE vault_id = ?1 AND request_id = ?2",
            (conn.scope().as_slice(), job.request_id.as_slice()),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((stored, control_rev)) = row else {
        return Ok(None);
    };
    if stored.as_slice() != digest {
        return Ok(None);
    }
    // `dedup_v2.control_rev` 只由提交路径写入且必然 ≥1；缺失/0 不是"已执行"证据。
    match control_rev.and_then(|value| u64::try_from(value).ok()) {
        Some(rev) if rev >= 1 => Ok(Some(rev)),
        _ => Ok(None),
    }
}

/// 投影成终态：写 `status/terminal_at/receipt_ref`，**清空正文与签名**（墓碑只留摘要）。
fn terminalize(
    conn: &ScopeConn,
    job: &mut ApprovalJob,
    status: ApprovalStatus,
    now: i64,
    receipt_ref: Option<u64>,
) -> Result<()> {
    let receipt = receipt_ref.and_then(|value| i64::try_from(value).ok());
    conn.execute(
        "UPDATE approval_jobs
            SET status = ?1, terminal_at = ?2, op_bytes = NULL,
                receipt_ref = COALESCE(?3, receipt_ref)
          WHERE vault_id = ?4 AND request_id = ?5 AND status IN ('PENDING','READY')",
        (
            status.as_str(),
            now,
            receipt,
            conn.scope().as_slice(),
            job.request_id.as_slice(),
        ),
    )?;
    conn.execute(
        "DELETE FROM approval_signatures WHERE vault_id = ?1 AND request_id = ?2",
        (conn.scope().as_slice(), job.request_id.as_slice()),
    )?;
    job.status = status;
    job.terminal_at = Some(now);
    job.op_bytes = None;
    job.signatures.clear();
    if receipt_ref.is_some() {
        job.receipt_ref = receipt_ref;
    }
    Ok(())
}

/// 非活状态的错误映射：到期 → 410；CAS → 409（带当前 rev）；其余 → 409 `INVALID_STATE`。
fn state_error(job: &ApprovalJob, vault: &Vault) -> Error {
    match job.status {
        ApprovalStatus::Expired => Error::api(ErrorCode::Expired),
        ApprovalStatus::Stale => {
            Error::Api(ApiError::new(ErrorCode::CasConflict).control_rev(vault.control_rev))
        }
        _ => Error::api(ErrorCode::InvalidState),
    }
}

// ---------------------------------------------------------------- 创建

/// `POST /v2/vaults/{V}/approvals`：上传 A 已签的半签包。
///
/// 返回 `(job, created)`：同一 `(V, request_id, h)` 的重放返回原 job（`created = false`），
/// **不重新做时间窗/签名/CAS 校验，也不改写任何状态**（幂等重放不重新授权）。注意 bearer
/// 仍在每个请求上重验 cap（`auth::authenticate`）：A 的 cap 一旦撤销/过期，HTTP 层就先
/// 401/403，重放只能发生在 bearer 仍有效期间；客户端必须自己保留原包。
pub fn create(
    conn: &ScopeConn,
    clock: &dyn Clock,
    auth: &AuthContext,
    envelope_bytes: &[u8],
) -> Result<(ApprovalJob, bool)> {
    let now = clock.now_secs();
    let (device, cap_id) = actor(auth)?;
    let envelope = ApprovalEnvelopeV1::decode(envelope_bytes)?;
    let operation = &envelope.operation;
    let vault = vault::load(conn)?;

    // 结构/租户/唯一路径：与提交同一断言（签名覆盖这些事实）。
    if operation.vault_id != vault.vault_id || operation.instance_id != vault.instance_id {
        return Err(not_found());
    }
    if operation.canonical_path != proto::canonical_submit_path(&vault.vault_id) {
        return Err(not_found());
    }

    // 幂等：先于时效/签名/cap（成功创建后的重放必须仍返回原 job）。
    let digest = sha256(&envelope.operation_bytes);
    if let Some(existing) = load_job(conn, &operation.request_id)? {
        if existing.op_digest != digest {
            return Err(Error::api(ErrorCode::IdConflict));
        }
        // 重放也必须来自 A 本人（同一 realm3 主体/cap），否则按不可见处理。
        if existing.initiator_device != device || existing.initiator_cap != cap_id {
            return Err(not_found());
        }
        return Ok((existing, false));
    }

    // 先做有界清理，再算限额（过期/陈旧 job 不占待办配额）。
    sweep(conn, now)?;

    if !inbox_kind(operation.kind) {
        // 单签/Upgrade/Reset/PolicyReplace 走通道 A（不开放通用收件箱）。
        return Err(invalid());
    }

    // A 必须就是 realm3 主体/cap 指向的那个冻结槽。
    let initiator = operation
        .signers
        .get(envelope.initiator_index)
        .ok_or_else(invalid)?;
    if initiator.role != proto::SignerRole::Device
        || initiator.device_id != Some(device)
        || initiator.cap_id != Some(cap_id)
    {
        return Err(not_found());
    }

    // cap 证据必须与 relay 已验证控制链里的 canonical cap 逐字节一致（裸 cap/伪造字段
    // 不是授权证据；客户端导入时按同一规则比较）。授权判定仍只用 DB，不用证据值。
    verify_cap_evidence(conn, &envelope)?;
    check_signature_layout(&envelope)?;
    // 与 `/control/submit` 同源的干跑：时间窗/CAS/authz/authority/payload/路径/全部
    // 已经出现的签名/每一个 Device 槽的 cap·scope·ACTIVE。EpochActivate 的候选 promote
    // 等副作用在 SAVEPOINT 里回滚。
    dry_run(conn, clock, &envelope)?;

    let (_, recipient) = other_device_signer(operation, envelope.initiator_index)?;
    let recipient_device = recipient.device_id.ok_or_else(invalid)?;
    if recipient_device == device {
        return Err(invalid());
    }
    let expires_at = approval_expiry(conn, now, &envelope)?;
    check_limits(conn, &device, envelope.operation_bytes.len())?;

    let seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM approval_jobs WHERE vault_id = ?1",
        [conn.scope().as_slice()],
        |row| row.get(0),
    )?;
    let status = if envelope.signatures.len() == operation.signers.len() {
        ApprovalStatus::Ready
    } else {
        ApprovalStatus::Pending
    };
    conn.execute(
        "INSERT INTO approval_jobs(
            vault_id, request_id, op_bytes, op_digest, kind, initiator_device, initiator_cap,
            initiator_index, recipient_device, expected_control_rev, expected_head_hash,
            expected_authz_rev, authority_rev, signed_at, not_after, created_at, expires_at,
            status, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                 ?18, ?19)",
        rusqlite::params![
            conn.scope().as_slice(),
            operation.request_id.as_slice(),
            envelope.operation_bytes.as_slice(),
            digest.as_slice(),
            i64::from(operation.kind.as_u8()),
            device.as_slice(),
            cap_id.as_slice(),
            i64::try_from(envelope.initiator_index).map_err(|_| invalid())?,
            recipient_device.as_slice(),
            i64_of(
                operation.expected_control_rev,
                "approval.expected_control_rev"
            )?,
            operation.prev_control_hash.as_slice(),
            i64_of(operation.expected_authz_rev, "approval.expected_authz_rev")?,
            i64_of(operation.authority_rev, "approval.authority_rev")?,
            i64_of(operation.signed_at, "approval.signed_at")?,
            i64_of(operation.not_after, "approval.not_after")?,
            now,
            expires_at,
            status.as_str(),
            seq,
        ],
    )?;
    for (index, signature) in &envelope.signatures {
        insert_signature(conn, &operation.request_id, *index, signature, now)?;
    }
    let job =
        load_job(conn, &operation.request_id)?.ok_or_else(|| Error::Invariant("approval job"))?;
    Ok((job, true))
}

/// 信封里的 cap 证据必须逐字节等于本 vault `management_caps.canonical_bytes`。
fn verify_cap_evidence(conn: &ScopeConn, envelope: &ApprovalEnvelopeV1) -> Result<()> {
    for cap in &envelope.caps {
        let stored: Option<Vec<u8>> = conn
            .query_row(
                "SELECT canonical_bytes FROM management_caps
                 WHERE vault_id = ?1 AND cap_id = ?2",
                (conn.scope().as_slice(), cap.cap_id.as_slice()),
                |row| row.get(0),
            )
            .optional()?;
        let Some(stored) = stored else {
            // 引用了不存在/别的租户的 cap：与"cap 无效"同一口径，不给细分 oracle。
            return Err(forbidden());
        };
        if stored != cap.encode().map_err(|_| invalid())? {
            return Err(invalid());
        }
    }
    Ok(())
}

/// A 已签；**非 Device 槽必须在创建时就已签**（目标 PoP/恢复根/旧 creator 沿 A 收集）；
/// Device 槽至少留一个未签（否则是满签包，应直接 submit）。
fn check_signature_layout(envelope: &ApprovalEnvelopeV1) -> Result<()> {
    if !envelope.signatures.contains_key(&envelope.initiator_index) {
        return Err(invalid());
    }
    let mut unsigned = 0usize;
    for (index, signer) in envelope.operation.signers.iter().enumerate() {
        if envelope.signatures.contains_key(&index) {
            continue;
        }
        unsigned += 1;
        if signer.role != proto::SignerRole::Device {
            // PENDING 目标 PoP / 离线根证明不收未签槽：由 A 在包内一并收集。
            return Err(invalid());
        }
    }
    if unsigned == 0 {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    Ok(())
}

/// 另一个 Device 槽（收件人 B）：必须恰好两个 Device 槽、彼此 device/pk 不同。
fn other_device_signer(
    operation: &proto::OperationV2,
    initiator_index: usize,
) -> Result<(usize, &proto::SignerV2)> {
    let device_slots: Vec<(usize, &proto::SignerV2)> = operation
        .signers
        .iter()
        .enumerate()
        .filter(|(_, signer)| signer.role == proto::SignerRole::Device)
        .collect();
    if device_slots.len() != 2 {
        return Err(invalid());
    }
    let (other_index, other) = device_slots
        .iter()
        .copied()
        .find(|(index, _)| *index != initiator_index)
        .ok_or_else(invalid)?;
    let initiator = operation.signers.get(initiator_index).ok_or_else(invalid)?;
    if other.device_id.is_none()
        || other.cap_id.is_none()
        || other.device_id == initiator.device_id
        || other.pk == initiator.pk
    {
        // 同设备两 key / 两 device 同 key 都不能凑门限（这里先拒绝，提交路径再验一遍）。
        return Err(forbidden());
    }
    Ok((other_index, other))
}

/// `expires_at = min(op.not_after, 参与 cap 到期)`；参与 cap = 每个 Device 槽引用的 cap
/// （以 relay DB 的有效 cap 为准，不用信封里的证据值）。
fn approval_expiry(conn: &ScopeConn, now: i64, envelope: &ApprovalEnvelopeV1) -> Result<i64> {
    let mut expires_at = i64_of(envelope.operation.not_after, "approval.not_after")?;
    for signer in &envelope.operation.signers {
        if signer.role != proto::SignerRole::Device {
            continue;
        }
        let cap_id = signer.cap_id.ok_or_else(invalid)?;
        let cap = control_v2::effective_cap(conn, now, &cap_id)?;
        expires_at = expires_at.min(i64_of(cap.expires_at, "cap.expires_at")?);
    }
    if expires_at <= now {
        return Err(Error::api(ErrorCode::Expired));
    }
    Ok(expires_at)
}

/// 限额：vault 待办 ≤64、源设备待办 ≤8、待办正文总字节 ≤16MiB、完整包 ≤1MiB。
fn check_limits(conn: &ScopeConn, initiator: &Id16, bytes: usize) -> Result<()> {
    if bytes > MAX_APPROVAL_ENVELOPE_BYTES {
        return Err(Error::api(ErrorCode::PayloadTooLarge));
    }
    let (jobs, total_bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(LENGTH(op_bytes)), 0) FROM approval_jobs
         WHERE vault_id = ?1 AND status IN ('PENDING','READY')",
        [conn.scope().as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if jobs >= MAX_APPROVAL_JOBS_PER_VAULT
        || total_bytes.saturating_add(i64::try_from(bytes).unwrap_or(i64::MAX))
            > MAX_APPROVAL_TOTAL_BYTES
    {
        return Err(quota_exceeded());
    }
    let per_device: i64 = conn.query_row(
        "SELECT COUNT(*) FROM approval_jobs
         WHERE vault_id = ?1 AND initiator_device = ?2 AND status IN ('PENDING','READY')",
        (conn.scope().as_slice(), initiator.as_slice()),
        |row| row.get(0),
    )?;
    if per_device >= MAX_APPROVAL_JOBS_PER_DEVICE {
        return Err(quota_exceeded());
    }
    Ok(())
}

fn insert_signature(
    conn: &ScopeConn,
    request_id: &Id16,
    index: usize,
    signature: &[u8; 64],
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO approval_signatures(vault_id, request_id, signer_index, sig64, received_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            conn.scope().as_slice(),
            request_id.as_slice(),
            i64::try_from(index).map_err(|_| invalid())?,
            signature.as_slice(),
            now,
        ),
    )?;
    Ok(())
}

/// 把干跑校验包在 SAVEPOINT 里：无论成败都回滚校验期间的投影副作用。
fn dry_run(conn: &ScopeConn, clock: &dyn Clock, envelope: &ApprovalEnvelopeV1) -> Result<()> {
    conn.execute_batch("SAVEPOINT approval_validate")?;
    let outcome =
        control_v2::validate_partial(conn, clock, &envelope.to_signed(), &envelope.present());
    let rolled = conn.execute_batch("ROLLBACK TO approval_validate; RELEASE approval_validate");
    rolled?;
    outcome
}

// ---------------------------------------------------------------- 列表/读取

/// 列表视角：收件人（待办，默认）或发起者（A 的 outgoing）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListRole {
    Recipient,
    Initiator,
}

/// `GET .../approvals`：只返回调用者自己的 job，按 `seq` 升序分页。
///
/// 默认只看**活状态**（PENDING/READY，即"待办"）；显式 `status` 可查墓碑摘要（24h 内）。
/// 调用者 cap 不覆盖该 kind 基础 scope 的行会被过滤掉（不 404 整个列表）。
pub fn list(
    conn: &ScopeConn,
    clock: &dyn Clock,
    auth: &AuthContext,
    role: ListRole,
    status: Option<ApprovalStatus>,
    after_seq: Option<i64>,
    limit: i64,
) -> Result<(Vec<ApprovalJob>, Option<i64>)> {
    let now = clock.now_secs();
    let (device, cap_id) = actor(auth)?;
    let vault = vault::load(conn)?;
    sweep(conn, now)?;

    let limit = limit.clamp(1, MAX_APPROVAL_PAGE);
    let column = match role {
        ListRole::Recipient => "recipient_device",
        ListRole::Initiator => "initiator_device",
    };
    // 固定参数个数：`?3 IS NULL` = 缺省"只看待办"，否则按给定状态过滤；
    // `?4 IS NULL` = 从头发；否则 `seq >= ?4`。行数 ≤64，表达式谓词不影响正确性。
    let sql = format!(
        "SELECT {JOB_COLUMNS} FROM approval_jobs
         WHERE vault_id = ?1 AND {column} = ?2
           AND ((?3 IS NULL AND status IN ('PENDING','READY'))
                OR (?3 IS NOT NULL AND status = ?3))
           AND (?4 IS NULL OR seq >= ?4)
         ORDER BY seq ASC LIMIT ?5"
    );
    let mut stmt = conn.prepare(&sql)?;
    let status_text = status.map(ApprovalStatus::as_str);
    let rows = stmt.query_map(
        rusqlite::params![
            conn.scope().as_slice(),
            device.as_slice(),
            status_text,
            after_seq,
            limit,
        ],
        read_job_row,
    )?;
    let mut jobs = Vec::new();
    let mut scanned = 0i64;
    let mut last_seq = None;
    let head = head_for(conn)?;
    for row in rows {
        let mut job = row?;
        scanned += 1;
        last_seq = Some(job.seq);
        job.signatures = load_signatures(conn, &job.request_id)?;
        // 惰性终结（可能在列表中变成终态；默认视图不再返回它）。
        refresh_live_job(conn, now, &vault, &head, &mut job)?;
        let matches = match status {
            Some(status) => job.status == status,
            None => job.status.is_live(),
        };
        if !matches {
            continue;
        }
        if !cap_covers(conn, now, &cap_id, required_scopes(job.kind)?)? {
            continue;
        }
        jobs.push(job);
    }
    let next = if scanned >= limit {
        last_seq.map(|seq| seq.saturating_add(1))
    } else {
        None
    };
    Ok((jobs, next))
}

/// 控制链 head 摘要（列表逐行惰性终结用）。
fn head_for(conn: &ScopeConn) -> Result<Hash32> {
    Ok(vault::control_head(conn)?.1)
}

/// `GET .../approvals/{R}`：A 或预选 B 才能读；返回摘要 + 完整 op/签名/cap 证据。
pub fn read(
    conn: &ScopeConn,
    clock: &dyn Clock,
    auth: &AuthContext,
    request_id: &Id16,
) -> Result<(ApprovalJob, Vec<proto::ManagementCapV2>)> {
    let now = clock.now_secs();
    let (device, cap_id) = actor(auth)?;
    let vault = vault::load(conn)?;
    sweep(conn, now)?;
    let mut job = load_job(conn, request_id)?.ok_or_else(not_found)?;
    authorize_job(conn, now, &job, &device, &cap_id)?;
    let head = head_for(conn)?;
    refresh_live_job(conn, now, &vault, &head, &mut job)?;
    let caps = evidence_caps(conn, &job)?;
    Ok((job, caps))
}

/// 已冻结槽位引用的 canonical cap 证据（GET 响应里给 B 重建信封用）。
fn evidence_caps(conn: &ScopeConn, job: &ApprovalJob) -> Result<Vec<proto::ManagementCapV2>> {
    let Some(op_bytes) = &job.op_bytes else {
        return Ok(Vec::new());
    };
    let operation =
        proto::OperationV2::decode(op_bytes).map_err(|_| Error::Invariant("approval op"))?;
    let mut cap_ids: BTreeSet<[u8; 16]> = BTreeSet::new();
    for signer in &operation.signers {
        if signer.role == proto::SignerRole::Device {
            if let Some(cap_id) = signer.cap_id {
                cap_ids.insert(cap_id);
            }
        }
    }
    let mut caps = Vec::with_capacity(cap_ids.len());
    for cap_id in cap_ids {
        let bytes: Vec<u8> = conn.query_row(
            "SELECT canonical_bytes FROM management_caps WHERE vault_id = ?1 AND cap_id = ?2",
            (conn.scope().as_slice(), cap_id.as_slice()),
            |row| row.get(0),
        )?;
        let cap = proto::ManagementCapV2::decode(&bytes)
            .map_err(|_| Error::Invariant("management_caps.canonical_bytes"))?;
        caps.push(cap);
    }
    Ok(caps)
}

// ---------------------------------------------------------------- 加签

/// `POST .../approvals/{R}/signatures`：B 给冻结槽位加签（body = 64B 签名）。
///
/// 事务内顺序：可见性/授权（统一 404）→ 惰性终结 → 槽位必须是调用者的 device/cap →
/// 幂等（同字节返回原 job，异字节 409）→ 验签（域/payload 由 op 摘要兜住）→
/// **整包干跑**（时间/授权 rev/CAS/全部槽位 cap/scope）→ 落库。
pub fn add_signature(
    conn: &ScopeConn,
    clock: &dyn Clock,
    auth: &AuthContext,
    request_id: &Id16,
    signature: &[u8],
) -> Result<ApprovalJob> {
    let now = clock.now_secs();
    let (device, cap_id) = actor(auth)?;
    let vault = vault::load(conn)?;
    sweep(conn, now)?;
    let mut job = load_job(conn, request_id)?.ok_or_else(not_found)?;
    authorize_job(conn, now, &job, &device, &cap_id)?;
    let head = head_for(conn)?;
    refresh_live_job(conn, now, &vault, &head, &mut job)?;
    if !job.status.is_live() {
        return Err(state_error(&job, &vault));
    }
    let op_bytes = job
        .op_bytes
        .clone()
        .ok_or_else(|| Error::Invariant("live op"))?;
    let operation =
        proto::OperationV2::decode(&op_bytes).map_err(|_| Error::Invariant("approval op"))?;

    // 固定槽位：realm3 主体/cap 必须就是这个 Device 槽（换 cap 不认）。
    let slot = operation.signers.iter().position(|signer| {
        signer.role == proto::SignerRole::Device
            && signer.device_id == Some(device)
            && signer.cap_id == Some(cap_id)
    });
    let Some(slot) = slot else {
        return Err(not_found());
    };
    if signature.len() != 64 {
        return Err(invalid());
    }
    let signature: [u8; 64] = signature.try_into().map_err(|_| invalid())?;
    if signature.iter().all(|byte| *byte == 0) {
        return Err(invalid());
    }
    if let Some((_, existing)) = job
        .signatures
        .iter()
        .find(|(index, _)| *index as usize == slot)
    {
        if existing == &signature {
            return Ok(job);
        }
        return Err(Error::api(ErrorCode::IdConflict));
    }

    let signer = operation.signers.get(slot).ok_or_else(invalid)?;
    let input = signing_input(&operation, signer)?;
    if crate::sig::verify_ed25519(&signer.pk, &input, &signature).is_err() {
        return Err(signature_invalid());
    }

    // 事务内重验整包：时间窗/授权 rev/CAS/每一个 Device 槽的 cap/scope/ACTIVE
    // （cap 撤销/到期、head 前进、目标设备被撤权都会在这里失败，job 已被标 STALE/EXPIRED）。
    let mut envelope = envelope_from_job(conn, &job, &operation)?;
    envelope.signatures.insert(slot, signature);
    dry_run(conn, clock, &envelope)?;

    insert_signature(conn, request_id, slot, &signature, now)?;
    let status = if envelope.signatures.len() == operation.signers.len() {
        ApprovalStatus::Ready
    } else {
        ApprovalStatus::Pending
    };
    conn.execute(
        "UPDATE approval_jobs SET status = ?1
          WHERE vault_id = ?2 AND request_id = ?3 AND status IN ('PENDING','READY')",
        (
            status.as_str(),
            conn.scope().as_slice(),
            request_id.as_slice(),
        ),
    )?;
    load_job(conn, request_id)?.ok_or_else(|| Error::Invariant("approval job"))
}

/// 从已存 job 重建信封（cap 证据取自 `management_caps` 的 canonical 字节）。
fn envelope_from_job(
    conn: &ScopeConn,
    job: &ApprovalJob,
    operation: &proto::OperationV2,
) -> Result<ApprovalEnvelopeV1> {
    let caps = evidence_caps(conn, job)?;
    let signatures: BTreeMap<usize, [u8; 64]> = job
        .signatures
        .iter()
        .map(|(index, signature)| (*index as usize, *signature))
        .collect();
    Ok(ApprovalEnvelopeV1 {
        operation: operation.clone(),
        operation_bytes: job
            .op_bytes
            .clone()
            .ok_or_else(|| Error::Invariant("live op"))?,
        initiator_index: usize::try_from(job.initiator_index)
            .map_err(|_| Error::Invariant("approval.initiator_index"))?,
        signatures,
        caps,
        control: ControlEvidenceV1 {
            control_rev: job.expected_control_rev,
            head_hash: job.expected_head_hash,
            authz_rev: job.expected_authz_rev,
            authority_rev: job.authority_rev,
        },
    })
}

/// 槽位签名输入（域分离与 `control_v2` 一致：设备/旧 creator/恢复根/目标 PoP）。
fn signing_input(operation: &proto::OperationV2, signer: &proto::SignerV2) -> Result<Vec<u8>> {
    let input = match signer.role {
        proto::SignerRole::Device => proto::control_signing_input(operation, sha256),
        proto::SignerRole::CreatorUpgrade => proto::upgrade_signing_input(operation, sha256),
        proto::SignerRole::OfflineRecovery => proto::recover_signing_input(operation, sha256),
        proto::SignerRole::TargetPop => proto::pop_signing_input(operation, &signer.pk, sha256),
    };
    input.map_err(|_| invalid())
}

// ---------------------------------------------------------------- 取消

/// `POST .../approvals/{R}/cancel`：A 或预选 B 停止协调。
///
/// 只把 job 投影成 `CANCELLED`（清正文/签名，摘要留 24h）；**不**撤回任何已外发签名、
/// **不**动控制链——任何持有满签包的人仍可 submit。
pub fn cancel(
    conn: &ScopeConn,
    clock: &dyn Clock,
    auth: &AuthContext,
    request_id: &Id16,
) -> Result<ApprovalJob> {
    let now = clock.now_secs();
    let (device, cap_id) = actor(auth)?;
    let vault = vault::load(conn)?;
    sweep(conn, now)?;
    let mut job = load_job(conn, request_id)?.ok_or_else(not_found)?;
    authorize_job(conn, now, &job, &device, &cap_id)?;
    let head = head_for(conn)?;
    refresh_live_job(conn, now, &vault, &head, &mut job)?;
    match job.status {
        ApprovalStatus::Cancelled => Ok(job),
        ApprovalStatus::Pending | ApprovalStatus::Ready => {
            terminalize(conn, &mut job, ApprovalStatus::Cancelled, now, None)?;
            load_job(conn, request_id)?.ok_or_else(|| Error::Invariant("approval job"))
        }
        // 终态不可取消：到期/陈旧/已执行都没有"协调"可停止。
        _ => Err(Error::api(ErrorCode::InvalidState)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(cap_id: [u8; 16]) -> proto::ManagementCapV2 {
        proto::ManagementCapV2 {
            version: proto::V2_WIRE_VERSION,
            instance_id: [0x11; 16],
            vault_id: [0x22; 16],
            cap_id,
            subject_device_id: [0x33; 16],
            subject_ed_pk: [0x44; 32],
            scopes: proto::CapScope::new(proto::CapScope::ALL).expect("scopes"),
            not_before: 10,
            expires_at: 100,
            issued_control_rev: 1,
            issued_authz_rev: 1,
            issuer_authority_rev: 1,
            delegable: false,
        }
    }

    fn envelope_bytes(version: u64, operation_bytes: Vec<u8>, caps: Vec<Vec<u8>>) -> Vec<u8> {
        let digest = sha256(&operation_bytes);
        wire::encode(vec![
            (0, wire::uint(version)),
            (1, wire::bytes(operation_bytes)),
            (2, wire::bytes(digest)),
            (3, wire::uint(0)),
            (4, wire::array(vec![])),
            (5, wire::array(caps.into_iter().map(wire::bytes).collect())),
            (
                6,
                wire::map(vec![
                    (0, wire::uint(0)),
                    (1, wire::bytes([0u8; 32])),
                    (2, wire::uint(0)),
                    (3, wire::uint(0)),
                ]),
            ),
        ])
        .expect("envelope")
    }

    fn operation_bytes(kind: proto::ControlKindV2, signers: Vec<proto::SignerV2>) -> Vec<u8> {
        proto::OperationV2 {
            version: proto::V2_WIRE_VERSION,
            instance_id: [0x11; 16],
            vault_id: [0x22; 16],
            kind,
            request_id: [0x55; 16],
            expected_control_rev: 0,
            prev_control_hash: [0u8; 32],
            expected_authz_rev: 0,
            signed_at: 0,
            not_after: 300,
            // 非 Upgrade：控制证据（全零）必须与 op 的 rev 字段逐项一致。
            authority_rev: 0,
            canonical_path: proto::canonical_submit_path(&[0x22; 16]),
            payload: proto::CapRevokePayloadV2 { cap_id: [0x99; 16] }
                .encode()
                .expect("payload"),
            signers,
        }
        .encode()
        .expect("op")
    }

    fn device_signer(device: [u8; 16]) -> proto::SignerV2 {
        proto::SignerV2 {
            role: proto::SignerRole::Device,
            device_id: Some(device),
            cap_id: Some([0xC0; 16]),
            pk: [0x44; 32],
        }
    }

    #[test]
    fn envelope_decode_rejects_tampering_and_noncanonical_bytes() {
        let signers = vec![device_signer([0x01; 16]), device_signer([0x02; 16])];
        let op = operation_bytes(proto::ControlKindV2::CapRevoke, signers);
        let cap_bytes = cap([0xC0; 16]).encode().expect("cap");
        let good = envelope_bytes(1, op.clone(), vec![cap_bytes.clone()]);
        assert!(ApprovalEnvelopeV1::decode(&good).is_ok());
        // h 被改
        let mut tampered = good.clone();
        let value = proto::decode_cde(&tampered).expect("cde");
        let proto::CdeValue::Map(mut fields) = value else {
            panic!("map")
        };
        fields[2] = (2, wire::bytes([0xFFu8; 32]));
        tampered = proto::encode_cde(&proto::CdeValue::Map(fields)).expect("encode");
        assert!(ApprovalEnvelopeV1::decode(&tampered).is_err());
        // 版本
        let bad_version = envelope_bytes(2, op.clone(), vec![cap_bytes.clone()]);
        assert!(ApprovalEnvelopeV1::decode(&bad_version).is_err());
        // 证据 cap 集合为空但槽位引用 cap
        let missing_caps = envelope_bytes(1, op.clone(), vec![]);
        assert!(ApprovalEnvelopeV1::decode(&missing_caps).is_err());
        // 非 canonical：尾部多余字节
        let mut trailing = good;
        trailing.push(0);
        assert!(ApprovalEnvelopeV1::decode(&trailing).is_err());
    }

    #[test]
    fn inbox_kinds_are_the_two_sign_operations_only() {
        for kind in [
            proto::ControlKindV2::CapGrant,
            proto::ControlKindV2::CapRevoke,
            proto::ControlKindV2::DeviceApprove,
            proto::ControlKindV2::DeviceRevoke,
            proto::ControlKindV2::DeviceRightsChange,
            proto::ControlKindV2::RotationBegin,
            proto::ControlKindV2::RotationAbort,
            proto::ControlKindV2::RotationRestart,
            proto::ControlKindV2::RotationAddRevokes,
            proto::ControlKindV2::EpochActivate,
            proto::ControlKindV2::RecoveryKeyChange,
        ] {
            assert!(inbox_kind(kind), "{kind:?} 应走收件箱");
        }
        for kind in [
            proto::ControlKindV2::InviteCreate,
            proto::ControlKindV2::InviteCancel,
            proto::ControlKindV2::UpgradePolicy,
            proto::ControlKindV2::RecoveryReset,
            proto::ControlKindV2::PolicyReplace,
        ] {
            assert!(!inbox_kind(kind), "{kind:?} 不应走收件箱");
        }
    }

    #[test]
    fn status_roundtrip_and_liveness() {
        for status in [
            ApprovalStatus::Pending,
            ApprovalStatus::Ready,
            ApprovalStatus::Executed,
            ApprovalStatus::Cancelled,
            ApprovalStatus::Expired,
            ApprovalStatus::Stale,
        ] {
            assert_eq!(ApprovalStatus::parse(status.as_str()), Some(status));
        }
        assert!(ApprovalStatus::Pending.is_live());
        assert!(ApprovalStatus::Ready.is_live());
        assert!(!ApprovalStatus::Executed.is_live());
        assert!(!ApprovalStatus::Cancelled.is_live());
    }
}
