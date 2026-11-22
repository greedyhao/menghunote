//! ADR 0016 **S4d1b**：`ApprovalEnvelopeV1`、typed `OperationV2` 构造、双签状态机与提交对账。
//!
//! ## 范围
//!
//! 本模块是**库级**手工双签通道（ADR 0016 的通道 A）：
//!
//! 1. **共享不可变操作 + 签名包**（[`ApprovalEnvelopeV1`]）：canonical CDE
//!    `version/完整 op_bytes/h=SHA256(op_bytes)/initiator_index/[(signer_index,sig64)]/
//!    cap 证据/控制链证据`。严格编解码（重新编码逐字节相同）、索引升序去重、`h` 重算、
//!    **缺签不用零占位**、签名槽在构造时冻结（换人/改 payload/换 cap 必须新建 Operation）。
//!    文件后缀 `.mn-approval`（二进制 CDE），复制用带版本前缀的 base64url。
//! 2. **typed Operation 构造**（[`ApprovalDraftV1::propose`]）：kind 0 Upgrade / 2 CapGrant /
//!    3 CapRevoke / 4 DeviceApprove / 5 DeviceRevoke / 11 InviteCreate / 12 InviteCancel /
//!    16 DeviceRightsChange。构造时从**已验证** [`ControlStateV2`] 取
//!    `expected_control_rev/prev head/expected_authz_rev/authority/request_id/signed_at/
//!    not_after/canonical path`，`not_after = signed_at + 300`。
//!    rotation（6–10）/recovery（13/14）与 `PolicyReplace`（kind 1）明确返回
//!    [`SyncError::Unsupported`]（后续切片，本客户端不构造）。
//! 3. **双签状态机**（[`ApprovalStateV1`]）：
//!    `Draft → Frozen → InitiatorSigned → AwaitingCosign → Ready → Submitting →
//!    Confirmed/Failed`。A 先签、B 导入验证后加签；满签前不 submit；换人/改 payload 作废。
//! 4. **提交与回执对账**（[`ApprovalDraftV1::submit`]）：`POST /v2/vaults/{lowerhex32}/control/submit`
//!    body `{0: bytes(SignedControlV2)}`（自证，不带 bearer）；严格解析
//!    `{control_rev,head_hash,kind,request_id,authz_rev,authority_rev}`，用本地复算的
//!    链 hash + 刷新后的控制链对账；成功后 `refresh_control` 推进 pin。失败分类
//!    （403/404/409 CAS/410/429/传输），**不清库、不换信任锚**；丢响应重发**原字节**
//!    （relay `dedup_v2` 返回原回执）。
//!
//! ## 验证纪律（不复制 crypto、不信任 relay）
//!
//! - 授权语义**不**在本模块重写：导入/提交都调用
//!   [`V2ControlReplay::validate_pending_control`]，与链上回放走同一条
//!   kind 门限/角色/scope/authority/payload 校验路径；
//! - 半签包用 `verify_signatures=false` 干跑（包内已存在的签名由本模块逐槽验），
//!   满签提交必须回到真实验签；缺失槽只用内存探针的零占位，**从不**写出/提交；
//! - 回执只是公开链事实：`head_hash` 必须等于用本地字节复算的 `SHA256(prev||signed)`，
//!   且刷新后的已验证链必须在同一 rev 上给出同一 hash；
//! - `Debug` 只打印公开证据（op/签名/cap/证书/rev），不包含私钥/root/seed。
//!
//! ## 非目标（S4d1c/S4d2）
//!
//! 不做 App UI（确认页/指纹显示）、不做收件箱 relay 表/API（`approval_jobs`）、
//! 不做 rotation/recovery 构造、不做自动签/后台重签、不把 envelope 写进公开配置
//! （`dev-sync.json` 之类）；草稿只落 vault 公开 meta（内容全是公开证据）。

use std::cmp::Ordering;
use std::path::Path;

use menghunote_storage::Vault;
use menghunote_sync_proto::{
    canonical_submit_path, control_signing_input, decode_cde, encode_cde, pop_signing_input,
    recover_signing_input, upgrade_signing_input, CapGrantPayloadV2, CapRevokePayloadV2, CapScope,
    CdeValue, ControlKindV2, DeviceCertV2, DeviceCredentialPayloadV2, DeviceRevokePayloadV2,
    GrantPackageV2, InviteCancelPayloadV2, InviteCreatePayloadV2, ManagementCapV2, OperationV2,
    PolicyV2, SignedControlV2, SignerRole, SignerV2, UpgradePolicyPayloadV2, V2_WIRE_VERSION,
};
use sha2::{Digest, Sha256};

use crate::base64url;
use crate::cbor;
use crate::device::{verify_signature, DeviceSigningKey};
use crate::error::SyncError;
use crate::http_relay::Clock;
use crate::http_transport::HttpTransport;
use crate::management_v2::{
    ControlReceiptV2, ControlStateV2, DeviceStatusV2, V2ControlReplay, V2ManagementClient,
};
use crate::random::{self, RandomSource};
use crate::DeviceId;

// ---------------------------------------------------------------- 常量

/// `ApprovalEnvelopeV1.version`（wire 版本；布局变更必须显式升版本）。
pub const APPROVAL_ENVELOPE_V1_VERSION: u64 = 1;
/// 审批包文件后缀（二进制 canonical CDE）。
pub const APPROVAL_FILE_EXTENSION: &str = ".mn-approval";
/// 复制/粘贴用的 base64url 版本前缀（`mn-approval-v1:<base64url>`）。
pub const APPROVAL_BASE64_PREFIX_V1: &str = "mn-approval-v1:";
/// 导入长度上限（ADR 0016：控制预算 ≤1MiB；超限**在解码前**拒绝）。
pub const MAX_APPROVAL_ENVELOPE_BYTES_V1: usize = 1_048_576;
/// base64url 文本长度上限（含版本前缀的粗上界，防"先解码再拒绝"）。
pub const MAX_APPROVAL_BASE64_CHARS_V1: usize = MAX_APPROVAL_ENVELOPE_BYTES_V1 / 3 * 4 + 64;
/// 草稿在 vault 公开 meta 里的键（内容 = 公开 op/签名/公开 cap 证据，无秘密）。
pub const META_APPROVAL_DRAFT_V1: &str = "sync.approval_draft_v1";
/// 草稿结构版本。
pub const APPROVAL_DRAFT_V1_VERSION: u64 = 1;

/// 提交短窗（与 relay/回放同值）：`not_after ≤ signed_at + 300`。
const DEADLINE_MAX_SECS: u64 = 300;
/// 签名时间允许晚于本地时钟的上界（relay 同值）。
const SIGNED_AT_FUTURE_SKEW: u64 = 30;
/// signer 上限（proto 同值，这里只用于数组/证据上限判断）。
const MAX_SIGNERS: usize = 16;
/// cap 证据上限（= signer 上限）。
const MAX_CAP_EVIDENCE: usize = 16;

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn proto_hash(bytes: &[u8]) -> [u8; 32] {
    sha256(bytes)
}

fn approval_error(message: impl Into<String>) -> SyncError {
    SyncError::Approval(message.into())
}

// ---------------------------------------------------------------- 签名槽

/// 构造时冻结的一个签名槽（角色/设备/cap/公钥；顺序 = `OperationV2.signers` 顺序）。
///
/// **冻结**：换 signer/换 cap 必须新建 `OperationV2`（`h` 变了，旧签名全部作废）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignerSlotV1 {
    /// 角色（0 设备/1 旧 creator/2 离线恢复/3 目标 PoP）。
    pub role: SignerRole,
    /// 设备/PoP 的设备 ID。
    pub device_id: Option<DeviceId>,
    /// role0 绑定的 cap（其余角色为空）。
    pub cap_id: Option<[u8; 16]>,
    /// 该槽要验签的公钥。
    pub pk: [u8; 32],
}

impl SignerSlotV1 {
    fn from_signer(signer: &SignerV2) -> Self {
        Self {
            role: signer.role,
            device_id: signer.device_id,
            cap_id: signer.cap_id,
            pk: signer.pk,
        }
    }

    /// 角色中文标签（展示/错误信息用，不含秘密）。
    #[must_use]
    pub const fn role_label(self) -> &'static str {
        match self.role {
            SignerRole::Device => "设备",
            SignerRole::CreatorUpgrade => "旧 creator 根",
            SignerRole::OfflineRecovery => "离线恢复根",
            SignerRole::TargetPop => "目标设备 PoP",
        }
    }
}

/// 一个已存在的签名（`(signer_index, sig64)`）；**缺签就是没有条目**，不用零占位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalSignatureV1 {
    /// 签名槽下标（`< operation.signers.len()`）。
    pub signer_index: u32,
    /// 64B Ed25519 签名（全零一律拒绝）。
    pub signature: [u8; 64],
}

/// 发起端看到的**控制链位置**（`cap/控制链证据`；不是信任根，B 仍按自己的 pin/回放验证）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlEvidenceV1 {
    /// 提交前的链尾 rev（= `OperationV2.expected_control_rev`）。
    pub control_rev: u64,
    /// 提交前的链尾 hash（= `OperationV2.prev_control_hash`）。
    pub head_hash: [u8; 32],
    /// 提交前的 `authz_rev`（Upgrade 为 0）。
    pub authz_rev: u64,
    /// 提交前的 `authority_rev`（Upgrade 为 0）。
    pub authority_rev: u64,
}

// ---------------------------------------------------------------- 共享包

/// ADR 0016 的共享不可变审批包（canonical CDE）。
///
/// ```text
/// {0: version=1, 1: op_bytes, 2: h=SHA256(op_bytes), 3: initiator_index,
///  4: [{0: signer_index, 1: sig64}...], 5: [cap_bytes...], 6: control_evidence}
/// ```
///
/// 字段严格齐全（键恰好 0..6）、签名索引严格升序去重、cap 证据按 cap_id 严格升序且
/// **恰好等于** role0 签名槽引用的 cap 集合、`h` 必须重算一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalEnvelopeV1 {
    operation: OperationV2,
    operation_bytes: Vec<u8>,
    digest: [u8; 32],
    initiator_index: u32,
    signatures: Vec<ApprovalSignatureV1>,
    caps: Vec<ManagementCapV2>,
    control: ControlEvidenceV1,
}

impl ApprovalEnvelopeV1 {
    // ---- 只读访问 ----

    /// 完整操作（已冻结的不可变事实）。
    #[must_use]
    pub const fn operation(&self) -> &OperationV2 {
        &self.operation
    }

    /// `OperationV2` 的 canonical CDE 字节（`h` 的预映像）。
    #[must_use]
    pub fn operation_bytes(&self) -> &[u8] {
        &self.operation_bytes
    }

    /// `h = SHA256(operation_bytes)`。
    #[must_use]
    pub const fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// 发起者在 `signers` 里的下标。
    #[must_use]
    pub const fn initiator_index(&self) -> u32 {
        self.initiator_index
    }

    /// 已存在的签名（按槽位升序；缺签就是没有条目）。
    #[must_use]
    pub fn signatures(&self) -> &[ApprovalSignatureV1] {
        &self.signatures
    }

    /// cap 证据（公开 `ManagementCapV2`，按 cap_id 升序）。
    #[must_use]
    pub fn caps(&self) -> &[ManagementCapV2] {
        &self.caps
    }

    /// 发起端看到的控制链位置。
    #[must_use]
    pub const fn control_evidence(&self) -> ControlEvidenceV1 {
        self.control
    }

    /// 冻结的签名槽（顺序 = `signers` 顺序）。
    #[must_use]
    pub fn slots(&self) -> Vec<SignerSlotV1> {
        self.operation
            .signers
            .iter()
            .map(SignerSlotV1::from_signer)
            .collect()
    }

    /// 是否满签（每个槽都有签名）。
    #[must_use]
    pub fn is_fully_signed(&self) -> bool {
        self.signatures.len() == self.operation.signers.len()
    }

    /// 某槽的签名（缺签为 `None`）。
    #[must_use]
    pub fn signature_for(&self, signer_index: u32) -> Option<[u8; 64]> {
        self.signatures
            .iter()
            .find(|entry| entry.signer_index == signer_index)
            .map(|entry| entry.signature)
    }

    // ---- 构造/校验 ----

    /// 由完整操作 + 已有签名 + 证据构造（计算 `h` 并做全部结构校验）。
    pub fn from_operation(
        operation: &OperationV2,
        initiator_index: u32,
        signatures: Vec<ApprovalSignatureV1>,
        caps: Vec<ManagementCapV2>,
        control: ControlEvidenceV1,
    ) -> Result<Self, SyncError> {
        let operation_bytes = operation.encode()?;
        let envelope = Self {
            operation: operation.clone(),
            digest: sha256(&operation_bytes),
            operation_bytes,
            initiator_index,
            signatures,
            caps,
            control,
        };
        envelope.validate()?;
        Ok(envelope)
    }

