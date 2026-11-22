//! ADR 0015：`ManagementCapV2` 与 V2 管理控制链的 wire 类型、严格 CDE 编解码与签名域。
//!
//! 本模块只做**纯字节布局与结构校验**：不计算 SHA-256、不验签、不碰 IO/数据库/时钟，
//! 与 v1 的 [`crate::records::management_signing_input`] 把 `body_digest` 交给调用方
//! 计算是同一口径（ADR 0003：sync-proto 是纯类型 crate，无 crypto 依赖）。
//!
//! 共享摘要 `h = SHA256(CDE(OperationV2))` 由调用方通过 [`Sha256Fn`] 注入：
//!
//! ```text
//! let h  = operation.digest(hash_fn)?;                 // = SHA256(CDE(OperationV2))
//! let in = control_signing_input(&operation, hash_fn)?; // = MN/v2/control\0 || h
//! ```
//!
//! # 固定对象（ADR 0015「固定对象」）
//!
//! | 对象 | 键 | 字段 |
//! | --- | --- | --- |
//! | `ManagementCapV2` | 0..12 | 2,I,V,C,subject_D,subject_ed_pk,scopes,not_before,expires_at,issued_control_rev,issued_authz_rev,issuer_authority_rev,false |
//! | `OperationV2` | 0..14 | 2,I,V,kind,R,expected_control_rev,prev_control_hash,expected_authz_rev,authority_rev,signed_at,not_after,2(POST),canonical_path,payload(CDE),signers[] |
//! | `PolicyV2` | 0..4 | 2,highrisk_threshold=2,min_quorum=2,max_cap_ttl,max_invite_ttl |
//! | `SignerV2` | 0..3 | role,device_id(0/16),cap_id(0/16),pk32 |
//! | `SignedControlV2` | 0..1 | bytes(OperationV2),array(sig64) |
//! | `DeviceCertV2` | 0..8 | 2,I,V,D,sign_pk,hpke_pk,serial,data_rights,issued_authz_rev |
//! | `GrantAadV2` | 0..8 | 2,I,V,epoch,recipient_D,recipient_hpke_pk,authority_rev,grant_id,issued_authz_rev |
//! | grant 包 `GrantPackageV2` | 0..2 | aad_bytes,enc,ciphertext |
//! | `ActivationV2` | 0..13 | 2,rotation_id,from_epoch,to_epoch,freeze_S,primary_CID,primary_commit_hash,fallback_CID,fallback_commit_hash,slot_set_hash,member_cert_hash,grants_hash,old_asset_hash,new_asset_hash |
//! | `DeviceCredentialPayloadV2` | 0..1 | cert_bytes, grant_packages[]（kind 4/16 共用一个 wire 形状）|
//! | `DeviceRevokePayloadV2` | 0..0 | sorted_device_ids[]（16B 数组，严格升序去重）|
//!
//! `I/V/D/C/R` 是 16B 的 instance/vault/device/cap/request ID；pk/hash 是 32B；
//! sig 是 64B；`u` 为 `0..=i64::MAX`（SQLite INTEGER 上界，计数/时间溢出即拒绝）。
//!
//! # 签名域（ADR 0015「签名域与提交」）
//!
//! 共同摘要 `h = SHA256(CDE(OperationV2))`，各域都已在 `constants` 中定义为
//! **真实 NUL 结尾**的字节串，与 v1 域互不复用：
//!
//! ```text
//! 设备签      MN/v2/control\0 || h
//! 升级根签    MN/v2/upgrade\0 || h
//! 恢复根签    MN/v2/recover\0 || h
//! PoP 签      MN/v2/pop\0     || h || pk32
//! 链 hash     SHA256(prev_hash || SignedControlV2_bytes)
//! 鉴权        MN/v2/auth\0    || challenge
//! ```
//!
//! # 严格 CDE
//!
//! 复用 [`crate::cde`] 的 canonical 编解码：整数最短形式、键升序、字段数精确、
//! 拒绝未知/重复键、null/undefined/tag/float/不定长、尾随字节、超深嵌套。
//! 每个记录再校验 ID/pk 非全零、长度、版本、枚举与取值范围；编解码是唯一的
//! 字节串（同一语义值只有一种 wire 形式）。

use std::cmp::Ordering;

use crate::cde::{
    decode, encode, expect_array, expect_bool, expect_bytes, expect_bytes_n, expect_i64_max,
    expect_u16, expect_u32, expect_u8, into_record_fields, CdeValue,
};
use crate::constants::{
    SIGN_AUTH_V2_DOMAIN, SIGN_V2_CONTROL_DOMAIN, SIGN_V2_POP_DOMAIN, SIGN_V2_RECOVER_DOMAIN,
    SIGN_V2_UPGRADE_DOMAIN,
};
use crate::error::ProtoError;
use crate::limits::{
    MAX_BOOTSTRAP_CAPS_V2, MAX_CANONICAL_PATH_BYTES_V2, MAX_CAP_TTL_SECS_V2, MAX_DEVICES,
    MAX_GRANT_PACKAGES_V2, MAX_INVITE_TTL_SECS_V2, MAX_OPERATION_SIGNERS_V2,
};
use crate::records::{
    DeviceId, Ed25519PublicKey, RelayInstanceId, Rights, Sha256, VaultId, X25519PublicKey,
};

/// 所有 V2 对象字段 0 的版本号。
pub const V2_WIRE_VERSION: u16 = 2;
/// `ManagementCapV2` 字段个数（键恰好 `0..13`）。
pub const MANAGEMENT_CAP_V2_FIELD_COUNT: usize = 13;
/// `OperationV2` 字段个数（键恰好 `0..15`）。
pub const OPERATION_V2_FIELD_COUNT: usize = 15;
/// `PolicyV2` 字段个数（键恰好 `0..5`）。
pub const POLICY_V2_FIELD_COUNT: usize = 5;
/// `SignerV2` 字段个数（键恰好 `0..4`）。
pub const SIGNER_V2_FIELD_COUNT: usize = 4;
/// `SignedControlV2` 字段个数（键恰好 `0..2`）。
pub const SIGNED_CONTROL_V2_FIELD_COUNT: usize = 2;
/// `DeviceCertV2` 字段个数（键恰好 `0..9`）。
pub const DEVICE_CERT_V2_FIELD_COUNT: usize = 9;
/// `GrantAadV2` 字段个数（键恰好 `0..9`）。
pub const GRANT_AAD_V2_FIELD_COUNT: usize = 9;
/// `GrantPackageV2` 字段个数（键恰好 `0..3`）。
pub const GRANT_PACKAGE_V2_FIELD_COUNT: usize = 3;
/// `ActivationV2` 字段个数（键恰好 `0..14`）。
pub const ACTIVATION_V2_FIELD_COUNT: usize = 14;
/// `UpgradePolicyPayloadV2` 字段个数（键恰好 `0..5`）。
pub const UPGRADE_POLICY_PAYLOAD_V2_FIELD_COUNT: usize = 5;
/// `CapGrantPayloadV2` 字段个数（键恰好 `0..1`）。
pub const CAP_GRANT_PAYLOAD_V2_FIELD_COUNT: usize = 1;
/// `CapRevokePayloadV2` 字段个数（键恰好 `0..1`）。
pub const CAP_REVOKE_PAYLOAD_V2_FIELD_COUNT: usize = 1;
/// `InviteCreatePayloadV2` 字段个数（键恰好 `0..3`）。
pub const INVITE_CREATE_PAYLOAD_V2_FIELD_COUNT: usize = 3;
/// `DeviceCredentialPayloadV2` 字段个数（键恰好 `0..2`）。
pub const DEVICE_CREDENTIAL_PAYLOAD_V2_FIELD_COUNT: usize = 2;
/// `DeviceRevokePayloadV2` 字段个数（键恰好 `0..1`）。
pub const DEVICE_REVOKE_PAYLOAD_V2_FIELD_COUNT: usize = 1;
/// `RotationBeginPayloadV2` 字段个数（键恰好 `0..4`）。
pub const ROTATION_BEGIN_PAYLOAD_V2_FIELD_COUNT: usize = 4;
/// `RotationAbortPayloadV2` 字段个数（键恰好 `0..1`）。
pub const ROTATION_ABORT_PAYLOAD_V2_FIELD_COUNT: usize = 1;
/// `RotationRestartPayloadV2` 字段个数（键恰好 `0..3`）。
pub const ROTATION_RESTART_PAYLOAD_V2_FIELD_COUNT: usize = 3;
/// `RotationAddRevokesPayloadV2` 字段个数（键恰好 `0..2`）。
pub const ROTATION_ADD_REVOKES_PAYLOAD_V2_FIELD_COUNT: usize = 2;
/// `EpochActivatePayloadV2` 字段个数（键恰好 `0..1`）。
pub const EPOCH_ACTIVATE_PAYLOAD_V2_FIELD_COUNT: usize = 1;
/// `InviteCancelPayloadV2` 字段个数（键恰好 `0..1`）。
pub const INVITE_CANCEL_PAYLOAD_V2_FIELD_COUNT: usize = 1;
/// `RecoveryResetPayloadV2` 字段个数（键恰好 `0..5`）。
pub const RECOVERY_RESET_PAYLOAD_V2_FIELD_COUNT: usize = 5;
/// `RecoveryKeyChangePayloadV2` 字段个数（键恰好 `0..2`）。
pub const RECOVERY_KEY_CHANGE_PAYLOAD_V2_FIELD_COUNT: usize = 2;
/// `ChallengeV2` 字段个数（键恰好 `0..13`）。
pub const CHALLENGE_V2_FIELD_COUNT: usize = 13;

/// cap ID（16B）。
pub type CapId = [u8; 16];
/// 请求 ID（16B，提交幂等键 `(vault_id, request_id)`）。
pub type RequestIdV2 = [u8; 16];
/// grant ID（16B）。
pub type GrantIdV2 = [u8; 16];
/// 轮换 ID（16B）。
pub type RotationIdV2 = [u8; 16];

/// 调用方注入的 SHA-256 实现（sync-proto 不依赖 crypto）。
///
/// 测试用独立实现，sync/relay 用 `sha2`：`|bytes| Sha256::digest(bytes).into()`。
pub type Sha256Fn = fn(&[u8]) -> Sha256;

// —— 内部校验小工具 ——

fn check_version(version: u16) -> Result<(), ProtoError> {
    if version != V2_WIRE_VERSION {
        return Err(ProtoError::UnsupportedVersion(version));
    }
    Ok(())
}

fn check_i64_max(value: u64, name: &str) -> Result<(), ProtoError> {
    if value > i64::MAX as u64 {
        return Err(ProtoError::Invalid(format!("{name} 超过 i64::MAX")));
    }
    Ok(())
}

fn check_nonzero(name: &str, bytes: &[u8]) -> Result<(), ProtoError> {
    if bytes.iter().all(|byte| *byte == 0) {
        return Err(ProtoError::Invalid(format!("{name} 不能全零")));
    }
    Ok(())
}

fn expect_optional_id16(value: CdeValue, field: &str) -> Result<Option<[u8; 16]>, ProtoError> {
    let CdeValue::Bytes(bytes) = value else {
        return Err(ProtoError::Invalid(format!(
            "{field} 必须是 byte string（0 或 16B）"
        )));
    };
    match bytes.len() {
        0 => Ok(None),
        16 => Ok(Some(bytes.try_into().expect("长度16"))),
        other => Err(ProtoError::Invalid(format!(
            "{field} 长度必须是 0 或 16，实际 {other}"
        ))),
    }
}

fn encode_optional_id16(value: Option<[u8; 16]>) -> CdeValue {
    CdeValue::Bytes(value.map(|id| id.to_vec()).unwrap_or_default())
}