    /// canonical CDE 编码（唯一 wire 形式）。
    pub fn encode(&self) -> Result<Vec<u8>, SyncError> {
        let value = CdeValue::Map(vec![
            (0, CdeValue::UInt(APPROVAL_ENVELOPE_V1_VERSION)),
            (1, CdeValue::Bytes(self.operation_bytes.clone())),
            (2, CdeValue::Bytes(self.digest.to_vec())),
            (3, CdeValue::UInt(u64::from(self.initiator_index))),
            (
                4,
                CdeValue::Array(
                    self.signatures
                        .iter()
                        .map(|entry| {
                            CdeValue::Map(vec![
                                (0, CdeValue::UInt(u64::from(entry.signer_index))),
                                (1, CdeValue::Bytes(entry.signature.to_vec())),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                5,
                CdeValue::Array(
                    self.caps
                        .iter()
                        .map(|cap| cap.encode().map(CdeValue::Bytes))
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            ),
            (
                6,
                CdeValue::Map(vec![
                    (0, CdeValue::UInt(self.control.control_rev)),
                    (1, CdeValue::Bytes(self.control.head_hash.to_vec())),
                    (2, CdeValue::UInt(self.control.authz_rev)),
                    (3, CdeValue::UInt(self.control.authority_rev)),
                ]),
            ),
        ]);
        encode_cde(&value).map_err(|_| approval_error("审批包无法 canonical 编码（内部字段非法）"))
    }

    /// 严格解码（限长、字段/版本/`h`/索引/证据全部校验）。
    pub fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        Self::decode_inner(bytes).map_err(|error| match error {
            SyncError::RelayWire(message) => {
                approval_error(format!("审批包字段缺失或类型不符（{message}）"))
            }
            other => other,
        })
    }

    fn decode_inner(bytes: &[u8]) -> Result<Self, SyncError> {
        if bytes.len() > MAX_APPROVAL_ENVELOPE_BYTES_V1 {
            return Err(approval_error(format!(
                "审批包超过导入上限（{} > {MAX_APPROVAL_ENVELOPE_BYTES_V1} 字节）",
                bytes.len()
            )));
        }
        let CdeValue::Map(fields) =
            decode_cde(bytes).map_err(|_| approval_error("审批包不是合法 canonical CDE"))?
        else {
            return Err(approval_error("审批包不是 CDE map"));
        };
        if fields.len() != 7 || fields.iter().any(|(key, _)| *key > 6) {
            return Err(approval_error("审批包字段集合不合法（必须恰好键 0..6）"));
        }
        let version = cbor::uint_field(&fields, 0)?;
        if version != APPROVAL_ENVELOPE_V1_VERSION {
            return Err(approval_error(format!(
                "审批包版本不受支持：{version}（本实现只接受 {APPROVAL_ENVELOPE_V1_VERSION}）"
            )));
        }
        let operation_bytes = cbor::bytes_field(&fields, 1)?;
        let declared_digest: [u8; 32] = cbor::bytes_fixed(&fields, 2)?;
        let initiator_index = u32::try_from(cbor::uint_field(&fields, 3)?)
            .map_err(|_| approval_error("审批包的 initiator_index 越界（u32）"))?;
        let operation = OperationV2::decode(&operation_bytes)?;
        if operation.encode()? != operation_bytes {
            return Err(approval_error(
                "审批包内 OperationV2 不是唯一 canonical 编码",
            ));
        }
        let recomputed = sha256(&operation_bytes);
        if declared_digest != recomputed {
            return Err(approval_error(
                "审批包的 h 与 SHA256(op_bytes) 不符（payload/rev/签名槽可能被改过）",
            ));
        }
        let mut signatures = Vec::new();
        for item in cbor::array_field(&fields, 4)? {
            let entry = cbor::value_map(&item)?;
            if entry.len() != 2 || entry.iter().any(|(key, _)| *key > 1) {
                return Err(approval_error("审批包签名条目的字段集合不合法"));
            }
            signatures.push(ApprovalSignatureV1 {
                signer_index: u32::try_from(cbor::uint_field(&entry, 0)?)
                    .map_err(|_| approval_error("审批包签名条目的槽位越界（u32）"))?,
                signature: cbor::bytes_fixed::<64>(&entry, 1)?,
            });
        }
        let caps = cbor::array_field(&fields, 5)?
            .into_iter()
            .map(|item| {
                let bytes = cbor::value_bytes(&item)?;
                let cap = ManagementCapV2::decode(&bytes)?;
                if cap.encode()? != bytes {
                    return Err(approval_error("审批包的 cap 证据不是唯一 canonical 编码"));
                }
                Ok(cap)
            })
            .collect::<Result<Vec<_>, SyncError>>()?;
        let control_fields = cbor::value_map(cbor::field(&fields, 6)?)
            .map_err(|_| approval_error("审批包缺控制链证据"))?;
        if control_fields.len() != 4 || control_fields.iter().any(|(key, _)| *key > 3) {
            return Err(approval_error("审批包控制链证据的字段集合不合法"));
        }
        let control = ControlEvidenceV1 {
            control_rev: cbor::uint_field(&control_fields, 0)?,
            head_hash: cbor::bytes_fixed::<32>(&control_fields, 1)?,
            authz_rev: cbor::uint_field(&control_fields, 2)?,
            authority_rev: cbor::uint_field(&control_fields, 3)?,
        };
        let envelope = Self {
            operation,
            operation_bytes,
            digest: recomputed,
            initiator_index,
            signatures,
            caps,
            control,
        };
        envelope.validate()?;
        Ok(envelope)
    }

    /// 复制用的 base64url（带版本前缀）。
    pub fn encode_base64(&self) -> Result<String, SyncError> {
        let bytes = self.encode()?;
        Ok(format!(
            "{APPROVAL_BASE64_PREFIX_V1}{}",
            base64url::encode(&bytes)
        ))
    }

    /// 解析 base64url 复制文本（限长；前缀必须精确匹配）。
    pub fn decode_base64(text: &str) -> Result<Self, SyncError> {
        if text.len() > MAX_APPROVAL_BASE64_CHARS_V1 {
            return Err(approval_error("审批包文本超过导入上限"));
        }
        let trimmed = text.trim();
        let Some(encoded) = trimmed.strip_prefix(APPROVAL_BASE64_PREFIX_V1) else {
            return Err(approval_error(format!(
                "审批包文本缺版本前缀 {APPROVAL_BASE64_PREFIX_V1}"
            )));
        };
        if encoded.is_empty() {
            return Err(approval_error("审批包文本为空"));
        }
        let bytes = base64url::decode(encoded)
            .map_err(|_| approval_error("审批包文本不是合法的 base64url"))?;
        Self::decode(&bytes)
    }

    /// 写入 `.mn-approval` 文件（先写临时文件再 rename；不自动执行/不联网）。
    pub fn write_file(&self, path: &Path) -> Result<(), SyncError> {
        let bytes = self.encode()?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| SyncError::Io(format!("创建审批包目录失败：{error}")))?;
            }
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = Path::new(&tmp);
        std::fs::write(tmp, &bytes)
            .map_err(|error| SyncError::Io(format!("写审批包临时文件失败：{error}")))?;
        std::fs::rename(tmp, path)
            .map_err(|error| SyncError::Io(format!("安装审批包文件失败：{error}")))?;
        Ok(())
    }

    /// 读取 `.mn-approval` 文件（限长；只解析结构，不做本地链验证）。
    pub fn read_file(path: &Path) -> Result<Self, SyncError> {
        let metadata = std::fs::metadata(path)
            .map_err(|error| SyncError::Io(format!("读取审批包文件失败：{error}")))?;
        if metadata.len() > MAX_APPROVAL_ENVELOPE_BYTES_V1 as u64 {
            return Err(approval_error("审批包文件超过导入上限"));
        }
        let bytes = std::fs::read(path)
            .map_err(|error| SyncError::Io(format!("读取审批包文件失败：{error}")))?;
        Self::decode(&bytes)
    }

    /// 逐槽验证**已存在**的签名（缺签不验；满签时等价于全量验签）。
    pub fn verify_present_signatures(&self) -> Result<(), SyncError> {
        for entry in &self.signatures {
            let index = entry.signer_index as usize;
            let Some(signer) = self.operation.signers.get(index) else {
                return Err(approval_error("审批包签名槽位超出 signer 数量"));
            };
            let input = signing_input(self.operation(), signer)?;
            verify_signature(&signer.pk, &input, &entry.signature).map_err(|_| {
                approval_error(format!(
                    "审批包第 {index} 槽（{}）的签名无效：换人/改 payload/换 cap 的旧签不能复用",
                    SignerSlotV1::from_signer(signer).role_label()
                ))
            })?;
        }
        Ok(())
    }

    /// 全部结构校验（构造/解码/导入/提交都走这里）。
    pub fn validate(&self) -> Result<(), SyncError> {
        let op = &self.operation;
        if op.encode()? != self.operation_bytes {
            return Err(approval_error("审批包的内嵌操作不是唯一 canonical 编码"));
        }
        if sha256(&self.operation_bytes) != self.digest {
            return Err(approval_error("审批包的 h 与 SHA256(op_bytes) 不一致"));
        }
        if op.signers.is_empty() || op.signers.len() > MAX_SIGNERS {
            return Err(approval_error("审批包的 signer 数量越界"));
        }
        if self.initiator_index as usize >= op.signers.len() {
            return Err(approval_error("审批包的 initiator_index 超出 signer 数量"));
        }
        // 签名条目：严格升序、去重、槽位合法、签名非全零。
        let mut previous: Option<u32> = None;
        for entry in &self.signatures {
            if entry.signer_index as usize >= op.signers.len() {
                return Err(approval_error("审批包签名条目的槽位超出 signer 数量"));
            }
            if let Some(previous) = previous {
                if entry.signer_index <= previous {
                    return Err(approval_error("审批包签名条目必须按槽位严格升序且不重复"));
                }
            }
            previous = Some(entry.signer_index);
            if entry.signature.iter().all(|byte| *byte == 0) {
                return Err(approval_error(
                    "审批包含全零签名（缺签不能用零占位，必须新建 Operation 重签）",
                ));
            }
        }
        // cap 证据：严格升序、无重复、恰好等于 role0 槽引用的 cap 集合。
        self.validate_caps()?;
        // 控制链证据必须与操作自身的链绑定一致。
        if self.control.control_rev != op.expected_control_rev
            || self.control.head_hash != op.prev_control_hash
        {
            return Err(approval_error(
                "审批包的控制链证据与操作的 rev/prev head 不一致",
            ));
        }
        if op.kind == ControlKindV2::UpgradePolicy {
            if op.expected_authz_rev != 0 || op.authority_rev != 1 {
                return Err(approval_error(
                    "UpgradePolicy 必须绑定 expected_authz_rev=0 / authority_rev=1",
                ));
            }
            if self.control.authz_rev != 0 || self.control.authority_rev != 0 {
                return Err(approval_error(
                    "UpgradePolicy 的控制链证据必须是升级前（authz/authority=0）",
                ));
            }
        } else if self.control.authz_rev != op.expected_authz_rev
            || self.control.authority_rev != op.authority_rev
        {
            return Err(approval_error(
                "审批包的控制链证据与操作的 authz/authority rev 不一致",
            ));
        }
        Ok(())
    }

    fn validate_caps(&self) -> Result<(), SyncError> {
        if self.caps.len() > MAX_CAP_EVIDENCE {
            return Err(approval_error("审批包的 cap 证据数量越界"));
        }
        let mut referenced = Vec::new();
        for signer in &self.operation.signers {
            match (signer.role, signer.cap_id) {
                (SignerRole::Device, Some(cap_id)) => referenced.push(cap_id),
                (SignerRole::Device, None) => {
                    if self.operation.kind != ControlKindV2::UpgradePolicy {
                        return Err(approval_error("非 Upgrade 的 role0 签名必须绑定 cap"));
                    }
                }
                (_, Some(_)) => {
                    return Err(approval_error("只有 role0 签名可以绑定 cap"));
                }
                _ => {}
            }
        }
        let mut seen = Vec::new();
        for cap in &self.caps {
            if seen.contains(&cap.cap_id) {
                return Err(approval_error("审批包的 cap 证据含重复 cap_id"));
            }
            if let Some(previous) = seen.last() {
                if cap.cap_id <= *previous {
                    return Err(approval_error("审批包的 cap 证据必须按 cap_id 严格升序"));
                }
            }
            seen.push(cap.cap_id);
            if !referenced.contains(&cap.cap_id) {
                return Err(approval_error("审批包含未被任何签名槽引用的 cap 证据"));
            }
            if self.operation.instance_id != cap.instance_id
                || self.operation.vault_id != cap.vault_id
            {
                return Err(approval_error(
                    "审批包的 cap 证据与操作的 instance/vault 不一致（跨租户）",
                ));
            }
        }
        for cap_id in referenced {
            if !self.caps.iter().any(|cap| cap.cap_id == cap_id) {
                return Err(approval_error(
                    "审批包缺少签名槽引用的 cap 证据（证据缺口不能跳过）",
                ));
            }
        }
        Ok(())
    }

    /// 构造一个内存探针 `SignedControlV2`（缺失槽用零占位，**只用于干跑验证**）。
    ///
    /// 零占位永远不会被编码进审批包、也永远不会被提交：`submit` 只接受满签包，
    /// 而 `SignedControlV2::validate` 要求签名数 == signer 数且全零签名在满签时会验签失败。
    fn probe_signed_control(&self) -> Result<SignedControlV2, SyncError> {
        let signatures = (0..self.operation.signers.len() as u32)
            .map(|index| self.signature_for(index).unwrap_or([0u8; 64]))
            .collect();
        Ok(SignedControlV2 {
            operation: self.operation.clone(),
            signatures,
        })
    }
}

// ---------------------------------------------------------------- 签名输入

/// 按签名槽的角色取待签字节（域分隔：control/upgrade/recover/pop）。
fn signing_input(operation: &OperationV2, signer: &SignerV2) -> Result<Vec<u8>, SyncError> {
    Ok(match signer.role {
        SignerRole::Device => control_signing_input(operation, proto_hash)?,
        SignerRole::CreatorUpgrade => upgrade_signing_input(operation, proto_hash)?,
        SignerRole::OfflineRecovery => recover_signing_input(operation, proto_hash)?,
        SignerRole::TargetPop => pop_signing_input(operation, &signer.pk, proto_hash)?,
    })
}

// ---------------------------------------------------------------- 状态机

/// 双签状态（ADR 0016「双签 UI/恢复」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalStateV1 {
    /// 已构造、签名槽冻结、尚未显式冻结（`freeze`/首次签名会前进）。
    Draft,
    /// 签名槽冻结，等待发起者签名。
    Frozen,
    /// 发起者已签，等待对方审核。
    InitiatorSigned,
    /// 已导交给对方（或由对方导入），等待对方加签。
    AwaitingCosign,
    /// 满签就绪（**只有**这个状态允许提交）。
    Ready,
    /// 已发出，等待回执/本地对账。
    Submitting,
    /// 回执已收到且本地已验证控制链包含该记录（终态；重放返回原回执）。
    Confirmed,
    /// 失败（分类见 [`ApprovalFailureV1`]；只有传输/限流/5xx 可重发同一字节）。
    Failed,
}

impl ApprovalStateV1 {
    const fn as_code(self) -> u64 {
        match self {
            Self::Draft => 0,
            Self::Frozen => 1,
            Self::InitiatorSigned => 2,
            Self::AwaitingCosign => 3,
            Self::Ready => 4,
            Self::Submitting => 5,
            Self::Confirmed => 6,
            Self::Failed => 7,
        }
    }

    const fn from_code(code: u64) -> Option<Self> {
        Some(match code {
            0 => Self::Draft,
            1 => Self::Frozen,
            2 => Self::InitiatorSigned,
            3 => Self::AwaitingCosign,
            4 => Self::Ready,
            5 => Self::Submitting,
            6 => Self::Confirmed,
            7 => Self::Failed,
            _ => return None,
        })
    }

    /// 中文标签（UI/错误信息用）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Draft => "草稿",
            Self::Frozen => "签名槽已冻结",
            Self::InitiatorSigned => "发起者已签",
            Self::AwaitingCosign => "等待对方加签",
            Self::Ready => "满签就绪",
            Self::Submitting => "提交中",
            Self::Confirmed => "已确认",
            Self::Failed => "失败",
        }
    }

    /// 是否终态。
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Confirmed)
    }
}

/// 提交/验证失败分类（ADR 0016：过期/CAS/rev 变化/撤权/目标变化 → 失效，必须重建）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalFailureV1 {
    /// 传输失败/响应丢失：可重发**同一字节**（relay 可能已接受；`dedup_v2` 幂等）。
    Transport,
    /// 429 限流：可稍后重发同一字节。
    RateLimited,
    /// 5xx：可稍后重发同一字节。
    Server,
    /// 403 签名/门限/角色/scope/ACTIVE/authority/PoP 不符（不可重试，需重建 Operation）。
    Forbidden,
    /// 404 目标不存在/跨租户（不可重试）。
    NotFound,
    /// 409 CAS/head/rev 陈旧：链已前进，必须刷新后**新建 Operation** 并全部重签。
    Stale,
    /// 409 其他（重复 cap/invite/grant、同 request_id 异摘要、状态不符）。
    Conflict,
    /// 410 签名时间/操作过期：必须新建 Operation 重签。
    Expired,
    /// 400 结构/CDE/签名编码非法（本地 bug 或被篡改）。
    Invalid,
    /// 本地前置：未满签/状态不符/未冻结。
    NotReady,
    /// 本地前置：签名槽/签名校验失败。
    Signature,
    /// 本地前置：与已验证控制链/证据不符（含跨 vault/路径/rev）。
    Mismatch,
}

impl ApprovalFailureV1 {
    /// 是否可"重发同一字节"（幂等安全）。
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Transport | Self::RateLimited | Self::Server)
    }
}

/// 一次失败的分类 + 人读信息（信息只含公开定位事实）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalFailureInfoV1 {
    /// 分类。
    pub kind: ApprovalFailureV1,
    /// 人读信息（不含秘密）。
    pub message: String,
}

/// 提交失败（分类 + 原始错误，可 `source()` 取回底层 `SyncError`）。
#[derive(Debug)]
pub struct ApprovalSubmitErrorV1 {
    /// 分类。
    pub kind: ApprovalFailureV1,
    /// 人读信息（不含秘密）。
    pub message: String,
    /// 原始错误（本地前置失败时通常也有：签名/链不符等）。
    pub source: Option<Box<SyncError>>,
}

impl ApprovalSubmitErrorV1 {
    fn local(kind: ApprovalFailureV1, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }
}