fn compare_optional_id(left: Option<&[u8; 16]>, right: Option<&[u8; 16]>) -> Ordering {
    let left = left.map_or(&[][..], |id| &id[..]);
    let right = right.map_or(&[][..], |id| &id[..]);
    left.cmp(right)
}

/// signer 的 canonical 升序键：`(role, device_id, cap_id, pk)`（空 ID 视为空字节串）。
fn compare_signers(left: &SignerV2, right: &SignerV2) -> Ordering {
    left.role
        .as_u8()
        .cmp(&right.role.as_u8())
        .then_with(|| compare_optional_id(left.device_id.as_ref(), right.device_id.as_ref()))
        .then_with(|| compare_optional_id(left.cap_id.as_ref(), right.cap_id.as_ref()))
        .then_with(|| left.pk.cmp(&right.pk))
}

fn record_map(fields: Vec<(u64, CdeValue)>) -> CdeValue {
    CdeValue::Map(fields)
}

// —— scopes ——

/// 管理能力 scope 位（u32）。拒绝零与未知位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CapScope(u32);

impl CapScope {
    pub const INVITE: u32 = 1;
    pub const APPROVE: u32 = 2;
    pub const REVOKE: u32 = 4;
    pub const CAP_ADMIN: u32 = 8;
    pub const ROTATE: u32 = 16;
    pub const POLICY: u32 = 32;
    /// 全部已知位；任何超出此掩码的位都是未知位。
    pub const ALL: u32 =
        Self::INVITE | Self::APPROVE | Self::REVOKE | Self::CAP_ADMIN | Self::ROTATE | Self::POLICY;

    /// 只接受非零且仅含已知位的组合。
    pub fn new(bits: u32) -> Result<Self, ProtoError> {
        if bits == 0 || bits & !Self::ALL != 0 {
            return Err(ProtoError::Invalid(format!("非法 cap scope 位 {bits:#x}")));
        }
        Ok(Self(bits))
    }

    #[must_use]
    pub fn bits(self) -> u32 {
        self.0
    }

    /// `self` 是否覆盖 `other` 的全部位（门限校验用）。
    #[must_use]
    pub fn contains(self, other: Self) -> bool {
        other.is_subset_of(self)
    }

    /// `self` 是否是 `other` 的子集（“签发 scope 不得超过签发者有效 scope”）。
    #[must_use]
    pub fn is_subset_of(self, other: Self) -> bool {
        self.0 & !other.0 == 0
    }
}

// —— 角色与 kind ——

/// `SignerV2.role`（0..3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum SignerRole {
    /// 0：ACTIVE 设备用自己的设备钥签名（可带签发它的 cap）。
    Device = 0,
    /// 1：旧 creator 管理根对 Upgrade 记录签名（无 device/cap）。
    CreatorUpgrade = 1,
    /// 2：离线恢复根签名（无 device/cap）。
    OfflineRecovery = 2,
    /// 3：目标设备 PoP（带目标 device，不带 cap）。
    TargetPop = 3,
}

impl SignerRole {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

impl TryFrom<u8> for SignerRole {
    type Error = ProtoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Device),
            1 => Ok(Self::CreatorUpgrade),
            2 => Ok(Self::OfflineRecovery),
            3 => Ok(Self::TargetPop),
            other => Err(ProtoError::Invalid(format!("未知 signer role {other}"))),
        }
    }
}

impl From<SignerRole> for u8 {
    fn from(value: SignerRole) -> Self {
        value.as_u8()
    }
}

/// 控制记录 kind（ADR 0015「控制记录 kind」）。
///
/// 0..=14 连续，**15 未分配**（必须拒绝，未知 kind fail-stop），16 = DeviceRightsChange。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum ControlKindV2 {
    UpgradePolicy = 0,
    PolicyReplace = 1,
    CapGrant = 2,
    CapRevoke = 3,
    DeviceApprove = 4,
    DeviceRevoke = 5,
    RotationBegin = 6,
    RotationAbort = 7,
    RotationRestart = 8,
    RotationAddRevokes = 9,
    EpochActivate = 10,
    InviteCreate = 11,
    InviteCancel = 12,
    RecoveryReset = 13,
    RecoveryKeyChange = 14,
    DeviceRightsChange = 16,
}

impl ControlKindV2 {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

impl TryFrom<u8> for ControlKindV2 {
    type Error = ProtoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::UpgradePolicy),
            1 => Ok(Self::PolicyReplace),
            2 => Ok(Self::CapGrant),
            3 => Ok(Self::CapRevoke),
            4 => Ok(Self::DeviceApprove),
            5 => Ok(Self::DeviceRevoke),
            6 => Ok(Self::RotationBegin),
            7 => Ok(Self::RotationAbort),
            8 => Ok(Self::RotationRestart),
            9 => Ok(Self::RotationAddRevokes),
            10 => Ok(Self::EpochActivate),
            11 => Ok(Self::InviteCreate),
            12 => Ok(Self::InviteCancel),
            13 => Ok(Self::RecoveryReset),
            14 => Ok(Self::RecoveryKeyChange),
            16 => Ok(Self::DeviceRightsChange),
            other => Err(ProtoError::Invalid(format!(
                "未知或未分配的 V2 控制 kind {other}"
            ))),
        }
    }
}

impl From<ControlKindV2> for u8 {
    fn from(value: ControlKindV2) -> Self {
        value.as_u8()
    }
}

// —— signer ——

/// `SignerV2`：`{0:role, 1:device_id(0/16), 2:cap_id(0/16), 3:pk32}`。
///
/// 字段长度按 ADR 允许 0/16；角色决定必填性（ADR 只写 0/16，这里是结构化解释）：
/// 设备/PoP 必须带 `device_id`，旧 creator 升级与离线恢复不带；只有设备签名可带
/// `cap_id`（cap 只用于授权设备自己的签名钥）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerV2 {
    pub role: SignerRole,
    pub device_id: Option<DeviceId>,
    pub cap_id: Option<CapId>,
    pub pk: Ed25519PublicKey,
}

impl SignerV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.role.as_u8()))),
            (1, encode_optional_id16(self.device_id)),
            (2, encode_optional_id16(self.cap_id)),
            (3, CdeValue::Bytes(self.pk.to_vec())),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, SIGNER_V2_FIELD_COUNT)?;
        let role = SignerRole::try_from(expect_u8(fields[0].clone(), "SignerV2.role")?)?;
        let signer = Self {
            role,
            device_id: expect_optional_id16(fields[1].clone(), "SignerV2.device_id")?,
            cap_id: expect_optional_id16(fields[2].clone(), "SignerV2.cap_id")?,
            pk: expect_bytes_n::<32>(fields[3].clone(), "SignerV2.pk")?,
        };
        signer.validate()?;
        Ok(signer)
    }

    /// 角色/ID 长度/ID 与 pk 非全零。
    pub fn validate(&self) -> Result<(), ProtoError> {
        match self.role {
            SignerRole::Device => {
                if self.device_id.is_none() {
                    return Err(ProtoError::Invalid(
                        "SignerV2: 设备签名必须带 device_id".to_owned(),
                    ));
                }
            }
            SignerRole::TargetPop => {
                if self.device_id.is_none() {
                    return Err(ProtoError::Invalid(
                        "SignerV2: PoP 签名必须带目标 device_id".to_owned(),
                    ));
                }
                if self.cap_id.is_some() {
                    return Err(ProtoError::Invalid(
                        "SignerV2: PoP 签名不能带 cap_id".to_owned(),
                    ));
                }
            }
            SignerRole::CreatorUpgrade | SignerRole::OfflineRecovery => {
                if self.device_id.is_some() {
                    return Err(ProtoError::Invalid(
                        "SignerV2: 根签名不能带 device_id".to_owned(),
                    ));
                }
                if self.cap_id.is_some() {
                    return Err(ProtoError::Invalid(
                        "SignerV2: 根签名不能带 cap_id".to_owned(),
                    ));
                }
            }
        }
        if let Some(device_id) = self.device_id {
            check_nonzero("SignerV2.device_id", &device_id)?;
        }
        if let Some(cap_id) = self.cap_id {
            check_nonzero("SignerV2.cap_id", &cap_id)?;
        }
        check_nonzero("SignerV2.pk", &self.pk)?;
        Ok(())
    }
}

// —— ManagementCapV2 ——

/// `ManagementCapV2`（键 `0..13`）：公开授权事实，不含任何私钥/root。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementCapV2 {
    pub version: u16,
    pub instance_id: RelayInstanceId,
    pub vault_id: VaultId,
    pub cap_id: CapId,
    pub subject_device_id: DeviceId,
    pub subject_ed_pk: Ed25519PublicKey,
    pub scopes: CapScope,
    pub not_before: u64,
    pub expires_at: u64,
    /// 授予提交后的 control rev（ADR：新 cap 的 issued rev 必须等于提交后的值）。
    pub issued_control_rev: u64,
    /// 授予时的 authz rev（仅历史证据，不要求等于当前 rev）。
    pub issued_authz_rev: u64,
    /// 签发时的离线根代（必须等于当前 `authority_rev`）。
    pub issuer_authority_rev: u64,
    /// ADR 0015 固定为 false：cap 不可自行转授。
    pub delegable: bool,
}

impl ManagementCapV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.version))),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.vault_id.to_vec())),
            (3, CdeValue::Bytes(self.cap_id.to_vec())),
            (4, CdeValue::Bytes(self.subject_device_id.to_vec())),
            (5, CdeValue::Bytes(self.subject_ed_pk.to_vec())),
            (6, CdeValue::UInt(u64::from(self.scopes.bits()))),
            (7, CdeValue::UInt(self.not_before)),
            (8, CdeValue::UInt(self.expires_at)),
            (9, CdeValue::UInt(self.issued_control_rev)),
            (10, CdeValue::UInt(self.issued_authz_rev)),
            (11, CdeValue::UInt(self.issuer_authority_rev)),
            (12, CdeValue::Bool(self.delegable)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, MANAGEMENT_CAP_V2_FIELD_COUNT)?;
        let cap = Self {
            version: expect_u16(fields[0].clone(), "ManagementCapV2.version")?,
            instance_id: expect_bytes_n::<16>(fields[1].clone(), "ManagementCapV2.instance_id")?,
            vault_id: expect_bytes_n::<16>(fields[2].clone(), "ManagementCapV2.vault_id")?,
            cap_id: expect_bytes_n::<16>(fields[3].clone(), "ManagementCapV2.cap_id")?,
            subject_device_id: expect_bytes_n::<16>(
                fields[4].clone(),
                "ManagementCapV2.subject_device_id",
            )?,
            subject_ed_pk: expect_bytes_n::<32>(
                fields[5].clone(),
                "ManagementCapV2.subject_ed_pk",
            )?,
            scopes: CapScope::new(expect_u32(fields[6].clone(), "ManagementCapV2.scopes")?)?,
            not_before: expect_i64_max(fields[7].clone(), "ManagementCapV2.not_before")?,
            expires_at: expect_i64_max(fields[8].clone(), "ManagementCapV2.expires_at")?,
            issued_control_rev: expect_i64_max(
                fields[9].clone(),
                "ManagementCapV2.issued_control_rev",
            )?,
            issued_authz_rev: expect_i64_max(
                fields[10].clone(),
                "ManagementCapV2.issued_authz_rev",
            )?,
            issuer_authority_rev: expect_i64_max(
                fields[11].clone(),
                "ManagementCapV2.issuer_authority_rev",
            )?,
            delegable: expect_bool(fields[12].clone(), "ManagementCapV2.delegable")?,
        };
        cap.validate()?;
        Ok(cap)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_version(self.version)?;
        check_nonzero("ManagementCapV2.instance_id", &self.instance_id)?;
        check_nonzero("ManagementCapV2.vault_id", &self.vault_id)?;
        check_nonzero("ManagementCapV2.cap_id", &self.cap_id)?;
        check_nonzero("ManagementCapV2.subject_device_id", &self.subject_device_id)?;
        check_nonzero("ManagementCapV2.subject_ed_pk", &self.subject_ed_pk)?;
        if self.delegable {
            return Err(ProtoError::Invalid(
                "ManagementCapV2.delegable 必须为 false".to_owned(),
            ));
        }
        if self.expires_at <= self.not_before {
            return Err(ProtoError::Invalid(format!(
                "ManagementCapV2.expires_at={} 必须大于 not_before={}",
                self.expires_at, self.not_before
            )));
        }
        for (name, value) in [
            ("ManagementCapV2.not_before", self.not_before),
            ("ManagementCapV2.expires_at", self.expires_at),
            (
                "ManagementCapV2.issued_control_rev",
                self.issued_control_rev,
            ),
            ("ManagementCapV2.issued_authz_rev", self.issued_authz_rev),
            (
                "ManagementCapV2.issuer_authority_rev",
                self.issuer_authority_rev,
            ),
        ] {
            check_i64_max(value, name)?;
        }
        // V2 只在 Upgrade（一次提交初始化 authority/authz rev=1）之后存在，
        // 因此 cap 的三个 rev 从 1 起；0 不可能是合法的“已签发”值。
        for (name, value) in [
            (
                "ManagementCapV2.issued_control_rev",
                self.issued_control_rev,
            ),
            ("ManagementCapV2.issued_authz_rev", self.issued_authz_rev),
            (
                "ManagementCapV2.issuer_authority_rev",
                self.issuer_authority_rev,
            ),
        ] {
            if value == 0 {
                return Err(ProtoError::Invalid(format!("{name} 必须 >0")));
            }
        }
        Ok(())
    }
}

// —— PolicyV2 ——

/// `PolicyV2`（键 `0..4`）：门限与 TTL 上限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyV2 {
    pub version: u16,
    /// 高危操作门限，ADR 固定 2。
    pub highrisk_threshold: u32,
    /// 最小 quorum，ADR 固定 2。
    pub min_quorum: u32,
    /// cap TTL 上限（秒），≤ 2_592_000。
    pub max_cap_ttl: u64,
    /// 邀请 TTL 上限（秒），≤ 86_400。
    pub max_invite_ttl: u64,
}

impl PolicyV2 {
    pub const HIGHRISK_THRESHOLD: u32 = 2;
    pub const MIN_QUORUM: u32 = 2;

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.version))),
            (1, CdeValue::UInt(u64::from(self.highrisk_threshold))),
            (2, CdeValue::UInt(u64::from(self.min_quorum))),
            (3, CdeValue::UInt(self.max_cap_ttl)),
            (4, CdeValue::UInt(self.max_invite_ttl)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, POLICY_V2_FIELD_COUNT)?;
        let policy = Self {
            version: expect_u16(fields[0].clone(), "PolicyV2.version")?,
            highrisk_threshold: expect_u32(fields[1].clone(), "PolicyV2.highrisk_threshold")?,
            min_quorum: expect_u32(fields[2].clone(), "PolicyV2.min_quorum")?,
            max_cap_ttl: expect_i64_max(fields[3].clone(), "PolicyV2.max_cap_ttl")?,
            max_invite_ttl: expect_i64_max(fields[4].clone(), "PolicyV2.max_invite_ttl")?,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_version(self.version)?;
        if self.highrisk_threshold != Self::HIGHRISK_THRESHOLD {
            return Err(ProtoError::Invalid(format!(
                "PolicyV2.highrisk_threshold 必须为 {}，实际 {}",
                Self::HIGHRISK_THRESHOLD,
                self.highrisk_threshold
            )));
        }
        if self.min_quorum != Self::MIN_QUORUM {
            return Err(ProtoError::Invalid(format!(
                "PolicyV2.min_quorum 必须为 {}，实际 {}",
                Self::MIN_QUORUM,
                self.min_quorum
            )));
        }
        check_i64_max(self.max_cap_ttl, "PolicyV2.max_cap_ttl")?;
        check_i64_max(self.max_invite_ttl, "PolicyV2.max_invite_ttl")?;
        if self.max_cap_ttl > MAX_CAP_TTL_SECS_V2 {
            return Err(ProtoError::Invalid(format!(
                "PolicyV2.max_cap_ttl={} 超过上限 {MAX_CAP_TTL_SECS_V2}",
                self.max_cap_ttl
            )));
        }
        if self.max_invite_ttl > MAX_INVITE_TTL_SECS_V2 {
            return Err(ProtoError::Invalid(format!(
                "PolicyV2.max_invite_ttl={} 超过上限 {MAX_INVITE_TTL_SECS_V2}",
                self.max_invite_ttl
            )));
        }
        Ok(())
    }
}

// —— 唯一提交路径 ——

/// `/v2/vaults/{lowerhex32}/control/submit`（ADR 0015 唯一提交路径）。
///
/// 禁止 query/转义别名；`OperationV2.canonical_path` 必须逐字节等于此值。
#[must_use]
pub fn canonical_submit_path(vault_id: &VaultId) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut path = String::with_capacity(11 + 32 + 15);
    path.push_str("/v2/vaults/");
    for byte in vault_id {
        path.push(HEX[(byte >> 4) as usize] as char);
        path.push(HEX[(byte & 0x0f) as usize] as char);
    }
    path.push_str("/control/submit");
    path
}

fn validate_canonical_path(path: &str, vault_id: &VaultId) -> Result<(), ProtoError> {
    if path.is_empty() || !path.starts_with('/') {
        return Err(ProtoError::Invalid(
            "OperationV2.canonical_path 必须是非空绝对路径".to_owned(),
        ));
    }
    if path.len() > MAX_CANONICAL_PATH_BYTES_V2 {
        return Err(ProtoError::TooLarge);
    }
    if !path.is_ascii() {
        return Err(ProtoError::Invalid(
            "OperationV2.canonical_path 只允许 ASCII".to_owned(),
        ));
    }
    if path.bytes().any(|byte| byte <= 0x20 || byte == 0x7f) {
        return Err(ProtoError::Invalid(
            "OperationV2.canonical_path 含空白/控制字符".to_owned(),
        ));
    }
    // query / fragment / 转义别名 / 反斜杠别名一律拒绝（ADR 0015）。
    if path
        .bytes()
        .any(|byte| matches!(byte, b'?' | b'#' | b'%' | b'\\'))
    {
        return Err(ProtoError::Invalid(
            "OperationV2.canonical_path 不允许 query/fragment/转义/反斜杠".to_owned(),
        ));
    }
    for segment in path.split('/').skip(1) {
        if segment.is_empty() {
            return Err(ProtoError::Invalid(
                "OperationV2.canonical_path 不允许空段".to_owned(),
            ));
        }
        if segment == "." || segment == ".." {
            return Err(ProtoError::Invalid(
                "OperationV2.canonical_path 不允许 . / .. 段".to_owned(),
            ));
        }
    }
    let expected = canonical_submit_path(vault_id);
    if path != expected {
        return Err(ProtoError::Invalid(format!(
            "OperationV2.canonical_path 必须是唯一提交路径 {expected}"
        )));
    }
    Ok(())
}

// —— OperationV2 ——

/// `OperationV2`（键 `0..15`）：一次控制提交的全部已签事实。
///
/// 键 11 的 method 在 wire 上固定为 2（POST），因此不作为结构体字段，
/// 由 [`OperationV2::to_cde`] 写常量、[`OperationV2::from_cde`] 严格校验。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationV2 {
    pub version: u16,
    pub instance_id: RelayInstanceId,
    pub vault_id: VaultId,
    pub kind: ControlKindV2,
    pub request_id: RequestIdV2,
    /// CAS 期望的提交后 control rev = 值 + 1。
    pub expected_control_rev: u64,
    /// 提交前链尾 hash；首条记录允许全零（genesis）。
    pub prev_control_hash: Sha256,
    pub expected_authz_rev: u64,
    /// 当前离线根代。
    pub authority_rev: u64,
    pub signed_at: u64,
    pub not_after: u64,
    pub canonical_path: String,
    /// kind 专属 payload 的 canonical CDE 字节（按 kind 由上层解码）。
    pub payload: Vec<u8>,
    /// 按 `(role, device_id, cap_id, pk)` 严格升序去重。
    pub signers: Vec<SignerV2>,
}

impl OperationV2 {
    /// `h = SHA256(CDE(OperationV2))`（ADR 0015 共同摘要）。
    pub fn digest(&self, hash: Sha256Fn) -> Result<Sha256, ProtoError> {
        Ok(hash(&self.encode()?))
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.version))),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.vault_id.to_vec())),
            (3, CdeValue::UInt(u64::from(self.kind.as_u8()))),
            (4, CdeValue::Bytes(self.request_id.to_vec())),
            (5, CdeValue::UInt(self.expected_control_rev)),
            (6, CdeValue::Bytes(self.prev_control_hash.to_vec())),
            (7, CdeValue::UInt(self.expected_authz_rev)),
            (8, CdeValue::UInt(self.authority_rev)),
            (9, CdeValue::UInt(self.signed_at)),
            (10, CdeValue::UInt(self.not_after)),
            (11, CdeValue::UInt(2)),
            (12, CdeValue::Text(self.canonical_path.clone())),
            (13, CdeValue::Bytes(self.payload.clone())),
            (
                14,
                CdeValue::Array(
                    self.signers
                        .iter()
                        .map(SignerV2::to_cde)
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            ),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, OPERATION_V2_FIELD_COUNT)?;
        let method = expect_u8(fields[11].clone(), "OperationV2.method")?;
        if method != 2 {
            return Err(ProtoError::Invalid(format!(
                "OperationV2.method 必须为 2(POST)，实际 {method}"
            )));
        }
        let signers = expect_array(fields[14].clone(), "OperationV2.signers")?
            .into_iter()
            .map(SignerV2::from_cde)
            .collect::<Result<Vec<_>, _>>()?;
        let operation = Self {
            version: expect_u16(fields[0].clone(), "OperationV2.version")?,
            instance_id: expect_bytes_n::<16>(fields[1].clone(), "OperationV2.instance_id")?,
            vault_id: expect_bytes_n::<16>(fields[2].clone(), "OperationV2.vault_id")?,
            kind: ControlKindV2::try_from(expect_u8(fields[3].clone(), "OperationV2.kind")?)?,
            request_id: expect_bytes_n::<16>(fields[4].clone(), "OperationV2.request_id")?,
            expected_control_rev: expect_i64_max(
                fields[5].clone(),
                "OperationV2.expected_control_rev",
            )?,
            prev_control_hash: expect_bytes_n::<32>(
                fields[6].clone(),
                "OperationV2.prev_control_hash",
            )?,
            expected_authz_rev: expect_i64_max(
                fields[7].clone(),
                "OperationV2.expected_authz_rev",
            )?,
            authority_rev: expect_i64_max(fields[8].clone(), "OperationV2.authority_rev")?,
            signed_at: expect_i64_max(fields[9].clone(), "OperationV2.signed_at")?,
            not_after: expect_i64_max(fields[10].clone(), "OperationV2.not_after")?,
            canonical_path: crate::cde::expect_text(
                fields[12].clone(),
                "OperationV2.canonical_path",
            )?,
            payload: expect_bytes(fields[13].clone(), "OperationV2.payload")?,
            signers,
        };
        operation.validate()?;
        Ok(operation)
    }

    /// 字段/枚举/路径/payload canonical/signer 升序与取值范围。
    pub fn validate(&self) -> Result<(), ProtoError> {
        check_version(self.version)?;
        check_nonzero("OperationV2.instance_id", &self.instance_id)?;
        check_nonzero("OperationV2.vault_id", &self.vault_id)?;
        check_nonzero("OperationV2.request_id", &self.request_id)?;
        validate_canonical_path(&self.canonical_path, &self.vault_id)?;

        if self.payload.is_empty() {
            return Err(ProtoError::Invalid(
                "OperationV2.payload 不能为空".to_owned(),
            ));
        }
        // payload 必须是 canonical CDE；kind 专属字段表由上层按 kind 再解码。
        let payload = decode(&self.payload)?;
        if encode(&payload)? != self.payload {
            return Err(ProtoError::NonCanonical(
                "OperationV2.payload 不是 canonical CDE".to_owned(),
            ));
        }

        for (name, value) in [
            (
                "OperationV2.expected_control_rev",
                self.expected_control_rev,
            ),
            ("OperationV2.expected_authz_rev", self.expected_authz_rev),
            ("OperationV2.authority_rev", self.authority_rev),
            ("OperationV2.signed_at", self.signed_at),
            ("OperationV2.not_after", self.not_after),
        ] {
            check_i64_max(value, name)?;
        }
        if self.not_after < self.signed_at {
            return Err(ProtoError::Invalid(format!(
                "OperationV2.not_after={} 不能早于 signed_at={}",
                self.not_after, self.signed_at
            )));
        }

        if self.signers.is_empty() {
            return Err(ProtoError::Invalid(
                "OperationV2.signers 不能为空".to_owned(),
            ));
        }
        if self.signers.len() > MAX_OPERATION_SIGNERS_V2 {
            return Err(ProtoError::TooLarge);
        }
        for signer in &self.signers {
            signer.validate()?;
        }
        for pair in self.signers.windows(2) {
            if compare_signers(&pair[0], &pair[1]) != Ordering::Less {
                return Err(ProtoError::Invalid(
                    "OperationV2.signers 必须按 (role,device,cap,pk) 严格升序且不重复".to_owned(),
                ));
            }
        }
        Ok(())
    }

    /// 解码 kind 专属 payload 的 CDE 值（字段表由上层按 kind 解释）。
    pub fn payload_cde(&self) -> Result<CdeValue, ProtoError> {
        decode(&self.payload)
    }
}