impl std::fmt::Display for ApprovalSubmitErrorV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "审批提交失败（{:?}）：{}",
            self.kind, self.message
        )?;
        if let Some(source) = &self.source {
            write!(formatter, "（底层：{source}）")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApprovalSubmitErrorV1 {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}

/// 构造请求（typed payload 的参数；构造器负责填 revs/head/request_id/时间窗）。
#[derive(Debug, Clone)]
pub enum ApprovalRequestV1 {
    /// kind 0 UpgradePolicy：旧 creator 根 + ≥2 台 ACTIVE 确认 + 新恢复钥 PoP。
    UpgradePolicy(UpgradePolicyRequestV1),
    /// kind 2 CapGrant：两台 CAP_ADMIN，scope 必须是每位签发者有效 scope 的子集。
    CapGrant(CapGrantRequestV1),
    /// kind 3 CapRevoke：两台 CAP_ADMIN。
    CapRevoke(CapRevokeRequestV1),
    /// kind 4 DeviceApprove：两台 APPROVE + 首次准入的目标 PoP。
    DeviceApprove(DeviceApproveRequestV1),
    /// kind 5 DeviceRevoke：两台 REVOKE。
    DeviceRevoke(DeviceRevokeRequestV1),
    /// kind 16 DeviceRightsChange：两台 APPROVE。
    DeviceRightsChange(DeviceRightsChangeRequestV1),
    /// kind 11 InviteCreate：一台 INVITE。
    InviteCreate(InviteCreateRequestV1),
    /// kind 12 InviteCancel：一台 INVITE。
    InviteCancel(InviteCancelRequestV1),
    /// rotation/activation（6–10）：本切片不构造。
    Rotation(ControlKindV2),
    /// 离线恢复（13/14）：本切片不构造。
    Recovery(ControlKindV2),
    /// `PolicyReplace`（kind 1）：relay 仍 501，本切片不构造。
    PolicyReplace(PolicyV2),
}

/// kind 0 UpgradePolicy 的构造参数（v1 事实全由调用方提供：客户端没有 V2 前置状态）。
#[derive(Debug, Clone)]
pub struct UpgradePolicyRequestV1 {
    /// v1 控制链尾 rev（`expected_control_rev`；升级前没有 V2 记录，V2 回放推不出）。
    pub control_rev: u64,
    /// v1 控制链尾 hash（`prev_control_hash`）。
    pub prev_control_hash: [u8; 32],
    /// 当前 v1 管理 rev（必须等于 relay `vaults.manager_rev`）。
    pub old_manager_rev: u64,
    /// 新 `PolicyV2`（门限固定 2/2）。
    pub policy: PolicyV2,
    /// 新离线恢复根公钥（独立随机 seed 的公钥；secret 不进 relay）。
    pub fresh_recovery_pk: [u8; 32],
    /// 原子安装的 bootstrap caps。
    pub bootstrap_caps: Vec<ManagementCapV2>,
    /// 全部当前 ACTIVE 设备的完整 `DeviceCertV2`（device_id 严格升序）。
    pub active_manifest: Vec<DeviceCertV2>,
    /// ≥2 台现有 ACTIVE 设备的确认（device_id + 公钥；发起者必须在其中）。
    pub confirmations: Vec<(DeviceId, [u8; 32])>,
}

/// kind 2 CapGrant 的构造参数。
#[derive(Debug, Clone)]
pub struct CapGrantRequestV1 {
    /// 新 cap ID（永不复用）。
    pub cap_id: [u8; 16],
    /// 受赠设备（必须 ACTIVE）。
    pub subject_device_id: DeviceId,
    /// 授予的 scope（必须是每位签发者有效 scope 的子集）。
    pub scopes: u32,
    /// 生效时刻（`not_before`）。
    pub not_before: u64,
    /// 到期时刻（`expires_at`）。
    pub expires_at: u64,
    /// 第二签名者设备（必须 ACTIVE 且持 CAP_ADMIN）。
    pub counterparty: DeviceId,
}

/// kind 3 CapRevoke 的构造参数。
#[derive(Debug, Clone)]
pub struct CapRevokeRequestV1 {
    /// 要撤销的 cap ID。
    pub cap_id: [u8; 16],
    /// 第二签名者设备。
    pub counterparty: DeviceId,
}

/// kind 4 DeviceApprove 的构造参数。
#[derive(Debug, Clone)]
pub struct DeviceApproveRequestV1 {
    /// 目标证书（PENDING 目标：serial=1、issued_authz_rev=提交后值）。
    pub cert: DeviceCertV2,
    /// 当前 epoch 的 grant 包（恰好一份，由本地现有 grant 逻辑生成）。
    pub grants: Vec<GrantPackageV2>,
    /// 第二签名者设备（APPROVE）。
    pub counterparty: DeviceId,
}

/// kind 5 DeviceRevoke 的构造参数。
#[derive(Debug, Clone)]
pub struct DeviceRevokeRequestV1 {
    /// 要撤销的设备（构造器会排序去重；空数组拒绝）。
    pub device_ids: Vec<DeviceId>,
    /// 第二签名者设备（REVOKE）。
    pub counterparty: DeviceId,
}

/// kind 16 DeviceRightsChange 的构造参数。
#[derive(Debug, Clone)]
pub struct DeviceRightsChangeRequestV1 {
    /// 新证书（serial=当前+1、公钥不变、权限真的变化）。
    pub cert: DeviceCertV2,
    /// 当前 epoch 的 grant 包（恰好一份）。
    pub grants: Vec<GrantPackageV2>,
    /// 第二签名者设备（APPROVE）。
    pub counterparty: DeviceId,
}

/// kind 11 InviteCreate 的构造参数。
#[derive(Debug, Clone)]
pub struct InviteCreateRequestV1 {
    /// 邀请 ID（永不复用）。
    pub invite_id: [u8; 16],
    /// `SHA256(token)`（relay 永不落明文 token）。
    pub token_hash: [u8; 32],
    /// 邀请到期时刻（`> signed_at` 且 ≤ policy.max_invite_ttl）。
    pub expires_at: u64,
}

/// kind 12 InviteCancel 的构造参数。
#[derive(Debug, Clone)]
pub struct InviteCancelRequestV1 {
    /// 要取消的邀请 ID（必须在已验证控制链上）。
    pub invite_id: [u8; 16],
}

// ---------------------------------------------------------------- 草稿

/// 一份手工双签草稿：冻结的不可变操作 + 已有签名 + 状态机状态。
///
/// 草稿**不持有任何私钥**：签名由调用方在需要时用 [`DeviceSigningKey`] 现签。
#[derive(Debug, Clone)]
pub struct ApprovalDraftV1 {
    envelope: ApprovalEnvelopeV1,
    state: ApprovalStateV1,
    receipt: Option<ControlReceiptV2>,
    failure: Option<ApprovalFailureInfoV1>,
    handed_off: bool,
}

impl ApprovalDraftV1 {
    // ---- 构造 ----

    /// 用**已验证**回放状态构造一份 typed 操作草稿（签名槽在此冻结）。
    ///
    /// `now` 是签名时刻（`signed_at`），`not_after = now + 300`；`request_id` 由 `rng`
    /// 生成（不复用）。构造末尾会用同一回放做一次**不验签的干跑**：kind 门限/角色/scope/
    /// authority/payload 语义/时间窗/链位置全部提前判定，错误信息明确。
    ///
    /// **调用前必须** `refresh_control`（否则 rev/head 可能陈旧：提交时会按已验证链判定
    /// `Stale`，不会自动改 rev/自动重签）。
    pub fn propose(
        replay: &V2ControlReplay,
        initiator: DeviceId,
        request: &ApprovalRequestV1,
        rng: &mut impl RandomSource,
        now: u64,
    ) -> Result<Self, SyncError> {
        let state = replay.state();
        let OperationParts {
            kind,
            payload,
            signers,
            caps,
        } = build_operation_parts(replay, initiator, request, now)?;
        let mut signers = signers;
        signers.sort_by(compare_signers);
        let initiator_index = signers
            .iter()
            .position(|signer| {
                signer.device_id == Some(initiator) && signer.role == SignerRole::Device
            })
            .ok_or_else(|| approval_error("发起者不在签名槽里（必须是一台 ACTIVE 设备）"))?;
        let initiator_index =
            u32::try_from(initiator_index).map_err(|_| approval_error("签名槽下标越界"))?;
        let mut request_id: [u8; 16] = random::array(rng)?;
        if request_id == [0u8; 16] {
            request_id[0] = 1;
        }
        let (expected_control_rev, prev_control_hash, expected_authz_rev, authority_rev) =
            match &request {
                // Upgrade 前没有 V2 记录：链位置由调用方从已验证 v1 控制链给出。
                ApprovalRequestV1::UpgradePolicy(request) => {
                    (request.control_rev, request.prev_control_hash, 0, 1)
                }
                _ => (
                    state.rev(),
                    state.head_hash(),
                    state.authz_rev(),
                    state.authority_rev(),
                ),
            };
        let operation = OperationV2 {
            version: V2_WIRE_VERSION,
            instance_id: state.instance_id(),
            vault_id: state.vault_id(),
            kind,
            request_id,
            expected_control_rev,
            prev_control_hash,
            expected_authz_rev,
            authority_rev,
            signed_at: now,
            not_after: now.saturating_add(DEADLINE_MAX_SECS),
            canonical_path: canonical_submit_path(&state.vault_id()),
            payload,
            signers,
        };
        let mut caps = caps;
        caps.sort_by_key(|cap| cap.cap_id);
        let control = ControlEvidenceV1 {
            control_rev: expected_control_rev,
            head_hash: prev_control_hash,
            authz_rev: state.authz_rev(),
            authority_rev: state.authority_rev(),
        };
        let envelope = ApprovalEnvelopeV1::from_operation(
            &operation,
            initiator_index,
            Vec::new(),
            caps,
            control,
        )?;
        // 干跑：与链上回放同一条验证路径（不验签，因为此刻还没有签名）。
        let probe = envelope.probe_signed_control()?;
        let dry_run = if kind == ControlKindV2::UpgradePolicy {
            replay.validate_pending_control_at(
                &probe,
                expected_control_rev,
                prev_control_hash,
                now,
                false,
            )
        } else {
            replay.validate_pending_control(&probe, now, false)
        };
        dry_run
            .map_err(|error| approval_error(format!("构造的控制操作未通过干跑验证：{error}")))?;
        Ok(Self {
            envelope,
            state: ApprovalStateV1::Draft,
            receipt: None,
            failure: None,
            handed_off: false,
        })
    }

    // ---- 只读 ----

    /// 共享审批包。
    #[must_use]
    pub const fn envelope(&self) -> &ApprovalEnvelopeV1 {
        &self.envelope
    }

    /// 状态。
    #[must_use]
    pub const fn state(&self) -> ApprovalStateV1 {
        self.state
    }

    /// 冻结的签名槽。
    #[must_use]
    pub fn slots(&self) -> Vec<SignerSlotV1> {
        self.envelope.slots()
    }

    /// 是否已满签。
    #[must_use]
    pub fn is_fully_signed(&self) -> bool {
        self.envelope.is_fully_signed()
    }

    /// 已确认的回执（`Confirmed` 后）。
    #[must_use]
    pub const fn receipt(&self) -> Option<ControlReceiptV2> {
        self.receipt
    }

    /// 最近一次失败（不含秘密）。
    #[must_use]
    pub fn failure(&self) -> Option<&ApprovalFailureInfoV1> {
        self.failure.as_ref()
    }

    /// 是否已导交给对方（`AwaitingCosign` 的显式标记）。
    #[must_use]
    pub const fn is_handed_off(&self) -> bool {
        self.handed_off
    }

    // ---- 状态机 ----

    /// 显式冻结签名槽（`Draft → Frozen`；已冻结/更后状态保持不变）。
    pub fn freeze(&mut self) {
        if self.state == ApprovalStateV1::Draft {
            self.state = ApprovalStateV1::Frozen;
        }
    }

    /// 用本机设备钥签**自己的槽**（`Frozen/InitiatorSigned/AwaitingCosign` 可用）。
    ///
    /// - 第一签必须是发起者（A 先签）；
    /// - 只接受公钥恰好匹配**一个**冻结槽的钥（不能签别人的槽，不能凑门限）；
    /// - 域按槽角色选择（设备/旧 creator/恢复根/目标 PoP）；
    /// - 满签 → `Ready`；否则 `InitiatorSigned`/`AwaitingCosign`。
    pub fn sign_with(&mut self, key: &DeviceSigningKey) -> Result<(), SyncError> {
        match self.state {
            ApprovalStateV1::Draft
            | ApprovalStateV1::Frozen
            | ApprovalStateV1::InitiatorSigned
            | ApprovalStateV1::AwaitingCosign => {}
            ApprovalStateV1::Ready => {
                return Err(approval_error(
                    "包已满签：不能修改签名（要换 payload 必须新建 Operation）",
                ))
            }
            ApprovalStateV1::Submitting | ApprovalStateV1::Confirmed | ApprovalStateV1::Failed => {
                return Err(approval_error(format!(
                    "当前状态「{}」不允许加签（换人/改 payload 必须新建 Operation）",
                    self.state.label()
                )))
            }
        }
        let pk = key.public_key();
        let mut matching = self
            .envelope
            .operation()
            .signers
            .iter()
            .enumerate()
            .filter(|(_, signer)| signer.pk == pk)
            .map(|(index, _)| index as u32);
        let Some(index) = matching.next() else {
            return Err(approval_error(
                "本机设备钥不在冻结的签名槽里（换 B/cap 必须新建 Operation，旧签名作废）",
            ));
        };
        if matching.next().is_some() {
            return Err(approval_error(
                "同一公钥对应多个签名槽（不允许用同钥/同设备凑门限）",
            ));
        }
        if self.envelope.signatures().is_empty() && index != self.envelope.initiator_index() {
            return Err(approval_error(
                "第一签必须是发起者的签名槽（A 先签，B 审核后加签）",
            ));
        }
        let signer = &self.envelope.operation().signers[index as usize];
        let input = signing_input(self.envelope.operation(), signer)?;
        let signature = key.sign(&input);
        if signature.iter().all(|byte| *byte == 0) {
            return Err(approval_error("签名结果为全零（拒绝写出无效签名）"));
        }
        let mut signatures = self.envelope.signatures().to_vec();
        match signatures
            .iter_mut()
            .find(|entry| entry.signer_index == index)
        {
            Some(entry) => entry.signature = signature,
            None => signatures.push(ApprovalSignatureV1 {
                signer_index: index,
                signature,
            }),
        }
        signatures.sort_by_key(|entry| entry.signer_index);
        let control = self.envelope.control_evidence();
        let initiator_index = self.envelope.initiator_index();
        self.envelope = ApprovalEnvelopeV1::from_operation(
            self.envelope.operation(),
            initiator_index,
            signatures,
            self.envelope.caps().to_vec(),
            control,
        )?;
        self.envelope.verify_present_signatures()?;
        self.state = if self.envelope.is_fully_signed() {
            ApprovalStateV1::Ready
        } else if self.envelope.signatures().len() == 1 {
            ApprovalStateV1::InitiatorSigned
        } else {
            ApprovalStateV1::AwaitingCosign
        };
        self.failure = None;
        Ok(())
    }

    /// 标记"已导交给对方"（`InitiatorSigned → AwaitingCosign`）。
    pub fn mark_handed_off(&mut self) {
        self.handed_off = true;
        if self.state == ApprovalStateV1::InitiatorSigned {
            self.state = ApprovalStateV1::AwaitingCosign;
        }
    }

    // ---- 导入（结构 + 本地验证） ----

    /// 解析审批包字节（只做结构与 `h` 校验；本地链验证见 [`Self::verify_import`]）。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SyncError> {
        let envelope = ApprovalEnvelopeV1::decode(bytes)?;
        Ok(Self::from_envelope(envelope, true))
    }

    /// 解析 base64url 复制文本（带版本前缀）。
    pub fn from_base64(text: &str) -> Result<Self, SyncError> {
        let envelope = ApprovalEnvelopeV1::decode_base64(text)?;
        Ok(Self::from_envelope(envelope, true))
    }

    /// 读取 `.mn-approval` 文件（只解析结构）。
    pub fn read_file(path: &Path) -> Result<Self, SyncError> {
        let envelope = ApprovalEnvelopeV1::read_file(path)?;
        Ok(Self::from_envelope(envelope, true))
    }

    fn from_envelope(envelope: ApprovalEnvelopeV1, imported: bool) -> Self {
        envelope.verify_present_signatures().ok();
        let mut draft = Self {
            state: ApprovalStateV1::Draft,
            envelope,
            receipt: None,
            failure: None,
            handed_off: imported,
        };
        draft.recompute_state();
        draft
    }

    /// 由包内容推导状态（导入/加载草稿时用；`Confirmed/Failed` 保留持久化值）。
    fn recompute_state(&mut self) {
        if matches!(
            self.state,
            ApprovalStateV1::Confirmed | ApprovalStateV1::Failed | ApprovalStateV1::Submitting
        ) {
            return;
        }
        let signatures = self.envelope.signatures().len();
        self.state = if signatures == 0 {
            ApprovalStateV1::Draft
        } else if self.envelope.is_fully_signed() {
            ApprovalStateV1::Ready
        } else if self.handed_off {
            ApprovalStateV1::AwaitingCosign
        } else if signatures == 1 {
            ApprovalStateV1::InitiatorSigned
        } else {
            ApprovalStateV1::AwaitingCosign
        };
    }

    /// 按本地**已验证**回放验证导入包（B 加签前的强制步骤）。
    ///
    /// 覆盖：完整 Operation（vault/instance/path/revs/kind/payload/signers/时间窗）、
    /// 控制链证据 == 本地已验证位置、A 的授权/cap/scope（与回放同一验证路径）、
    /// 目标/证书/grant/集合承诺、签名槽与已存在签名。
    pub fn verify_import(&self, replay: &V2ControlReplay, now: u64) -> Result<(), SyncError> {
        self.envelope.validate()?;
        self.envelope.verify_present_signatures()?;
        if self.envelope.signatures().is_empty() {
            return Err(approval_error(
                "导入的审批包没有任何签名（必须先由发起者签署）",
            ));
        }
        if self
            .envelope
            .signature_for(self.envelope.initiator_index())
            .is_none()
        {
            return Err(approval_error("导入的审批包缺发起者签名（A 必须先签）"));
        }
        let operation = self.envelope.operation();
        let state = replay.state();
        let vault_id = state.vault_id();
        if operation.vault_id != vault_id {
            return Err(approval_error(
                "审批包的 vault 与本地不符（跨租户包不能导入）",
            ));
        }
        if operation.instance_id != state.instance_id() {
            return Err(approval_error(
                "审批包的 instance 与本地 pin 不符（跨 relay 包不能导入）",
            ));
        }
        if operation.canonical_path != canonical_submit_path(&vault_id) {
            return Err(approval_error("审批包不是唯一提交路径"));
        }
        let control = self.envelope.control_evidence();
        if operation.kind == ControlKindV2::UpgradePolicy {
            // 升级前没有 V2 记录：V2 回放不含 v1 前缀，链位置只能由包内证据 + **签名**
            // 给出（relay 按 DB 校验 CAS）。本地能验的是"还没升级"与"回放不比包更靠前"。
            if state.authz_state().is_some() {
                return Err(approval_error(
                    "vault 已启用 V2：UpgradePolicy 包已陈旧（必须是首条 V2 记录）",
                ));
            }
            if state.rev() > operation.expected_control_rev {
                return Err(approval_error(
                    "审批包的 v1 链位置比本地已验证回放更靠后（包已陈旧）",
                ));
            }
        } else if control.control_rev != state.rev()
            || control.head_hash != state.head_hash()
            || control.authz_rev != state.authz_rev()
            || control.authority_rev != state.authority_rev()
        {
            return Err(approval_error(
                "审批包的控制链证据与本地已验证位置不符（链已前进/分叉或包已陈旧）",
            ));
        }
        if now > operation.not_after {
            return Err(approval_error(
                "审批包已超出短窗（not_after 已过）：必须新建 Operation 并全部重签",
            ));
        }
        if operation.signed_at > now.saturating_add(SIGNED_AT_FUTURE_SKEW) {
            return Err(approval_error("审批包的 signed_at 在未来（超过允许偏差）"));
        }
        // cap 证据必须与本地已验证 cap 逐字节一致（裸 cap 不是授权证据）。
        for cap in self.envelope.caps() {
            let Some(entry) = state.cap(&cap.cap_id) else {
                return Err(approval_error(
                    "审批包的 cap 证据不在本地已验证控制链上（证据缺口不能跳过）",
                ));
            };
            if entry.cap != *cap {
                return Err(approval_error(
                    "审批包的 cap 证据与本地已验证 cap 不一致（被篡改或跨链）",
                ));
            }
        }
        // 语义干跑：与链上回放同一路径（半签跳过验签，已存在的签名上面已逐槽验过）。
        let probe = self.envelope.probe_signed_control()?;
        let verify_signatures = self.envelope.is_fully_signed();
        let dry_run = if operation.kind == ControlKindV2::UpgradePolicy {
            replay.validate_pending_control_at(
                &probe,
                operation.expected_control_rev,
                operation.prev_control_hash,
                now,
                verify_signatures,
            )
        } else {
            replay.validate_pending_control(&probe, now, verify_signatures)
        };
        dry_run.map_err(|error| approval_error(format!("审批包未通过本地授权验证：{error}")))?;
        Ok(())
    }

    // ---- 导出 ----

    /// canonical CDE 字节。
    pub fn to_bytes(&self) -> Result<Vec<u8>, SyncError> {
        self.envelope.encode()
    }

    /// 复制用的 base64url（带版本前缀）。
    pub fn to_base64(&self) -> Result<String, SyncError> {
        self.envelope.encode_base64()
    }

    /// 写 `.mn-approval` 文件（并标记已导交给对方）。
    pub fn write_file(&mut self, path: &Path) -> Result<(), SyncError> {
        self.envelope.write_file(path)?;
        self.mark_handed_off();
        Ok(())
    }

    // ---- 提交 ----

    /// 提交到 `/v2/.../control/submit`（自证；满签才允许；不改变本地 pin/库）。
    ///
    /// 顺序：本地结构/签名校验 → `refresh_control`（签前/提交前刷新）→ 时间窗/链位置
    /// （陈旧 = `Stale`，**不**自动改 rev）→ 提交 → 回执交叉校验 → 再次刷新并**对账**
    /// （本地已验证链必须在同一 rev 给出同一 hash）。丢响应/重复提交时重发原字节，
    /// relay `dedup_v2` 返回原回执。
    pub fn submit<T: HttpTransport, C: Clock>(
        &mut self,
        client: &mut V2ManagementClient<T, C>,
    ) -> Result<ControlReceiptV2, ApprovalSubmitErrorV1> {
        if self.state == ApprovalStateV1::Confirmed {
            if let Some(receipt) = self.receipt {
                return Ok(receipt);
            }
        }
        if !self.envelope.is_fully_signed() {
            return Err(ApprovalSubmitErrorV1::local(
                ApprovalFailureV1::NotReady,
                format!(
                    "包未满签（{}/{} 槽）：满签前禁止提交",
                    self.envelope.signatures().len(),
                    self.envelope.operation().signers.len()
                ),
            ));
        }
        if let Err(error) = self.envelope.validate() {
            return Err(self.fail(ApprovalFailureV1::Mismatch, error));
        }
        if let Err(error) = self.envelope.verify_present_signatures() {
            return Err(self.fail(ApprovalFailureV1::Signature, error));
        }
        let operation = self.envelope.operation().clone();
        let signed = match self.envelope.probe_signed_control() {
            Ok(signed) => signed,
            Err(error) => return Err(self.fail(ApprovalFailureV1::Mismatch, error)),
        };
        // 签前/提交前刷新已验证控制链（失败按传输/业务分类；不清库/不换信任锚）。
        if let Err(error) = client.refresh_control() {
            let kind = classify_sync_error(&error);
            return Err(self.fail(kind, error));
        }
        let now = client.now_secs();
        let state = client.replay().state().clone();
        let control = self.envelope.control_evidence();
        // 本次提交是否已经在链上（丢响应重发/重复提交）：用本地复算的链 hash 判定。
        let our_chain_hash = {
            let signed_bytes = signed
                .encode()
                .map_err(|error| self.fail(ApprovalFailureV1::Invalid, SyncError::from(error)))?;
            let mut input = Vec::with_capacity(32 + signed_bytes.len());
            input.extend_from_slice(&operation.prev_control_hash);
            input.extend_from_slice(&signed_bytes);
            sha256(&input)
        };
        let committed = client.record_hash_at(operation.expected_control_rev.saturating_add(1))
            == Some(our_chain_hash)
            || (state.rev() == operation.expected_control_rev.saturating_add(1)
                && state.head_hash() == our_chain_hash);
        if !committed {
            if now > operation.not_after {
                return Err(ApprovalSubmitErrorV1::local(
                    ApprovalFailureV1::Expired,
                    "操作已超出短窗（not_after 已过）：必须新建 Operation 并全部重签",
                ));
            }
            if operation.kind == ControlKindV2::UpgradePolicy {
                // 升级前没有 V2 记录：V2 回放不含 v1 前缀，链位置由包内（v1 已验证事实）
                // 给出，relay 仍按 DB 做 CAS；本地只验"还没升级"与全部语义/签名。
                if state.authz_state().is_some() {
                    return Err(ApprovalSubmitErrorV1::local(
                        ApprovalFailureV1::Stale,
                        "vault 已启用 V2：UpgradePolicy 只能作为首条 V2 记录",
                    ));
                }
                if state.rev() > operation.expected_control_rev {
                    return Err(ApprovalSubmitErrorV1::local(
                        ApprovalFailureV1::Stale,
                        "本地已验证回放比包内 v1 链位置更靠后（包已陈旧）",
                    ));
                }
                if let Err(error) = client.replay().validate_pending_control_at(
                    &signed,
                    operation.expected_control_rev,
                    operation.prev_control_hash,
                    now,
                    true,
                ) {
                    return Err(self.fail(ApprovalFailureV1::Mismatch, error));
                }
            } else {
                if control.control_rev != state.rev()
                    || control.head_hash != state.head_hash()
                    || control.authz_rev != state.authz_rev()
                    || control.authority_rev != state.authority_rev()
                {
                    return Err(ApprovalSubmitErrorV1::local(
                        ApprovalFailureV1::Stale,
                        "控制链已前进（rev/head/authz/authority 与包内证据不符）：必须刷新后新建 Operation 并全部重签",
                    ));
                }
                // 本地满签验证（含全部签名）：与链上回放同一路径。
                if let Err(error) = client.replay().validate_pending_control(&signed, now, true) {
                    return Err(self.fail(ApprovalFailureV1::Mismatch, error));
                }
            }
        }
        self.state = ApprovalStateV1::Submitting;
        let receipt = match client.submit_control_bytes(&signed) {
            Ok(receipt) => receipt,
            Err(error) => {
                let kind = classify_sync_error(&error);
                return Err(self.fail(kind, error));
            }
        };
        // 回执交叉校验：用本地字节复算链 hash（不信任 relay 摘要）。
        if let Err(error) = self.verify_receipt(&receipt) {
            return Err(self.fail(ApprovalFailureV1::Mismatch, error));
        }
        // 对账：刷新后本地已验证链必须在同一 rev 给出同一 hash。
        if let Err(error) = client.refresh_control() {
            let kind = classify_sync_error(&error);
            return Err(self.fail(
                kind,
                approval_error(format!(
                    "回执已收到（rev={}），但本地控制链刷新失败，稍后重发原字节可幂等确认：{error}",
                    receipt.control_rev
                )),
            ));
        }
        if let Err(error) = self.reconcile(&receipt, client) {
            return Err(self.fail(ApprovalFailureV1::Mismatch, error));
        }
        self.receipt = Some(receipt);
        self.state = ApprovalStateV1::Confirmed;
        self.failure = None;
        Ok(receipt)
    }

    fn verify_receipt(&self, receipt: &ControlReceiptV2) -> Result<(), SyncError> {
        let operation = self.envelope.operation();
        if receipt.control_rev != operation.expected_control_rev.saturating_add(1) {
            return Err(approval_error("回执 rev 与操作的 CAS 绑定不符"));
        }
        if receipt.kind != operation.kind.as_u8() || receipt.request_id != operation.request_id {
            return Err(approval_error("回执的 kind/request_id 与本地操作不符"));
        }
        let signed_bytes = self.envelope.probe_signed_control()?.encode()?;
        let mut input = Vec::with_capacity(32 + signed_bytes.len());
        input.extend_from_slice(&operation.prev_control_hash);
        input.extend_from_slice(&signed_bytes);
        if sha256(&input) != receipt.head_hash {
            return Err(approval_error(
                "回执 head_hash 与本地复算的链 hash 不符（relay 摘要不可信）",
            ));
        }
        Ok(())
    }

    fn reconcile<T: HttpTransport, C: Clock>(
        &self,
        receipt: &ControlReceiptV2,
        client: &V2ManagementClient<T, C>,
    ) -> Result<(), SyncError> {
        let state = client.replay().state();
        if state.rev() < receipt.control_rev {
            return Err(approval_error(
                "刷新后的控制链未包含本次提交（本地回放落后）",
            ));
        }
        match client.record_hash_at(receipt.control_rev) {
            Some(hash) if hash == receipt.head_hash => {}
            Some(_) => {
                return Err(approval_error(
                    "本地回放在同一 rev 上给出不同 hash（分叉/替换）",
                ))
            }
            None => {
                if state.rev() != receipt.control_rev {
                    return Err(approval_error("本地回放缺少本次提交的 hash（无法对账）"));
                }
            }
        }
        if state.rev() == receipt.control_rev {
            if state.authz_rev() != receipt.authz_rev
                || state.authority_rev() != receipt.authority_rev
            {
                return Err(approval_error(
                    "回执的 authz/authority rev 与本地已验证回放不符",
                ));
            }
        } else if state.authz_rev() < receipt.authz_rev
            || state.authority_rev() < receipt.authority_rev
        {
            return Err(approval_error(
                "本地回放的 authz/authority rev 低于回执（回滚/分叉）",
            ));
        }
        Ok(())
    }

    fn fail(&mut self, kind: ApprovalFailureV1, error: SyncError) -> ApprovalSubmitErrorV1 {
        let message = error.to_string();
        self.state = ApprovalStateV1::Failed;
        self.failure = Some(ApprovalFailureInfoV1 {
            kind,
            message: message.clone(),
        });
        ApprovalSubmitErrorV1 {
            kind,
            message,
            source: Some(Box::new(error)),
        }
    }

    // ---- 草稿持久化 ----

    /// 保存草稿到 vault 公开 meta（内容 = 公开 op/签名/公开 cap 证据；**不**含私钥）。
    pub fn save_draft(&self, vault: &Vault) -> Result<(), SyncError> {
        let bytes = self.encode_draft()?;
        vault.set_meta(META_APPROVAL_DRAFT_V1, &bytes)?;
        Ok(())
    }

    /// 读取草稿（不存在 → `Ok(None)`；损坏/版本不符 → 明确错误，不当成"没有草稿"）。
    pub fn load_draft(vault: &Vault) -> Result<Option<Self>, SyncError> {
        let Some(bytes) = vault.get_meta(META_APPROVAL_DRAFT_V1)? else {
            return Ok(None);
        };
        if bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self::decode_draft(&bytes)?))
    }

    /// 清掉草稿（写空串表示"没有草稿"）。
    pub fn clear_draft(vault: &Vault) -> Result<(), SyncError> {
        vault.set_meta(META_APPROVAL_DRAFT_V1, &[])?;
        Ok(())
    }

    fn encode_draft(&self) -> Result<Vec<u8>, SyncError> {
        let mut fields = vec![
            (0, CdeValue::UInt(APPROVAL_DRAFT_V1_VERSION)),
            (1, CdeValue::Bytes(self.envelope.encode()?)),
            (2, CdeValue::UInt(self.state.as_code())),
            (3, CdeValue::Bool(self.handed_off)),
        ];
        if let Some(receipt) = self.receipt {
            fields.push((
                4,
                CdeValue::Map(vec![
                    (0, CdeValue::UInt(receipt.control_rev)),
                    (1, CdeValue::Bytes(receipt.head_hash.to_vec())),
                    (2, CdeValue::UInt(u64::from(receipt.kind))),
                    (3, CdeValue::Bytes(receipt.request_id.to_vec())),
                    (4, CdeValue::UInt(receipt.authz_rev)),
                    (5, CdeValue::UInt(receipt.authority_rev)),
                ]),
            ));
        }
        if let Some(failure) = &self.failure {
            fields.push((
                5,
                CdeValue::Map(vec![
                    (0, CdeValue::UInt(failure_kind_code(failure.kind))),
                    (1, CdeValue::Text(failure.message.clone())),
                ]),
            ));
        }
        encode_cde(&CdeValue::Map(fields))
            .map_err(|_| approval_error("审批草稿无法 canonical 编码"))
    }

    fn decode_draft(bytes: &[u8]) -> Result<Self, SyncError> {
        Self::decode_draft_inner(bytes).map_err(|error| match error {
            SyncError::RelayWire(message) => {
                approval_error(format!("审批草稿字段缺失或类型不符（{message}）"))
            }
            other => other,
        })
    }

    fn decode_draft_inner(bytes: &[u8]) -> Result<Self, SyncError> {
        if bytes.len() > MAX_APPROVAL_ENVELOPE_BYTES_V1 + 4096 {
            return Err(approval_error("审批草稿超过上限"));
        }
        let fields =
            cbor::decode(bytes).map_err(|_| approval_error("审批草稿不是合法 canonical CDE"))?;
        if fields.len() < 4 || fields.iter().any(|(key, _)| *key > 5) {
            return Err(approval_error("审批草稿字段集合不合法"));
        }
        if cbor::uint_field(&fields, 0)? != APPROVAL_DRAFT_V1_VERSION {
            return Err(approval_error("审批草稿版本不受支持"));
        }
        let envelope = ApprovalEnvelopeV1::decode(&cbor::bytes_field(&fields, 1)?)?;
        let state = ApprovalStateV1::from_code(cbor::uint_field(&fields, 2)?)
            .ok_or_else(|| approval_error("审批草稿的状态码不受支持"))?;
        let handed_off = cbor::bool_field(&fields, 3)?;
        let receipt = match cbor::opt_field(&fields, 4) {
            None => None,
            Some(value) => {
                let receipt = cbor::value_map(value)?;
                if receipt.len() != 6 || receipt.iter().any(|(key, _)| *key > 5) {
                    return Err(approval_error("审批草稿的回执字段集合不合法"));
                }
                Some(ControlReceiptV2 {
                    control_rev: cbor::uint_field(&receipt, 0)?,
                    head_hash: cbor::bytes_fixed::<32>(&receipt, 1)?,
                    kind: u8::try_from(cbor::uint_field(&receipt, 2)?)
                        .map_err(|_| approval_error("审批草稿回执的 kind 越界"))?,
                    request_id: cbor::bytes_fixed::<16>(&receipt, 3)?,
                    authz_rev: cbor::uint_field(&receipt, 4)?,
                    authority_rev: cbor::uint_field(&receipt, 5)?,
                })
            }
        };
        let failure = match cbor::opt_field(&fields, 5) {
            None => None,
            Some(value) => {
                let failure = cbor::value_map(value)?;
                if failure.len() != 2 || failure.iter().any(|(key, _)| *key > 1) {
                    return Err(approval_error("审批草稿的失败信息字段集合不合法"));
                }
                let kind = failure_kind_from_code(cbor::uint_field(&failure, 0)?)
                    .ok_or_else(|| approval_error("审批草稿的失败分类不受支持"))?;
                Some(ApprovalFailureInfoV1 {
                    kind,
                    message: cbor::text_field(&failure, 1)?,
                })
            }
        };
        let mut draft = Self {
            envelope,
            state,
            receipt,
            failure,
            handed_off,
        };
        draft.recompute_state();
        if draft.state == ApprovalStateV1::Confirmed && draft.receipt.is_none() {
            return Err(approval_error("审批草稿标为已确认但缺回执"));
        }
        Ok(draft)
    }
}

fn failure_kind_code(kind: ApprovalFailureV1) -> u64 {
    match kind {
        ApprovalFailureV1::Transport => 0,
        ApprovalFailureV1::RateLimited => 1,
        ApprovalFailureV1::Server => 2,
        ApprovalFailureV1::Forbidden => 3,
        ApprovalFailureV1::NotFound => 4,
        ApprovalFailureV1::Stale => 5,
        ApprovalFailureV1::Conflict => 6,
        ApprovalFailureV1::Expired => 7,
        ApprovalFailureV1::Invalid => 8,
        ApprovalFailureV1::NotReady => 9,
        ApprovalFailureV1::Signature => 10,
        ApprovalFailureV1::Mismatch => 11,
    }
}

fn failure_kind_from_code(code: u64) -> Option<ApprovalFailureV1> {
    Some(match code {
        0 => ApprovalFailureV1::Transport,
        1 => ApprovalFailureV1::RateLimited,
        2 => ApprovalFailureV1::Server,
        3 => ApprovalFailureV1::Forbidden,
        4 => ApprovalFailureV1::NotFound,
        5 => ApprovalFailureV1::Stale,
        6 => ApprovalFailureV1::Conflict,
        7 => ApprovalFailureV1::Expired,
        8 => ApprovalFailureV1::Invalid,
        9 => ApprovalFailureV1::NotReady,
        10 => ApprovalFailureV1::Signature,
        11 => ApprovalFailureV1::Mismatch,
        _ => return None,
    })
}