// —— SignedControlV2 ——

/// `SignedControlV2`：`{0:bytes(OperationV2), 1:[sig64…]}`。
///
/// 签名数组与 signer 一一对应；数量/长度不符即拒绝。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedControlV2 {
    pub operation: OperationV2,
    /// 与 `operation.signers` 一一对应的 64B 签名。
    pub signatures: Vec<[u8; 64]>,
}

impl SignedControlV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.operation.encode()?)),
            (
                1,
                CdeValue::Array(
                    self.signatures
                        .iter()
                        .map(|signature| CdeValue::Bytes(signature.to_vec()))
                        .collect(),
                ),
            ),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, SIGNED_CONTROL_V2_FIELD_COUNT)?;
        let operation_bytes = expect_bytes(fields[0].clone(), "SignedControlV2.operation")?;
        let signatures = expect_array(fields[1].clone(), "SignedControlV2.signatures")?
            .into_iter()
            .map(|value| expect_bytes_n::<64>(value, "SignedControlV2.signatures[]"))
            .collect::<Result<Vec<_>, _>>()?;
        let signed = Self {
            operation: OperationV2::decode(&operation_bytes)?,
            signatures,
        };
        signed.validate()?;
        Ok(signed)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        self.operation.validate()?;
        if self.signatures.len() != self.operation.signers.len() {
            return Err(ProtoError::Invalid(format!(
                "SignedControlV2 签名数 {} 与 signer 数 {} 不一致",
                self.signatures.len(),
                self.operation.signers.len()
            )));
        }
        if self.signatures.len() > MAX_OPERATION_SIGNERS_V2 {
            return Err(ProtoError::TooLarge);
        }
        Ok(())
    }

    /// ADR 0015：链 hash 为 `SHA256(prev_hash || SignedControlV2_bytes)`。
    ///
    /// `prev_hash` 对链首记录用 32B 全零。
    pub fn chain_hash(&self, prev_hash: &Sha256, hash: Sha256Fn) -> Result<Sha256, ProtoError> {
        let signed = self.encode()?;
        let mut input = Vec::with_capacity(32 + signed.len());
        input.extend_from_slice(prev_hash);
        input.extend_from_slice(&signed);
        Ok(hash(&input))
    }
}

/// 设备控制签名输入：`MN/v2/control\0 || SHA256(CDE(OperationV2))`。
pub fn control_signing_input(
    operation: &OperationV2,
    hash: Sha256Fn,
) -> Result<Vec<u8>, ProtoError> {
    Ok(domain_digest_input(
        SIGN_V2_CONTROL_DOMAIN,
        &operation.digest(hash)?,
    ))
}

/// 旧 creator 升级签名输入：`MN/v2/upgrade\0 || SHA256(CDE(OperationV2))`。
pub fn upgrade_signing_input(
    operation: &OperationV2,
    hash: Sha256Fn,
) -> Result<Vec<u8>, ProtoError> {
    Ok(domain_digest_input(
        SIGN_V2_UPGRADE_DOMAIN,
        &operation.digest(hash)?,
    ))
}

/// 离线恢复根签名输入：`MN/v2/recover\0 || SHA256(CDE(OperationV2))`。
pub fn recover_signing_input(
    operation: &OperationV2,
    hash: Sha256Fn,
) -> Result<Vec<u8>, ProtoError> {
    Ok(domain_digest_input(
        SIGN_V2_RECOVER_DOMAIN,
        &operation.digest(hash)?,
    ))
}

/// 目标设备 PoP 签名输入：`MN/v2/pop\0 || SHA256(CDE(OperationV2)) || pk32`。
pub fn pop_signing_input(
    operation: &OperationV2,
    pk32: &Ed25519PublicKey,
    hash: Sha256Fn,
) -> Result<Vec<u8>, ProtoError> {
    let digest = operation.digest(hash)?;
    let mut input = Vec::with_capacity(SIGN_V2_POP_DOMAIN.len() + 32 + 32);
    input.extend_from_slice(SIGN_V2_POP_DOMAIN);
    input.extend_from_slice(&digest);
    input.extend_from_slice(pk32);
    Ok(input)
}

/// V2 鉴权签名输入：`MN/v2/auth\0 || challenge`（challenge 是请求里的原字节）。
#[must_use]
pub fn auth_signing_input_v2(challenge: &[u8]) -> Vec<u8> {
    let mut input = Vec::with_capacity(SIGN_AUTH_V2_DOMAIN.len() + challenge.len());
    input.extend_from_slice(SIGN_AUTH_V2_DOMAIN);
    input.extend_from_slice(challenge);
    input
}

fn domain_digest_input(domain: &[u8], digest: &Sha256) -> Vec<u8> {
    let mut input = Vec::with_capacity(domain.len() + 32);
    input.extend_from_slice(domain);
    input.extend_from_slice(digest);
    input
}

// —— SetDigestV2（ADR 0015 S4b 冻结的集合摘要）——

/// 集合摘要的输入项：`(id16, canonical_bytes)`。
///
/// `canonical_bytes` 必须是该对象的**完整** canonical CDE（例如整份 `DeviceCertV2`、
/// 整份 `GrantPackageV2`、整份 `ManagementCapV2`），不是裸拼接字段。
pub type SetDigestEntryV2 = ([u8; 16], Vec<u8>);

/// `SetDigestV2`：`SHA256(CDE(array(bytes)))`，按 id 字节升序去重后编码。
///
/// ADR 0015 附「集合摘要 `SetDigestV2`」：
/// - 输入为 `(id16, canonical_bytes)` 列表；
/// - 按 `id` 字节升序**去重**（同 id 必须是同一份字节，冲突即拒绝）；
/// - 摘要预映像 = `CDE(array(bytes))`，每个元素是该对象的完整 canonical CDE bytes；
/// - 最终 `SHA256` 由调用方注入（sync-proto 无 crypto 依赖）。
///
/// 空集合合法（`CDE([])` = `0x80`），由上层按具体语义决定是否允许。
pub fn set_digest_v2(entries: &[SetDigestEntryV2], hash: Sha256Fn) -> Result<Sha256, ProtoError> {
    let mut sorted: Vec<&SetDigestEntryV2> = entries.iter().collect();
    sorted.sort_by_key(|left| left.0);
    let mut unique: Vec<&SetDigestEntryV2> = Vec::with_capacity(sorted.len());
    for entry in sorted {
        // 每个元素必须自身是 canonical CDE（S4b 冻结：禁止裸拼接）。
        let decoded = decode(&entry.1)?;
        if encode(&decoded)? != entry.1 {
            return Err(ProtoError::NonCanonical(
                "SetDigestV2 元素不是 canonical CDE".to_owned(),
            ));
        }
        match unique.last() {
            Some(previous) if previous.0 == entry.0 => {
                if previous.1 != entry.1 {
                    return Err(ProtoError::Invalid(
                        "SetDigestV2 同一个 id 对应不同 canonical bytes".to_owned(),
                    ));
                }
                // 完全相同的重复项只保留一份。
            }
            _ => unique.push(entry),
        }
    }
    let value = CdeValue::Array(
        unique
            .into_iter()
            .map(|entry| CdeValue::Bytes(entry.1.clone()))
            .collect(),
    );
    Ok(hash(&encode(&value)?))
}

// —— kind 专属 payload（ADR 0015「控制记录 kind」+ S4b 前置冻结）——

/// `UpgradePolicy`（kind 0）payload：`{0:old_manager_rev,1:P,2:fresh_recovery_pk,
/// 3:bootstrap_caps[],4:ACTIVE_manifest[]}`。
///
/// `P` 是 `CDE(PolicyV2)` 原字节；caps 是 `CDE(ManagementCapV2)` 原字节；
/// manifest 是 `CDE(DeviceCertV2)` 原字节（S4b 冻结 §1/§2/§3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradePolicyPayloadV2 {
    /// 当前 v1 管理 rev（`vaults.manager_rev`）。
    pub old_manager_rev: u64,
    pub policy: PolicyV2,
    /// 新离线恢复根公钥（独立随机 seed 的公钥，secret 永不进 relay）。
    pub fresh_recovery_pk: Ed25519PublicKey,
    /// 升级时原子安装的 bootstrap caps。
    pub bootstrap_caps: Vec<ManagementCapV2>,
    /// 全部当前 ACTIVE 设备的完整 `DeviceCertV2`（按 device_id 严格升序）。
    pub active_manifest: Vec<DeviceCertV2>,
}