// ---------------------------------------------------------------- 失败分类

fn classify_sync_error(error: &SyncError) -> ApprovalFailureV1 {
    match error {
        SyncError::RelayApi { code, .. } => match code.as_str() {
            "CAS_CONFLICT" => ApprovalFailureV1::Stale,
            "EXPIRED" | "LEASE_EXPIRED" => ApprovalFailureV1::Expired,
            "RATE_LIMITED" => ApprovalFailureV1::RateLimited,
            "NOT_FOUND" => ApprovalFailureV1::NotFound,
            "INVALID_STATE" | "ID_CONFLICT" | "INVITE_USED" | "CHALLENGE_USED" => {
                ApprovalFailureV1::Conflict
            }
            "INVALID_REQUEST" | "REQUEST_TOO_LARGE" | "SIGNATURE_INVALID" => {
                ApprovalFailureV1::Invalid
            }
            "NOT_IMPLEMENTED" => ApprovalFailureV1::Forbidden,
            "FORBIDDEN" | "REVOKED" | "MANAGEMENT_REQUIRED" | "UNAUTHORIZED" => {
                ApprovalFailureV1::Forbidden
            }
            other => {
                if other.starts_with("HTTP_5") {
                    ApprovalFailureV1::Server
                } else {
                    ApprovalFailureV1::Invalid
                }
            }
        },
        SyncError::HttpStatus(status) => match *status {
            400 => ApprovalFailureV1::Invalid,
            403 => ApprovalFailureV1::Forbidden,
            404 => ApprovalFailureV1::NotFound,
            409 => ApprovalFailureV1::Conflict,
            410 => ApprovalFailureV1::Expired,
            429 => ApprovalFailureV1::RateLimited,
            status if status >= 500 => ApprovalFailureV1::Server,
            _ => ApprovalFailureV1::Invalid,
        },
        SyncError::Transport(_) | SyncError::Io(_) | SyncError::Tls(_) => {
            ApprovalFailureV1::Transport
        }
        SyncError::Signature => ApprovalFailureV1::Signature,
        SyncError::Proto(_) => ApprovalFailureV1::Invalid,
        _ => ApprovalFailureV1::Mismatch,
    }
}

// ---------------------------------------------------------------- 构造辅助

/// `build_operation_parts` 的返回值（kind/payload/签名槽/cap 证据）。
struct OperationParts {
    kind: ControlKindV2,
    payload: Vec<u8>,
    signers: Vec<SignerV2>,
    caps: Vec<ManagementCapV2>,
}

fn compare_signers(left: &SignerV2, right: &SignerV2) -> Ordering {
    left.role
        .as_u8()
        .cmp(&right.role.as_u8())
        .then_with(|| left.device_id.cmp(&right.device_id))
        .then_with(|| left.cap_id.cmp(&right.cap_id))
        .then_with(|| left.pk.cmp(&right.pk))
}

fn build_operation_parts(
    replay: &V2ControlReplay,
    initiator: DeviceId,
    request: &ApprovalRequestV1,
    now: u64,
) -> Result<OperationParts, SyncError> {
    match request {
        ApprovalRequestV1::UpgradePolicy(request) => build_upgrade(replay, initiator, request),
        ApprovalRequestV1::CapGrant(request) => build_cap_grant(replay, initiator, request, now),
        ApprovalRequestV1::CapRevoke(request) => build_cap_revoke(replay, initiator, request, now),
        ApprovalRequestV1::DeviceApprove(request) => {
            build_device_approve(replay, initiator, request, now)
        }
        ApprovalRequestV1::DeviceRevoke(request) => {
            build_device_revoke(replay, initiator, request, now)
        }
        ApprovalRequestV1::DeviceRightsChange(request) => {
            build_device_rights_change(replay, initiator, request, now)
        }
        ApprovalRequestV1::InviteCreate(request) => {
            build_invite_create(replay, initiator, request, now)
        }
        ApprovalRequestV1::InviteCancel(request) => {
            build_invite_cancel(replay, initiator, request, now)
        }
        ApprovalRequestV1::Rotation(kind) => Err(SyncError::Unsupported(match kind {
            ControlKindV2::RotationBegin => "RotationBegin（kind 6）本切片不构造（S4d2 之后）",
            ControlKindV2::RotationAbort => "RotationAbort（kind 7）本切片不构造（S4d2 之后）",
            ControlKindV2::RotationRestart => "RotationRestart（kind 8）本切片不构造",
            ControlKindV2::RotationAddRevokes => "RotationAddRevokes（kind 9）本切片不构造",
            ControlKindV2::EpochActivate => "EpochActivate（kind 10）本切片不构造",
            _ => "非 rotation kind 传入 Rotation 请求",
        })),
        ApprovalRequestV1::Recovery(kind) => Err(SyncError::Unsupported(match kind {
            ControlKindV2::RecoveryReset => "RecoveryReset（kind 13）本切片不构造（后续切片）",
            ControlKindV2::RecoveryKeyChange => {
                "RecoveryKeyChange（kind 14）本切片不构造（后续切片）"
            }
            _ => "非 recovery kind 传入 Recovery 请求",
        })),
        ApprovalRequestV1::PolicyReplace(_) => Err(SyncError::Unsupported(
            "PolicyReplace（kind 1）relay 仍 501：本切片不构造",
        )),
    }
}

fn require_v2(state: &ControlStateV2) -> Result<(), SyncError> {
    if state.authz_state().is_none() {
        return Err(approval_error(
            "vault 尚未启用 V2 授权（没有已回放的 UpgradePolicy）",
        ));
    }
    Ok(())
}

fn active_device_pk(state: &ControlStateV2, device_id: &DeviceId) -> Result<[u8; 32], SyncError> {
    let Some(device) = state.device(device_id) else {
        return Err(approval_error(
            "设备不在已验证控制链的成员集合里（未批准或链不完整）",
        ));
    };
    match device.status {
        DeviceStatusV2::Active => Ok(device.cert.sign_ed25519_pub),
        DeviceStatusV2::Revoked => Err(approval_error("设备已被撤销（永不复活）")),
    }
}

fn effective_cap(
    state: &ControlStateV2,
    device_id: &DeviceId,
    device_pk: &[u8; 32],
    required: CapScope,
    now: u64,
) -> Result<ManagementCapV2, SyncError> {
    state
        .select_cap(device_id, device_pk, required, now)
        .map(|entry| entry.cap.clone())
}

/// 两台 ACTIVE 设备 + 各自一张覆盖 `required` 的 cap（不拼 cap/不拼人）。
fn dual_signers(
    state: &ControlStateV2,
    initiator: DeviceId,
    counterparty: DeviceId,
    required: CapScope,
    now: u64,
) -> Result<(SignerV2, SignerV2, Vec<ManagementCapV2>), SyncError> {
    if initiator == counterparty {
        return Err(approval_error(
            "两签必须来自两台不同设备（同设备/同钥不能凑门限）",
        ));
    }
    let initiator_pk = active_device_pk(state, &initiator)?;
    let counterparty_pk = active_device_pk(state, &counterparty)?;
    let cap_a = effective_cap(state, &initiator, &initiator_pk, required, now)?;
    let cap_b = effective_cap(state, &counterparty, &counterparty_pk, required, now)?;
    let signer_a = SignerV2 {
        role: SignerRole::Device,
        device_id: Some(initiator),
        cap_id: Some(cap_a.cap_id),
        pk: initiator_pk,
    };
    let signer_b = SignerV2 {
        role: SignerRole::Device,
        device_id: Some(counterparty),
        cap_id: Some(cap_b.cap_id),
        pk: counterparty_pk,
    };
    Ok((signer_a, signer_b, vec![cap_a, cap_b]))
}

fn build_upgrade(
    replay: &V2ControlReplay,
    initiator: DeviceId,
    request: &UpgradePolicyRequestV1,
) -> Result<OperationParts, SyncError> {
    let state = replay.state();
    if state.authz_state().is_some() {
        return Err(approval_error(
            "vault 已启用 V2：UpgradePolicy 只能作为首条 V2 控制记录",
        ));
    }
    let Some(manager_pub) = replay.manager_pub() else {
        return Err(SyncError::NeedsTrust(
            "缺少 pin 住的管理公钥：无法构造旧 creator 升级记录",
        ));
    };
    let mut signers = Vec::new();
    let mut devices = Vec::new();
    let mut pks = Vec::new();
    let mut seen_initiator = false;
    for (device_id, pk) in &request.confirmations {
        if devices.contains(device_id) || pks.contains(pk) {
            return Err(approval_error(
                "UpgradePolicy 的确认设备/公钥重复（不能同设备凑确认）",
            ));
        }
        devices.push(*device_id);
        pks.push(*pk);
        if *device_id == initiator {
            seen_initiator = true;
        }
        signers.push(SignerV2 {
            role: SignerRole::Device,
            device_id: Some(*device_id),
            cap_id: None,
            pk: *pk,
        });
    }
    if !seen_initiator {
        return Err(approval_error(
            "发起者必须在 UpgradePolicy 的 ACTIVE 确认里",
        ));
    }
    if signers.len() < 2 {
        return Err(approval_error(
            "UpgradePolicy 需要至少两台现有 ACTIVE 设备确认",
        ));
    }
    signers.push(SignerV2 {
        role: SignerRole::CreatorUpgrade,
        device_id: None,
        cap_id: None,
        pk: manager_pub,
    });
    signers.push(SignerV2 {
        role: SignerRole::OfflineRecovery,
        device_id: None,
        cap_id: None,
        pk: request.fresh_recovery_pk,
    });
    let manifest_sorted = request
        .active_manifest
        .windows(2)
        .all(|pair| pair[0].device_id < pair[1].device_id);
    if request.active_manifest.len() < 2 || !manifest_sorted {
        return Err(approval_error(
            "UpgradePolicy 的 ACTIVE manifest 至少两台且必须按 device_id 严格升序",
        ));
    }
    let payload = UpgradePolicyPayloadV2 {
        old_manager_rev: request.old_manager_rev,
        policy: request.policy.clone(),
        fresh_recovery_pk: request.fresh_recovery_pk,
        bootstrap_caps: request.bootstrap_caps.clone(),
        active_manifest: request.active_manifest.clone(),
    }
    .encode()?;
    Ok(OperationParts {
        kind: ControlKindV2::UpgradePolicy,
        payload,
        signers,
        caps: Vec::new(),
    })
}

fn build_cap_grant(
    replay: &V2ControlReplay,
    initiator: DeviceId,
    request: &CapGrantRequestV1,
    now: u64,
) -> Result<OperationParts, SyncError> {
    let state = replay.state();
    require_v2(state)?;
    let required = CapScope::new(CapScope::CAP_ADMIN)?;
    let (signer_a, signer_b, caps) =
        dual_signers(state, initiator, request.counterparty, required, now)?;
    if state.cap(&request.cap_id).is_some() {
        return Err(approval_error("cap_id 已出现过（cap ID 永不复用）"));
    }
    let subject_pk = active_device_pk(state, &request.subject_device_id)?;
    let scopes = CapScope::new(request.scopes)?;
    let cap = ManagementCapV2 {
        version: V2_WIRE_VERSION,
        instance_id: state.instance_id(),
        vault_id: state.vault_id(),
        cap_id: request.cap_id,
        subject_device_id: request.subject_device_id,
        subject_ed_pk: subject_pk,
        scopes,
        not_before: request.not_before,
        expires_at: request.expires_at,
        issued_control_rev: state.rev().saturating_add(1),
        issued_authz_rev: state.authz_rev().saturating_add(1),
        issuer_authority_rev: state.authority_rev(),
        delegable: false,
    };
    let payload = CapGrantPayloadV2 { cap: cap.clone() }.encode()?;
    Ok(OperationParts {
        kind: ControlKindV2::CapGrant,
        payload,
        signers: vec![signer_a, signer_b],
        caps,
    })
}

fn build_cap_revoke(
    replay: &V2ControlReplay,
    initiator: DeviceId,
    request: &CapRevokeRequestV1,
    now: u64,
) -> Result<OperationParts, SyncError> {
    let state = replay.state();
    require_v2(state)?;
    let Some(entry) = state.cap(&request.cap_id) else {
        return Err(approval_error("撤销目标 cap 不在已验证控制链上"));
    };
    if entry.is_revoked() {
        return Err(approval_error(
            "cap 已被撤销：重复撤销必须换 request_id 重签",
        ));
    }
    let required = CapScope::new(CapScope::CAP_ADMIN)?;
    let (signer_a, signer_b, caps) =
        dual_signers(state, initiator, request.counterparty, required, now)?;
    let payload = CapRevokePayloadV2 {
        cap_id: request.cap_id,
    }
    .encode()?;
    Ok(OperationParts {
        kind: ControlKindV2::CapRevoke,
        payload,
        signers: vec![signer_a, signer_b],
        caps,
    })
}