impl UpgradePolicyPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(self.old_manager_rev)),
            (1, CdeValue::Bytes(self.policy.encode()?)),
            (2, CdeValue::Bytes(self.fresh_recovery_pk.to_vec())),
            (
                3,
                CdeValue::Array(
                    self.bootstrap_caps
                        .iter()
                        .map(|cap| Ok(CdeValue::Bytes(cap.encode()?)))
                        .collect::<Result<Vec<_>, ProtoError>>()?,
                ),
            ),
            (
                4,
                CdeValue::Array(
                    self.active_manifest
                        .iter()
                        .map(|cert| Ok(CdeValue::Bytes(cert.encode()?)))
                        .collect::<Result<Vec<_>, ProtoError>>()?,
                ),
            ),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, UPGRADE_POLICY_PAYLOAD_V2_FIELD_COUNT)?;
        let policy = PolicyV2::decode(&expect_bytes(
            fields[1].clone(),
            "UpgradePolicyPayloadV2.policy",
        )?)?;
        let bootstrap_caps =
            expect_array(fields[3].clone(), "UpgradePolicyPayloadV2.bootstrap_caps")?
                .into_iter()
                .map(|item| {
                    ManagementCapV2::decode(&expect_bytes(
                        item,
                        "UpgradePolicyPayloadV2.bootstrap_caps[]",
                    )?)
                })
                .collect::<Result<Vec<_>, ProtoError>>()?;
        let active_manifest =
            expect_array(fields[4].clone(), "UpgradePolicyPayloadV2.ACTIVE_manifest")?
                .into_iter()
                .map(|item| {
                    DeviceCertV2::decode(&expect_bytes(
                        item,
                        "UpgradePolicyPayloadV2.ACTIVE_manifest[]",
                    )?)
                })
                .collect::<Result<Vec<_>, ProtoError>>()?;
        let payload = Self {
            old_manager_rev: expect_i64_max(
                fields[0].clone(),
                "UpgradePolicyPayloadV2.old_manager_rev",
            )?,
            policy,
            fresh_recovery_pk: expect_bytes_n::<32>(
                fields[2].clone(),
                "UpgradePolicyPayloadV2.fresh_recovery_pk",
            )?,
            bootstrap_caps,
            active_manifest,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.old_manager_rev == 0 {
            return Err(ProtoError::Invalid(
                "UpgradePolicyPayloadV2.old_manager_rev 必须 >0".to_owned(),
            ));
        }
        check_i64_max(
            self.old_manager_rev,
            "UpgradePolicyPayloadV2.old_manager_rev",
        )?;
        self.policy.validate()?;
        check_nonzero(
            "UpgradePolicyPayloadV2.fresh_recovery_pk",
            &self.fresh_recovery_pk,
        )?;
        if self.bootstrap_caps.len() > MAX_BOOTSTRAP_CAPS_V2 {
            return Err(ProtoError::TooLarge);
        }
        let mut cap_ids: Vec<&[u8; 16]> = Vec::with_capacity(self.bootstrap_caps.len());
        for cap in &self.bootstrap_caps {
            cap.validate()?;
            if cap_ids.contains(&&cap.cap_id) {
                return Err(ProtoError::Invalid(
                    "UpgradePolicyPayloadV2.bootstrap_caps 含重复 cap_id".to_owned(),
                ));
            }
            cap_ids.push(&cap.cap_id);
        }
        if self.active_manifest.is_empty() {
            return Err(ProtoError::Invalid(
                "UpgradePolicyPayloadV2.ACTIVE_manifest 不能为空".to_owned(),
            ));
        }
        for cert in &self.active_manifest {
            cert.validate()?;
        }
        for pair in self.active_manifest.windows(2) {
            if pair[0].device_id >= pair[1].device_id {
                return Err(ProtoError::Invalid(
                    "UpgradePolicyPayloadV2.ACTIVE_manifest 必须按 device_id 严格升序且不重复"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// `CapGrant`（kind 2）payload：`{0: bytes(ManagementCapV2 canonical CDE)}`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapGrantPayloadV2 {
    pub cap: ManagementCapV2,
}

impl CapGrantPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.cap.validate()?;
        Ok(record_map(vec![(0, CdeValue::Bytes(self.cap.encode()?))]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, CAP_GRANT_PAYLOAD_V2_FIELD_COUNT)?;
        let cap =
            ManagementCapV2::decode(&expect_bytes(fields[0].clone(), "CapGrantPayloadV2.cap")?)?;
        Ok(Self { cap })
    }
}

/// `CapRevoke`（kind 3）payload：`{0: cap_id16}`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapRevokePayloadV2 {
    pub cap_id: CapId,
}

impl CapRevokePayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        check_nonzero("CapRevokePayloadV2.cap_id", &self.cap_id)?;
        Ok(record_map(vec![(0, CdeValue::Bytes(self.cap_id.to_vec()))]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, CAP_REVOKE_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            cap_id: expect_bytes_n::<16>(fields[0].clone(), "CapRevokePayloadV2.cap_id")?,
        };
        payload.to_cde()?;
        Ok(payload)
    }
}

/// `InviteCreate`（kind 11）payload：`{0:invite_id,1:token_hash32,2:expires_at}`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteCreatePayloadV2 {
    pub invite_id: [u8; 16],
    /// `SHA256(T)`；relay 永不落明文邀请令牌。
    pub token_hash: Sha256,
    pub expires_at: u64,
}

impl InviteCreatePayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.invite_id.to_vec())),
            (1, CdeValue::Bytes(self.token_hash.to_vec())),
            (2, CdeValue::UInt(self.expires_at)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, INVITE_CREATE_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            invite_id: expect_bytes_n::<16>(fields[0].clone(), "InviteCreatePayloadV2.invite_id")?,
            token_hash: expect_bytes_n::<32>(
                fields[1].clone(),
                "InviteCreatePayloadV2.token_hash",
            )?,
            expires_at: expect_i64_max(fields[2].clone(), "InviteCreatePayloadV2.expires_at")?,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_nonzero("InviteCreatePayloadV2.invite_id", &self.invite_id)?;
        check_nonzero("InviteCreatePayloadV2.token_hash", &self.token_hash)?;
        if self.expires_at == 0 {
            return Err(ProtoError::Invalid(
                "InviteCreatePayloadV2.expires_at 必须 >0".to_owned(),
            ));
        }
        check_i64_max(self.expires_at, "InviteCreatePayloadV2.expires_at")?;
        Ok(())
    }
}

// —— DeviceApprove / DeviceRightsChange payload（ADR 0015 kind 4/16）——

/// `DeviceApprove`（kind 4）/ `DeviceRightsChange`（kind 16）的 payload：
/// `{0: cert_bytes(DeviceCertV2), 1: grant_packages[]}`。
///
/// ADR 0015 只把两种 kind 的 payload 写成同一个形状，wire 上因此**只有一种**编码：
/// 键 0 是整份 `DeviceCertV2` 的 canonical CDE bytes（不是嵌套 map，与
/// `UpgradePolicyPayloadV2` 的 manifest / `CapGrantPayloadV2` 的 cap 同口径），
/// 键 1 是「每个元素为整份 `GrantPackageV2` canonical CDE bytes」的数组。
///
/// 证书与 grant 包都被两签 `OperationV2` 的摘要承诺（ADR：「证书、AAD、enc、完整
/// ciphertext 都由两签 Operation 承诺，不另造单签旁路」），因此换包/换证书必然验签失败。
///
/// 本类型只做**结构**校验：证书与每个 grant 自身合法、grant 的 `grant_id` 两两不同
/// （对象身份不能有二义）、`epoch` 两两不同（`device_grants` 的 PK 是
/// `(vault_id, epoch, device_id)`）。证书与 grant 之间的语义一致性（recipient/hpke/
/// authority/issued rev/当前 epoch）由 relay 按 DB 事实校验。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCredentialPayloadV2 {
    pub cert: DeviceCertV2,
    pub grants: Vec<GrantPackageV2>,
}

/// kind 4 `DeviceApprove` 的 payload（与 kind 16 同一 wire 形状）。
pub type DeviceApprovePayloadV2 = DeviceCredentialPayloadV2;
/// kind 16 `DeviceRightsChange` 的 payload（与 kind 4 同一 wire 形状）。
pub type DeviceRightsChangePayloadV2 = DeviceCredentialPayloadV2;

impl DeviceCredentialPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.cert.encode()?)),
            (
                1,
                CdeValue::Array(
                    self.grants
                        .iter()
                        .map(|grant| Ok(CdeValue::Bytes(grant.encode()?)))
                        .collect::<Result<Vec<_>, ProtoError>>()?,
                ),
            ),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, DEVICE_CREDENTIAL_PAYLOAD_V2_FIELD_COUNT)?;
        let cert = DeviceCertV2::decode(&expect_bytes(
            fields[0].clone(),
            "DeviceCredentialPayloadV2.cert",
        )?)?;
        let grants = expect_array(fields[1].clone(), "DeviceCredentialPayloadV2.grants")?
            .into_iter()
            .map(|item| {
                GrantPackageV2::decode(&expect_bytes(item, "DeviceCredentialPayloadV2.grants[]")?)
            })
            .collect::<Result<Vec<_>, ProtoError>>()?;
        let payload = Self { cert, grants };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        self.cert.validate()?;
        if self.grants.is_empty() {
            return Err(ProtoError::Invalid(
                "DeviceCredentialPayloadV2.grants 不能为空".to_owned(),
            ));
        }
        if self.grants.len() > MAX_GRANT_PACKAGES_V2 {
            return Err(ProtoError::TooLarge);
        }
        let mut grant_ids: Vec<&GrantIdV2> = Vec::with_capacity(self.grants.len());
        let mut epochs: Vec<u64> = Vec::with_capacity(self.grants.len());
        for grant in &self.grants {
            grant.validate()?;
            if grant_ids.contains(&&grant.aad.grant_id) {
                return Err(ProtoError::Invalid(
                    "DeviceCredentialPayloadV2.grants 含重复 grant_id".to_owned(),
                ));
            }
            grant_ids.push(&grant.aad.grant_id);
            if epochs.contains(&grant.aad.epoch) {
                return Err(ProtoError::Invalid(
                    "DeviceCredentialPayloadV2.grants 含重复 epoch".to_owned(),
                ));
            }
            epochs.push(grant.aad.epoch);
        }
        Ok(())
    }
}

// —— DeviceRevoke payload（ADR 0015 kind 5）——

/// `DeviceRevoke`（kind 5）payload：`{0: array(bytes16)}`（升序、去重的设备 ID）。
///
/// S4b 冻结 §1：`ids` 是「元素为 16B byte string 的数组」。实现要求**严格升序**
/// （排序 + 去重同时成立），这样集合摘要/链上字节唯一，重复目标不会产生两种编码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRevokePayloadV2 {
    pub device_ids: Vec<DeviceId>,
}

impl DeviceRevokePayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![(
            0,
            CdeValue::Array(
                self.device_ids
                    .iter()
                    .map(|device_id| CdeValue::Bytes(device_id.to_vec()))
                    .collect(),
            ),
        )]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, DEVICE_REVOKE_PAYLOAD_V2_FIELD_COUNT)?;
        let device_ids = expect_array(fields[0].clone(), "DeviceRevokePayloadV2.device_ids")?
            .into_iter()
            .map(|item| expect_bytes_n::<16>(item, "DeviceRevokePayloadV2.device_ids[]"))
            .collect::<Result<Vec<_>, ProtoError>>()?;
        let payload = Self { device_ids };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.device_ids.is_empty() {
            return Err(ProtoError::Invalid(
                "DeviceRevokePayloadV2.device_ids 不能为空".to_owned(),
            ));
        }
        if self.device_ids.len() > MAX_DEVICES {
            return Err(ProtoError::TooLarge);
        }
        for device_id in &self.device_ids {
            check_nonzero("DeviceRevokePayloadV2.device_ids[]", device_id)?;
        }
        for pair in self.device_ids.windows(2) {
            if pair[0] >= pair[1] {
                return Err(ProtoError::Invalid(
                    "DeviceRevokePayloadV2.device_ids 必须严格升序且不重复".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

// —— 轮换控制（ADR 0015 kind 6/7/8/9/10）——

/// 校验「严格升序去重」的设备 ID 数组（集合摘要/链上字节唯一）。
///
/// 允许为空（`RotationBegin`/`RotationRestart` 的撤销列表可以空），非空时元素必须
/// 非全零、严格升序；`max` 是协议上限（`MAX_DEVICES`）。
fn validate_sorted_device_ids(
    name: &str,
    device_ids: &[DeviceId],
    max: usize,
) -> Result<(), ProtoError> {
    if device_ids.len() > max {
        return Err(ProtoError::TooLarge);
    }
    for device_id in device_ids {
        check_nonzero(name, device_id)?;
    }
    for pair in device_ids.windows(2) {
        if pair[0] >= pair[1] {
            return Err(ProtoError::Invalid(format!("{name} 必须严格升序且不重复")));
        }
    }
    Ok(())
}

fn device_id_array(device_ids: &[DeviceId]) -> CdeValue {
    CdeValue::Array(
        device_ids
            .iter()
            .map(|device_id| CdeValue::Bytes(device_id.to_vec()))
            .collect(),
    )
}

fn decode_device_id_array(value: CdeValue, name: &str) -> Result<Vec<DeviceId>, ProtoError> {
    expect_array(value, name)?
        .into_iter()
        .map(|item| expect_bytes_n::<16>(item, name))
        .collect()
}

/// `RotationBegin`（kind 6）payload：`{0:rotation_id,1:from_epoch,2:to_epoch,3:revoke_ids[]}`。
///
/// 两签 `ROTATE`；`revoke_ids` 非空时**每一位**签名者还必须具备 `REVOKE`（relay 校验）。
/// `to_epoch` 由客户端指定（必须是本 vault 从未分配过的更高 epoch），relay 只做
/// 「不复用候选」的持久事实校验。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationBeginPayloadV2 {
    pub rotation_id: RotationIdV2,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub revoke_ids: Vec<DeviceId>,
}

impl RotationBeginPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.rotation_id.to_vec())),
            (1, CdeValue::UInt(self.from_epoch)),
            (2, CdeValue::UInt(self.to_epoch)),
            (3, device_id_array(&self.revoke_ids)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, ROTATION_BEGIN_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            rotation_id: expect_bytes_n::<16>(
                fields[0].clone(),
                "RotationBeginPayloadV2.rotation_id",
            )?,
            from_epoch: expect_i64_max(fields[1].clone(), "RotationBeginPayloadV2.from_epoch")?,
            to_epoch: expect_i64_max(fields[2].clone(), "RotationBeginPayloadV2.to_epoch")?,
            revoke_ids: decode_device_id_array(
                fields[3].clone(),
                "RotationBeginPayloadV2.revoke_ids",
            )?,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_nonzero("RotationBeginPayloadV2.rotation_id", &self.rotation_id)?;
        check_i64_max(self.from_epoch, "RotationBeginPayloadV2.from_epoch")?;
        check_i64_max(self.to_epoch, "RotationBeginPayloadV2.to_epoch")?;
        if self.from_epoch == 0 {
            return Err(ProtoError::Invalid(
                "RotationBeginPayloadV2.from_epoch 必须 >0".to_owned(),
            ));
        }
        if self.to_epoch <= self.from_epoch {
            return Err(ProtoError::Invalid(format!(
                "RotationBeginPayloadV2.to_epoch={} 必须大于 from_epoch={}",
                self.to_epoch, self.from_epoch
            )));
        }
        validate_sorted_device_ids(
            "RotationBeginPayloadV2.revoke_ids[]",
            &self.revoke_ids,
            MAX_DEVICES,
        )
    }
}