fn build_device_approve(
    replay: &V2ControlReplay,
    initiator: DeviceId,
    request: &DeviceApproveRequestV1,
    now: u64,
) -> Result<OperationParts, SyncError> {
    let state = replay.state();
    require_v2(state)?;
    let cert = &request.cert;
    if cert.instance_id != state.instance_id() || cert.vault_id != state.vault_id() {
        return Err(approval_error("批准证书的 instance/vault 与本地不符"));
    }
    match state.device(&cert.device_id).map(|entry| entry.status) {
        None => {}
        Some(DeviceStatusV2::Active) => {
            return Err(approval_error(
                "目标设备已 ACTIVE：重复批准请重放原包，改权必须走 kind 16",
            ))
        }
        Some(DeviceStatusV2::Revoked) => return Err(approval_error("目标设备已撤销：永不复活")),
    }
    if cert.cert_serial != 1 {
        return Err(approval_error("首次批准必须 serial=1（serial 不复用）"));
    }
    if !cert.data_rights.can_read() {
        return Err(approval_error("批准的设备必须至少具备 READ 权限"));
    }
    if cert.issued_authz_rev != state.authz_rev().saturating_add(1) {
        return Err(approval_error(
            "批准证书的 issued_authz_rev 必须是提交后的 authz rev",
        ));
    }
    if request.grants.len() != 1 {
        return Err(approval_error(
            "首次批准必须恰好一份当前 epoch 的 grant 包（多 epoch 属轮换/激活）",
        ));
    }
    let required = CapScope::new(CapScope::APPROVE)?;
    let (signer_a, signer_b, caps) =
        dual_signers(state, initiator, request.counterparty, required, now)?;
    let pop = SignerV2 {
        role: SignerRole::TargetPop,
        device_id: Some(cert.device_id),
        cap_id: None,
        pk: cert.sign_ed25519_pub,
    };
    let payload = DeviceCredentialPayloadV2 {
        cert: cert.clone(),
        grants: request.grants.clone(),
    }
    .encode()?;
    Ok(OperationParts {
        kind: ControlKindV2::DeviceApprove,
        payload,
        signers: vec![signer_a, signer_b, pop],
        caps,
    })
}

fn build_device_revoke(
    replay: &V2ControlReplay,
    initiator: DeviceId,
    request: &DeviceRevokeRequestV1,
    now: u64,
) -> Result<OperationParts, SyncError> {
    let state = replay.state();
    require_v2(state)?;
    let mut ids = request.device_ids.clone();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Err(approval_error("设备撤销的目标不能为空"));
    }
    for device_id in &ids {
        let Some(device) = state.device(device_id) else {
            return Err(approval_error(
                "撤销目标不在已验证成员集合（PENDING 兑换不是控制记录）",
            ));
        };
        if device.status != DeviceStatusV2::Active {
            // 已撤销属幂等目标；仍允许提交（relay 侧 changed=false）。
        }
    }
    let required = CapScope::new(CapScope::REVOKE)?;
    let (signer_a, signer_b, caps) =
        dual_signers(state, initiator, request.counterparty, required, now)?;
    let payload = DeviceRevokePayloadV2 { device_ids: ids }.encode()?;
    Ok(OperationParts {
        kind: ControlKindV2::DeviceRevoke,
        payload,
        signers: vec![signer_a, signer_b],
        caps,
    })
}

fn build_device_rights_change(
    replay: &V2ControlReplay,
    initiator: DeviceId,
    request: &DeviceRightsChangeRequestV1,
    now: u64,
) -> Result<OperationParts, SyncError> {
    let state = replay.state();
    require_v2(state)?;
    let cert = &request.cert;
    let Some(existing) = state.device(&cert.device_id) else {
        return Err(approval_error(
            "改权目标不在已验证成员集合（PENDING 必须走批准）",
        ));
    };
    if existing.status != DeviceStatusV2::Active {
        return Err(approval_error("改权目标不是 ACTIVE 设备"));
    }
    if cert.instance_id != state.instance_id() || cert.vault_id != state.vault_id() {
        return Err(approval_error("改权证书的 instance/vault 与本地不符"));
    }
    if cert.cert_serial != existing.cert.cert_serial.saturating_add(1) {
        return Err(approval_error(
            "改权证书 serial 必须是当前值 +1（历史不覆盖）",
        ));
    }
    if cert.sign_ed25519_pub != existing.cert.sign_ed25519_pub
        || cert.hpke_x25519_pub != existing.cert.hpke_x25519_pub
    {
        return Err(approval_error("改权不得更换设备公钥（换钥属轮换/激活）"));
    }
    if !cert.data_rights.can_read() {
        return Err(approval_error("改权后必须至少保留 READ 权限"));
    }
    if cert.data_rights == existing.cert.data_rights {
        return Err(approval_error("改权记录必须真的改变权限"));
    }
    if cert.issued_authz_rev != state.authz_rev().saturating_add(1) {
        return Err(approval_error(
            "改权证书的 issued_authz_rev 必须是提交后的 authz rev",
        ));
    }
    if request.grants.len() != 1 {
        return Err(approval_error("改权必须恰好一份当前 epoch 的 grant 包"));
    }
    let required = CapScope::new(CapScope::APPROVE)?;
    let (signer_a, signer_b, caps) =
        dual_signers(state, initiator, request.counterparty, required, now)?;
    let payload = DeviceCredentialPayloadV2 {
        cert: cert.clone(),
        grants: request.grants.clone(),
    }
    .encode()?;
    Ok(OperationParts {
        kind: ControlKindV2::DeviceRightsChange,
        payload,
        signers: vec![signer_a, signer_b],
        caps,
    })
}

fn build_invite_create(
    replay: &V2ControlReplay,
    initiator: DeviceId,
    request: &InviteCreateRequestV1,
    now: u64,
) -> Result<OperationParts, SyncError> {
    let state = replay.state();
    require_v2(state)?;
    let policy = state
        .policy()
        .ok_or_else(|| approval_error("邀请创建缺已验证 policy"))?;
    if request.expires_at <= now || request.expires_at.saturating_sub(now) > policy.max_invite_ttl {
        return Err(approval_error(
            "邀请 TTL 超出 policy 上限或已过期（expires_at 必须 > signed_at）",
        ));
    }
    if state
        .invites()
        .any(|invite| invite.invite_id == request.invite_id)
    {
        return Err(approval_error("邀请 ID 已出现过（永不复用）"));
    }
    if state
        .invites()
        .any(|invite| invite.token_hash == request.token_hash)
    {
        return Err(approval_error("邀请 token_hash 已出现过（永不复用）"));
    }
    let required = CapScope::new(CapScope::INVITE)?;
    let initiator_pk = active_device_pk(state, &initiator)?;
    let cap = effective_cap(state, &initiator, &initiator_pk, required, now)?;
    let signer = SignerV2 {
        role: SignerRole::Device,
        device_id: Some(initiator),
        cap_id: Some(cap.cap_id),
        pk: initiator_pk,
    };
    let payload = InviteCreatePayloadV2 {
        invite_id: request.invite_id,
        token_hash: request.token_hash,
        expires_at: request.expires_at,
    }
    .encode()?;
    Ok(OperationParts {
        kind: ControlKindV2::InviteCreate,
        payload,
        signers: vec![signer],
        caps: vec![cap],
    })
}

fn build_invite_cancel(
    replay: &V2ControlReplay,
    initiator: DeviceId,
    request: &InviteCancelRequestV1,
    now: u64,
) -> Result<OperationParts, SyncError> {
    let state = replay.state();
    require_v2(state)?;
    if !state
        .invites()
        .any(|invite| invite.invite_id == request.invite_id)
    {
        return Err(approval_error("取消目标邀请不在已验证控制链上"));
    }
    let required = CapScope::new(CapScope::INVITE)?;
    let initiator_pk = active_device_pk(state, &initiator)?;
    let cap = effective_cap(state, &initiator, &initiator_pk, required, now)?;
    let signer = SignerV2 {
        role: SignerRole::Device,
        device_id: Some(initiator),
        cap_id: Some(cap.cap_id),
        pk: initiator_pk,
    };
    let payload = InviteCancelPayloadV2 {
        invite_id: request.invite_id,
    }
    .encode()?;
    Ok(OperationParts {
        kind: ControlKindV2::InviteCancel,
        payload,
        signers: vec![signer],
        caps: vec![cap],
    })
}

// ---------------------------------------------------------------- 客户端便捷入口

impl<T: HttpTransport, C: Clock> V2ManagementClient<T, C> {
    /// 从 `.mn-approval` 文件导入并**按本地已验证链验证**（先 `refresh_control`）。
    pub fn import_approval_file(&mut self, path: &Path) -> Result<ApprovalDraftV1, SyncError> {
        self.refresh_control()?;
        let draft = ApprovalDraftV1::read_file(path)?;
        draft.verify_import(self.replay(), self.now_secs())?;
        Ok(draft)
    }

    /// 从复制文本导入并验证（base64url + 版本前缀）。
    pub fn import_approval_base64(&mut self, text: &str) -> Result<ApprovalDraftV1, SyncError> {
        self.refresh_control()?;
        let draft = ApprovalDraftV1::from_base64(text)?;
        draft.verify_import(self.replay(), self.now_secs())?;
        Ok(draft)
    }

    /// 从原始字节导入并验证。
    pub fn import_approval_bytes(&mut self, bytes: &[u8]) -> Result<ApprovalDraftV1, SyncError> {
        self.refresh_control()?;
        let draft = ApprovalDraftV1::from_bytes(bytes)?;
        draft.verify_import(self.replay(), self.now_secs())?;
        Ok(draft)
    }

    /// 提交一份满签草稿（便捷入口，等价于 [`ApprovalDraftV1::submit`]）。
    pub fn submit_approval(
        &mut self,
        draft: &mut ApprovalDraftV1,
    ) -> Result<ControlReceiptV2, ApprovalSubmitErrorV1> {
        draft.submit(self)
    }
}