/// `RotationAbort`（kind 7）payload：`{0:rotation_id}`。
///
/// 两签 `ROTATE`；abort **不复活**已吊销的 epoch（relay 按 `epochs.revoked_once` 判定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationAbortPayloadV2 {
    pub rotation_id: RotationIdV2,
}

impl RotationAbortPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        check_nonzero("RotationAbortPayloadV2.rotation_id", &self.rotation_id)?;
        Ok(record_map(vec![(
            0,
            CdeValue::Bytes(self.rotation_id.to_vec()),
        )]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, ROTATION_ABORT_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            rotation_id: expect_bytes_n::<16>(
                fields[0].clone(),
                "RotationAbortPayloadV2.rotation_id",
            )?,
        };
        payload.to_cde()?;
        Ok(payload)
    }
}

/// `RotationRestart`（kind 8）payload：`{0:rotation_id,1:new_to_epoch,2:extra_ids[]}`。
///
/// 两签 `ROTATE`；`extra_ids` 非空时每一位签名者还必须具备 `REVOKE`。旧候选 epoch
/// 永久作废（不得复用），新候选 epoch 必须是本 vault 从未分配过的值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationRestartPayloadV2 {
    pub rotation_id: RotationIdV2,
    pub new_to_epoch: u64,
    pub extra_ids: Vec<DeviceId>,
}

impl RotationRestartPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.rotation_id.to_vec())),
            (1, CdeValue::UInt(self.new_to_epoch)),
            (2, device_id_array(&self.extra_ids)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, ROTATION_RESTART_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            rotation_id: expect_bytes_n::<16>(
                fields[0].clone(),
                "RotationRestartPayloadV2.rotation_id",
            )?,
            new_to_epoch: expect_i64_max(
                fields[1].clone(),
                "RotationRestartPayloadV2.new_to_epoch",
            )?,
            extra_ids: decode_device_id_array(
                fields[2].clone(),
                "RotationRestartPayloadV2.extra_ids",
            )?,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_nonzero("RotationRestartPayloadV2.rotation_id", &self.rotation_id)?;
        check_i64_max(self.new_to_epoch, "RotationRestartPayloadV2.new_to_epoch")?;
        if self.new_to_epoch == 0 {
            return Err(ProtoError::Invalid(
                "RotationRestartPayloadV2.new_to_epoch 必须 >0".to_owned(),
            ));
        }
        validate_sorted_device_ids(
            "RotationRestartPayloadV2.extra_ids[]",
            &self.extra_ids,
            MAX_DEVICES,
        )
    }
}

/// `RotationAddRevokes`（kind 9）payload：`{0:rotation_id,1:ids[]}`。
///
/// 两签 `ROTATE` + 每一位签名者 `REVOKE`；`ids` 必须非空且严格升序去重。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationAddRevokesPayloadV2 {
    pub rotation_id: RotationIdV2,
    pub device_ids: Vec<DeviceId>,
}

impl RotationAddRevokesPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.rotation_id.to_vec())),
            (1, device_id_array(&self.device_ids)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, ROTATION_ADD_REVOKES_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            rotation_id: expect_bytes_n::<16>(
                fields[0].clone(),
                "RotationAddRevokesPayloadV2.rotation_id",
            )?,
            device_ids: decode_device_id_array(
                fields[1].clone(),
                "RotationAddRevokesPayloadV2.device_ids",
            )?,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_nonzero("RotationAddRevokesPayloadV2.rotation_id", &self.rotation_id)?;
        if self.device_ids.is_empty() {
            return Err(ProtoError::Invalid(
                "RotationAddRevokesPayloadV2.device_ids 不能为空".to_owned(),
            ));
        }
        validate_sorted_device_ids(
            "RotationAddRevokesPayloadV2.device_ids[]",
            &self.device_ids,
            MAX_DEVICES,
        )
    }
}

/// `EpochActivate`（kind 10）payload：`{0: bytes(ActivationV2)}`。
///
/// 与 `CapGrant`/`UpgradePolicy` 的 manifest 同口径：键 0 是**整份** `ActivationV2`
/// canonical CDE 的 byte string（不是嵌套 map）。集合摘要由客户端取包重算，relay 只按
/// DB 事实复算并逐字段比对（ADR：「客户端必须取包重算，不能只信 relay 摘要」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochActivatePayloadV2 {
    pub activation: ActivationV2,
}

impl EpochActivatePayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.activation.validate()?;
        Ok(record_map(vec![(
            0,
            CdeValue::Bytes(self.activation.encode()?),
        )]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, EPOCH_ACTIVATE_PAYLOAD_V2_FIELD_COUNT)?;
        let activation = ActivationV2::decode(&expect_bytes(
            fields[0].clone(),
            "EpochActivatePayloadV2.activation",
        )?)?;
        Ok(Self { activation })
    }
}

// —— InviteCancel（ADR 0015 kind 12）——

/// `InviteCancel`（kind 12）payload：`{0: invite_id}`。
///
/// 一台 `INVITE` cap（或签发该邀请的 cap 本人）即可取消；只对**未兑换**邀请成立，
/// 已兑换/已用是永久事实（relay 拒绝，不伪造 `used_at`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteCancelPayloadV2 {
    pub invite_id: [u8; 16],
}

impl InviteCancelPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        check_nonzero("InviteCancelPayloadV2.invite_id", &self.invite_id)?;
        Ok(record_map(vec![(
            0,
            CdeValue::Bytes(self.invite_id.to_vec()),
        )]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, INVITE_CANCEL_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            invite_id: expect_bytes_n::<16>(fields[0].clone(), "InviteCancelPayloadV2.invite_id")?,
        };
        payload.to_cde()?;
        Ok(payload)
    }
}

// —— RecoveryReset / RecoveryKeyChange（ADR 0015 kind 13/14）——

/// 新代 bootstrap cap 的公共结构校验（Reset/KeyChange 共用）。
///
/// 只校验**结构**：cap 自身合法、cap_id 两两不同、对象身份（instance/vault）由 relay
/// 按 DB 事实校验；`issued_control_rev`/`issued_authz_rev`/`issuer_authority_rev` 必须
/// 等于**提交后**的值（relay 校验，wire 只承诺字节）。
fn validate_bootstrap_caps_structure(caps: &[ManagementCapV2]) -> Result<(), ProtoError> {
    if caps.is_empty() {
        return Err(ProtoError::Invalid(
            "bootstrap_caps 不能为空（旧代 cap 会随 authority_rev+1 全部失效）".to_owned(),
        ));
    }
    if caps.len() > MAX_BOOTSTRAP_CAPS_V2 {
        return Err(ProtoError::TooLarge);
    }
    let mut cap_ids: Vec<&CapId> = Vec::with_capacity(caps.len());
    for cap in caps {
        cap.validate()?;
        if cap_ids.contains(&&cap.cap_id) {
            return Err(ProtoError::Invalid(
                "bootstrap_caps 含重复 cap_id".to_owned(),
            ));
        }
        cap_ids.push(&cap.cap_id);
    }
    Ok(())
}

fn bootstrap_caps_cde(caps: &[ManagementCapV2]) -> Result<CdeValue, ProtoError> {
    Ok(CdeValue::Array(
        caps.iter()
            .map(|cap| Ok(CdeValue::Bytes(cap.encode()?)))
            .collect::<Result<Vec<_>, ProtoError>>()?,
    ))
}

fn decode_bootstrap_caps(value: CdeValue, name: &str) -> Result<Vec<ManagementCapV2>, ProtoError> {
    expect_array(value, name)?
        .into_iter()
        .map(|item| ManagementCapV2::decode(&expect_bytes(item, name)?))
        .collect()
}

/// `RecoveryReset`（kind 13）payload：
/// `{0: policy_bytes(PolicyV2), 1: fresh_recovery_pk, 2: bootstrap_caps[],
///   3: device_adds[], 4: revoke_ids[]}`。
///
/// 授权 = 离线恢复根（**当前** `recovery_pub`，`recover` 域）**加上**新恢复钥 PoP
/// （新钥对同一摘要的 `recover` 域签名）；设备 adds 每一位还须目标设备 PoP（role3）。
/// 一次提交原子安装新 policy/新代 caps/设备增删，并把 `authority_rev+1`
/// （旧代 cap 与管理材料全部失效）。
///
/// 不足两台 ACTIVE 设备时进入 `recovery_only`（只许再 Reset，普通管理写 403）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryResetPayloadV2 {
    pub policy: PolicyV2,
    /// 新离线恢复根公钥（独立随机 seed；secret 永不进 relay）。
    pub fresh_recovery_pk: Ed25519PublicKey,
    pub bootstrap_caps: Vec<ManagementCapV2>,
    /// 设备 adds：每项是「证书 + 该设备当前 epoch 的 grant 包」。
    pub device_adds: Vec<DeviceCredentialPayloadV2>,
    /// 要撤销的设备（严格升序去重；已 revoked 的重复项幂等）。
    pub revoke_ids: Vec<DeviceId>,
}