// ---------------------------------------------------------------- 单元测试

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceSigningKey;
    use crate::keys::Secret32;
    use crate::test_support::TestRng;

    const NOW: u64 = 1_800_000_000;

    fn key(seed: u8) -> DeviceSigningKey {
        DeviceSigningKey::from_seed(Secret32::from_array([seed; 32]))
    }

    fn vault_id() -> crate::VaultId {
        [0x5au8; 16]
    }

    fn instance() -> [u8; 16] {
        [0x11u8; 16]
    }

    fn operation(kind: ControlKindV2, payload: Vec<u8>, signers: Vec<SignerV2>) -> OperationV2 {
        OperationV2 {
            version: V2_WIRE_VERSION,
            instance_id: instance(),
            vault_id: vault_id(),
            kind,
            request_id: [0xA1u8; 16],
            expected_control_rev: 7,
            prev_control_hash: [0xB1u8; 32],
            expected_authz_rev: 3,
            authority_rev: 1,
            signed_at: NOW,
            not_after: NOW + DEADLINE_MAX_SECS,
            canonical_path: canonical_submit_path(&vault_id()),
            payload,
            signers,
        }
    }

    fn device_signer(device_id: DeviceId, pk: [u8; 32], cap_id: [u8; 16]) -> SignerV2 {
        SignerV2 {
            role: SignerRole::Device,
            device_id: Some(device_id),
            cap_id: Some(cap_id),
            pk,
        }
    }

    fn cap(
        cap_id: [u8; 16],
        device_id: DeviceId,
        pk: [u8; 32],
        expires_at: u64,
    ) -> ManagementCapV2 {
        ManagementCapV2 {
            version: V2_WIRE_VERSION,
            instance_id: instance(),
            vault_id: vault_id(),
            cap_id,
            subject_device_id: device_id,
            subject_ed_pk: pk,
            scopes: CapScope::new(CapScope::ALL).expect("scopes"),
            not_before: NOW - 60,
            expires_at,
            issued_control_rev: 6,
            issued_authz_rev: 3,
            issuer_authority_rev: 1,
            delegable: false,
        }
    }

    fn control() -> ControlEvidenceV1 {
        ControlEvidenceV1 {
            control_rev: 7,
            head_hash: [0xB1u8; 32],
            authz_rev: 3,
            authority_rev: 1,
        }
    }

    struct Fixture {
        a: DeviceSigningKey,
        b: DeviceSigningKey,
        signers: Vec<SignerV2>,
        caps: Vec<ManagementCapV2>,
    }

    fn fixture() -> Fixture {
        let a = key(0x21);
        let b = key(0x22);
        let cap_a = cap([0xC1u8; 16], [0x01u8; 16], a.public_key(), NOW + 600);
        let cap_b = cap([0xC2u8; 16], [0x02u8; 16], b.public_key(), NOW + 600);
        let signers = vec![
            device_signer([0x01u8; 16], a.public_key(), cap_a.cap_id),
            device_signer([0x02u8; 16], b.public_key(), cap_b.cap_id),
        ];
        Fixture {
            a,
            b,
            signers,
            caps: vec![cap_a, cap_b],
        }
    }

    fn sample_draft(fixture: &Fixture) -> ApprovalDraftV1 {
        let payload = CapRevokePayloadV2 {
            cap_id: [0xC1u8; 16],
        }
        .encode()
        .expect("payload");
        let op = operation(ControlKindV2::CapRevoke, payload, fixture.signers.clone());
        let envelope =
            ApprovalEnvelopeV1::from_operation(&op, 0, Vec::new(), fixture.caps.clone(), control())
                .expect("envelope");
        ApprovalDraftV1::from_envelope(envelope, false)
    }

    fn sign_slot(draft: &ApprovalDraftV1, index: usize, key: &DeviceSigningKey) -> [u8; 64] {
        let signer = &draft.envelope.operation().signers[index];
        key.sign(&signing_input(draft.envelope.operation(), signer).expect("input"))
    }

    fn fresh_vault() -> (tempfile::TempDir, Vault) {
        let base = std::env::var("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/tmp"));
        std::fs::create_dir_all(&base).expect("target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("approval-v1-")
            .tempdir_in(&base)
            .expect("临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
        (dir, vault)
    }

    #[test]
    fn envelope_roundtrip_is_strict_and_canonical() {
        let fixture = fixture();
        let envelope = sample_draft(&fixture).envelope().clone();
        assert!(!envelope.is_fully_signed());
        let bytes = envelope.encode().expect("encode");
        let decoded = ApprovalEnvelopeV1::decode(&bytes).expect("decode");
        assert_eq!(decoded, envelope);
        assert_eq!(decoded.encode().expect("re-encode"), bytes);
        // base64 复制往返（版本前缀）。
        let text = decoded.encode_base64().expect("base64");
        assert!(text.starts_with(APPROVAL_BASE64_PREFIX_V1));
        assert_eq!(
            ApprovalEnvelopeV1::decode_base64(&text).expect("decode base64"),
            envelope
        );
        assert!(
            ApprovalEnvelopeV1::decode_base64(&text[APPROVAL_BASE64_PREFIX_V1.len()..]).is_err()
        );

        // A 先签 → B 加签 → 满签；wire 往返仍逐字节相同。
        let mut draft = sample_draft(&fixture);
        draft.freeze();
        assert_eq!(draft.state(), ApprovalStateV1::Frozen);
        draft.sign_with(&fixture.a).expect("A 签");
        assert_eq!(draft.state(), ApprovalStateV1::InitiatorSigned);
        draft.mark_handed_off();
        assert_eq!(draft.state(), ApprovalStateV1::AwaitingCosign);
        draft.sign_with(&fixture.b).expect("B 签");
        assert_eq!(draft.state(), ApprovalStateV1::Ready);
        let bytes = draft.to_bytes().expect("encode");
        let reloaded = ApprovalDraftV1::from_bytes(&bytes).expect("import");
        assert_eq!(reloaded.state(), ApprovalStateV1::Ready);
        assert!(reloaded.is_fully_signed());
        reloaded
            .envelope()
            .verify_present_signatures()
            .expect("两签有效");
        assert_eq!(reloaded.to_bytes().expect("re-encode"), bytes);
    }

    #[test]
    fn envelope_rejects_zero_missing_index_and_order_violations() {
        let fixture = fixture();
        let envelope = sample_draft(&fixture).envelope().clone();
        let op = envelope.operation();
        let good = sign_slot(&sample_draft(&fixture), 0, &fixture.a);
        // 全零签名 = 用零占位，必须拒绝。
        assert!(ApprovalEnvelopeV1::from_operation(
            op,
            0,
            vec![ApprovalSignatureV1 {
                signer_index: 0,
                signature: [0u8; 64]
            }],
            envelope.caps().to_vec(),
            control(),
        )
        .is_err());
        // 槽位越界。
        assert!(ApprovalEnvelopeV1::from_operation(
            op,
            0,
            vec![ApprovalSignatureV1 {
                signer_index: 2,
                signature: good
            }],
            envelope.caps().to_vec(),
            control(),
        )
        .is_err());
        // 重复/乱序索引。
        for entries in [
            vec![
                ApprovalSignatureV1 {
                    signer_index: 1,
                    signature: good,
                },
                ApprovalSignatureV1 {
                    signer_index: 1,
                    signature: good,
                },
            ],
            vec![
                ApprovalSignatureV1 {
                    signer_index: 1,
                    signature: good,
                },
                ApprovalSignatureV1 {
                    signer_index: 0,
                    signature: good,
                },
            ],
        ] {
            assert!(ApprovalEnvelopeV1::from_operation(
                op,
                0,
                entries,
                envelope.caps().to_vec(),
                control(),
            )
            .is_err());
        }
        // initiator_index 越界。
        assert!(ApprovalEnvelopeV1::from_operation(
            op,
            2,
            Vec::new(),
            envelope.caps().to_vec(),
            control(),
        )
        .is_err());
        // cap 证据缺口 / 多余 / 乱序。
        assert!(ApprovalEnvelopeV1::from_operation(
            op,
            0,
            Vec::new(),
            vec![fixture.caps[0].clone()],
            control(),
        )
        .is_err());
        assert!(ApprovalEnvelopeV1::from_operation(
            op,
            0,
            Vec::new(),
            vec![
                fixture.caps[0].clone(),
                fixture.caps[1].clone(),
                cap(
                    [0xC3u8; 16],
                    [0x03u8; 16],
                    fixture.a.public_key(),
                    NOW + 600
                ),
            ],
            control(),
        )
        .is_err());
        assert!(ApprovalEnvelopeV1::from_operation(
            op,
            0,
            Vec::new(),
            vec![fixture.caps[1].clone(), fixture.caps[0].clone()],
            control(),
        )
        .is_err());
        // 控制链证据与操作不一致。
        let mut wrong = control();
        wrong.control_rev = 6;
        assert!(ApprovalEnvelopeV1::from_operation(
            op,
            0,
            Vec::new(),
            envelope.caps().to_vec(),
            wrong
        )
        .is_err());
        // 篡改 wire：h 不符 / 未知字段 / 版本。
        let bytes = envelope.encode().expect("encode");
        let mut value = decode_cde(&bytes).expect("decode cde");
        let CdeValue::Map(ref mut fields) = value else {
            panic!("map");
        };
        fields[2] = (2, CdeValue::Bytes(vec![0xAAu8; 32]));
        let tampered = encode_cde(&value).expect("encode cde");
        assert!(ApprovalEnvelopeV1::decode(&tampered).is_err());
        let mut value = decode_cde(&bytes).expect("decode cde");
        let CdeValue::Map(ref mut fields) = value else {
            panic!("map");
        };
        fields.push((7, CdeValue::UInt(1)));
        let extra = encode_cde(&value).expect("encode cde");
        assert!(ApprovalEnvelopeV1::decode(&extra).is_err());
        let mut value = decode_cde(&bytes).expect("decode cde");
        let CdeValue::Map(ref mut fields) = value else {
            panic!("map");
        };
        fields[0] = (0, CdeValue::UInt(2));
        let version = encode_cde(&value).expect("encode cde");
        assert!(ApprovalEnvelopeV1::decode(&version).is_err());
        // 导入限长。
        let oversized = vec![0u8; MAX_APPROVAL_ENVELOPE_BYTES_V1 + 1];
        assert!(ApprovalEnvelopeV1::decode(&oversized).is_err());
        assert!(
            ApprovalEnvelopeV1::decode_base64(&"A".repeat(MAX_APPROVAL_BASE64_CHARS_V1 + 1))
                .is_err()
        );
    }

    #[test]
    fn draft_signing_rejects_foreign_and_out_of_order_keys() {
        let fixture = fixture();
        let third = key(0x33);
        let mut draft = sample_draft(&fixture);
        // 第一签必须是发起者。
        let error = draft.sign_with(&fixture.b).expect_err("B 不能先签");
        assert!(error.to_string().contains("第一签"), "{error}");
        // 不在槽里的钥不能签。
        let error = draft.sign_with(&third).expect_err("陌生钥不能签");
        assert!(error.to_string().contains("签名槽"), "{error}");
        draft.sign_with(&fixture.a).expect("A 签");
        // 已签槽重复签：幂等（同钥同签）。
        draft.sign_with(&fixture.a).expect("A 重签幂等");
        draft.sign_with(&fixture.b).expect("B 签");
        // 满签后禁止再加签。
        let error = draft.sign_with(&fixture.a).expect_err("满签后禁止改签");
        assert!(error.to_string().contains("满签"), "{error}");
    }

    #[test]
    fn draft_persistence_roundtrip_preserves_state_and_receipt() {
        let (_dir, vault) = fresh_vault();
        let fixture = fixture();
        let mut draft = sample_draft(&fixture);
        draft.freeze();
        draft.sign_with(&fixture.a).expect("A 签");
        draft.save_draft(&vault).expect("保存草稿");
        let loaded = ApprovalDraftV1::load_draft(&vault)
            .expect("读取草稿")
            .expect("草稿存在");
        assert_eq!(loaded.state(), ApprovalStateV1::InitiatorSigned);
        assert_eq!(loaded.state(), draft.state());
        assert_eq!(
            loaded.to_bytes().expect("bytes"),
            draft.to_bytes().expect("bytes")
        );
        // 继续加签并保存 → 满签状态可恢复。
        draft.sign_with(&fixture.b).expect("B 签");
        draft.save_draft(&vault).expect("保存满签");
        let loaded = ApprovalDraftV1::load_draft(&vault)
            .expect("读取草稿")
            .expect("草稿存在");
        assert_eq!(loaded.state(), ApprovalStateV1::Ready);
        // 空草稿 = 没有草稿。
        ApprovalDraftV1::clear_draft(&vault).expect("清草稿");
        assert!(ApprovalDraftV1::load_draft(&vault).expect("读取").is_none());
        // 损坏草稿是明确错误，不当成"没有草稿"。
        vault
            .set_meta(META_APPROVAL_DRAFT_V1, b"not-a-draft")
            .expect("写坏草稿");
        assert!(ApprovalDraftV1::load_draft(&vault).is_err());
    }

    #[test]
    fn submit_failure_classification_matrix() {
        let cases: Vec<(SyncError, ApprovalFailureV1)> = vec![
            (
                SyncError::RelayApi {
                    code: "CAS_CONFLICT".to_owned(),
                    retryable: false,
                    current_epoch: None,
                    current_control_rev: Some(9),
                    bootstrap_hint: None,
                },
                ApprovalFailureV1::Stale,
            ),
            (
                SyncError::RelayApi {
                    code: "FORBIDDEN".to_owned(),
                    retryable: false,
                    current_epoch: None,
                    current_control_rev: None,
                    bootstrap_hint: None,
                },
                ApprovalFailureV1::Forbidden,
            ),
            (
                SyncError::RelayApi {
                    code: "NOT_FOUND".to_owned(),
                    retryable: false,
                    current_epoch: None,
                    current_control_rev: None,
                    bootstrap_hint: None,
                },
                ApprovalFailureV1::NotFound,
            ),
            (
                SyncError::RelayApi {
                    code: "INVALID_STATE".to_owned(),
                    retryable: false,
                    current_epoch: None,
                    current_control_rev: None,
                    bootstrap_hint: None,
                },
                ApprovalFailureV1::Conflict,
            ),
            (
                SyncError::RelayApi {
                    code: "EXPIRED".to_owned(),
                    retryable: false,
                    current_epoch: None,
                    current_control_rev: None,
                    bootstrap_hint: None,
                },
                ApprovalFailureV1::Expired,
            ),
            (
                SyncError::RelayApi {
                    code: "RATE_LIMITED".to_owned(),
                    retryable: true,
                    current_epoch: None,
                    current_control_rev: None,
                    bootstrap_hint: None,
                },
                ApprovalFailureV1::RateLimited,
            ),
            (
                SyncError::RelayApi {
                    code: "SIGNATURE_INVALID".to_owned(),
                    retryable: false,
                    current_epoch: None,
                    current_control_rev: None,
                    bootstrap_hint: None,
                },
                ApprovalFailureV1::Invalid,
            ),
            (SyncError::Transport("lost"), ApprovalFailureV1::Transport),
            (SyncError::HttpStatus(429), ApprovalFailureV1::RateLimited),
            (SyncError::HttpStatus(503), ApprovalFailureV1::Server),
            (SyncError::Signature, ApprovalFailureV1::Signature),
        ];
        for (error, expected) in cases {
            assert_eq!(classify_sync_error(&error), expected, "{error:?}");
            assert_eq!(
                classify_sync_error(&error).is_retryable(),
                expected.is_retryable()
            );
        }
        // 只有传输/限流/5xx 可"重发同一字节"。
        assert!(ApprovalFailureV1::Transport.is_retryable());
        assert!(ApprovalFailureV1::RateLimited.is_retryable());
        assert!(ApprovalFailureV1::Server.is_retryable());
        assert!(!ApprovalFailureV1::Stale.is_retryable());
        assert!(!ApprovalFailureV1::Expired.is_retryable());
    }

    #[test]
    fn unsupported_kinds_fail_fast() {
        let replay = V2ControlReplay::new(vault_id(), instance());
        let mut rng = TestRng::new(7);
        for kind in [
            ControlKindV2::RotationBegin,
            ControlKindV2::RotationAbort,
            ControlKindV2::RotationRestart,
            ControlKindV2::RotationAddRevokes,
            ControlKindV2::EpochActivate,
        ] {
            let error = ApprovalDraftV1::propose(
                &replay,
                [0x01u8; 16],
                &ApprovalRequestV1::Rotation(kind),
                &mut rng,
                NOW,
            )
            .expect_err("rotation 必须 Unsupported");
            assert!(matches!(error, SyncError::Unsupported(_)), "{error:?}");
        }
        for kind in [
            ControlKindV2::RecoveryReset,
            ControlKindV2::RecoveryKeyChange,
        ] {
            let error = ApprovalDraftV1::propose(
                &replay,
                [0x01u8; 16],
                &ApprovalRequestV1::Recovery(kind),
                &mut rng,
                NOW,
            )
            .expect_err("recovery 必须 Unsupported");
            assert!(matches!(error, SyncError::Unsupported(_)), "{error:?}");
        }
        let policy = PolicyV2 {
            version: V2_WIRE_VERSION,
            highrisk_threshold: PolicyV2::HIGHRISK_THRESHOLD,
            min_quorum: PolicyV2::MIN_QUORUM,
            max_cap_ttl: 86_400,
            max_invite_ttl: 3_600,
        };
        let error = ApprovalDraftV1::propose(
            &replay,
            [0x01u8; 16],
            &ApprovalRequestV1::PolicyReplace(policy),
            &mut rng,
            NOW,
        )
        .expect_err("kind 1 必须 Unsupported");
        assert!(matches!(error, SyncError::Unsupported(_)), "{error:?}");
    }
}