impl RecoveryResetPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.policy.encode()?)),
            (1, CdeValue::Bytes(self.fresh_recovery_pk.to_vec())),
            (2, bootstrap_caps_cde(&self.bootstrap_caps)?),
            (
                3,
                CdeValue::Array(
                    self.device_adds
                        .iter()
                        .map(|add| Ok(CdeValue::Bytes(add.encode()?)))
                        .collect::<Result<Vec<_>, ProtoError>>()?,
                ),
            ),
            (4, device_id_array(&self.revoke_ids)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, RECOVERY_RESET_PAYLOAD_V2_FIELD_COUNT)?;
        let policy = PolicyV2::decode(&expect_bytes(
            fields[0].clone(),
            "RecoveryResetPayloadV2.policy",
        )?)?;
        let fresh_recovery_pk = expect_bytes_n::<32>(
            fields[1].clone(),
            "RecoveryResetPayloadV2.fresh_recovery_pk",
        )?;
        let bootstrap_caps =
            decode_bootstrap_caps(fields[2].clone(), "RecoveryResetPayloadV2.bootstrap_caps[]")?;
        let device_adds = expect_array(fields[3].clone(), "RecoveryResetPayloadV2.device_adds")?
            .into_iter()
            .map(|item| {
                DeviceCredentialPayloadV2::decode(&expect_bytes(
                    item,
                    "RecoveryResetPayloadV2.device_adds[]",
                )?)
            })
            .collect::<Result<Vec<_>, ProtoError>>()?;
        let revoke_ids =
            decode_device_id_array(fields[4].clone(), "RecoveryResetPayloadV2.revoke_ids")?;
        let payload = Self {
            policy,
            fresh_recovery_pk,
            bootstrap_caps,
            device_adds,
            revoke_ids,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        self.policy.validate()?;
        check_nonzero(
            "RecoveryResetPayloadV2.fresh_recovery_pk",
            &self.fresh_recovery_pk,
        )?;
        validate_bootstrap_caps_structure(&self.bootstrap_caps)?;
        if self.device_adds.len() > MAX_DEVICES {
            return Err(ProtoError::TooLarge);
        }
        let mut device_ids: Vec<&DeviceId> = Vec::with_capacity(self.device_adds.len());
        for add in &self.device_adds {
            add.validate()?;
            if device_ids.contains(&&add.cert.device_id) {
                return Err(ProtoError::Invalid(
                    "RecoveryResetPayloadV2.device_adds 含重复 device_id".to_owned(),
                ));
            }
            device_ids.push(&add.cert.device_id);
        }
        validate_sorted_device_ids(
            "RecoveryResetPayloadV2.revoke_ids[]",
            &self.revoke_ids,
            MAX_DEVICES,
        )?;
        // 同一台设备不能既 add 又 revoke（效果互相抵消，链上事实必须唯一）。
        for device_id in &self.revoke_ids {
            if device_ids.contains(&device_id) {
                return Err(ProtoError::Invalid(
                    "RecoveryResetPayloadV2 同一设备不能同时 add 与 revoke".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// `RecoveryKeyChange`（kind 14）payload：`{0: fresh_recovery_pk, 1: bootstrap_caps[]}`。
///
/// 授权 = 两台不同 ACTIVE 设备的 `POLICY` cap **加上**新恢复钥 PoP。只换恢复钥
/// （不改 policy/成员集合），但 `authority_rev+1` 让旧代 cap 全部失效，因此必须在
/// 同一提交里原子安装新代 bootstrap caps（否则管理面死锁）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryKeyChangePayloadV2 {
    pub fresh_recovery_pk: Ed25519PublicKey,
    pub bootstrap_caps: Vec<ManagementCapV2>,
}

impl RecoveryKeyChangePayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.fresh_recovery_pk.to_vec())),
            (1, bootstrap_caps_cde(&self.bootstrap_caps)?),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, RECOVERY_KEY_CHANGE_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            fresh_recovery_pk: expect_bytes_n::<32>(
                fields[0].clone(),
                "RecoveryKeyChangePayloadV2.fresh_recovery_pk",
            )?,
            bootstrap_caps: decode_bootstrap_caps(
                fields[1].clone(),
                "RecoveryKeyChangePayloadV2.bootstrap_caps[]",
            )?,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_nonzero(
            "RecoveryKeyChangePayloadV2.fresh_recovery_pk",
            &self.fresh_recovery_pk,
        )?;
        validate_bootstrap_caps_structure(&self.bootstrap_caps)
    }
}

// —— ChallengeV2（ADR 0015 §鉴权）——

/// `ChallengeV2.realm`：普通设备（v2 鉴权，token 不带 cap）。
pub const CHALLENGE_V2_REALM_DEVICE: u8 = 1;
/// `ChallengeV2.realm`：管理设备（必须绑定一个有效 `ManagementCapV2`）。
pub const CHALLENGE_V2_REALM_MANAGEMENT_DEVICE: u8 = 3;
/// `ChallengeV2.realm`：离线恢复（S4c；本切片只保留取值）。
pub const CHALLENGE_V2_REALM_RECOVERY: u8 = 4;

/// `ChallengeV2`（键 `0..12`）：V2 challenge-response 的**被签名字节**。
///
/// ADR 0015：`{version,I,origin_hash,V,realm,subject,challenge_id,nonce,expires_at,
/// auth_rev,authz_rev,cap_id,authority_rev}`；客户端对**整份字节**签
/// `MN/v2/auth\0 || CDE(ChallengeV2)`，relay 从 DB 重算/逐字段比对后消费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeV2 {
    pub version: u16,
    pub instance_id: RelayInstanceId,
    /// relay origin 绑定摘要（relay 侧取值见实现记录；客户端必须原样回传）。
    pub origin_hash: Sha256,
    pub vault_id: VaultId,
    /// 1 设备 / 3 管理设备 / 4 恢复（2 仅旧 v1 wire，V2 不使用）。
    pub realm: u8,
    pub subject: DeviceId,
    pub challenge_id: [u8; 16],
    pub nonce: Sha256,
    pub expires_at: u64,
    pub auth_rev: u64,
    pub authz_rev: u64,
    /// 仅 realm 3 必填（管理与 cap 绑定）；realm 1/4 必须为空。
    pub cap_id: Option<CapId>,
    pub authority_rev: u64,
}

impl ChallengeV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.version))),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.origin_hash.to_vec())),
            (3, CdeValue::Bytes(self.vault_id.to_vec())),
            (4, CdeValue::UInt(u64::from(self.realm))),
            (5, CdeValue::Bytes(self.subject.to_vec())),
            (6, CdeValue::Bytes(self.challenge_id.to_vec())),
            (7, CdeValue::Bytes(self.nonce.to_vec())),
            (8, CdeValue::UInt(self.expires_at)),
            (9, CdeValue::UInt(self.auth_rev)),
            (10, CdeValue::UInt(self.authz_rev)),
            (11, encode_optional_id16(self.cap_id)),
            (12, CdeValue::UInt(self.authority_rev)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, CHALLENGE_V2_FIELD_COUNT)?;
        let challenge = Self {
            version: expect_u16(fields[0].clone(), "ChallengeV2.version")?,
            instance_id: expect_bytes_n::<16>(fields[1].clone(), "ChallengeV2.instance_id")?,
            origin_hash: expect_bytes_n::<32>(fields[2].clone(), "ChallengeV2.origin_hash")?,
            vault_id: expect_bytes_n::<16>(fields[3].clone(), "ChallengeV2.vault_id")?,
            realm: expect_u8(fields[4].clone(), "ChallengeV2.realm")?,
            subject: expect_bytes_n::<16>(fields[5].clone(), "ChallengeV2.subject")?,
            challenge_id: expect_bytes_n::<16>(fields[6].clone(), "ChallengeV2.challenge_id")?,
            nonce: expect_bytes_n::<32>(fields[7].clone(), "ChallengeV2.nonce")?,
            expires_at: expect_i64_max(fields[8].clone(), "ChallengeV2.expires_at")?,
            auth_rev: expect_i64_max(fields[9].clone(), "ChallengeV2.auth_rev")?,
            authz_rev: expect_i64_max(fields[10].clone(), "ChallengeV2.authz_rev")?,
            cap_id: expect_optional_id16(fields[11].clone(), "ChallengeV2.cap_id")?,
            authority_rev: expect_i64_max(fields[12].clone(), "ChallengeV2.authority_rev")?,
        };
        challenge.validate()?;
        Ok(challenge)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_version(self.version)?;
        check_nonzero("ChallengeV2.instance_id", &self.instance_id)?;
        check_nonzero("ChallengeV2.origin_hash", &self.origin_hash)?;
        check_nonzero("ChallengeV2.vault_id", &self.vault_id)?;
        check_nonzero("ChallengeV2.subject", &self.subject)?;
        check_nonzero("ChallengeV2.challenge_id", &self.challenge_id)?;
        check_nonzero("ChallengeV2.nonce", &self.nonce)?;
        match self.realm {
            CHALLENGE_V2_REALM_DEVICE | CHALLENGE_V2_REALM_RECOVERY => {
                if self.cap_id.is_some() {
                    return Err(ProtoError::Invalid(
                        "ChallengeV2: realm 1/4 不能绑定 cap".to_owned(),
                    ));
                }
            }
            CHALLENGE_V2_REALM_MANAGEMENT_DEVICE => {
                if self.cap_id.is_none() {
                    return Err(ProtoError::Invalid(
                        "ChallengeV2: realm 3 必须绑定 cap_id".to_owned(),
                    ));
                }
            }
            other => {
                return Err(ProtoError::Invalid(format!(
                    "ChallengeV2.realm 非法 {other}"
                )));
            }
        }
        if let Some(cap_id) = self.cap_id {
            check_nonzero("ChallengeV2.cap_id", &cap_id)?;
        }
        for (name, value) in [
            ("ChallengeV2.expires_at", self.expires_at),
            ("ChallengeV2.auth_rev", self.auth_rev),
            ("ChallengeV2.authz_rev", self.authz_rev),
            ("ChallengeV2.authority_rev", self.authority_rev),
        ] {
            check_i64_max(value, name)?;
        }
        if self.expires_at == 0 {
            return Err(ProtoError::Invalid(
                "ChallengeV2.expires_at 必须 >0".to_owned(),
            ));
        }
        if self.auth_rev == 0 {
            return Err(ProtoError::Invalid(
                "ChallengeV2.auth_rev 必须 >0".to_owned(),
            ));
        }
        Ok(())
    }
}

// —— DeviceCertV2 ——

/// `DeviceCertV2`（键 `0..9`）：V2 数据面权限，不兼任管理权限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCertV2 {
    pub version: u16,
    pub instance_id: RelayInstanceId,
    pub vault_id: VaultId,
    pub device_id: DeviceId,
    pub sign_ed25519_pub: Ed25519PublicKey,
    pub hpke_x25519_pub: X25519PublicKey,
    pub cert_serial: u64,
    /// 仅 READ/WRITE。
    pub data_rights: Rights,
    pub issued_authz_rev: u64,
}

impl DeviceCertV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.version))),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.vault_id.to_vec())),
            (3, CdeValue::Bytes(self.device_id.to_vec())),
            (4, CdeValue::Bytes(self.sign_ed25519_pub.to_vec())),
            (5, CdeValue::Bytes(self.hpke_x25519_pub.to_vec())),
            (6, CdeValue::UInt(self.cert_serial)),
            (7, CdeValue::UInt(u64::from(self.data_rights.bits()))),
            (8, CdeValue::UInt(self.issued_authz_rev)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, DEVICE_CERT_V2_FIELD_COUNT)?;
        let cert = Self {
            version: expect_u16(fields[0].clone(), "DeviceCertV2.version")?,
            instance_id: expect_bytes_n::<16>(fields[1].clone(), "DeviceCertV2.instance_id")?,
            vault_id: expect_bytes_n::<16>(fields[2].clone(), "DeviceCertV2.vault_id")?,
            device_id: expect_bytes_n::<16>(fields[3].clone(), "DeviceCertV2.device_id")?,
            sign_ed25519_pub: expect_bytes_n::<32>(fields[4].clone(), "DeviceCertV2.sign_pub")?,
            hpke_x25519_pub: expect_bytes_n::<32>(fields[5].clone(), "DeviceCertV2.hpke_pub")?,
            cert_serial: expect_i64_max(fields[6].clone(), "DeviceCertV2.cert_serial")?,
            data_rights: Rights::new(expect_u32(fields[7].clone(), "DeviceCertV2.data_rights")?)?,
            issued_authz_rev: expect_i64_max(fields[8].clone(), "DeviceCertV2.issued_authz_rev")?,
        };
        cert.validate()?;
        Ok(cert)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_version(self.version)?;
        check_nonzero("DeviceCertV2.instance_id", &self.instance_id)?;
        check_nonzero("DeviceCertV2.vault_id", &self.vault_id)?;
        check_nonzero("DeviceCertV2.device_id", &self.device_id)?;
        check_nonzero("DeviceCertV2.sign_ed25519_pub", &self.sign_ed25519_pub)?;
        check_nonzero("DeviceCertV2.hpke_x25519_pub", &self.hpke_x25519_pub)?;
        if self.cert_serial == 0 {
            return Err(ProtoError::Invalid(
                "DeviceCertV2.cert_serial 必须 >0".to_owned(),
            ));
        }
        // 只允许 READ/WRITE；不得把管理 scope 塞进数据权限位。
        Rights::new(self.data_rights.bits())?;
        check_i64_max(self.cert_serial, "DeviceCertV2.cert_serial")?;
        check_i64_max(self.issued_authz_rev, "DeviceCertV2.issued_authz_rev")?;
        if self.issued_authz_rev == 0 {
            return Err(ProtoError::Invalid(
                "DeviceCertV2.issued_authz_rev 必须 >0".to_owned(),
            ));
        }
        Ok(())
    }
}

// —— GrantAadV2 / GrantPackageV2 ——

/// `GrantAadV2`（键 `0..9`）：HPKE grant 的 AAD；与 v1 的 `GrantAadV1` 不兼容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantAadV2 {
    pub version: u16,
    pub instance_id: RelayInstanceId,
    pub vault_id: VaultId,
    pub epoch: u64,
    pub recipient_device_id: DeviceId,
    pub recipient_hpke_pub: X25519PublicKey,
    pub authority_rev: u64,
    pub grant_id: GrantIdV2,
    pub issued_authz_rev: u64,
}

impl GrantAadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.version))),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.vault_id.to_vec())),
            (3, CdeValue::UInt(self.epoch)),
            (4, CdeValue::Bytes(self.recipient_device_id.to_vec())),
            (5, CdeValue::Bytes(self.recipient_hpke_pub.to_vec())),
            (6, CdeValue::UInt(self.authority_rev)),
            (7, CdeValue::Bytes(self.grant_id.to_vec())),
            (8, CdeValue::UInt(self.issued_authz_rev)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, GRANT_AAD_V2_FIELD_COUNT)?;
        let aad = Self {
            version: expect_u16(fields[0].clone(), "GrantAadV2.version")?,
            instance_id: expect_bytes_n::<16>(fields[1].clone(), "GrantAadV2.instance_id")?,
            vault_id: expect_bytes_n::<16>(fields[2].clone(), "GrantAadV2.vault_id")?,
            epoch: expect_i64_max(fields[3].clone(), "GrantAadV2.epoch")?,
            recipient_device_id: expect_bytes_n::<16>(
                fields[4].clone(),
                "GrantAadV2.recipient_device_id",
            )?,
            recipient_hpke_pub: expect_bytes_n::<32>(
                fields[5].clone(),
                "GrantAadV2.recipient_hpke_pub",
            )?,
            authority_rev: expect_i64_max(fields[6].clone(), "GrantAadV2.authority_rev")?,
            grant_id: expect_bytes_n::<16>(fields[7].clone(), "GrantAadV2.grant_id")?,
            issued_authz_rev: expect_i64_max(fields[8].clone(), "GrantAadV2.issued_authz_rev")?,
        };
        aad.validate()?;
        Ok(aad)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_version(self.version)?;
        check_nonzero("GrantAadV2.instance_id", &self.instance_id)?;
        check_nonzero("GrantAadV2.vault_id", &self.vault_id)?;
        check_nonzero("GrantAadV2.recipient_device_id", &self.recipient_device_id)?;
        check_nonzero("GrantAadV2.recipient_hpke_pub", &self.recipient_hpke_pub)?;
        check_nonzero("GrantAadV2.grant_id", &self.grant_id)?;
        if self.epoch == 0 {
            return Err(ProtoError::Invalid("GrantAadV2.epoch 必须 >0".to_owned()));
        }
        check_i64_max(self.epoch, "GrantAadV2.epoch")?;
        check_i64_max(self.authority_rev, "GrantAadV2.authority_rev")?;
        check_i64_max(self.issued_authz_rev, "GrantAadV2.issued_authz_rev")?;
        if self.authority_rev == 0 {
            return Err(ProtoError::Invalid(
                "GrantAadV2.authority_rev 必须 >0".to_owned(),
            ));
        }
        if self.issued_authz_rev == 0 {
            return Err(ProtoError::Invalid(
                "GrantAadV2.issued_authz_rev 必须 >0".to_owned(),
            ));
        }
        Ok(())
    }

    /// AAD 字节 = `CDE(GrantAadV2)`。
    pub fn aad_bytes(&self) -> Result<Vec<u8>, ProtoError> {
        self.encode()
    }
}

/// grant 包 `{0:aad_bytes,1:enc,2:ciphertext}`；AAD 以类型化对象保存并在编解码时校验 canonical。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantPackageV2 {
    pub aad: GrantAadV2,
    pub enc: [u8; 32],
    pub ciphertext: Vec<u8>,
}

impl GrantPackageV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.aad.aad_bytes()?)),
            (1, CdeValue::Bytes(self.enc.to_vec())),
            (2, CdeValue::Bytes(self.ciphertext.clone())),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, GRANT_PACKAGE_V2_FIELD_COUNT)?;
        let aad_bytes = expect_bytes(fields[0].clone(), "GrantPackageV2.aad")?;
        let package = Self {
            aad: GrantAadV2::decode(&aad_bytes)?,
            enc: expect_bytes_n::<32>(fields[1].clone(), "GrantPackageV2.enc")?,
            ciphertext: expect_bytes(fields[2].clone(), "GrantPackageV2.ciphertext")?,
        };
        package.validate()?;
        Ok(package)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        self.aad.validate()?;
        check_nonzero("GrantPackageV2.enc", &self.enc)?;
        if self.ciphertext.is_empty() {
            return Err(ProtoError::Invalid(
                "GrantPackageV2.ciphertext 不能为空".to_owned(),
            ));
        }
        Ok(())
    }
}

// —— ActivationV2 ——

/// `ActivationV2`（键 `0..13`）：V2 epoch 激活材料；上下文/前链头由 `OperationV2` 绑定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationV2 {
    pub version: u16,
    pub rotation_id: RotationIdV2,
    pub from_epoch: u64,
    pub to_epoch: u64,
    /// 冻结截止 S。
    pub freeze_seq: u64,
    pub primary_cid: [u8; 16],
    pub primary_commit_hash: Sha256,
    pub fallback_cid: [u8; 16],
    pub fallback_commit_hash: Sha256,
    pub slot_set_hash: Sha256,
    pub member_cert_hash: Sha256,
    pub grants_hash: Sha256,
    pub old_asset_hash: Sha256,
    pub new_asset_hash: Sha256,
}

impl ActivationV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.version))),
            (1, CdeValue::Bytes(self.rotation_id.to_vec())),
            (2, CdeValue::UInt(self.from_epoch)),
            (3, CdeValue::UInt(self.to_epoch)),
            (4, CdeValue::UInt(self.freeze_seq)),
            (5, CdeValue::Bytes(self.primary_cid.to_vec())),
            (6, CdeValue::Bytes(self.primary_commit_hash.to_vec())),
            (7, CdeValue::Bytes(self.fallback_cid.to_vec())),
            (8, CdeValue::Bytes(self.fallback_commit_hash.to_vec())),
            (9, CdeValue::Bytes(self.slot_set_hash.to_vec())),
            (10, CdeValue::Bytes(self.member_cert_hash.to_vec())),
            (11, CdeValue::Bytes(self.grants_hash.to_vec())),
            (12, CdeValue::Bytes(self.old_asset_hash.to_vec())),
            (13, CdeValue::Bytes(self.new_asset_hash.to_vec())),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, ACTIVATION_V2_FIELD_COUNT)?;
        let activation = Self {
            version: expect_u16(fields[0].clone(), "ActivationV2.version")?,
            rotation_id: expect_bytes_n::<16>(fields[1].clone(), "ActivationV2.rotation_id")?,
            from_epoch: expect_i64_max(fields[2].clone(), "ActivationV2.from_epoch")?,
            to_epoch: expect_i64_max(fields[3].clone(), "ActivationV2.to_epoch")?,
            freeze_seq: expect_i64_max(fields[4].clone(), "ActivationV2.freeze_seq")?,
            primary_cid: expect_bytes_n::<16>(fields[5].clone(), "ActivationV2.primary_cid")?,
            primary_commit_hash: expect_bytes_n::<32>(
                fields[6].clone(),
                "ActivationV2.primary_commit_hash",
            )?,
            fallback_cid: expect_bytes_n::<16>(fields[7].clone(), "ActivationV2.fallback_cid")?,
            fallback_commit_hash: expect_bytes_n::<32>(
                fields[8].clone(),
                "ActivationV2.fallback_commit_hash",
            )?,
            slot_set_hash: expect_bytes_n::<32>(fields[9].clone(), "ActivationV2.slot_set_hash")?,
            member_cert_hash: expect_bytes_n::<32>(
                fields[10].clone(),
                "ActivationV2.member_cert_hash",
            )?,
            grants_hash: expect_bytes_n::<32>(fields[11].clone(), "ActivationV2.grants_hash")?,
            old_asset_hash: expect_bytes_n::<32>(
                fields[12].clone(),
                "ActivationV2.old_asset_hash",
            )?,
            new_asset_hash: expect_bytes_n::<32>(
                fields[13].clone(),
                "ActivationV2.new_asset_hash",
            )?,
        };
        activation.validate()?;
        Ok(activation)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_version(self.version)?;
        for (name, value) in [
            ("ActivationV2.rotation_id", &self.rotation_id[..]),
            ("ActivationV2.primary_cid", &self.primary_cid[..]),
            (
                "ActivationV2.primary_commit_hash",
                &self.primary_commit_hash[..],
            ),
            ("ActivationV2.fallback_cid", &self.fallback_cid[..]),
            (
                "ActivationV2.fallback_commit_hash",
                &self.fallback_commit_hash[..],
            ),
            ("ActivationV2.slot_set_hash", &self.slot_set_hash[..]),
            ("ActivationV2.member_cert_hash", &self.member_cert_hash[..]),
            ("ActivationV2.grants_hash", &self.grants_hash[..]),
            ("ActivationV2.old_asset_hash", &self.old_asset_hash[..]),
            ("ActivationV2.new_asset_hash", &self.new_asset_hash[..]),
        ] {
            check_nonzero(name, value)?;
        }
        for (name, value) in [
            ("ActivationV2.from_epoch", self.from_epoch),
            ("ActivationV2.to_epoch", self.to_epoch),
            ("ActivationV2.freeze_seq", self.freeze_seq),
        ] {
            check_i64_max(value, name)?;
        }
        if self.from_epoch == 0 {
            return Err(ProtoError::Invalid(
                "ActivationV2.from_epoch 必须 >0".to_owned(),
            ));
        }
        if self.to_epoch <= self.from_epoch {
            return Err(ProtoError::Invalid(format!(
                "ActivationV2.to_epoch={} 必须大于 from_epoch={}",
                self.to_epoch, self.from_epoch
            )));
        }
        Ok(())
    }
}
