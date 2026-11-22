//! ADR 0016 **S4d1a**：客户端 V2 管理基础（控制链回放 / cap 缓存与选择 / realm3 鉴权）。
//!
//! ## 范围
//!
//! 本模块只做"从**已验证控制链**推导授权状态"与"用管理 bearer 选定本机 cap"：
//!
//! 1. **控制链回放**（[`V2ControlReplay`]）：用设备 token（realm 1）分页
//!    `GET /v2/vaults/{V}/control?after_rev=N`（≤256/页），对每条 `SignedControlV2` 严格
//!    解码、复算 `SHA256(prev || signed_bytes)`、校验 rev 连续/时间窗/signers 顺序/签名域/
//!    设备 ACTIVE/cap 有效/双签门限与 scope，再按 kind 应用成 [`ControlStateV2`]。
//!    失败一律 fail-stop（[`SyncError`]），**不**相信 relay 的任何摘要或投影。
//! 2. **cap 缓存/选择**（[`ControlStateV2::select_cap`]）：本机 device/key、当前 authority、
//!    未撤销、时间有效、**单张 cap 覆盖全部所需 scope**（绝不拼 cap/拼人）。缓存落在
//!    vault SQLite 公开 meta（可重建）；V2 control pin 存 `I/V/head/authz/authority` +
//!    缓存摘要，损坏即 fail-stop（ADR 0013 §2 的同一纪律）。
//! 3. **realm3 鉴权**（[`V2ManagementClient::authorize_management`]）：
//!    `/v2/vaults/{V}/authz/challenges`（realm 3 + cap_id）→ 严格验证
//!    [`ChallengeV2`]（origin/vault/instance/两个 rev/cap）→ 签
//!    `MN/v2/auth\0 || challenge` → `/authz/tokens` → 保存 token 与 cap/revs。
//!    每次管理请求先 `refresh_control` 再取 token；token 的两个 rev 必须**等于**当前
//!    回放值（不是 `<`）。管理 bearer 只"选定一个 cap"，**不代替**管理签名。
//! 4. **控制提交（S4d1b 的底层入口）**（[`V2ManagementClient::submit_control_bytes`]）：
//!    把**已满签** `SignedControlV2` 发到唯一提交路径（自证、不带 bearer），严格解析回执；
//!    `ApprovalEnvelopeV1`/typed 构造/双签状态机/对账在 `crate::approval_v1`
//!    （它用 [`V2ControlReplay::validate_pending_control`] 与链上回放走同一条验证路径）。
//!
//! ## 非目标
//!
//! - 本模块不构造/签名 `OperationV2`、不做 `ApprovalEnvelopeV1`/UI（见 `approval_v1`）；
//! - 不执行 rotation 6–10 的客户端数据切换（只回放授权/成员事实）；
//! - 不改 relay、不改 v1 wire：legacy vault 的 v1 鉴权/控制链路径完全不受影响
//!   （本模块只在 `authz_state != legacy` 后生效；legacy 会返回明确的"未启用 V2"）。
//!
//! ## 与 relay 的关系
//!
//! relay 的 `BEGIN IMMEDIATE` 内校验是**服务端事实**；本模块按同一 wire 规则在客户端
//! 复算授权事实，但**不复制** relay 的 DB 语义。两者冲突时 fail-stop，而不是"选一个信"。
//!
//! ### 客户端**无法**从链上独立证明的事实（明确记录，不假装验证过）
//!
//! - **PENDING 设备登记公钥绑定**：兑换邀请时提交的 `sign_pk`/`hpke_pk` 只进 relay DB，
//!   不在控制链上。客户端能验证的是"目标设备 PoP（`MN/v2/pop\0||h||pk32`）证明它确实
//!   持有证书里的私钥"，binding 到兑换事实只能由 relay 保证；
//! - **UpgradePolicy 的 `old_manager_rev`/ACTIVE 清单完整性**：由 pin 住的管理公钥对整条
//!   Operation 的签名兜底（清单被管理根授权），逐字段与 v1 DB 的比对属 relay 事实；
//! - **EpochActivate 的 `member_cert_hash`/`grants_hash`/资产摘要**：v1 时代设备的
//!   `devices.certificate` 是 `DeviceCertV1` 字节，不在 V2 链上，客户端无法重算。
//!   本切片只回放 `to_epoch`/成员变化（授权影响），不把数据面摘要当授权依据；
//! - **邀请是否已兑换**：兑换不是控制记录。客户端只知道"已创建且未取消"。

use std::collections::{BTreeMap, BTreeSet};

use menghunote_storage::{AffectedGuard, Statement, StorageError, Value, Vault};
use menghunote_sync_proto::{
    auth_signing_input_v2, canonical_submit_path, control_signing_input, pop_signing_input,
    recover_signing_input, upgrade_signing_input, CapGrantPayloadV2, CapRevokePayloadV2, CapScope,
    CdeValue, ChallengeV2, ControlKindV2, DeviceCertV2, DeviceCredentialPayloadV2,
    DeviceRevokePayloadV2, EpochActivatePayloadV2, GrantPackageV2, InviteCancelPayloadV2,
    InviteCreatePayloadV2, ManagementCapV2, OperationV2, PolicyV2, RecoveryKeyChangePayloadV2,
    RecoveryResetPayloadV2, RotationAbortPayloadV2, RotationAddRevokesPayloadV2,
    RotationBeginPayloadV2, RotationRestartPayloadV2, SignedControlV2, SignerRole, SignerV2,
    UpgradePolicyPayloadV2, CHALLENGE_V2_REALM_DEVICE, CHALLENGE_V2_REALM_MANAGEMENT_DEVICE,
};
use sha2::{Digest, Sha256};

use crate::api_version::{RelayApi, RelayApiVersion};
use crate::cbor;
use crate::device::{verify_signature, DeviceSigningKey};
use crate::error::SyncError;
use crate::http_relay::{Clock, SystemClock, CONTENT_TYPE_CBOR};
use crate::http_transport::{HttpRequest, HttpResponse, HttpTransport};
use crate::{DeviceId, VaultId};

// ---------------------------------------------------------------- 常量

/// V2 control pin 的 meta 键（`I/V/head/authz/authority` + 缓存摘要）。
pub const META_CONTROL_PIN_V2: &str = "sync.control_pin_v2";
/// V2 授权缓存（可重建）的 meta 键。
pub const META_MANAGEMENT_CACHE_V2: &str = "sync.management_v2_cache";
/// control pin 结构版本。
pub const CONTROL_PIN_V2_VERSION: u64 = 1;
/// 授权缓存结构版本。
pub const MANAGEMENT_CACHE_V2_VERSION: u64 = 1;

/// relay `/control` 的固定单页上限（服务端 256）。
const CONTROL_PAGE_LIMIT: usize = 256;
/// 分页安全上限（防恶意 relay"每页 1 条 + 无限转发"）。
const MAX_CONTROL_PAGES: usize = 4096;
/// 控制链最多接受的记录数（防分页内存放大）。
const MAX_CONTROL_RECORDS: u64 = 65_536;

/// 提交时间窗（与 relay 同值，ADR 0015 §能力、门限与恢复）。
const SIGNED_AT_FUTURE_SKEW: u64 = 30;
/// `not_after - signed_at` 上限（必须短窗）。
const DEADLINE_MAX_SECS: u64 = 300;
/// challenge 允许的本地时钟偏差窗口（客户端只做上界检查）。
const CHALLENGE_MAX_AHEAD_SECS: u64 = 120;
/// token 提前刷新窗口（realm 1 / realm 3 相同：600s 里提前 60s）。
const TOKEN_REFRESH_AHEAD_SECS: u64 = 60;

/// 高危 scope（撤销不得把其中任一的两设备 quorum 降到 2 以下）。
const HIGH_RISK_SCOPES: [u32; 5] = [
    CapScope::APPROVE,
    CapScope::REVOKE,
    CapScope::CAP_ADMIN,
    CapScope::ROTATE,
    CapScope::POLICY,
];

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn proto_hash(bytes: &[u8]) -> [u8; 32] {
    sha256(bytes)
}

fn invariant(message: &'static str) -> SyncError {
    SyncError::RelayWire(message)
}

// ---------------------------------------------------------------- 授权状态投影

/// V2 授权状态（`legacy` 不进入本状态机：`None` 表示还没有 V2 控制记录）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthzStateV2 {
    /// 正常：管理面可用（仍需 cap 门限）。
    Active,
    /// 可用设备不足两台：只许离线 `RecoveryReset`（realm 3 不再签出有效 bearer）。
    RecoveryOnly,
}

impl AuthzStateV2 {
    const fn as_code(self) -> u64 {
        match self {
            Self::Active => 1,
            Self::RecoveryOnly => 2,
        }
    }

    const fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::Active),
            2 => Some(Self::RecoveryOnly),
            _ => None,
        }
    }

    /// 诊断/展示用的稳定文本。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::RecoveryOnly => "recovery_only",
        }
    }
}

/// 一张已在链上验证过的 cap（来源 rev + 撤销投影）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapEntryV2 {
    /// canonical cap（公开授权事实）。
    pub cap: ManagementCapV2,
    /// 授予它的控制提交 rev（`issued_control_rev` 的交叉校验锚点）。
    pub granted_rev: u64,
    /// 撤销它的控制提交 rev；`None` = 未撤销（**自然到期不造记录**）。
    pub revoked_control_rev: Option<u64>,
}

impl CapEntryV2 {
    /// 是否命中显式撤销（自然到期看 [`Self::cap`] 的 `expires_at`）。
    #[must_use]
    pub const fn is_revoked(&self) -> bool {
        self.revoked_control_rev.is_some()
    }
}

/// 链上设备状态（`Pending` 不出现在控制链上：兑换不是控制记录）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStatusV2 {
    /// 已批准并持有 `DeviceCertV2`。
    Active,
    /// 已签名撤销（永久事实，绝不复活）。
    Revoked,
}

/// 一台设备的回放事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceEntryV2 {
    /// 当前证书（最近一次 kind 4/16/13 的 `DeviceCertV2`）。
    pub cert: DeviceCertV2,
    /// 状态。
    pub status: DeviceStatusV2,
    /// 当前证书的批准控制 rev。
    pub approved_rev: u64,
    /// 撤销控制 rev（仅 `Revoked`）。
    pub revoked_rev: Option<u64>,
    /// 按 epoch 索引的 grant 包（公开材料；本切片不消费明文）。
    pub grants: BTreeMap<u64, GrantPackageV2>,
}

/// 链上邀请：只代表"已签名创建、未签名取消"；**兑换**是 relay 事实（不在链上）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteEntryV2 {
    /// 邀请 ID。
    pub invite_id: [u8; 16],
    /// `SHA256(T)`（relay 永不落明文）。
    pub token_hash: [u8; 32],
    /// 创建时指定的到期时刻。
    pub expires_at: u64,
    /// 签发它的 cap。
    pub issuer_cap_id: [u8; 16],
    /// 创建控制 rev。
    pub created_rev: u64,
    /// 取消控制 rev（`None` = 未取消）。
    pub cancelled_rev: Option<u64>,
}

/// 轮换回放状态（**只记录授权/成员事实**，不做客户端数据切换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationStateV2 {
    /// `RotationBegin` 之后、候选 epoch 冻结中。
    Frozen,
    /// `RotationAbort`。
    Aborted,
    /// `RotationRestart`（旧候选废弃、新候选接管）。
    Restarted,
    /// `EpochActivate`。
    Activated,
}

impl RotationStateV2 {
    const fn as_code(self) -> u64 {
        match self {
            Self::Frozen => 1,
            Self::Aborted => 2,
            Self::Restarted => 3,
            Self::Activated => 4,
        }
    }

    const fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::Frozen),
            2 => Some(Self::Aborted),
            3 => Some(Self::Restarted),
            4 => Some(Self::Activated),
            _ => None,
        }
    }
}

/// 一条轮换的回放事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationRecordV2 {
    /// 轮换 ID（永不复用）。
    pub rotation_id: [u8; 16],
    /// 源 epoch。
    pub from_epoch: u64,
    /// 候选 epoch。
    pub to_epoch: u64,
    /// 冻结截止 S。
    pub freeze_seq: u64,
    /// 状态。
    pub state: RotationStateV2,
    /// `RotationBegin` 时冻结的成员集合摘要（`SHA256(concat(sorted active ids))`）。
    pub member_set_hash: [u8; 32],
    /// 创建控制 rev。
    pub created_rev: u64,
    /// 终结（abort/activate/restart）控制 rev。
    pub terminal_rev: Option<u64>,
}

/// 从**已验证控制链**回放出的 V2 授权状态。
///
/// 所有字段都只由控制记录推导；relay 的 `member.status`/`/state` 投影**不进入**这里。
/// 缓存（[`Self::encode_cache`]）只是加速重建，不是信任根。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlStateV2 {
    vault_id: VaultId,
    instance_id: [u8; 16],
    rev: u64,
    head_hash: [u8; 32],
    authority_rev: u64,
    authz_rev: u64,
    authz_state: Option<AuthzStateV2>,
    policy: Option<PolicyV2>,
    recovery_pub: Option<[u8; 32]>,
    caps: BTreeMap<[u8; 16], CapEntryV2>,
    devices: BTreeMap<[u8; 16], DeviceEntryV2>,
    invites: BTreeMap<[u8; 16], InviteEntryV2>,
    rotations: BTreeMap<[u8; 16], RotationRecordV2>,
    /// 历史上出现过的 cap id（含已撤销）：**永不复用**（relay 主键事实）。
    cap_ids_seen: BTreeSet<[u8; 16]>,
    /// 历史上出现过的 grant id：永不复用。
    grant_ids: BTreeSet<[u8; 16]>,
    /// 链上被撤销、但客户端从未见过账号信息的设备（PENDING 兑换不在链上）：
    /// 后续任何 add/批准一律拒绝（不复活）。
    revoked_unseen: BTreeSet<[u8; 16]>,
}

impl ControlStateV2 {
    fn new(vault_id: VaultId, instance_id: [u8; 16]) -> Self {
        Self {
            vault_id,
            instance_id,
            rev: 0,
            head_hash: [0u8; 32],
            authority_rev: 0,
            authz_rev: 0,
            authz_state: None,
            policy: None,
            recovery_pub: None,
            caps: BTreeMap::new(),
            devices: BTreeMap::new(),
            invites: BTreeMap::new(),
            rotations: BTreeMap::new(),
            cap_ids_seen: BTreeSet::new(),
            grant_ids: BTreeSet::new(),
            revoked_unseen: BTreeSet::new(),
        }
    }

    /// 绑定的 vault id。
    #[must_use]
    pub const fn vault_id(&self) -> VaultId {
        self.vault_id
    }

    /// 绑定的 relay instance id（来自本地 pin/身份，不来自 relay 投影）。
    #[must_use]
    pub const fn instance_id(&self) -> [u8; 16] {
        self.instance_id
    }

    /// 已验证链尾 `control_rev`（回放水位）。
    #[must_use]
    pub const fn rev(&self) -> u64 {
        self.rev
    }

    /// 已验证链尾 `record_hash`。
    #[must_use]
    pub const fn head_hash(&self) -> [u8; 32] {
        self.head_hash
    }

    /// 当前离线根代（`authority_rev`）。
    #[must_use]
    pub const fn authority_rev(&self) -> u64 {
        self.authority_rev
    }

    /// 当前授权版本（`authz_rev`）。
    #[must_use]
    pub const fn authz_rev(&self) -> u64 {
        self.authz_rev
    }

    /// 授权状态；`None` = 还没有任何 V2 控制记录（legacy）。
    #[must_use]
    pub const fn authz_state(&self) -> Option<AuthzStateV2> {
        self.authz_state
    }

    /// 是否已启用 V2 授权（链上出现过 `UpgradePolicy`）。
    #[must_use]
    pub const fn is_v2(&self) -> bool {
        self.authz_state.is_some()
    }

    /// 当前 `PolicyV2`（Upgrade 之后必有）。
    #[must_use]
    pub const fn policy(&self) -> Option<&PolicyV2> {
        self.policy.as_ref()
    }

    /// 当前离线恢复根公钥。
    #[must_use]
    pub const fn recovery_pub(&self) -> Option<[u8; 32]> {
        self.recovery_pub
    }

    /// 全部已知 cap（含已撤销；按 cap_id 升序）。
    pub fn caps(&self) -> impl Iterator<Item = &CapEntryV2> {
        self.caps.values()
    }

    /// 一张 cap（按 id）。
    #[must_use]
    pub fn cap(&self, cap_id: &[u8; 16]) -> Option<&CapEntryV2> {
        self.caps.get(cap_id)
    }

    /// 全部已知设备（含已撤销；按 device_id 升序）。
    pub fn devices(&self) -> impl Iterator<Item = &DeviceEntryV2> {
        self.devices.values()
    }

    /// 一台设备（按 id）。
    #[must_use]
    pub fn device(&self, device_id: &[u8; 16]) -> Option<&DeviceEntryV2> {
        self.devices.get(device_id)
    }

    /// 全部链上邀请（按 invite_id 升序）。
    pub fn invites(&self) -> impl Iterator<Item = &InviteEntryV2> {
        self.invites.values()
    }

    /// 全部轮换记录（按 rotation_id 升序）。
    pub fn rotations(&self) -> impl Iterator<Item = &RotationRecordV2> {
        self.rotations.values()
    }

    /// 当前 ACTIVE 设备数。
    #[must_use]
    pub fn active_device_count(&self) -> usize {
        self.devices
            .values()
            .filter(|device| device.status == DeviceStatusV2::Active)
            .count()
    }

    /// 某设备是否 ACTIVE。
    #[must_use]
    pub fn is_active_device(&self, device_id: &[u8; 16]) -> bool {
        self.devices
            .get(device_id)
            .is_some_and(|device| device.status == DeviceStatusV2::Active)
    }

    /// cap 在 `at` 时刻是否**此刻有效**（ADR 0015）：
    /// 已提交、未撤销、`issuer_authority_rev` 等于当前代、时间窗内、主体 ACTIVE 且
    /// 登记公钥匹配。`issued_authz_rev` 只是历史证据，不参与。
    #[must_use]
    pub fn cap_is_effective_at(&self, entry: &CapEntryV2, at: u64) -> bool {
        let cap = &entry.cap;
        if entry.is_revoked() {
            return false;
        }
        if cap.instance_id != self.instance_id || cap.vault_id != self.vault_id {
            return false;
        }
        if cap.issuer_authority_rev != self.authority_rev {
            return false;
        }
        if at < cap.not_before || at >= cap.expires_at {
            return false;
        }
        let Some(device) = self.devices.get(&cap.subject_device_id) else {
            return false;
        };
        device.status == DeviceStatusV2::Active && device.cert.sign_ed25519_pub == cap.subject_ed_pk
    }

    /// 选择**一张**覆盖全部 `required` scope 的 cap（绝不拼 cap/拼人）。
    ///
    /// 确定性选择：先按"多余 scope 位更少"（更贴合权限最小化），再按"到期更晚"，
    /// 最后按 cap_id 升序。无可用 cap 时返回带原因的错误（`Unsupported`）。
    pub fn select_cap(
        &self,
        device_id: &[u8; 16],
        device_pk: &[u8; 32],
        required: CapScope,
        now: u64,
    ) -> Result<&CapEntryV2, SyncError> {
        let state = self.authz_state.ok_or(SyncError::Unsupported(
            "vault 尚未启用 V2 授权（没有已回放的 UpgradePolicy）",
        ))?;
        if state == AuthzStateV2::RecoveryOnly {
            return Err(SyncError::Unsupported(
                "授权状态 recovery_only：管理面关闭，只能离线 RecoveryReset",
            ));
        }
        let Some(device) = self.devices.get(device_id) else {
            return Err(SyncError::Unsupported(
                "本机设备不在已验证控制链的成员集合里（未批准或链不完整）",
            ));
        };
        if device.status != DeviceStatusV2::Active {
            return Err(SyncError::Unsupported("本机设备已被撤销（永不复活）"));
        }
        if device.cert.sign_ed25519_pub != *device_pk {
            return Err(SyncError::Unsupported(
                "本机设备公钥与已验证证书不一致（身份被换过或链不完整）",
            ));
        }
        let mut candidates: Vec<&CapEntryV2> = self
            .caps
            .values()
            .filter(|entry| {
                entry.cap.subject_device_id == *device_id
                    && entry.cap.subject_ed_pk == *device_pk
                    && entry.cap.scopes.contains(required)
                    && self.cap_is_effective_at(entry, now)
            })
            .collect();
        if candidates.is_empty() {
            // 只为错误信息做分类（不改变"必须单张 cap"的判定）。
            let owned: Vec<&CapEntryV2> = self
                .caps
                .values()
                .filter(|entry| {
                    entry.cap.subject_device_id == *device_id
                        && entry.cap.subject_ed_pk == *device_pk
                })
                .collect();
            if owned.is_empty() {
                return Err(SyncError::Unsupported(
                    "本机设备没有已验证来源的 cap（先由两台 CAP_ADMIN 设备授予）",
                ));
            }
            if owned.iter().all(|entry| entry.is_revoked()) {
                return Err(SyncError::Unsupported("本机 cap 已被撤销"));
            }
            if owned
                .iter()
                .all(|entry| entry.cap.issuer_authority_rev != self.authority_rev)
            {
                return Err(SyncError::Unsupported(
                    "本机 cap 属于旧 authority 代（换恢复钥/Reset 后失效）",
                ));
            }
            if owned
                .iter()
                .all(|entry| now >= entry.cap.expires_at || now < entry.cap.not_before)
            {
                return Err(SyncError::Unsupported(
                    "本机 cap 不在有效时间窗内（自然到期即时失效）",
                ));
            }
            return Err(SyncError::Unsupported(
                "没有单张 cap 覆盖全部所需 scope（不允许拼接多张 cap 的权限）",
            ));
        }
        candidates.sort_by(|left, right| {
            let left_extra = left.cap.scopes.bits() & !required.bits();
            let right_extra = right.cap.scopes.bits() & !required.bits();
            left_extra
                .count_ones()
                .cmp(&right_extra.count_ones())
                .then_with(|| right.cap.expires_at.cmp(&left.cap.expires_at))
                .then_with(|| left.cap.cap_id.cmp(&right.cap.cap_id))
        });
        candidates
            .first()
            .copied()
            .ok_or(SyncError::Unsupported("没有可用的管理 cap"))
    }
}

// ---------------------------------------------------------------- 回放器

/// `GET /control` 的一页提交（wire 已验证过 cabability 由调用方负责）。
#[derive(Debug, Clone)]
struct ControlCommitV2 {
    rev: u64,
    prev_hash: [u8; 32],
    record_hash: [u8; 32],
    signed_bytes: Vec<u8>,
}

/// 一条 V2 控制记录的**验证后效果**（验证与执行分两段，避免持有 `&mut self`）。
enum EffectV2 {
    Upgrade {
        policy: PolicyV2,
        recovery_pub: [u8; 32],
        caps: Vec<ManagementCapV2>,
        manifest: Vec<DeviceCertV2>,
    },
    CapGrant(ManagementCapV2),
    CapRevoke([u8; 16]),
    DeviceApproval {
        cert: DeviceCertV2,
        grants: Vec<GrantPackageV2>,
        new_credential: bool,
    },
    DeviceRevoke {
        targets: Vec<[u8; 16]>,
        changed: bool,
    },
    RotationBegin {
        rotation_id: [u8; 16],
        from_epoch: u64,
        to_epoch: u64,
        revoke_ids: Vec<[u8; 16]>,
        member_set_hash: [u8; 32],
    },
    RotationAbort {
        rotation_id: [u8; 16],
    },
    RotationRestart {
        old_rotation_id: [u8; 16],
        new_to_epoch: u64,
        extra_ids: Vec<[u8; 16]>,
        member_set_hash: [u8; 32],
    },
    RotationAddRevokes {
        rotation_id: [u8; 16],
        device_ids: Vec<[u8; 16]>,
    },
    EpochActivate {
        rotation_id: [u8; 16],
        freeze_seq: u64,
        member_changed: bool,
    },
    InviteCreate(InviteEntryV2),
    InviteCancel([u8; 16]),
    RecoveryReset {
        policy: PolicyV2,
        recovery_pub: [u8; 32],
        caps: Vec<ManagementCapV2>,
        adds: Vec<(DeviceCertV2, Vec<GrantPackageV2>)>,
        revoke_ids: Vec<[u8; 16]>,
    },
    RecoveryKeyChange {
        recovery_pub: [u8; 32],
        caps: Vec<ManagementCapV2>,
    },
}

/// 签名校验模式（只用于**待提交**操作的干跑验证，不用于链上回放）。
///
/// 半签的 `ApprovalEnvelopeV1` 缺少部分签名槽，无法编码成合法的
/// [`SignedControlV2`]（proto 要求签名数 == signer 数且不得用零占位）；
/// 导入方（S4d1b）先逐槽验证**已存在**的签名，再用 [`SignatureCheckV2::Skip`] 干跑
/// 其余全部语义（角色集合/cap/scope/authority/payload/时间窗/链位置），满签提交时
/// 必须回到 [`SignatureCheckV2::Verify`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignatureCheckV2 {
    /// 真的验签（链上回放与满签提交的默认模式）。
    Verify,
    /// 跳过签名校验（只允许干跑半签包；调用方必须自己验已存在的签名）。
    Skip,
}

/// V2 控制链回放器：从 head=0 起逐条验证并投影授权状态。
#[derive(Debug, Clone)]
pub struct V2ControlReplay {
    state: ControlStateV2,
    /// pin 住的管理公钥（UpgradePolicy 的 role1 信任锚）；`None` 时 Upgrade 一律
    /// `NeedsTrust`（fail-closed）。
    manager_pub: Option<[u8; 32]>,
    /// `rev → record_hash`：本次回放看到的链（用于验证本地 pin 仍是新链前缀）。
    record_hashes: BTreeMap<u64, [u8; 32]>,
    /// 上一条 V2 记录的签名时刻（V2 链签名时间不得倒退）。
    last_signed_at: Option<u64>,
    /// 签名校验模式（链上回放恒为 `Verify`）。
    signature_check: SignatureCheckV2,
}

impl V2ControlReplay {
    /// 新回放器（必须显式给出**本地**instance/vault 期望值）。
    #[must_use]
    pub fn new(vault_id: VaultId, instance_id: [u8; 16]) -> Self {
        Self {
            state: ControlStateV2::new(vault_id, instance_id),
            manager_pub: None,
            record_hashes: BTreeMap::new(),
            last_signed_at: None,
            signature_check: SignatureCheckV2::Verify,
        }
    }

    /// 注入 pin 住的管理公钥（UpgradePolicy 的 role1 信任锚）。
    #[must_use]
    pub fn with_manager_pub(mut self, manager_pub: [u8; 32]) -> Self {
        self.manager_pub = Some(manager_pub);
        self
    }

    /// 由**已验证**缓存恢复（缓存摘要与 pin 一致性由调用方校验）。
    pub(crate) fn from_cached(state: ControlStateV2) -> Self {
        Self {
            state,
            manager_pub: None,
            record_hashes: BTreeMap::new(),
            last_signed_at: None,
            signature_check: SignatureCheckV2::Verify,
        }
    }

    /// 当前状态。
    #[must_use]
    pub const fn state(&self) -> &ControlStateV2 {
        &self.state
    }

    /// 取出状态（持久化/测试用）。
    #[must_use]
    pub fn into_state(self) -> ControlStateV2 {
        self.state
    }

    /// 某 rev 的记录 hash（只覆盖本次回放过的记录）。
    #[must_use]
    pub fn record_hash_at(&self, rev: u64) -> Option<[u8; 32]> {
        self.record_hashes.get(&rev).copied()
    }

    /// pin 住的管理公钥（UpgradePolicy 的 role1 信任锚）。
    #[must_use]
    pub const fn manager_pub(&self) -> Option<[u8; 32]> {
        self.manager_pub
    }

    /// 上一条 V2 记录的签名时刻（链时间不倒退的锚点）。
    #[must_use]
    pub const fn last_signed_at(&self) -> Option<u64> {
        self.last_signed_at
    }

    /// 用**已验证的当前状态**干跑验证一条待提交记录（不改变任何状态）。
    ///
    /// 与链上回放走同一条验证代码路径（[`Self::verify_record`]），因此 kind 门限/角色/
    /// scope/authority/payload 语义/时间窗/链位置（`expected_control_rev`/`prev_control_hash`）
    /// 全部复用：提交前放行的操作在链上必然也放行（除非状态已经前进）。
    ///
    /// `verify_signatures=false` 只用于**半签**审批包的导入验证：调用方必须先逐槽验证
    /// 包内已存在的签名，满签提交时必须传 `true`。
    pub fn validate_pending_control(
        &self,
        signed: &SignedControlV2,
        now: u64,
        verify_signatures: bool,
    ) -> Result<(), SyncError> {
        self.validate_pending_control_at(
            signed,
            self.state.rev,
            self.state.head_hash,
            now,
            verify_signatures,
        )
    }

    /// 与 [`Self::validate_pending_control`] 相同，但**显式**给出链位置。
    ///
    /// 只用于 `UpgradePolicy`（kind 0）：升级前没有 V2 记录，V2 回放无法提供 v1 链尾，
    /// 链位置由调用方从**已验证的 v1 控制链**给出（relay 仍按自己的 DB 校验 CAS）。
    /// 其它 kind 必须走 `state.rev/head`（不得绕过链位置绑定）。
    pub fn validate_pending_control_at(
        &self,
        signed: &SignedControlV2,
        expected_control_rev: u64,
        prev_control_hash: [u8; 32],
        now: u64,
        verify_signatures: bool,
    ) -> Result<(), SyncError> {
        let mut probe = Self {
            state: self.state.clone(),
            manager_pub: self.manager_pub,
            record_hashes: BTreeMap::new(),
            last_signed_at: self.last_signed_at,
            signature_check: if verify_signatures {
                SignatureCheckV2::Verify
            } else {
                SignatureCheckV2::Skip
            },
        };
        probe.verify_record(
            signed,
            expected_control_rev.saturating_add(1),
            prev_control_hash,
            now,
        )?;
        Ok(())
    }

    /// 验签（或按 [`SignatureCheckV2`] 跳过：只允许干跑半签包）。
    fn check_signature(
        &self,
        pk: &[u8; 32],
        input: &[u8],
        signature: &[u8; 64],
    ) -> Result<(), SyncError> {
        if self.signature_check == SignatureCheckV2::Skip {
            return Ok(());
        }
        verify_signature(pk, input, signature)
    }

    /// 应用一条已拉取的控制提交（严格验证 + 投影）。
    ///
    /// 顺序：rev 连续 → prev 链尾 → `SHA256(prev || signed_bytes)` 复算 →
    /// 记录分类（V2/v1）→ V2 严格解码/canonical → `OperationV2` 上下文/时间窗 →
    /// kind 专属门限/签名/scope/authority → 投影。任何一步失败都 fail-stop。
    fn apply_commit(
        &mut self,
        commit: &ControlCommitV2,
        now: u64,
    ) -> Result<Option<ControlKindV2>, SyncError> {
        if commit.rev != self.state.rev.saturating_add(1) {
            return Err(invariant("V2 控制链 rev 不连续（缺页/重排/回滚）"));
        }
        if commit.prev_hash != self.state.head_hash {
            return Err(invariant("V2 控制链 prev_hash 与前链尾不符（替换/分叉）"));
        }
        let computed = {
            let mut input = Vec::with_capacity(32 + commit.signed_bytes.len());
            input.extend_from_slice(&commit.prev_hash);
            input.extend_from_slice(&commit.signed_bytes);
            sha256(&input)
        };
        if computed != commit.record_hash {
            return Err(invariant("控制提交的 record_hash 与复算值不符"));
        }
        if commit.rev > MAX_CONTROL_RECORDS {
            return Err(invariant("控制链记录数超过安全上限"));
        }

        // 记录分类：只有"键 0 是 bytes、键 1 是数组"的 map 才是 V2 记录；
        // 其余（rev1 设备证书、SignedControlRecord、槽激活等）是 v1 历史，跳过投影。
        let effect = match classify_record(&commit.signed_bytes)? {
            RecordClass::Legacy => None,
            RecordClass::V2 => {
                let signed = SignedControlV2::decode(&commit.signed_bytes)?;
                // 链 hash 的输入必须唯一：重新编码后逐字节相同。
                if signed.encode()? != commit.signed_bytes {
                    return Err(invariant("V2 控制记录不是唯一 canonical 编码"));
                }
                let effect = self.verify_record(&signed, commit.rev, commit.prev_hash, now)?;
                Some((signed.operation.kind, effect))
            }
        };

        self.state.rev = commit.rev;
        self.state.head_hash = commit.record_hash;
        self.record_hashes.insert(commit.rev, commit.record_hash);
        let kind = match effect {
            None => None,
            Some((kind, effect)) => {
                self.apply_effect(effect, commit.rev)?;
                Some(kind)
            }
        };
        Ok(kind)
    }

    /// 应用一页 `/control` 响应（键 0 commits、1 control_rev、2 active_epoch）。
    ///
    /// 这是**测试/诊断**用的页级入口：生产路径由 [`V2ManagementClient::refresh_control`]
    /// 负责分页/水位/pin。返回值 = `(page_head_rev, 本页应用的 (rev, kind))`。
    pub fn apply_page(
        &mut self,
        fields: &cbor::Fields,
        now: u64,
    ) -> Result<(u64, Vec<(u64, ControlKindV2)>), SyncError> {
        let page_head =
            cbor::uint_field(fields, 1).map_err(|_| invariant("控制链分页缺 control_rev"))?;
        let commits = cbor::array_field(fields, 0)?;
        if commits.len() > CONTROL_PAGE_LIMIT {
            return Err(invariant("控制链分页超过 256 条（上限）"));
        }
        let mut applied = Vec::new();
        for commit in commits {
            let commit = parse_control_commit(&commit)?;
            if let Some(kind) = self.apply_commit(&commit, now)? {
                applied.push((commit.rev, kind));
            }
        }
        Ok((page_head, applied))
    }

    /// 成员集合摘要：`SHA256(concat(sorted active device ids))`（与 relay 同公式）。
    fn member_set_hash(&self) -> [u8; 32] {
        let mut buf = Vec::with_capacity(self.state.devices.len() * 16);
        for (device_id, device) in &self.state.devices {
            if device.status == DeviceStatusV2::Active {
                buf.extend_from_slice(device_id);
            }
        }
        sha256(&buf)
    }

    // 验证与投影的其余部分在下一个实现段（`impl V2ControlReplay`）。
}

/// 一条记录的 wire 分类：V2 控制记录 vs v1 历史记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordClass {
    /// `{0: bytes(SignedControlV2), 1: [sig64…]}`——必须严格验证，坏字节即 fail-stop。
    V2,
    /// v1 历史（rev1 设备证书、`SignedControlRecord`、槽激活等）：链 hash 仍要连续，
    /// 但不产生 V2 授权事实。
    Legacy,
}

fn classify_record(bytes: &[u8]) -> Result<RecordClass, SyncError> {
    let value = menghunote_sync_proto::decode_cde(bytes)
        .map_err(|_| invariant("控制链记录不是合法 canonical CDE"))?;
    let CdeValue::Map(fields) = value else {
        // v1 从不写非 map 的 signed_record；但为了不把"未知 v1 形状"误判成 V2，
        // 这里按 v1 处理（V2 记录一定是 map）。
        return Ok(RecordClass::Legacy);
    };
    let first = fields
        .iter()
        .find(|(key, _)| *key == 0)
        .map(|(_, value)| value);
    let second = fields
        .iter()
        .find(|(key, _)| *key == 1)
        .map(|(_, value)| value);
    match (first, second) {
        (Some(CdeValue::Bytes(_)), Some(CdeValue::Array(_))) => Ok(RecordClass::V2),
        // `SignedControlRecord` 的键 0 是 text（v1 typed 记录）；rev1 是 `DeviceCertV1`
        // map（键 0 是 uint）；两者都不是 V2。
        _ => Ok(RecordClass::Legacy),
    }
}

impl V2ControlReplay {
    /// 严格校验一条 V2 控制记录并返回待应用效果（不修改状态）。
    fn verify_record(
        &mut self,
        signed: &SignedControlV2,
        rev: u64,
        prev_hash: [u8; 32],
        now: u64,
    ) -> Result<EffectV2, SyncError> {
        let op = &signed.operation;
        if op.instance_id != self.state.instance_id {
            return Err(invariant(
                "V2 控制记录的 instance 与本地 pin 不符（跨 relay）",
            ));
        }
        if op.vault_id != self.state.vault_id {
            return Err(invariant("V2 控制记录的 vault 与本地不符（跨租户）"));
        }
        if op.canonical_path != canonical_submit_path(&self.state.vault_id) {
            return Err(invariant("V2 控制记录不是唯一提交路径"));
        }
        if op.expected_control_rev.saturating_add(1) != rev || op.prev_control_hash != prev_hash {
            return Err(invariant("V2 控制记录未绑定当前链位置（rev/prev hash）"));
        }
        if op.signed_at > now.saturating_add(SIGNED_AT_FUTURE_SKEW) {
            return Err(invariant("V2 控制记录的签名时间在未来（超过允许偏差）"));
        }
        if op.not_after < op.signed_at
            || op.not_after > op.signed_at.saturating_add(DEADLINE_MAX_SECS)
        {
            return Err(invariant("V2 控制记录的 not_after 超出短窗"));
        }
        if let Some(last) = self.last_signed_at {
            if op.signed_at < last {
                return Err(invariant("V2 控制链签名时间倒退"));
            }
        }

        // Upgrade 之外的所有 kind 必须绑定**当前**授权状态与 revs。
        if op.kind != ControlKindV2::UpgradePolicy {
            match self.state.authz_state {
                None => return Err(invariant("V2 控制链缺少 UpgradePolicy 起点")),
                Some(AuthzStateV2::RecoveryOnly) if op.kind != ControlKindV2::RecoveryReset => {
                    return Err(invariant(
                        "recovery_only 只接受离线 RecoveryReset（其余管理写一律拒绝）",
                    ))
                }
                Some(_) => {}
            }
            if op.expected_authz_rev != self.state.authz_rev
                || op.authority_rev != self.state.authority_rev
            {
                return Err(invariant(
                    "V2 控制记录未绑定当前 authz/authority rev（陈旧或跨代）",
                ));
            }
        }

        let effect = match op.kind {
            ControlKindV2::UpgradePolicy => self.verify_upgrade(signed, rev)?,
            ControlKindV2::CapGrant => self.verify_cap_grant(signed, rev)?,
            ControlKindV2::CapRevoke => self.verify_cap_revoke(signed)?,
            ControlKindV2::DeviceApprove | ControlKindV2::DeviceRightsChange => {
                self.verify_device_credential(signed)?
            }
            ControlKindV2::DeviceRevoke => self.verify_device_revoke(signed)?,
            ControlKindV2::RotationBegin => self.verify_rotation_begin(signed)?,
            ControlKindV2::RotationAbort => self.verify_rotation_abort(signed)?,
            ControlKindV2::RotationRestart => self.verify_rotation_restart(signed)?,
            ControlKindV2::RotationAddRevokes => self.verify_rotation_add_revokes(signed)?,
            ControlKindV2::EpochActivate => self.verify_epoch_activate(signed)?,
            ControlKindV2::InviteCreate => self.verify_invite_create(signed, rev)?,
            ControlKindV2::InviteCancel => self.verify_invite_cancel(signed)?,
            ControlKindV2::RecoveryReset => self.verify_recovery_reset(signed, rev)?,
            ControlKindV2::RecoveryKeyChange => self.verify_recovery_key_change(signed, rev)?,
            // 已知但本客户端不受理：fail-stop（绝不静默接受）。
            ControlKindV2::PolicyReplace => {
                return Err(SyncError::Unsupported(
                    "客户端不支持 PolicyReplace（kind 1）：拒绝回放",
                ))
            }
        };
        self.last_signed_at = Some(op.signed_at);
        Ok(effect)
    }

    // ------------------------------------------------------------ 门限/签名辅助

    /// 两签门限 + 签名验证：`role0` 必须恰好 2 个、不同 device id 与公钥，各自 cap
    /// **在签名时刻**有效且**本人**具备全部 `required` scope；`allowed_extras` 必须
    /// 逐项一致（多/少/重复都拒绝）。
    fn device_signers<'a>(
        &'a self,
        signed: &'a SignedControlV2,
        required: &[u32],
        allowed_extras: &[SignerRole],
        at: u64,
    ) -> Result<[(&'a SignerV2, &'a CapEntryV2); 2], SyncError> {
        let op = &signed.operation;
        let digest = control_signing_input(op, proto_hash)?;
        let mut approvers: Vec<(&SignerV2, &CapEntryV2)> = Vec::new();
        let mut extras: Vec<SignerRole> = Vec::new();
        for (index, signer) in op.signers.iter().enumerate() {
            if signer.role != SignerRole::Device {
                extras.push(signer.role);
                continue;
            }
            let (Some(device_id), Some(cap_id)) = (signer.device_id, signer.cap_id) else {
                return Err(invariant("设备 signer 必须同时带 device_id 与 cap_id"));
            };
            let entry = self.state.caps.get(&cap_id).ok_or(SyncError::Unsupported(
                "signer 引用的 cap 不在已验证控制链上",
            ))?;
            if entry.cap.subject_device_id != device_id || entry.cap.subject_ed_pk != signer.pk {
                return Err(invariant("signer 的 device/cap/公钥不一致"));
            }
            if !self.state.cap_is_effective_at(entry, at) {
                return Err(SyncError::Unsupported(
                    "signer 的 cap 在签名时刻无效（撤销/跨 authority/过期/主体非 ACTIVE）",
                ));
            }
            for scope in required {
                if entry.cap.scopes.bits() & scope == 0 {
                    return Err(SyncError::Unsupported(
                        "signer 的 cap 缺少该操作所需的 scope（不能靠两签拼权限）",
                    ));
                }
            }
            self.check_signature(&signer.pk, &digest, &signed.signatures[index])?;
            approvers.push((signer, entry));
        }
        if approvers.len() != 2 {
            return Err(invariant("该 kind 必须恰好两台 ACTIVE 设备签名"));
        }
        if approvers[0].0.device_id == approvers[1].0.device_id
            || approvers[0].1.cap.subject_ed_pk == approvers[1].1.cap.subject_ed_pk
        {
            return Err(invariant("两签必须来自不同 device id 与不同公钥"));
        }
        let mut expected = allowed_extras.to_vec();
        expected.sort_by_key(|role| role.as_u8());
        extras.sort_by_key(|role| role.as_u8());
        if extras != expected {
            return Err(invariant("signer 角色集合不符合该 kind 的门限"));
        }
        Ok([approvers[0], approvers[1]])
    }

    /// 校验一条记录的 PoP signer（首次准入/恢复设备 add）：device/cap/pk 精确匹配。
    fn verify_target_pops(
        &self,
        signed: &SignedControlV2,
        expected: &[(&[u8; 16], &[u8; 32])],
    ) -> Result<(), SyncError> {
        let op = &signed.operation;
        let mut pops: Vec<(&[u8; 16], &[u8; 32])> = Vec::new();
        for (index, signer) in op.signers.iter().enumerate() {
            if signer.role != SignerRole::TargetPop {
                continue;
            }
            if signer.cap_id.is_some() {
                return Err(invariant("PoP signer 不能绑定 cap"));
            }
            let Some(device_id) = signer.device_id.as_ref() else {
                return Err(invariant("PoP signer 缺目标 device_id"));
            };
            let input = pop_signing_input(op, &signer.pk, proto_hash)?;
            self.check_signature(&signer.pk, &input, &signed.signatures[index])?;
            pops.push((device_id, &signer.pk));
        }
        let mut expected_sorted: Vec<(&[u8; 16], &[u8; 32])> = expected.to_vec();
        expected_sorted
            .sort_by(|left, right| left.0.cmp(right.0).then_with(|| left.1.cmp(right.1)));
        pops.sort_by(|left, right| left.0.cmp(right.0).then_with(|| left.1.cmp(right.1)));
        if pops != expected_sorted {
            return Err(invariant("PoP signer 集合与目标设备不匹配"));
        }
        Ok(())
    }

    // ------------------------------------------------------------ kind 0 Upgrade

    fn verify_upgrade(&self, signed: &SignedControlV2, rev: u64) -> Result<EffectV2, SyncError> {
        if self.state.authz_state.is_some() || self.state.authz_rev != 0 {
            return Err(invariant("UpgradePolicy 只能作为首条 V2 控制记录"));
        }
        let op = &signed.operation;
        if op.expected_authz_rev != 0 || op.authority_rev != 1 {
            return Err(invariant("UpgradePolicy 的 authz/authority rev 绑定不符"));
        }
        let Some(manager_pub) = self.manager_pub else {
            return Err(SyncError::NeedsTrust(
                "缺少 pin 住的管理公钥：无法验证 UpgradePolicy 的旧 creator 签名",
            ));
        };
        let payload = UpgradePolicyPayloadV2::decode(&op.payload)?;
        payload.validate()?;
        let control_input = control_signing_input(op, proto_hash)?;
        let upgrade_input = upgrade_signing_input(op, proto_hash)?;
        let recover_input = recover_signing_input(op, proto_hash)?;

        let mut creator: Option<(usize, &SignerV2)> = None;
        let mut recovery: Option<(usize, &SignerV2)> = None;
        let mut confirmations: Vec<(usize, &SignerV2)> = Vec::new();
        for (index, signer) in op.signers.iter().enumerate() {
            match signer.role {
                SignerRole::CreatorUpgrade => {
                    if creator.replace((index, signer)).is_some() {
                        return Err(invariant("UpgradePolicy 只能有一个旧 creator 签名"));
                    }
                }
                SignerRole::OfflineRecovery => {
                    if recovery.replace((index, signer)).is_some() {
                        return Err(invariant("UpgradePolicy 只能有一个新恢复钥 PoP"));
                    }
                }
                SignerRole::Device => {
                    if signer.cap_id.is_some() {
                        return Err(invariant("升级前不存在 cap：role0 不得带 cap_id"));
                    }
                    confirmations.push((index, signer));
                }
                SignerRole::TargetPop => {
                    return Err(invariant("UpgradePolicy 不接受目标设备 PoP"));
                }
            }
        }
        let (creator_index, creator) =
            creator.ok_or_else(|| invariant("UpgradePolicy 缺旧 creator"))?;
        if creator.pk != manager_pub {
            return Err(invariant("UpgradePolicy 的旧 creator 公钥与本地管理锚不符"));
        }
        self.check_signature(
            &creator.pk,
            &upgrade_input,
            &signed.signatures[creator_index],
        )?;
        let (recovery_index, recovery) =
            recovery.ok_or_else(|| invariant("UpgradePolicy 缺新恢复钥 PoP"))?;
        if recovery.pk != payload.fresh_recovery_pk {
            return Err(invariant("UpgradePolicy 的新恢复钥与 payload 不符"));
        }
        self.check_signature(
            &recovery.pk,
            &recover_input,
            &signed.signatures[recovery_index],
        )?;

        if confirmations.len() < 2 {
            return Err(invariant("UpgradePolicy 需要至少两台现有 ACTIVE 设备确认"));
        }
        let mut devices = BTreeSet::new();
        let mut keys = BTreeSet::new();
        for (index, signer) in confirmations {
            let device_id = signer
                .device_id
                .ok_or_else(|| invariant("UpgradePolicy 的 role0 缺 device_id"))?;
            let cert = payload
                .active_manifest
                .iter()
                .find(|cert| cert.device_id == device_id)
                .ok_or_else(|| invariant("UpgradePolicy 的确认设备不在 ACTIVE 清单里"))?;
            if cert.sign_ed25519_pub != signer.pk {
                return Err(invariant("UpgradePolicy 的确认公钥与清单不符"));
            }
            if !devices.insert(device_id) || !keys.insert(signer.pk) {
                return Err(invariant("UpgradePolicy 的确认设备/公钥重复"));
            }
            self.check_signature(&signer.pk, &control_input, &signed.signatures[index])?;
        }

        if payload.active_manifest.len() < 2 {
            return Err(invariant("UpgradePolicy 的 ACTIVE 清单至少要有两台设备"));
        }
        // manifest 的字段级一致性：对象身份与 authz rev 口径（S4b 冻结 §3/§7）。
        for cert in &payload.active_manifest {
            if cert.instance_id != self.state.instance_id
                || cert.vault_id != self.state.vault_id
                || cert.issued_authz_rev != 1
            {
                return Err(invariant("UpgradePolicy 的 ACTIVE 清单字段不符"));
            }
        }
        // bootstrap caps：rev 绑定提交后的值、subject 是清单内设备、TTL 在 policy 内，
        // 且至少两台不同设备持 CAP_ADMIN（不许把高危 quorum 降到 1）。
        let mut cap_admin = BTreeSet::new();
        for cap in &payload.bootstrap_caps {
            if cap.instance_id != self.state.instance_id
                || cap.vault_id != self.state.vault_id
                || cap.issued_control_rev != rev
                || cap.issued_authz_rev != 1
                || cap.issuer_authority_rev != 1
            {
                return Err(invariant("UpgradePolicy 的 bootstrap cap rev 绑定不符"));
            }
            let Some(cert) = payload
                .active_manifest
                .iter()
                .find(|cert| cert.device_id == cap.subject_device_id)
            else {
                return Err(invariant("bootstrap cap 的 subject 不在 ACTIVE 清单里"));
            };
            if cert.sign_ed25519_pub != cap.subject_ed_pk {
                return Err(invariant("bootstrap cap 的主体公钥与清单不符"));
            }
            if op.signed_at < cap.not_before || op.signed_at >= cap.expires_at {
                return Err(invariant("bootstrap cap 的签发时间不在有效时间窗内"));
            }
            if cap.expires_at.saturating_sub(cap.not_before) > payload.policy.max_cap_ttl {
                return Err(invariant("bootstrap cap 的 TTL 超过 policy 上限"));
            }
            if cap.scopes.bits() & CapScope::CAP_ADMIN != 0 {
                cap_admin.insert(cap.subject_device_id);
            }
        }
        if cap_admin.len() < 2 {
            return Err(invariant("UpgradePolicy 必须让两台不同设备持 CAP_ADMIN"));
        }
        Ok(EffectV2::Upgrade {
            policy: payload.policy,
            recovery_pub: payload.fresh_recovery_pk,
            caps: payload.bootstrap_caps,
            manifest: payload.active_manifest,
        })
    }

    // ------------------------------------------------------------ kind 2/3 cap

    fn verify_cap_grant(&self, signed: &SignedControlV2, rev: u64) -> Result<EffectV2, SyncError> {
        let op = &signed.operation;
        let payload = CapGrantPayloadV2::decode(&op.payload)?;
        let cap = payload.cap;
        let policy = self
            .state
            .policy
            .as_ref()
            .ok_or_else(|| invariant("cap 授予缺已验证 policy"))?;
        let signers = self.device_signers(signed, &[CapScope::CAP_ADMIN], &[], op.signed_at)?;
        for (_, entry) in &signers {
            if !cap.scopes.is_subset_of(entry.cap.scopes) {
                return Err(invariant("新 cap 的 scope 不是每位签发者有效 scope 的子集"));
            }
        }
        if cap.instance_id != self.state.instance_id || cap.vault_id != self.state.vault_id {
            return Err(invariant("新 cap 的 instance/vault 与本地不符"));
        }
        if cap.issued_control_rev != rev
            || cap.issued_authz_rev != self.state.authz_rev.saturating_add(1)
            || cap.issuer_authority_rev != self.state.authority_rev
        {
            return Err(invariant("新 cap 的 rev 绑定不符"));
        }
        if op.signed_at < cap.not_before || op.signed_at >= cap.expires_at {
            return Err(invariant("新 cap 的签发时间不在有效时间窗内"));
        }
        if cap.expires_at.saturating_sub(cap.not_before) > policy.max_cap_ttl {
            return Err(invariant("新 cap 的 TTL 超过 policy 上限"));
        }
        let subject = self
            .state
            .devices
            .get(&cap.subject_device_id)
            .ok_or_else(|| invariant("新 cap 的 subject 不是已验证成员"))?;
        if subject.status != DeviceStatusV2::Active
            || subject.cert.sign_ed25519_pub != cap.subject_ed_pk
        {
            return Err(invariant("新 cap 的 subject 不是 ACTIVE 或公钥不匹配"));
        }
        if self.state.cap_ids_seen.contains(&cap.cap_id) {
            return Err(invariant("cap_id 已出现过（cap ID 永不复用）"));
        }
        Ok(EffectV2::CapGrant(cap))
    }

    fn verify_cap_revoke(&self, signed: &SignedControlV2) -> Result<EffectV2, SyncError> {
        let op = &signed.operation;
        let payload = CapRevokePayloadV2::decode(&op.payload)?;
        self.device_signers(signed, &[CapScope::CAP_ADMIN], &[], op.signed_at)?;
        let Some(entry) = self.state.caps.get(&payload.cap_id) else {
            return Err(invariant("撤销目标 cap 不在已验证控制链上"));
        };
        if entry.is_revoked() {
            return Err(invariant("cap 已被撤销（重复撤销必须换 request_id 重签）"));
        }
        Ok(EffectV2::CapRevoke(payload.cap_id))
    }

    // ------------------------------------------------------------ kind 4/5/16 设备

    fn verify_device_credential(&self, signed: &SignedControlV2) -> Result<EffectV2, SyncError> {
        let op = &signed.operation;
        let payload = DeviceCredentialPayloadV2::decode(&op.payload)?;
        let cert = payload.cert.clone();
        if cert.instance_id != self.state.instance_id || cert.vault_id != self.state.vault_id {
            return Err(invariant("设备证书的 instance/vault 与本地不符"));
        }
        let existing = self.state.devices.get(&cert.device_id).cloned();
        match op.kind {
            ControlKindV2::DeviceApprove => match existing.as_ref().map(|entry| entry.status) {
                // 首次准入：必须有目标 PoP（证明目标持有证书私钥）。
                None => {
                    self.device_signers(
                        signed,
                        &[CapScope::APPROVE],
                        &[SignerRole::TargetPop],
                        op.signed_at,
                    )?;
                    self.verify_target_pops(signed, &[(&cert.device_id, &cert.sign_ed25519_pub)])?;
                    if cert.cert_serial != 1 {
                        return Err(invariant("首次批准必须 serial=1（serial 不复用）"));
                    }
                    if !cert.data_rights.can_read() {
                        return Err(invariant("批准的设备必须至少具备 READ 权限"));
                    }
                    if cert.issued_authz_rev != self.state.authz_rev.saturating_add(1) {
                        return Err(invariant("批准证书的 issued_authz_rev 不是提交后的值"));
                    }
                    self.verify_credential_grants(&cert, &payload.grants)?;
                    Ok(EffectV2::DeviceApproval {
                        cert,
                        grants: payload.grants,
                        new_credential: true,
                    })
                }
                // 已 ACTIVE：只接受**逐字节相同**的证书/grant 重放，不推进 authz。
                Some(DeviceStatusV2::Active) => {
                    let Some(existing) = existing else {
                        return Err(invariant("设备状态投影不一致"));
                    };
                    self.device_signers(signed, &[CapScope::APPROVE], &[], op.signed_at)?;
                    if existing.cert != cert {
                        return Err(invariant("已 ACTIVE 设备的证书不同（改权必须走 kind 16）"));
                    }
                    for grant in &payload.grants {
                        if existing.grants.get(&grant.aad.epoch) != Some(grant) {
                            return Err(invariant("已 ACTIVE 设备的 grant 包与已批准事实不同"));
                        }
                    }
                    Ok(EffectV2::DeviceApproval {
                        cert,
                        grants: payload.grants,
                        new_credential: false,
                    })
                }
                // 永不复活。
                Some(DeviceStatusV2::Revoked) => {
                    Err(invariant("已撤销设备不能被再次批准（永不复活）"))
                }
            },
            ControlKindV2::DeviceRightsChange => {
                let Some(existing) = existing else {
                    return Err(invariant(
                        "改权目标不在已验证成员集合（PENDING 必须走批准）",
                    ));
                };
                if existing.status != DeviceStatusV2::Active {
                    return Err(invariant("改权目标不是 ACTIVE 设备"));
                }
                if cert.cert_serial != existing.cert.cert_serial.saturating_add(1) {
                    return Err(invariant("改权证书 serial 必须是当前值 +1（历史不覆盖）"));
                }
                if cert.sign_ed25519_pub != existing.cert.sign_ed25519_pub
                    || cert.hpke_x25519_pub != existing.cert.hpke_x25519_pub
                {
                    return Err(invariant("改权不得更换设备公钥（换钥属轮换/激活）"));
                }
                if !cert.data_rights.can_read() {
                    return Err(invariant("改权后必须至少保留 READ 权限"));
                }
                if cert.data_rights == existing.cert.data_rights {
                    return Err(invariant("改权记录必须真的改变权限"));
                }
                if cert.issued_authz_rev != self.state.authz_rev.saturating_add(1) {
                    return Err(invariant("改权证书的 issued_authz_rev 不是提交后的值"));
                }
                self.device_signers(signed, &[CapScope::APPROVE], &[], op.signed_at)?;
                self.verify_credential_grants(&cert, &payload.grants)?;
                // 数据权限 quorum：变更前可写设备 ≥2 时，变更后不得 <2。
                let writable_before = self
                    .state
                    .devices
                    .values()
                    .filter(|entry| {
                        entry.status == DeviceStatusV2::Active && entry.cert.data_rights.can_write()
                    })
                    .count();
                if writable_before >= 2 {
                    let writable_after = writable_before
                        - usize::from(existing.cert.data_rights.can_write())
                        + usize::from(cert.data_rights.can_write());
                    if writable_after < 2 {
                        return Err(invariant("改权后可用写设备低于 2 台（数据权限 quorum）"));
                    }
                }
                Ok(EffectV2::DeviceApproval {
                    cert,
                    grants: payload.grants,
                    new_credential: true,
                })
            }
            _ => Err(invariant("内部错误：非设备凭证 kind 进入凭证校验")),
        }
    }

    /// 凭证 grant 包的公共校验：recipient/hpke/authority/issued rev/grant_id 不复用。
    fn verify_credential_grants(
        &self,
        cert: &DeviceCertV2,
        grants: &[GrantPackageV2],
    ) -> Result<(), SyncError> {
        let post_authz_rev = self.state.authz_rev.saturating_add(1);
        for grant in grants {
            if grant.aad.recipient_device_id != cert.device_id
                || grant.aad.recipient_hpke_pub != cert.hpke_x25519_pub
            {
                return Err(invariant("grant 包的 recipient/hpke 与证书不符"));
            }
            if grant.aad.instance_id != self.state.instance_id
                || grant.aad.vault_id != self.state.vault_id
            {
                return Err(invariant("grant 包的 instance/vault 与本地不符"));
            }
            if grant.aad.authority_rev != self.state.authority_rev {
                return Err(invariant("grant 包的 authority 代不是当前代"));
            }
            if grant.aad.issued_authz_rev != post_authz_rev {
                return Err(invariant("grant 包的 issued_authz_rev 不是提交后的值"));
            }
            if self.state.grant_ids.contains(&grant.aad.grant_id) {
                return Err(invariant("grant_id 已出现过（grant ID 永不复用）"));
            }
        }
        Ok(())
    }

    fn verify_device_revoke(&self, signed: &SignedControlV2) -> Result<EffectV2, SyncError> {
        let op = &signed.operation;
        let payload = DeviceRevokePayloadV2::decode(&op.payload)?;
        self.device_signers(signed, &[CapScope::REVOKE], &[], op.signed_at)?;
        let removed = self.ensure_revoke_quorum(&payload.device_ids, op.signed_at)?;
        Ok(EffectV2::DeviceRevoke {
            targets: payload.device_ids,
            changed: !removed.is_empty(),
        })
    }

    /// 撤销 quorum：目标必须都是已验证成员（PENDING 不在链上 → fail-stop）；撤销后剩余
    /// ACTIVE 不低于 `min_quorum`；任一高危 scope 原本具备两设备 quorum 时不得被降到 2
    /// 以下。返回**此前 ACTIVE**、被本次真正撤销的设备集合。
    fn ensure_revoke_quorum(
        &self,
        device_ids: &[[u8; 16]],
        at: u64,
    ) -> Result<BTreeSet<[u8; 16]>, SyncError> {
        let policy = self
            .state
            .policy
            .as_ref()
            .ok_or_else(|| invariant("设备撤销缺已验证 policy"))?;
        let mut removed = BTreeSet::new();
        for device_id in device_ids {
            let Some(device) = self.state.devices.get(device_id) else {
                return Err(invariant(
                    "撤销目标不在已验证成员集合（PENDING 兑换不是控制记录，客户端无法安全回放）",
                ));
            };
            if device.status == DeviceStatusV2::Active {
                removed.insert(*device_id);
            }
        }
        let active_after = self
            .state
            .active_device_count()
            .saturating_sub(removed.len());
        if active_after < usize::try_from(policy.min_quorum).unwrap_or(usize::MAX) {
            return Err(invariant("撤销后剩余 ACTIVE 设备低于 min_quorum"));
        }
        for scope in HIGH_RISK_SCOPES {
            let before = self.scope_quorum(scope, at, &BTreeSet::new());
            let after = self.scope_quorum(scope, at, &removed);
            if before >= 2 && after < 2 {
                return Err(invariant(
                    "撤销会让某高危 scope 的两设备 quorum 低于 2（必须先续期/补 cap）",
                ));
            }
        }
        Ok(removed)
    }

    /// `at` 时刻具备 `scope` 有效 cap 的 ACTIVE 设备数（`removed` 视为已撤销）。
    fn scope_quorum(&self, scope: u32, at: u64, removed: &BTreeSet<[u8; 16]>) -> usize {
        let mut devices = BTreeSet::new();
        for entry in self.state.caps.values() {
            if entry.cap.scopes.bits() & scope == 0 {
                continue;
            }
            if removed.contains(&entry.cap.subject_device_id) {
                continue;
            }
            if self.state.cap_is_effective_at(entry, at) {
                devices.insert(entry.cap.subject_device_id);
            }
        }
        devices.len()
    }

    // ------------------------------------------------------------ kind 6–10 rotation

    fn rotation_must_exist(
        &self,
        rotation_id: &[u8; 16],
        state: RotationStateV2,
    ) -> Result<&RotationRecordV2, SyncError> {
        let Some(record) = self.state.rotations.get(rotation_id) else {
            return Err(invariant(
                "轮换记录不在已验证控制链上（relay 在 restart 后使用链外随机 rotation_id）",
            ));
        };
        if record.state != state {
            return Err(invariant("轮换记录状态与操作不符"));
        }
        Ok(record)
    }

    fn verify_rotation_begin(&self, signed: &SignedControlV2) -> Result<EffectV2, SyncError> {
        let op = &signed.operation;
        let payload = RotationBeginPayloadV2::decode(&op.payload)?;
        let required: &[u32] = if payload.revoke_ids.is_empty() {
            &[CapScope::ROTATE]
        } else {
            // 含撤销时**每一位**签名者还必须具备 REVOKE。
            &[CapScope::ROTATE, CapScope::REVOKE]
        };
        self.device_signers(signed, required, &[], op.signed_at)?;
        if self.state.rotations.contains_key(&payload.rotation_id) {
            return Err(invariant("rotation_id 已出现过（轮换 ID 永不复用）"));
        }
        if payload.to_epoch <= payload.from_epoch {
            return Err(invariant("候选 epoch 必须高于源 epoch"));
        }
        if !payload.revoke_ids.is_empty() {
            self.ensure_revoke_quorum(&payload.revoke_ids, op.signed_at)?;
        }
        Ok(EffectV2::RotationBegin {
            rotation_id: payload.rotation_id,
            from_epoch: payload.from_epoch,
            to_epoch: payload.to_epoch,
            revoke_ids: payload.revoke_ids,
            member_set_hash: self.member_set_hash(),
        })
    }

    fn verify_rotation_abort(&self, signed: &SignedControlV2) -> Result<EffectV2, SyncError> {
        let op = &signed.operation;
        let payload = RotationAbortPayloadV2::decode(&op.payload)?;
        self.device_signers(signed, &[CapScope::ROTATE], &[], op.signed_at)?;
        self.rotation_must_exist(&payload.rotation_id, RotationStateV2::Frozen)?;
        Ok(EffectV2::RotationAbort {
            rotation_id: payload.rotation_id,
        })
    }

    fn verify_rotation_restart(&self, signed: &SignedControlV2) -> Result<EffectV2, SyncError> {
        let op = &signed.operation;
        let payload = RotationRestartPayloadV2::decode(&op.payload)?;
        let required: &[u32] = if payload.extra_ids.is_empty() {
            &[CapScope::ROTATE]
        } else {
            &[CapScope::ROTATE, CapScope::REVOKE]
        };
        self.device_signers(signed, required, &[], op.signed_at)?;
        let old = self.rotation_must_exist(&payload.rotation_id, RotationStateV2::Frozen)?;
        if payload.new_to_epoch <= old.to_epoch {
            return Err(invariant(
                "重启的候选 epoch 必须严格高于旧候选（不复用/不回退）",
            ));
        }
        if !payload.extra_ids.is_empty() {
            self.ensure_revoke_quorum(&payload.extra_ids, op.signed_at)?;
        }
        Ok(EffectV2::RotationRestart {
            old_rotation_id: payload.rotation_id,
            new_to_epoch: payload.new_to_epoch,
            extra_ids: payload.extra_ids,
            member_set_hash: self.member_set_hash(),
        })
    }

    fn verify_rotation_add_revokes(&self, signed: &SignedControlV2) -> Result<EffectV2, SyncError> {
        let op = &signed.operation;
        let payload = RotationAddRevokesPayloadV2::decode(&op.payload)?;
        self.device_signers(
            signed,
            &[CapScope::ROTATE, CapScope::REVOKE],
            &[],
            op.signed_at,
        )?;
        self.rotation_must_exist(&payload.rotation_id, RotationStateV2::Frozen)?;
        self.ensure_revoke_quorum(&payload.device_ids, op.signed_at)?;
        Ok(EffectV2::RotationAddRevokes {
            rotation_id: payload.rotation_id,
            device_ids: payload.device_ids,
        })
    }

    fn verify_epoch_activate(&self, signed: &SignedControlV2) -> Result<EffectV2, SyncError> {
        let op = &signed.operation;
        let payload = EpochActivatePayloadV2::decode(&op.payload)?;
        let activation = payload.activation;
        self.device_signers(signed, &[CapScope::ROTATE], &[], op.signed_at)?;
        // relay 在 `RotationRestart` 时会生成链外的随机 rotation_id；客户端只能按
        // `(from_epoch, to_epoch)` 唯一匹配一条 `Restarted` 记录（该记录已由两签 ROTATE
        // 授权）。匹配不唯一或不存在都 fail-stop。
        let record = match self.state.rotations.get(&activation.rotation_id) {
            Some(record) if record.state == RotationStateV2::Frozen => record,
            Some(_) => return Err(invariant("激活目标轮换记录状态不符")),
            None => {
                let matches: Vec<&RotationRecordV2> = self
                    .state
                    .rotations
                    .values()
                    .filter(|record| {
                        record.state == RotationStateV2::Restarted
                            && record.from_epoch == activation.from_epoch
                            && record.to_epoch == activation.to_epoch
                    })
                    .collect();
                if matches.len() != 1 {
                    return Err(invariant("激活记录无法唯一绑定到已验证轮换候选"));
                }
                matches[0]
            }
        };
        if activation.from_epoch != record.from_epoch || activation.to_epoch != record.to_epoch {
            return Err(invariant("激活记录的 from/to epoch 与轮换记录不符"));
        }
        if let Some(terminal) = record.terminal_rev {
            if record.state != RotationStateV2::Restarted || terminal > op.expected_control_rev {
                return Err(invariant("激活记录指向的轮换已被终结"));
            }
        }
        let member_changed = record.member_set_hash != self.member_set_hash();
        Ok(EffectV2::EpochActivate {
            rotation_id: record.rotation_id,
            freeze_seq: activation.freeze_seq,
            member_changed,
        })
    }

    // ------------------------------------------------------------ kind 11/12 邀请

    fn verify_invite_create(
        &self,
        signed: &SignedControlV2,
        rev: u64,
    ) -> Result<EffectV2, SyncError> {
        let op = &signed.operation;
        let payload = InviteCreatePayloadV2::decode(&op.payload)?;
        if op.signers.len() != 1 {
            return Err(invariant("邀请创建只要一台 INVITE cap"));
        }
        let (signer, entry) = self.single_invite_signer(signed, op.signed_at)?;
        let policy = self
            .state
            .policy
            .as_ref()
            .ok_or_else(|| invariant("邀请创建缺已验证 policy"))?;
        if payload.expires_at <= op.signed_at
            || payload.expires_at.saturating_sub(op.signed_at) > policy.max_invite_ttl
        {
            return Err(invariant("邀请 TTL 超出 policy 上限或已过期"));
        }
        if self.state.invites.contains_key(&payload.invite_id)
            || self
                .state
                .invites
                .values()
                .any(|invite| invite.token_hash == payload.token_hash)
        {
            return Err(invariant("邀请 ID / token_hash 已出现过（永不复用）"));
        }
        let _ = signer;
        Ok(EffectV2::InviteCreate(InviteEntryV2 {
            invite_id: payload.invite_id,
            token_hash: payload.token_hash,
            expires_at: payload.expires_at,
            issuer_cap_id: entry.cap.cap_id,
            created_rev: rev,
            cancelled_rev: None,
        }))
    }

    fn verify_invite_cancel(&self, signed: &SignedControlV2) -> Result<EffectV2, SyncError> {
        let op = &signed.operation;
        let payload = InviteCancelPayloadV2::decode(&op.payload)?;
        if op.signers.len() != 1 {
            return Err(invariant("邀请取消只要一台 INVITE cap"));
        }
        self.single_invite_signer(signed, op.signed_at)?;
        let Some(invite) = self.state.invites.get(&payload.invite_id) else {
            return Err(invariant("取消目标邀请不在已验证控制链上"));
        };
        if invite.cancelled_rev.is_some() {
            // 已取消：幂等成功（只追加控制记录）。
            return Ok(EffectV2::InviteCancel(payload.invite_id));
        }
        Ok(EffectV2::InviteCancel(payload.invite_id))
    }

    fn single_invite_signer<'a>(
        &'a self,
        signed: &'a SignedControlV2,
        at: u64,
    ) -> Result<(&'a SignerV2, &'a CapEntryV2), SyncError> {
        let op = &signed.operation;
        let signer = &op.signers[0];
        if signer.role != SignerRole::Device {
            return Err(invariant("邀请记录必须是设备签名"));
        }
        let (Some(device_id), Some(cap_id)) = (signer.device_id, signer.cap_id) else {
            return Err(invariant("邀请 signer 必须带 device_id 与 cap_id"));
        };
        let entry = self.state.caps.get(&cap_id).ok_or(SyncError::Unsupported(
            "邀请 signer 的 cap 不在已验证控制链上",
        ))?;
        if entry.cap.subject_device_id != device_id || entry.cap.subject_ed_pk != signer.pk {
            return Err(invariant("邀请 signer 的 device/cap/公钥不一致"));
        }
        if !self.state.cap_is_effective_at(entry, at) {
            return Err(SyncError::Unsupported("邀请 signer 的 cap 在签名时刻无效"));
        }
        if entry.cap.scopes.bits() & CapScope::INVITE == 0 {
            return Err(SyncError::Unsupported("邀请 signer 的 cap 缺 INVITE scope"));
        }
        let input = control_signing_input(op, proto_hash)?;
        self.check_signature(&signer.pk, &input, &signed.signatures[0])?;
        Ok((signer, entry))
    }

    // ------------------------------------------------------------ kind 13/14 恢复

    fn verify_recovery_reset(
        &self,
        signed: &SignedControlV2,
        rev: u64,
    ) -> Result<EffectV2, SyncError> {
        let op = &signed.operation;
        let payload = RecoveryResetPayloadV2::decode(&op.payload)?;
        payload.validate()?;
        let Some(current_recovery) = self.state.recovery_pub else {
            return Err(invariant("RecoveryReset 缺当前恢复根（尚未升级）"));
        };
        // 授权面：旧恢复根 + 新钥 PoP；设备 add 每位还须目标 PoP。**不接受 role0**。
        let recover_input = recover_signing_input(op, proto_hash)?;
        let mut recovery_pks: Vec<[u8; 32]> = Vec::new();
        for (index, signer) in op.signers.iter().enumerate() {
            match signer.role {
                SignerRole::Device | SignerRole::CreatorUpgrade => {
                    return Err(invariant("恢复记录不接受 role0/旧 creator 签名"))
                }
                SignerRole::OfflineRecovery => {
                    if signer.device_id.is_some() || signer.cap_id.is_some() {
                        return Err(invariant("恢复根签名不能带 device/cap"));
                    }
                    self.check_signature(&signer.pk, &recover_input, &signed.signatures[index])?;
                    recovery_pks.push(signer.pk);
                }
                SignerRole::TargetPop => {}
            }
        }
        let mut expected_roots = vec![current_recovery, payload.fresh_recovery_pk];
        expected_roots.sort();
        recovery_pks.sort();
        if recovery_pks != expected_roots {
            return Err(invariant(
                "RecoveryReset 必须由当前恢复根与新恢复钥 PoP 各签一次",
            ));
        }
        let mut add_expected: Vec<([u8; 16], [u8; 32])> = Vec::new();
        for add in &payload.device_adds {
            if add.cert.instance_id != self.state.instance_id
                || add.cert.vault_id != self.state.vault_id
            {
                return Err(invariant("恢复 add 证书的 instance/vault 与本地不符"));
            }
            if add.cert.cert_serial != 1 {
                return Err(invariant("恢复 add 必须 serial=1"));
            }
            if !add.cert.data_rights.can_read() {
                return Err(invariant("恢复 add 必须至少具备 READ 权限"));
            }
            if add.cert.issued_authz_rev != self.state.authz_rev.saturating_add(1) {
                return Err(invariant("恢复 add 的 issued_authz_rev 不是提交后的值"));
            }
            if let Some(known) = self.state.devices.get(&add.cert.device_id) {
                if known.status == DeviceStatusV2::Revoked {
                    return Err(invariant("恢复 add 不能复活已撤销设备"));
                }
                if known.status == DeviceStatusV2::Active {
                    return Err(invariant("恢复 add 只接受 PENDING 设备"));
                }
            }
            if self.state.revoked_unseen.contains(&add.cert.device_id) {
                return Err(invariant("恢复 add 不能复活链上已撤销的设备"));
            }
            if add.grants.len() != 1 {
                return Err(invariant("恢复 add 必须恰好一份当前 epoch 的 grant 包"));
            }
            self.verify_credential_grants_with(
                &add.cert,
                &add.grants,
                self.state.authz_rev.saturating_add(1),
                self.state.authority_rev.saturating_add(1),
            )?;
            add_expected.push((add.cert.device_id, add.cert.sign_ed25519_pub));
        }
        let add_refs: Vec<(&[u8; 16], &[u8; 32])> = add_expected
            .iter()
            .map(|(device_id, pk)| (device_id, pk))
            .collect();
        self.verify_target_pops(signed, &add_refs)?;
        // 新代 bootstrap caps：rev 指向提交后的值、subject 必须是提交后仍 ACTIVE 的设备。
        let post_authz = self.state.authz_rev.saturating_add(1);
        let post_authority = self.state.authority_rev.saturating_add(1);
        let revoke: BTreeSet<[u8; 16]> = payload.revoke_ids.iter().copied().collect();
        for device_id in &payload.revoke_ids {
            if let Some(known) = self.state.devices.get(device_id) {
                if known.status == DeviceStatusV2::Active {
                    continue;
                }
                // 已是 revoked：幂等目标。
                continue;
            }
            if !self.state.revoked_unseen.contains(device_id) {
                return Err(invariant(
                    "恢复撤销目标不在已验证成员集合（PENDING 兑换不是控制记录）",
                ));
            }
        }
        let mut cap_admin = BTreeSet::new();
        for cap in &payload.bootstrap_caps {
            if cap.instance_id != self.state.instance_id || cap.vault_id != self.state.vault_id {
                return Err(invariant("恢复 bootstrap cap 的 instance/vault 与本地不符"));
            }
            if cap.issued_control_rev != rev
                || cap.issued_authz_rev != post_authz
                || cap.issuer_authority_rev != post_authority
            {
                return Err(invariant("恢复 bootstrap cap 的 rev 绑定不符"));
            }
            if op.signed_at < cap.not_before || op.signed_at >= cap.expires_at {
                return Err(invariant("恢复 bootstrap cap 的签发时间不在有效时间窗内"));
            }
            if cap.expires_at.saturating_sub(cap.not_before) > payload.policy.max_cap_ttl {
                return Err(invariant("恢复 bootstrap cap 的 TTL 超过新 policy 上限"));
            }
            let subject_pk = if let Some(add) = payload
                .device_adds
                .iter()
                .find(|add| add.cert.device_id == cap.subject_device_id)
            {
                add.cert.sign_ed25519_pub
            } else if revoke.contains(&cap.subject_device_id) {
                return Err(invariant("恢复 bootstrap cap 不能装在被撤销设备上"));
            } else {
                let Some(known) = self.state.devices.get(&cap.subject_device_id) else {
                    return Err(invariant("恢复 bootstrap cap 的 subject 不是 ACTIVE 设备"));
                };
                if known.status != DeviceStatusV2::Active {
                    return Err(invariant("恢复 bootstrap cap 的 subject 不是 ACTIVE 设备"));
                }
                known.cert.sign_ed25519_pub
            };
            if subject_pk != cap.subject_ed_pk {
                return Err(invariant("恢复 bootstrap cap 的主体公钥不符"));
            }
            if cap.scopes.bits() & CapScope::CAP_ADMIN != 0 {
                cap_admin.insert(cap.subject_device_id);
            }
        }
        // 结果仍有 ≥2 台 ACTIVE 时必须至少两台不同设备持 CAP_ADMIN；不足两台只有
        // "成员数 ≥2 且 cap quorum 也 ≥2" 才保持 active。
        let active_after = self.active_after_recovery(&payload.device_adds, &revoke);
        if active_after >= 2 && cap_admin.len() < 2 {
            return Err(invariant("恢复后 ACTIVE ≥2 时必须保持两台 CAP_ADMIN"));
        }
        Ok(EffectV2::RecoveryReset {
            policy: payload.policy,
            recovery_pub: payload.fresh_recovery_pk,
            caps: payload.bootstrap_caps,
            adds: payload
                .device_adds
                .into_iter()
                .map(|add| {
                    let grants = add.grants;
                    (add.cert, grants)
                })
                .collect(),
            revoke_ids: payload.revoke_ids,
        })
    }

    fn verify_recovery_key_change(
        &self,
        signed: &SignedControlV2,
        rev: u64,
    ) -> Result<EffectV2, SyncError> {
        let op = &signed.operation;
        let payload = RecoveryKeyChangePayloadV2::decode(&op.payload)?;
        payload.validate()?;
        self.device_signers(
            signed,
            &[CapScope::POLICY],
            &[SignerRole::OfflineRecovery],
            op.signed_at,
        )?;
        // 新恢复钥 PoP：role2 且 pk == fresh（同一 recover 域）。
        let recover_input = recover_signing_input(op, proto_hash)?;
        let mut fresh_signed = false;
        for (index, signer) in op.signers.iter().enumerate() {
            if signer.role != SignerRole::OfflineRecovery {
                continue;
            }
            if signer.device_id.is_some() || signer.cap_id.is_some() {
                return Err(invariant("恢复根签名不能带 device/cap"));
            }
            if signer.pk != payload.fresh_recovery_pk {
                return Err(invariant("RecoveryKeyChange 的 PoP 公钥与 payload 不符"));
            }
            self.check_signature(&signer.pk, &recover_input, &signed.signatures[index])?;
            fresh_signed = true;
        }
        if !fresh_signed {
            return Err(invariant("RecoveryKeyChange 缺新恢复钥 PoP"));
        }
        let post_authz = self.state.authz_rev.saturating_add(1);
        let post_authority = self.state.authority_rev.saturating_add(1);
        let mut cap_admin = BTreeSet::new();
        for cap in &payload.bootstrap_caps {
            if cap.instance_id != self.state.instance_id || cap.vault_id != self.state.vault_id {
                return Err(invariant("换钥 bootstrap cap 的 instance/vault 与本地不符"));
            }
            if cap.issued_control_rev != rev
                || cap.issued_authz_rev != post_authz
                || cap.issuer_authority_rev != post_authority
            {
                return Err(invariant("换钥 bootstrap cap 的 rev 绑定不符"));
            }
            if op.signed_at < cap.not_before || op.signed_at >= cap.expires_at {
                return Err(invariant("换钥 bootstrap cap 的签发时间不在有效时间窗内"));
            }
            if cap.expires_at.saturating_sub(cap.not_before) > payload_cap_ttl_bound(self)? {
                return Err(invariant("换钥 bootstrap cap 的 TTL 超过 policy 上限"));
            }
            let Some(known) = self.state.devices.get(&cap.subject_device_id) else {
                return Err(invariant("换钥 bootstrap cap 的 subject 不是 ACTIVE 设备"));
            };
            if known.status != DeviceStatusV2::Active
                || known.cert.sign_ed25519_pub != cap.subject_ed_pk
            {
                return Err(invariant(
                    "换钥 bootstrap cap 的 subject 不是 ACTIVE 设备或公钥不符",
                ));
            }
            if cap.scopes.bits() & CapScope::CAP_ADMIN != 0 {
                cap_admin.insert(cap.subject_device_id);
            }
        }
        if self.state.active_device_count() >= 2 && cap_admin.len() < 2 {
            return Err(invariant("换钥后必须保持两台不同设备持 CAP_ADMIN"));
        }
        Ok(EffectV2::RecoveryKeyChange {
            recovery_pub: payload.fresh_recovery_pk,
            caps: payload.bootstrap_caps,
        })
    }

    fn verify_credential_grants_with(
        &self,
        cert: &DeviceCertV2,
        grants: &[GrantPackageV2],
        post_authz_rev: u64,
        post_authority_rev: u64,
    ) -> Result<(), SyncError> {
        for grant in grants {
            if grant.aad.recipient_device_id != cert.device_id
                || grant.aad.recipient_hpke_pub != cert.hpke_x25519_pub
            {
                return Err(invariant("grant 包的 recipient/hpke 与证书不符"));
            }
            if grant.aad.instance_id != self.state.instance_id
                || grant.aad.vault_id != self.state.vault_id
            {
                return Err(invariant("grant 包的 instance/vault 与本地不符"));
            }
            if grant.aad.authority_rev != post_authority_rev
                || grant.aad.issued_authz_rev != post_authz_rev
            {
                return Err(invariant("grant 包的 authority/issued rev 不是提交后的值"));
            }
            if self.state.grant_ids.contains(&grant.aad.grant_id) {
                return Err(invariant("grant_id 已出现过（grant ID 永不复用）"));
            }
            if let Some(epoch) = self.state.devices.get(&cert.device_id) {
                if epoch.grants.contains_key(&grant.aad.epoch) {
                    return Err(invariant("同一设备同一 epoch 的 grant 包重复"));
                }
            }
        }
        Ok(())
    }

    fn active_after_recovery(
        &self,
        adds: &[DeviceCredentialPayloadV2],
        revoke: &BTreeSet<[u8; 16]>,
    ) -> usize {
        let surviving = self
            .state
            .devices
            .iter()
            .filter(|(device_id, device)| {
                device.status == DeviceStatusV2::Active && !revoke.contains(*device_id)
            })
            .count();
        surviving + adds.len()
    }

    // ------------------------------------------------------------ 投影

    fn insert_cap(&mut self, cap: ManagementCapV2, rev: u64) -> Result<(), SyncError> {
        if !self.state.cap_ids_seen.insert(cap.cap_id) {
            return Err(invariant("cap_id 已出现过（cap ID 永不复用）"));
        }
        self.state.caps.insert(
            cap.cap_id,
            CapEntryV2 {
                cap,
                granted_rev: rev,
                revoked_control_rev: None,
            },
        );
        Ok(())
    }

    fn revoke_devices_by_rotation(&mut self, rev: u64, device_ids: &[[u8; 16]]) {
        for device_id in device_ids {
            match self.state.devices.get_mut(device_id) {
                Some(device) if device.status == DeviceStatusV2::Active => {
                    device.status = DeviceStatusV2::Revoked;
                    device.revoked_rev = Some(rev);
                }
                Some(_) => {}
                // 链上未出现过的设备（PENDING 兑换不是控制记录）：记录为"已撤销但未知"，
                // 后续任何 add/批准一律拒绝（不复活）。
                None => {
                    self.state.revoked_unseen.insert(*device_id);
                }
            }
        }
    }

    fn apply_effect(&mut self, effect: EffectV2, rev: u64) -> Result<(), SyncError> {
        match effect {
            EffectV2::Upgrade {
                policy,
                recovery_pub,
                caps,
                manifest,
            } => {
                self.state.authority_rev = 1;
                self.state.authz_rev = 1;
                self.state.authz_state = Some(AuthzStateV2::Active);
                self.state.policy = Some(policy);
                self.state.recovery_pub = Some(recovery_pub);
                for cap in caps {
                    self.insert_cap(cap, rev)?;
                }
                for cert in manifest {
                    self.state.devices.insert(
                        cert.device_id,
                        DeviceEntryV2 {
                            cert,
                            status: DeviceStatusV2::Active,
                            approved_rev: rev,
                            revoked_rev: None,
                            grants: BTreeMap::new(),
                        },
                    );
                }
            }
            EffectV2::CapGrant(cap) => {
                self.insert_cap(cap, rev)?;
                self.bump_authz()?;
            }
            EffectV2::CapRevoke(cap_id) => {
                let entry = self
                    .state
                    .caps
                    .get_mut(&cap_id)
                    .ok_or_else(|| invariant("撤销投影缺目标 cap"))?;
                entry.revoked_control_rev = Some(rev);
                self.bump_authz()?;
            }
            EffectV2::DeviceApproval {
                cert,
                grants,
                new_credential,
            } => {
                let mut map = self
                    .state
                    .devices
                    .get(&cert.device_id)
                    .map(|device| device.grants.clone())
                    .unwrap_or_default();
                for grant in grants {
                    self.state.grant_ids.insert(grant.aad.grant_id);
                    map.insert(grant.aad.epoch, grant);
                }
                self.state.devices.insert(
                    cert.device_id,
                    DeviceEntryV2 {
                        cert,
                        status: DeviceStatusV2::Active,
                        approved_rev: rev,
                        revoked_rev: None,
                        grants: map,
                    },
                );
                if new_credential {
                    self.bump_authz()?;
                }
            }
            EffectV2::DeviceRevoke { targets, changed } => {
                for device_id in &targets {
                    if let Some(device) = self.state.devices.get_mut(device_id) {
                        if device.status == DeviceStatusV2::Active {
                            device.status = DeviceStatusV2::Revoked;
                            device.revoked_rev = Some(rev);
                        }
                    }
                }
                let target_set: BTreeSet<[u8; 16]> = targets.iter().copied().collect();
                for entry in self.state.caps.values_mut() {
                    if target_set.contains(&entry.cap.subject_device_id)
                        && entry.revoked_control_rev.is_none()
                    {
                        entry.revoked_control_rev = Some(rev);
                    }
                }
                if changed {
                    self.bump_authz()?;
                }
            }
            EffectV2::RotationBegin {
                rotation_id,
                from_epoch,
                to_epoch,
                revoke_ids,
                member_set_hash,
            } => {
                self.revoke_devices_by_rotation(rev, &revoke_ids);
                self.state.rotations.insert(
                    rotation_id,
                    RotationRecordV2 {
                        rotation_id,
                        from_epoch,
                        to_epoch,
                        freeze_seq: 0,
                        state: RotationStateV2::Frozen,
                        member_set_hash,
                        created_rev: rev,
                        terminal_rev: None,
                    },
                );
            }
            EffectV2::RotationAbort { rotation_id } => {
                let record = self
                    .state
                    .rotations
                    .get_mut(&rotation_id)
                    .ok_or_else(|| invariant("轮换中止投影缺记录"))?;
                record.state = RotationStateV2::Aborted;
                record.terminal_rev = Some(rev);
            }
            EffectV2::RotationRestart {
                old_rotation_id,
                new_to_epoch,
                extra_ids,
                member_set_hash,
            } => {
                let old = self
                    .state
                    .rotations
                    .get_mut(&old_rotation_id)
                    .ok_or_else(|| invariant("轮换重启投影缺旧记录"))?;
                old.state = RotationStateV2::Restarted;
                old.to_epoch = new_to_epoch;
                old.terminal_rev = Some(rev);
                old.member_set_hash = member_set_hash;
                self.revoke_devices_by_rotation(rev, &extra_ids);
            }
            EffectV2::RotationAddRevokes {
                rotation_id,
                device_ids,
            } => {
                let record = self
                    .state
                    .rotations
                    .get(&rotation_id)
                    .ok_or_else(|| invariant("轮换追加撤销投影缺记录"))?;
                if record.state != RotationStateV2::Frozen {
                    return Err(invariant("轮换追加撤销投影状态不符"));
                }
                self.revoke_devices_by_rotation(rev, &device_ids);
            }
            EffectV2::EpochActivate {
                rotation_id,
                freeze_seq,
                member_changed,
            } => {
                let record = self
                    .state
                    .rotations
                    .get_mut(&rotation_id)
                    .ok_or_else(|| invariant("轮换激活投影缺记录"))?;
                record.state = RotationStateV2::Activated;
                record.terminal_rev = Some(rev);
                record.freeze_seq = freeze_seq;
                if member_changed {
                    self.bump_authz()?;
                }
            }
            EffectV2::InviteCreate(entry) => {
                self.state.invites.insert(entry.invite_id, entry);
            }
            EffectV2::InviteCancel(invite_id) => {
                if let Some(invite) = self.state.invites.get_mut(&invite_id) {
                    if invite.cancelled_rev.is_none() {
                        invite.cancelled_rev = Some(rev);
                    }
                }
            }
            EffectV2::RecoveryReset {
                policy,
                recovery_pub,
                caps,
                adds,
                revoke_ids,
            } => {
                for device_id in &revoke_ids {
                    if let Some(device) = self.state.devices.get_mut(device_id) {
                        device.status = DeviceStatusV2::Revoked;
                        device.revoked_rev = Some(rev);
                    }
                    for entry in self.state.caps.values_mut() {
                        if entry.cap.subject_device_id == *device_id
                            && entry.revoked_control_rev.is_none()
                        {
                            entry.revoked_control_rev = Some(rev);
                        }
                    }
                }
                for (cert, grants) in adds {
                    let mut map = BTreeMap::new();
                    for grant in grants {
                        self.state.grant_ids.insert(grant.aad.grant_id);
                        map.insert(grant.aad.epoch, grant);
                    }
                    self.state.devices.insert(
                        cert.device_id,
                        DeviceEntryV2 {
                            cert,
                            status: DeviceStatusV2::Active,
                            approved_rev: rev,
                            revoked_rev: None,
                            grants: map,
                        },
                    );
                }
                self.state.policy = Some(policy);
                self.state.authority_rev = self.state.authority_rev.saturating_add(1);
                self.state.recovery_pub = Some(recovery_pub);
                self.bump_authz()?;
                for cap in caps {
                    self.insert_cap(cap, rev)?;
                }
                self.refresh_authz_state();
            }
            EffectV2::RecoveryKeyChange { recovery_pub, caps } => {
                self.state.authority_rev = self.state.authority_rev.saturating_add(1);
                self.state.recovery_pub = Some(recovery_pub);
                self.bump_authz()?;
                for cap in caps {
                    self.insert_cap(cap, rev)?;
                }
                self.refresh_authz_state();
            }
        }
        Ok(())
    }

    fn bump_authz(&mut self) -> Result<(), SyncError> {
        self.state.authz_rev = self
            .state
            .authz_rev
            .checked_add(1)
            .ok_or_else(|| invariant("authz_rev 溢出"))?;
        Ok(())
    }

    /// 恢复之后重新判定 `active`/`recovery_only`（成员与 CAP_ADMIN quorum 双条件）。
    fn refresh_authz_state(&mut self) {
        let mut cap_admin = BTreeSet::new();
        for entry in self.state.caps.values() {
            if entry.is_revoked() || entry.cap.issuer_authority_rev != self.state.authority_rev {
                continue;
            }
            if entry.cap.scopes.bits() & CapScope::CAP_ADMIN != 0
                && self.state.is_active_device(&entry.cap.subject_device_id)
            {
                cap_admin.insert(entry.cap.subject_device_id);
            }
        }
        let active = self.state.active_device_count();
        let min = self
            .state
            .policy
            .as_ref()
            .map_or(2, |policy| policy.min_quorum as usize);
        self.state.authz_state = Some(if active >= min && cap_admin.len() >= 2 {
            AuthzStateV2::Active
        } else {
            AuthzStateV2::RecoveryOnly
        });
    }
}

/// `RecoveryKeyChange` 的 cap TTL 上限来自当前 policy（换钥不改 policy）。
fn payload_cap_ttl_bound(replay: &V2ControlReplay) -> Result<u64, SyncError> {
    replay
        .state
        .policy
        .as_ref()
        .map(|policy| policy.max_cap_ttl)
        .ok_or_else(|| invariant("换钥缺已验证 policy"))
}

// ---------------------------------------------------------------- 持久化：V2 pin

/// durable V2 control pin：`I/V/head/authz/authority` + 缓存摘要。
///
/// 与 v1 `sync.control_pin`（[`crate::control_pin`]）同一纪律：只允许前进/同 rev 相等；
/// 损坏/版本未知/绑定不符是**明确错误**（fail-stop），不当成"没有 pin"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V2PinV1 {
    /// pin 绑定的 vault id。
    pub vault_id: [u8; 16],
    /// relay instance id。
    pub instance_id: [u8; 16],
    /// 已验证链尾 `control_rev`。
    pub rev: u64,
    /// 已验证链尾 `record_hash`。
    pub head_hash: [u8; 32],
    /// 该链尾对应的 `authz_rev`。
    pub authz_rev: u64,
    /// 该链尾对应的 `authority_rev`。
    pub authority_rev: u64,
    /// 授权缓存（[`META_MANAGEMENT_CACHE_V2`]）的 canonical 字节摘要。
    ///
    /// 只用于**识别缓存损坏/被篡改**（缓存不是信任根：摘要不符就丢弃并从 0 重建）。
    pub state_digest: [u8; 32],
}

const PIN_KEY_VERSION: u64 = 0;
const PIN_KEY_VAULT: u64 = 1;
const PIN_KEY_INSTANCE: u64 = 2;
const PIN_KEY_REV: u64 = 3;
const PIN_KEY_HEAD: u64 = 4;
const PIN_KEY_AUTHZ: u64 = 5;
const PIN_KEY_AUTHORITY: u64 = 6;
const PIN_KEY_DIGEST: u64 = 7;

/// canonical CDE 编码（唯一编码器）。
pub fn encode_v2_pin(pin: &V2PinV1) -> Result<Vec<u8>, SyncError> {
    let value = CdeValue::Map(vec![
        (PIN_KEY_VERSION, CdeValue::UInt(CONTROL_PIN_V2_VERSION)),
        (PIN_KEY_VAULT, CdeValue::Bytes(pin.vault_id.to_vec())),
        (PIN_KEY_INSTANCE, CdeValue::Bytes(pin.instance_id.to_vec())),
        (PIN_KEY_REV, CdeValue::UInt(pin.rev)),
        (PIN_KEY_HEAD, CdeValue::Bytes(pin.head_hash.to_vec())),
        (PIN_KEY_AUTHZ, CdeValue::UInt(pin.authz_rev)),
        (PIN_KEY_AUTHORITY, CdeValue::UInt(pin.authority_rev)),
        (PIN_KEY_DIGEST, CdeValue::Bytes(pin.state_digest.to_vec())),
    ]);
    menghunote_sync_proto::encode_cde(&value)
        .map_err(|_| SyncError::PinSaveFailed("sync.control_pin_v2 无法 canonical 编码".to_owned()))
}

/// canonical CDE 解码 + 严格字段/版本/长度校验（损坏即 `PinCorrupt`）。
pub fn decode_v2_pin(bytes: &[u8]) -> Result<V2PinV1, SyncError> {
    let CdeValue::Map(fields) = menghunote_sync_proto::decode_cde(bytes)
        .map_err(|_| SyncError::PinCorrupt("sync.control_pin_v2 不是合法 canonical CDE"))?
    else {
        return Err(SyncError::PinCorrupt("sync.control_pin_v2 不是 CDE map"));
    };
    let allowed = [
        PIN_KEY_VERSION,
        PIN_KEY_VAULT,
        PIN_KEY_INSTANCE,
        PIN_KEY_REV,
        PIN_KEY_HEAD,
        PIN_KEY_AUTHZ,
        PIN_KEY_AUTHORITY,
        PIN_KEY_DIGEST,
    ];
    if fields.iter().any(|(key, _)| !allowed.contains(key)) || fields.len() != allowed.len() {
        return Err(SyncError::PinCorrupt("sync.control_pin_v2 字段集合不合法"));
    }
    let version = cache_uint(&fields, PIN_KEY_VERSION, "pin 版本")?;
    if version != CONTROL_PIN_V2_VERSION {
        return Err(SyncError::PinCorrupt("sync.control_pin_v2 版本不支持"));
    }
    Ok(V2PinV1 {
        vault_id: cache_fixed::<16>(&fields, PIN_KEY_VAULT, "pin vault_id")?,
        instance_id: cache_fixed::<16>(&fields, PIN_KEY_INSTANCE, "pin instance_id")?,
        rev: cache_uint(&fields, PIN_KEY_REV, "pin rev")?,
        head_hash: cache_fixed::<32>(&fields, PIN_KEY_HEAD, "pin head_hash")?,
        authz_rev: cache_uint(&fields, PIN_KEY_AUTHZ, "pin authz_rev")?,
        authority_rev: cache_uint(&fields, PIN_KEY_AUTHORITY, "pin authority_rev")?,
        state_digest: cache_fixed::<32>(&fields, PIN_KEY_DIGEST, "pin state_digest")?,
    })
}

/// 读取 durable V2 pin：不存在 → `Ok(None)`；损坏/绑定不符 → 明确错误。
pub fn load_v2_pin(vault: &Vault, expected_vault: [u8; 16]) -> Result<Option<V2PinV1>, SyncError> {
    let Some(bytes) = vault.get_meta(META_CONTROL_PIN_V2)? else {
        return Ok(None);
    };
    let pin = decode_v2_pin(&bytes)?;
    if pin.vault_id != expected_vault {
        return Err(SyncError::PinCorrupt(
            "sync.control_pin_v2 的 vault 绑定与本地 vault 不一致",
        ));
    }
    Ok(Some(pin))
}

/// 提交已验证 head（CAS：只前进/同 rev 相等；冲突不覆盖赢家）。
fn commit_v2_pin(
    vault: &Vault,
    expected: Option<&V2PinV1>,
    new: &V2PinV1,
) -> Result<(), SyncError> {
    let new_bytes = encode_v2_pin(new)?;
    let key = Value::Text(META_CONTROL_PIN_V2.to_owned());
    let statement = match expected {
        None => Statement::new(
            "INSERT INTO meta(key, value) SELECT ?1, ?2 \
             WHERE NOT EXISTS (SELECT 1 FROM meta WHERE key = ?1)",
            vec![key, Value::Blob(new_bytes)],
        ),
        Some(pin) => {
            let old_bytes = encode_v2_pin(pin)?;
            Statement::new(
                "UPDATE meta SET value = ?2 \
                 WHERE key = ?1 AND typeof(value) = 'blob' AND value = ?3",
                vec![key, Value::Blob(new_bytes), Value::Blob(old_bytes)],
            )
        }
    };
    match vault.exec_transaction(&[statement], &[AffectedGuard::exactly(0, 1)]) {
        Ok(_) => Ok(()),
        Err(StorageError::GuardFailed { .. }) => Err(SyncError::PinConflict),
        Err(error) => Err(SyncError::PinSaveFailed(format!(
            "sync.control_pin_v2 写入失败：{error}"
        ))),
    }
}

// ---------------------------------------------------------------- 持久化：cap 缓存

const CACHE_KEY_VERSION: u64 = 0;
const CACHE_KEY_VAULT: u64 = 1;
const CACHE_KEY_INSTANCE: u64 = 2;
const CACHE_KEY_REV: u64 = 3;
const CACHE_KEY_HEAD: u64 = 4;
const CACHE_KEY_AUTHORITY: u64 = 5;
const CACHE_KEY_AUTHZ: u64 = 6;
const CACHE_KEY_AUTHZ_STATE: u64 = 7;
const CACHE_KEY_POLICY: u64 = 8;
const CACHE_KEY_RECOVERY: u64 = 9;
const CACHE_KEY_CAPS: u64 = 10;
const CACHE_KEY_DEVICES: u64 = 11;
const CACHE_KEY_INVITES: u64 = 12;
const CACHE_KEY_ROTATIONS: u64 = 13;
const CACHE_KEY_CAP_IDS: u64 = 14;
const CACHE_KEY_GRANT_IDS: u64 = 15;
const CACHE_KEY_REVOKED_UNSEEN: u64 = 16;

fn cache_uint(fields: &[(u64, CdeValue)], key: u64, what: &'static str) -> Result<u64, SyncError> {
    match fields.iter().find(|(index, _)| *index == key) {
        Some((_, CdeValue::UInt(value))) => Ok(*value),
        _ => Err(SyncError::LocalCorrupt(what)),
    }
}

fn cache_opt_uint(
    fields: &[(u64, CdeValue)],
    key: u64,
    what: &'static str,
) -> Result<Option<u64>, SyncError> {
    match fields.iter().find(|(index, _)| *index == key) {
        None => Ok(None),
        Some((_, CdeValue::UInt(value))) => Ok(Some(*value)),
        Some(_) => Err(SyncError::LocalCorrupt(what)),
    }
}

fn cache_bytes(
    fields: &[(u64, CdeValue)],
    key: u64,
    what: &'static str,
) -> Result<Vec<u8>, SyncError> {
    match fields.iter().find(|(index, _)| *index == key) {
        Some((_, CdeValue::Bytes(value))) => Ok(value.clone()),
        _ => Err(SyncError::LocalCorrupt(what)),
    }
}

fn cache_opt_bytes(
    fields: &[(u64, CdeValue)],
    key: u64,
    what: &'static str,
) -> Result<Option<Vec<u8>>, SyncError> {
    match fields.iter().find(|(index, _)| *index == key) {
        None => Ok(None),
        Some((_, CdeValue::Bytes(value))) => Ok(Some(value.clone())),
        Some(_) => Err(SyncError::LocalCorrupt(what)),
    }
}

fn cache_fixed<const N: usize>(
    fields: &[(u64, CdeValue)],
    key: u64,
    what: &'static str,
) -> Result<[u8; N], SyncError> {
    cache_bytes(fields, key, what)?
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt(what))
}

fn cache_array(
    fields: &[(u64, CdeValue)],
    key: u64,
    what: &'static str,
) -> Result<Vec<CdeValue>, SyncError> {
    match fields.iter().find(|(index, _)| *index == key) {
        Some((_, CdeValue::Array(values))) => Ok(values.clone()),
        _ => Err(SyncError::LocalCorrupt(what)),
    }
}

fn cache_map(value: &CdeValue, what: &'static str) -> Result<Vec<(u64, CdeValue)>, SyncError> {
    match value {
        CdeValue::Map(fields) => Ok(fields.clone()),
        _ => Err(SyncError::LocalCorrupt(what)),
    }
}

impl ControlStateV2 {
    /// canonical 缓存编码（可重建的加速结构，不含任何秘密）。
    pub fn encode_cache(&self) -> Result<Vec<u8>, SyncError> {
        let mut fields = vec![
            (
                CACHE_KEY_VERSION,
                CdeValue::UInt(MANAGEMENT_CACHE_V2_VERSION),
            ),
            (CACHE_KEY_VAULT, CdeValue::Bytes(self.vault_id.to_vec())),
            (
                CACHE_KEY_INSTANCE,
                CdeValue::Bytes(self.instance_id.to_vec()),
            ),
            (CACHE_KEY_REV, CdeValue::UInt(self.rev)),
            (CACHE_KEY_HEAD, CdeValue::Bytes(self.head_hash.to_vec())),
            (CACHE_KEY_AUTHORITY, CdeValue::UInt(self.authority_rev)),
            (CACHE_KEY_AUTHZ, CdeValue::UInt(self.authz_rev)),
            (
                CACHE_KEY_CAP_IDS,
                CdeValue::Array(
                    self.cap_ids_seen
                        .iter()
                        .map(|id| CdeValue::Bytes(id.to_vec()))
                        .collect(),
                ),
            ),
            (
                CACHE_KEY_GRANT_IDS,
                CdeValue::Array(
                    self.grant_ids
                        .iter()
                        .map(|id| CdeValue::Bytes(id.to_vec()))
                        .collect(),
                ),
            ),
            (
                CACHE_KEY_REVOKED_UNSEEN,
                CdeValue::Array(
                    self.revoked_unseen
                        .iter()
                        .map(|id| CdeValue::Bytes(id.to_vec()))
                        .collect(),
                ),
            ),
        ];
        if let Some(state) = self.authz_state {
            fields.push((CACHE_KEY_AUTHZ_STATE, CdeValue::UInt(state.as_code())));
        }
        if let Some(policy) = &self.policy {
            fields.push((CACHE_KEY_POLICY, CdeValue::Bytes(policy.encode()?)));
        }
        if let Some(recovery_pub) = self.recovery_pub {
            fields.push((CACHE_KEY_RECOVERY, CdeValue::Bytes(recovery_pub.to_vec())));
        }
        let caps = self
            .caps
            .values()
            .map(|entry| {
                let mut fields = vec![
                    (0, CdeValue::Bytes(entry.cap.encode()?)),
                    (1, CdeValue::UInt(entry.granted_rev)),
                ];
                if let Some(revoked) = entry.revoked_control_rev {
                    fields.push((2, CdeValue::UInt(revoked)));
                }
                Ok(CdeValue::Map(fields))
            })
            .collect::<Result<Vec<_>, SyncError>>()?;
        fields.push((CACHE_KEY_CAPS, CdeValue::Array(caps)));
        let devices = self
            .devices
            .values()
            .map(|entry| {
                let grants = entry
                    .grants
                    .values()
                    .map(|grant| Ok(CdeValue::Bytes(grant.encode()?)))
                    .collect::<Result<Vec<_>, SyncError>>()?;
                let mut fields = vec![
                    (0, CdeValue::Bytes(entry.cert.device_id.to_vec())),
                    (1, CdeValue::UInt(device_status_code(entry.status))),
                    (2, CdeValue::Bytes(entry.cert.encode()?)),
                    (3, CdeValue::UInt(entry.approved_rev)),
                    (5, CdeValue::Array(grants)),
                ];
                if let Some(revoked) = entry.revoked_rev {
                    fields.push((4, CdeValue::UInt(revoked)));
                }
                Ok(CdeValue::Map(fields))
            })
            .collect::<Result<Vec<_>, SyncError>>()?;
        fields.push((CACHE_KEY_DEVICES, CdeValue::Array(devices)));
        let invites = self
            .invites
            .values()
            .map(|invite| {
                let mut fields = vec![
                    (0, CdeValue::Bytes(invite.invite_id.to_vec())),
                    (1, CdeValue::Bytes(invite.token_hash.to_vec())),
                    (2, CdeValue::UInt(invite.expires_at)),
                    (3, CdeValue::Bytes(invite.issuer_cap_id.to_vec())),
                    (4, CdeValue::UInt(invite.created_rev)),
                ];
                if let Some(cancelled) = invite.cancelled_rev {
                    fields.push((5, CdeValue::UInt(cancelled)));
                }
                Ok(CdeValue::Map(fields))
            })
            .collect::<Result<Vec<_>, SyncError>>()?;
        fields.push((CACHE_KEY_INVITES, CdeValue::Array(invites)));
        let rotations = self
            .rotations
            .values()
            .map(|rotation| {
                let mut fields = vec![
                    (0, CdeValue::Bytes(rotation.rotation_id.to_vec())),
                    (1, CdeValue::UInt(rotation.from_epoch)),
                    (2, CdeValue::UInt(rotation.to_epoch)),
                    (3, CdeValue::UInt(rotation.freeze_seq)),
                    (4, CdeValue::UInt(rotation.state.as_code())),
                    (5, CdeValue::Bytes(rotation.member_set_hash.to_vec())),
                    (6, CdeValue::UInt(rotation.created_rev)),
                ];
                if let Some(terminal) = rotation.terminal_rev {
                    fields.push((7, CdeValue::UInt(terminal)));
                }
                CdeValue::Map(fields)
            })
            .collect::<Vec<_>>();
        fields.push((CACHE_KEY_ROTATIONS, CdeValue::Array(rotations)));
        fields.sort_by_key(|(key, _)| *key);
        let value = CdeValue::Map(fields);
        let bytes = menghunote_sync_proto::encode_cde(&value)
            .map_err(|_| SyncError::LocalCorrupt("V2 授权缓存无法 canonical 编码"))?;
        Ok(bytes)
    }

    /// 严格解码缓存；绑定不符/字段残缺/非 canonical 一律 `LocalCorrupt`（调用方丢弃重建）。
    pub fn decode_cache(
        bytes: &[u8],
        expected_vault: VaultId,
        expected_instance: [u8; 16],
    ) -> Result<Self, SyncError> {
        let value = menghunote_sync_proto::decode_cde(bytes)
            .map_err(|_| SyncError::LocalCorrupt("V2 授权缓存不是合法 canonical CDE"))?;
        // 重新编码必须逐字节相同（非 canonical 视为损坏）。
        if menghunote_sync_proto::encode_cde(&value)
            .map_err(|_| SyncError::LocalCorrupt("V2 授权缓存无法重新编码"))?
            != bytes
        {
            return Err(SyncError::LocalCorrupt("V2 授权缓存不是 canonical 编码"));
        }
        let fields = cache_map(&value, "V2 授权缓存不是 map")?;
        let allowed = [
            CACHE_KEY_VERSION,
            CACHE_KEY_VAULT,
            CACHE_KEY_INSTANCE,
            CACHE_KEY_REV,
            CACHE_KEY_HEAD,
            CACHE_KEY_AUTHORITY,
            CACHE_KEY_AUTHZ,
            CACHE_KEY_AUTHZ_STATE,
            CACHE_KEY_POLICY,
            CACHE_KEY_RECOVERY,
            CACHE_KEY_CAPS,
            CACHE_KEY_DEVICES,
            CACHE_KEY_INVITES,
            CACHE_KEY_ROTATIONS,
            CACHE_KEY_CAP_IDS,
            CACHE_KEY_GRANT_IDS,
            CACHE_KEY_REVOKED_UNSEEN,
        ];
        if fields.iter().any(|(key, _)| !allowed.contains(key)) {
            return Err(SyncError::LocalCorrupt("V2 授权缓存含未知字段"));
        }
        if cache_uint(&fields, CACHE_KEY_VERSION, "缓存版本")? != MANAGEMENT_CACHE_V2_VERSION {
            return Err(SyncError::LocalCorrupt("V2 授权缓存版本不支持"));
        }
        let vault_id: VaultId = cache_fixed(&fields, CACHE_KEY_VAULT, "缓存 vault_id")?;
        let instance_id: [u8; 16] = cache_fixed(&fields, CACHE_KEY_INSTANCE, "缓存 instance_id")?;
        if vault_id != expected_vault || instance_id != expected_instance {
            return Err(SyncError::LocalCorrupt(
                "V2 授权缓存与本地 vault/instance 绑定不符",
            ));
        }
        let rev = cache_uint(&fields, CACHE_KEY_REV, "缓存 rev")?;
        let head_hash = cache_fixed(&fields, CACHE_KEY_HEAD, "缓存 head_hash")?;
        let authority_rev = cache_uint(&fields, CACHE_KEY_AUTHORITY, "缓存 authority_rev")?;
        let authz_rev = cache_uint(&fields, CACHE_KEY_AUTHZ, "缓存 authz_rev")?;
        let authz_state = match cache_opt_uint(&fields, CACHE_KEY_AUTHZ_STATE, "缓存 authz_state")?
        {
            None => None,
            Some(code) => Some(
                AuthzStateV2::from_code(code)
                    .ok_or(SyncError::LocalCorrupt("缓存 authz_state 非法"))?,
            ),
        };
        let policy = match cache_opt_bytes(&fields, CACHE_KEY_POLICY, "缓存 policy")? {
            None => None,
            Some(bytes) => {
                let policy = PolicyV2::decode(&bytes)?;
                if policy.encode()? != bytes {
                    return Err(SyncError::LocalCorrupt("缓存 policy 非 canonical"));
                }
                Some(policy)
            }
        };
        let recovery_pub = match cache_opt_bytes(&fields, CACHE_KEY_RECOVERY, "缓存 recovery_pub")?
        {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SyncError::LocalCorrupt("缓存 recovery_pub 长度非法"))?,
            ),
        };
        let mut caps = BTreeMap::new();
        for value in cache_array(&fields, CACHE_KEY_CAPS, "缓存 caps")? {
            let entry = cache_map(&value, "缓存 cap 不是 map")?;
            let bytes = cache_bytes(&entry, 0, "缓存 cap canonical")?;
            let cap = ManagementCapV2::decode(&bytes)?;
            if cap.encode()? != bytes {
                return Err(SyncError::LocalCorrupt("缓存 cap 非 canonical"));
            }
            let granted_rev = cache_uint(&entry, 1, "缓存 cap granted_rev")?;
            let revoked_control_rev = cache_opt_uint(&entry, 2, "缓存 cap revoked_rev")?;
            if caps
                .insert(
                    cap.cap_id,
                    CapEntryV2 {
                        cap,
                        granted_rev,
                        revoked_control_rev,
                    },
                )
                .is_some()
            {
                return Err(SyncError::LocalCorrupt("缓存 cap_id 重复"));
            }
        }
        let mut devices = BTreeMap::new();
        for value in cache_array(&fields, CACHE_KEY_DEVICES, "缓存 devices")? {
            let entry = cache_map(&value, "缓存 device 不是 map")?;
            let device_id: [u8; 16] = cache_fixed(&entry, 0, "缓存 device_id")?;
            let status = match cache_uint(&entry, 1, "缓存 device status")? {
                1 => DeviceStatusV2::Active,
                2 => DeviceStatusV2::Revoked,
                _ => return Err(SyncError::LocalCorrupt("缓存 device status 非法")),
            };
            let cert_bytes = cache_bytes(&entry, 2, "缓存 device cert")?;
            let cert = DeviceCertV2::decode(&cert_bytes)?;
            if cert.encode()? != cert_bytes || cert.device_id != device_id {
                return Err(SyncError::LocalCorrupt("缓存 device cert 不一致"));
            }
            let approved_rev = cache_uint(&entry, 3, "缓存 device approved_rev")?;
            let revoked_rev = cache_opt_uint(&entry, 4, "缓存 device revoked_rev")?;
            let mut grants = BTreeMap::new();
            for value in cache_array(&entry, 5, "缓存 device grants")? {
                let bytes = match value {
                    CdeValue::Bytes(bytes) => bytes,
                    _ => return Err(SyncError::LocalCorrupt("缓存 grant 不是 bytes")),
                };
                let grant = GrantPackageV2::decode(&bytes)?;
                if grant.encode()? != bytes {
                    return Err(SyncError::LocalCorrupt("缓存 grant 非 canonical"));
                }
                if grants.insert(grant.aad.epoch, grant).is_some() {
                    return Err(SyncError::LocalCorrupt("缓存 grant epoch 重复"));
                }
            }
            let device = DeviceEntryV2 {
                cert,
                status,
                approved_rev,
                revoked_rev,
                grants,
            };
            if devices.insert(device_id, device).is_some() {
                return Err(SyncError::LocalCorrupt("缓存 device_id 重复"));
            }
        }
        let mut invites = BTreeMap::new();
        for value in cache_array(&fields, CACHE_KEY_INVITES, "缓存 invites")? {
            let entry = cache_map(&value, "缓存 invite 不是 map")?;
            let invite_id: [u8; 16] = cache_fixed(&entry, 0, "缓存 invite_id")?;
            let invite = InviteEntryV2 {
                invite_id,
                token_hash: cache_fixed(&entry, 1, "缓存 token_hash")?,
                expires_at: cache_uint(&entry, 2, "缓存 invite expires_at")?,
                issuer_cap_id: cache_fixed(&entry, 3, "缓存 invite issuer")?,
                created_rev: cache_uint(&entry, 4, "缓存 invite created_rev")?,
                cancelled_rev: cache_opt_uint(&entry, 5, "缓存 invite cancelled_rev")?,
            };
            if invites.insert(invite_id, invite).is_some() {
                return Err(SyncError::LocalCorrupt("缓存 invite_id 重复"));
            }
        }
        let mut rotations = BTreeMap::new();
        for value in cache_array(&fields, CACHE_KEY_ROTATIONS, "缓存 rotations")? {
            let entry = cache_map(&value, "缓存 rotation 不是 map")?;
            let rotation_id: [u8; 16] = cache_fixed(&entry, 0, "缓存 rotation_id")?;
            let state = RotationStateV2::from_code(cache_uint(&entry, 4, "缓存 rotation state")?)
                .ok_or(SyncError::LocalCorrupt("缓存 rotation state 非法"))?;
            let rotation = RotationRecordV2 {
                rotation_id,
                from_epoch: cache_uint(&entry, 1, "缓存 rotation from_epoch")?,
                to_epoch: cache_uint(&entry, 2, "缓存 rotation to_epoch")?,
                freeze_seq: cache_uint(&entry, 3, "缓存 rotation freeze_seq")?,
                state,
                member_set_hash: cache_fixed(&entry, 5, "缓存 rotation member_set_hash")?,
                created_rev: cache_uint(&entry, 6, "缓存 rotation created_rev")?,
                terminal_rev: cache_opt_uint(&entry, 7, "缓存 rotation terminal_rev")?,
            };
            if rotations.insert(rotation_id, rotation).is_some() {
                return Err(SyncError::LocalCorrupt("缓存 rotation_id 重复"));
            }
        }
        let mut cap_ids_seen = BTreeSet::new();
        for value in cache_array(&fields, CACHE_KEY_CAP_IDS, "缓存 cap_ids")? {
            match value {
                CdeValue::Bytes(bytes) => {
                    cap_ids_seen.insert(
                        bytes
                            .as_slice()
                            .try_into()
                            .map_err(|_| SyncError::LocalCorrupt("缓存 cap_id 长度非法"))?,
                    );
                }
                _ => return Err(SyncError::LocalCorrupt("缓存 cap_id 不是 bytes")),
            }
        }
        let mut grant_ids = BTreeSet::new();
        for value in cache_array(&fields, CACHE_KEY_GRANT_IDS, "缓存 grant_ids")? {
            match value {
                CdeValue::Bytes(bytes) => {
                    grant_ids.insert(
                        bytes
                            .as_slice()
                            .try_into()
                            .map_err(|_| SyncError::LocalCorrupt("缓存 grant_id 长度非法"))?,
                    );
                }
                _ => return Err(SyncError::LocalCorrupt("缓存 grant_id 不是 bytes")),
            }
        }
        let mut revoked_unseen = BTreeSet::new();
        for value in cache_array(&fields, CACHE_KEY_REVOKED_UNSEEN, "缓存 revoked_unseen")? {
            match value {
                CdeValue::Bytes(bytes) => {
                    revoked_unseen.insert(
                        bytes
                            .as_slice()
                            .try_into()
                            .map_err(|_| SyncError::LocalCorrupt("缓存 revoked_unseen 长度非法"))?,
                    );
                }
                _ => return Err(SyncError::LocalCorrupt("缓存 revoked_unseen 不是 bytes")),
            }
        }
        if authz_state.is_some() && (policy.is_none() || recovery_pub.is_none()) {
            return Err(SyncError::LocalCorrupt(
                "V2 授权缓存缺 policy/recovery_pub（状态不完整）",
            ));
        }
        Ok(Self {
            vault_id,
            instance_id,
            rev,
            head_hash,
            authority_rev,
            authz_rev,
            authz_state,
            policy,
            recovery_pub,
            caps,
            devices,
            invites,
            rotations,
            cap_ids_seen,
            grant_ids,
            revoked_unseen,
        })
    }
}

const fn device_status_code(status: DeviceStatusV2) -> u64 {
    match status {
        DeviceStatusV2::Active => 1,
        DeviceStatusV2::Revoked => 2,
    }
}

// ---------------------------------------------------------------- realm3 鉴权结果

/// realm 3 管理 bearer（明文 token 只在内存；DB 只存哈希）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementTokenV2 {
    /// 明文 bearer（不落盘/不日志）。
    token: String,
    /// 绑定的 cap。
    cap_id: [u8; 16],
    /// 该 cap 的 scope（选定时的快照；每次使用仍按回放状态重验）。
    scopes: CapScope,
    /// 到期时刻（`min(now+600, cap 剩余)`）。
    expires_at: u64,
    /// 签发视图的 `authz_rev`。
    authz_rev: u64,
    /// 签发视图的 `authority_rev`。
    authority_rev: u64,
    /// 签发视图的 `auth_rev`。
    auth_rev: u64,
}

impl ManagementTokenV2 {
    /// bearer 文本（调用方只应放进 `Authorization` 头）。
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// 绑定的 cap。
    #[must_use]
    pub const fn cap_id(&self) -> [u8; 16] {
        self.cap_id
    }

    /// 到期时刻。
    #[must_use]
    pub const fn expires_at(&self) -> u64 {
        self.expires_at
    }

    /// 签发视图的 `authz_rev`。
    #[must_use]
    pub const fn authz_rev(&self) -> u64 {
        self.authz_rev
    }

    /// 签发视图的 `authority_rev`。
    #[must_use]
    pub const fn authority_rev(&self) -> u64 {
        self.authority_rev
    }
}

/// relay `/control` 响应里的**只读成员投影**（relay `devices` 表；含 PENDING 兑换）。
///
/// **不是授权证据**（ADR 0016）：ACTIVE/REVOKED 一律以已验证控制链为准，本类型只用于
/// UI 补全"已兑换但未批准（PENDING）"的设备行。relay 伪造它最多影响展示，不参与任何
/// cap 选择、门限或管理动作；客户端也不会把这里的状态写进 pin/缓存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayMemberHintV2 {
    /// 设备 id。
    pub device_id: [u8; 16],
    /// relay 投影状态（`active` / `pending` / `revoked`；未知值原样保留）。
    pub status: String,
}

/// 控制提交回执（ADR 0015 §1 的公开链事实；`{0:rev,1:head,2:kind,3:request_id,4:authz,5:authority}`）。
///
/// 回执**只**是"relay 说这次提交落在哪一 rev"的公开事实：客户端仍要用自己的已验证
/// 回放复算链 hash/记录，绝不把回执当授权依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlReceiptV2 {
    /// 本次提交写出的控制提交 rev（= `expected_control_rev + 1`）。
    pub control_rev: u64,
    /// 该提交的链尾 hash（`SHA256(prev_hash || SignedControlV2_bytes)`）。
    pub head_hash: [u8; 32],
    /// 控制记录 kind。
    pub kind: u8,
    /// 操作 `request_id`（dedup 键的一部分）。
    pub request_id: [u8; 16],
    /// 提交后的 `authz_rev`。
    pub authz_rev: u64,
    /// 提交后的 `authority_rev`。
    pub authority_rev: u64,
}

/// 客户端配置（全部来自本地 pin/身份，不来自 relay 投影）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V2ManagementConfig {
    /// vault id。
    pub vault_id: VaultId,
    /// 本地 pin 的 relay instance id。
    pub instance_id: [u8; 16],
    /// 本机设备 id。
    pub device_id: DeviceId,
    /// pin 住的管理公钥（Upgrade 的 role1 信任锚）。
    pub manager_pub: [u8; 32],
}

#[derive(Debug, Clone)]
struct DeviceTokenV2 {
    token: String,
    expires_at: u64,
}

// ---------------------------------------------------------------- 客户端

/// V2 管理客户端：控制链回放 + cap 选择 + realm3 鉴权 + 控制提交（低层）。
///
/// 授权事实与 bearer 都是已验证的；`OperationV2` 的构造/签名/双签状态机在
/// `crate::approval_v1`（本类型只提供 `submit_control_bytes` 与干跑验证入口）。
pub struct V2ManagementClient<T: HttpTransport, C: Clock = SystemClock> {
    transport: T,
    api: RelayApi,
    config: V2ManagementConfig,
    signing_key: DeviceSigningKey,
    clock: C,
    device_token: Option<DeviceTokenV2>,
    management_token: Option<ManagementTokenV2>,
    replay: V2ControlReplay,
    store: Option<Vault>,
    pin: Option<V2PinV1>,
    relay_epoch_hint: Option<u64>,
    /// relay `/control` 的只读成员投影（含 PENDING；**不是**授权证据）。
    relay_member_hints: Vec<RelayMemberHintV2>,
}

impl<T: HttpTransport> V2ManagementClient<T, SystemClock> {
    /// 构造（未接持久化；`with_store` 后才会读/写 pin 与缓存）。
    pub fn new(
        transport: T,
        origin: impl Into<String>,
        config: V2ManagementConfig,
        signing_key: DeviceSigningKey,
    ) -> Self {
        let replay = V2ControlReplay::new(config.vault_id, config.instance_id)
            .with_manager_pub(config.manager_pub);
        Self {
            transport,
            api: RelayApi::new(origin, RelayApiVersion::V2),
            config,
            signing_key,
            clock: SystemClock,
            device_token: None,
            management_token: None,
            replay,
            store: None,
            pin: None,
            relay_epoch_hint: None,
            relay_member_hints: Vec::new(),
        }
    }
}

impl<T: HttpTransport, C: Clock> V2ManagementClient<T, C> {
    /// 指定时钟构造（测试/受控时间）。
    pub fn with_clock(
        transport: T,
        origin: impl Into<String>,
        config: V2ManagementConfig,
        signing_key: DeviceSigningKey,
        clock: C,
    ) -> Self {
        let replay = V2ControlReplay::new(config.vault_id, config.instance_id)
            .with_manager_pub(config.manager_pub);
        Self {
            transport,
            api: RelayApi::new(origin, RelayApiVersion::V2),
            config,
            signing_key,
            clock,
            device_token: None,
            management_token: None,
            replay,
            store: None,
            pin: None,
            relay_epoch_hint: None,
            relay_member_hints: Vec::new(),
        }
    }

    /// 接持久化：读 durable pin + 可重建缓存；pin 损坏即 fail-stop，缓存损坏则丢弃重建。
    pub fn with_store(mut self, vault: Vault) -> Result<Self, SyncError> {
        self.store = Some(vault);
        self.load_persisted()?;
        Ok(self)
    }

    /// 已验证的授权状态。
    #[must_use]
    pub const fn state(&self) -> &ControlStateV2 {
        self.replay.state()
    }

    /// 是否已回放出 V2 状态。
    #[must_use]
    pub fn is_v2(&self) -> bool {
        self.replay.state().is_v2()
    }

    /// relay `/control` 报的 `active_epoch`（只用于展示/交叉核对，**不参与授权**）。
    #[must_use]
    pub const fn relay_epoch_hint(&self) -> Option<u64> {
        self.relay_epoch_hint
    }

    /// 当前 durable pin（诊断/测试）。
    #[must_use]
    pub const fn pin(&self) -> Option<&V2PinV1> {
        self.pin.as_ref()
    }

    /// relay `/control` 的只读成员投影（含 PENDING；**不是**授权证据）。
    ///
    /// ACTIVE/REVOKED 一律以 [`Self::state`] 的已验证控制链为准；这里只补
    /// "已兑换但未批准（PENDING）"的设备行。relay 伪造它不影响任何授权判断。
    #[must_use]
    pub fn relay_member_hints(&self) -> &[RelayMemberHintV2] {
        &self.relay_member_hints
    }

    fn load_persisted(&mut self) -> Result<(), SyncError> {
        let Some(vault) = &self.store else {
            return Ok(());
        };
        let pin = load_v2_pin(vault, self.config.vault_id)?;
        if let Some(pin) = &pin {
            if pin.instance_id != self.config.instance_id {
                return Err(SyncError::InstanceMismatch);
            }
        }
        // 缓存只有"pin 存在且摘要逐字节相符"时才可能被信任；否则丢弃（不是信任根）。
        if let Some(bytes) = vault.get_meta(META_MANAGEMENT_CACHE_V2)? {
            let usable = pin
                .as_ref()
                .is_some_and(|pin| pin.state_digest == sha256(&bytes));
            if usable {
                if let Ok(state) = ControlStateV2::decode_cache(
                    &bytes,
                    self.config.vault_id,
                    self.config.instance_id,
                ) {
                    let pin = pin.as_ref().expect("usable 需要 pin");
                    let matches = state.rev == pin.rev
                        && state.head_hash == pin.head_hash
                        && state.authz_rev == pin.authz_rev
                        && state.authority_rev == pin.authority_rev
                        && state.authz_state.is_some();
                    if matches {
                        self.replay = V2ControlReplay::from_cached(state);
                        self.pin = Some(*pin);
                        return Ok(());
                    }
                }
            }
            // 损坏/不一致的缓存：丢弃（pin 仍在；下次成功回放会重建并覆盖它）。
        }
        self.pin = pin;
        Ok(())
    }

    // ------------------------------------------------------------ 传输

    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
        if !self.api.origin().starts_with("http") {
            return Err(SyncError::ConfigInvalid(
                "V2ManagementClient 需要 canonical origin（http(s)://host[:port]）".to_owned(),
            ));
        }
        self.transport.send(request)
    }

    fn decode_ok(response: &HttpResponse) -> Result<cbor::Fields, SyncError> {
        if !(200..300).contains(&response.status) {
            return Err(crate::http_relay::error_from_response(response));
        }
        cbor::decode(&response.body)
    }

    fn post_cbor(&mut self, path: String, fields: cbor::Fields) -> Result<cbor::Fields, SyncError> {
        let body = cbor::encode(fields)?;
        let response = self.send(&HttpRequest {
            method: "POST",
            path,
            headers: vec![("content-type".to_owned(), CONTENT_TYPE_CBOR.to_owned())],
            body,
        })?;
        Self::decode_ok(&response)
    }

    fn vault_path(&self, suffix: &str) -> String {
        self.api.vault_path(&self.config.vault_id, suffix)
    }

    fn authorized_get(&mut self, path: String, now: u64) -> Result<cbor::Fields, SyncError> {
        for attempt in 0..2 {
            let token = self.device_token(now)?;
            let request = HttpRequest {
                method: "GET",
                path: path.clone(),
                headers: vec![("authorization".to_owned(), format!("Bearer {token}"))],
                body: Vec::new(),
            };
            let response = self.send(&request)?;
            if (200..300).contains(&response.status) {
                return cbor::decode(&response.body);
            }
            if attempt == 0 && is_revoked_response(&response) {
                // auth_rev 前进（吊销/轮换/恢复）：旧 bearer 立即失效，重新认证后重试一次。
                self.clear_auth_cache();
                continue;
            }
            return Err(crate::http_relay::error_from_response(&response));
        }
        Err(SyncError::Unsupported(
            "V2 设备 bearer 在重新认证后仍被拒绝",
        ))
    }

    fn clear_auth_cache(&mut self) {
        self.device_token = None;
        self.management_token = None;
    }

    // ------------------------------------------------------------ realm1 设备 token

    fn device_token(&mut self, now: u64) -> Result<String, SyncError> {
        if let Some(token) = &self.device_token {
            if token.expires_at > now.saturating_add(TOKEN_REFRESH_AHEAD_SECS) {
                return Ok(token.token.clone());
            }
        }
        let challenge_bytes =
            self.request_challenge(CHALLENGE_V2_REALM_DEVICE, self.config.device_id, None, true)?;
        let challenge = ChallengeV2::decode(&challenge_bytes)?;
        if challenge.realm != CHALLENGE_V2_REALM_DEVICE
            || challenge.subject != self.config.device_id
            || challenge.cap_id.is_some()
        {
            return Err(invariant("设备 challenge 的 realm/subject/cap 不符"));
        }
        self.verify_challenge(&challenge, now, false)?;
        let signature = self
            .signing_key
            .sign(&auth_signing_input_v2(&challenge_bytes));
        let fields = self.post_cbor(
            self.vault_path("/authz/tokens"),
            vec![
                (0, cbor::bytes(challenge_bytes.clone())),
                (1, cbor::bytes(signature.to_vec())),
            ],
        )?;
        let (token, expires_at, _realm, _authz_rev, _authority_rev) =
            self.parse_token_response(&fields, CHALLENGE_V2_REALM_DEVICE, None)?;
        self.device_token = Some(DeviceTokenV2 { token, expires_at });
        self.device_token
            .as_ref()
            .map(|token| token.token.clone())
            .ok_or(SyncError::RelayWire("token 刷新后仍为空"))
    }

    fn request_challenge(
        &mut self,
        realm: u8,
        subject: [u8; 16],
        cap_id: Option<[u8; 16]>,
        allow_legacy: bool,
    ) -> Result<Vec<u8>, SyncError> {
        let mut request = vec![(0, cbor::uint(u64::from(realm))), (1, cbor::bytes(subject))];
        if let Some(cap_id) = cap_id {
            request.push((2, cbor::bytes(cap_id.to_vec())));
        }
        let response = self.send(&HttpRequest {
            method: "POST",
            path: self.vault_path("/authz/challenges"),
            headers: vec![("content-type".to_owned(), CONTENT_TYPE_CBOR.to_owned())],
            body: cbor::encode(request)?,
        })?;
        if !(200..300).contains(&response.status) {
            let error = crate::http_relay::error_from_response(&response);
            if allow_legacy {
                if let SyncError::RelayApi { code, .. } = &error {
                    if code == "INVALID_STATE" {
                        return Err(SyncError::Unsupported(
                            "vault 尚未启用 V2 授权（relay 报 INVALID_STATE）",
                        ));
                    }
                }
            }
            return Err(error);
        }
        let fields = cbor::decode(&response.body)?;
        cbor::bytes_field(&fields, 0)
    }

    fn parse_token_response(
        &self,
        fields: &cbor::Fields,
        expected_realm: u8,
        expected_cap: Option<[u8; 16]>,
    ) -> Result<(String, u64, u8, u64, u64), SyncError> {
        let token = cbor::text_field(fields, 0)?;
        let expires_at = cbor::uint_field(fields, 1)?;
        let _server_time = cbor::uint_field(fields, 2)?;
        let realm = cbor::uint_field(fields, 3)?;
        let auth_rev = cbor::uint_field(fields, 4)?;
        let authz_rev = cbor::uint_field(fields, 5)?;
        let authority_rev = cbor::uint_field(fields, 6)?;
        let cap = match cbor::opt_bytes_field(fields, 7)? {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SyncError::RelayWire("token 响应的 cap_id 长度非法"))?,
            ),
        };
        if token.is_empty() {
            return Err(SyncError::RelayWire("token 为空"));
        }
        if realm != u64::from(expected_realm) {
            return Err(SyncError::RelayWire("token realm 不符"));
        }
        if cap != expected_cap {
            return Err(SyncError::RelayWire("token 的 cap 绑定与请求不符"));
        }
        if auth_rev == 0 {
            return Err(SyncError::RelayWire("token 的 auth_rev 非法"));
        }
        Ok((token, expires_at, expected_realm, authz_rev, authority_rev))
    }

    // ------------------------------------------------------------ 控制链回放

    /// 拉取并验证控制链（从本地水位续；legacy vault 返回明确的未启用错误）。
    pub fn refresh_control(&mut self) -> Result<(), SyncError> {
        let now = self.clock.now_secs();
        // legacy：`/authz/challenges` 会报 INVALID_STATE（V2 未启用）。
        match self.device_token(now) {
            Ok(_) => {}
            Err(SyncError::Unsupported(message)) if message.contains("尚未启用 V2") => {
                if self.replay.state().is_v2() {
                    return Err(invariant("relay 报 legacy 但本地已有 V2 回放状态"));
                }
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        let start_rev = self.replay.state().rev;
        let start_head = self.replay.state().head_hash;
        let mut after_rev = start_rev;
        let mut head_rev: Option<u64> = None;
        let mut pages = 0usize;
        loop {
            let path = format!("{}/control?after_rev={after_rev}", self.vault_path(""));
            let fields = self.authorized_get(path, now)?;
            // 只读成员投影（含 PENDING）：解析失败就当没有，不影响控制链验证。
            self.relay_member_hints = parse_relay_member_hints(&fields);
            let page_head =
                cbor::uint_field(&fields, 1).map_err(|_| invariant("控制链分页缺 control_rev"))?;
            if let Some(expected) = head_rev {
                if page_head != expected {
                    return Err(invariant("控制链分页的 control_rev 不一致（重排/替换）"));
                }
            } else {
                head_rev = Some(page_head);
            }
            self.relay_epoch_hint = cbor::opt_uint_field(&fields, 2)?.or(self.relay_epoch_hint);
            let commits = cbor::array_field(&fields, 0)?;
            if commits.len() > CONTROL_PAGE_LIMIT {
                return Err(invariant("控制链分页超过 256 条（上限）"));
            }
            let page_last = match commits.last() {
                Some(commit) => {
                    let commit = cbor::value_map(commit)?;
                    cbor::uint_field(&commit, 0)?
                }
                None => after_rev,
            };
            if page_last > page_head {
                return Err(invariant("控制链分页的 rev 超过 control_rev"));
            }
            for commit in commits {
                let commit = parse_control_commit(&commit)?;
                self.replay.apply_commit(&commit, now)?;
            }
            pages += 1;
            if page_last == page_head {
                break;
            }
            if page_last <= after_rev {
                return Err(invariant("控制链分页未前进（缺页）"));
            }
            after_rev = page_last;
            if pages > MAX_CONTROL_PAGES {
                return Err(invariant("控制链分页超过安全上限"));
            }
        }
        let head = head_rev.unwrap_or(0);
        if head < self.replay.state().rev {
            return Err(invariant("控制链 head 低于已回放水位（回滚）"));
        }
        if self.replay.state().rev != head {
            return Err(invariant("控制链未拉到 head（缺页/截断）"));
        }
        self.verify_and_commit_pin(start_rev, start_head)?;
        Ok(())
    }

    fn verify_and_commit_pin(
        &mut self,
        start_rev: u64,
        start_head: [u8; 32],
    ) -> Result<(), SyncError> {
        let state = self.replay.state();
        if let Some(pin) = &self.pin {
            if pin.rev > state.rev {
                return Err(SyncError::ControlRollback {
                    expected: pin.rev,
                    actual: state.rev,
                });
            }
            if pin.rev == state.rev {
                if pin.head_hash != state.head_hash {
                    return Err(SyncError::Equivocation(
                        "同 rev 的控制链 head 与本地 pin 不符",
                    ));
                }
                if pin.authz_rev != state.authz_rev || pin.authority_rev != state.authority_rev {
                    return Err(SyncError::Equivocation(
                        "同 rev 的 authz/authority rev 与本地 pin 不符",
                    ));
                }
            } else {
                match self.replay.record_hash_at(pin.rev) {
                    Some(hash) if hash == pin.head_hash => {}
                    Some(_) => {
                        return Err(SyncError::Equivocation(
                            "控制链未包含本地 pin 的 head（替换/分叉）",
                        ))
                    }
                    None => {
                        // 增量回放（从 pin.rev 续）：首条新记录的 prev_hash 已证明前缀。
                        if start_rev != pin.rev || start_head != pin.head_hash {
                            return Err(SyncError::NeedsTrust(
                                "无法证明新链包含本地 pin 的历史前缀",
                            ));
                        }
                    }
                }
            }
        }
        if !state.is_v2() {
            // 还没有 UpgradePolicy：不建立 V2 pin（legacy vault 不该有 V2 事实）。
            return Ok(());
        }
        let bytes = state.encode_cache()?;
        let digest = sha256(&bytes);
        let new_pin = V2PinV1 {
            vault_id: self.config.vault_id,
            instance_id: self.config.instance_id,
            rev: state.rev,
            head_hash: state.head_hash,
            authz_rev: state.authz_rev,
            authority_rev: state.authority_rev,
            state_digest: digest,
        };
        if let Some(vault) = &self.store {
            vault.set_meta(META_MANAGEMENT_CACHE_V2, &bytes)?;
            commit_v2_pin(vault, self.pin.as_ref(), &new_pin)?;
        }
        self.pin = Some(new_pin);
        Ok(())
    }

    // ------------------------------------------------------------ cap 选择

    /// 选一张覆盖 `required` 的本机 cap（先按需刷新，再用**已验证**状态判定）。
    pub fn select_cap(&mut self, required: CapScope) -> Result<CapEntryV2, SyncError> {
        self.refresh_control()?;
        let now = self.clock.now_secs();
        let device_pk = self.signing_key.public_key();
        self.replay
            .state()
            .select_cap(&self.config.device_id, &device_pk, required, now)
            .cloned()
    }

    // ------------------------------------------------------------ realm3 管理 bearer

    /// 取得（必要时刷新）realm 3 管理 token；每次请求前先 `refresh_control`。
    pub fn management_bearer(
        &mut self,
        required: CapScope,
    ) -> Result<ManagementTokenV2, SyncError> {
        self.refresh_control()?;
        let now = self.clock.now_secs();
        if let Some(token) = &self.management_token {
            let fresh = token.expires_at > now.saturating_add(TOKEN_REFRESH_AHEAD_SECS)
                && token.scopes.contains(required);
            if fresh {
                match self.verify_management_token(token, required, now) {
                    Ok(()) => return Ok(token.clone()),
                    // 撤销/到期/换代：旧 bearer 不可再用，按新回放状态重选 cap 重新鉴权。
                    Err(_) => self.management_token = None,
                }
            }
        }
        self.authorize_management(required)
    }

    /// 强制重新走一次 realm3 challenge→token（启动/签前/提交前/rev 变化/失败后）。
    pub fn authorize_management(
        &mut self,
        required: CapScope,
    ) -> Result<ManagementTokenV2, SyncError> {
        self.refresh_control()?;
        let now = self.clock.now_secs();
        let device_pk = self.signing_key.public_key();
        let selected = self
            .replay
            .state()
            .select_cap(&self.config.device_id, &device_pk, required, now)?
            .clone();
        let cap_id = selected.cap.cap_id;
        let challenge_bytes = self.request_challenge(
            CHALLENGE_V2_REALM_MANAGEMENT_DEVICE,
            self.config.device_id,
            Some(cap_id),
            false,
        )?;
        let challenge = ChallengeV2::decode(&challenge_bytes)?;
        if challenge.realm != CHALLENGE_V2_REALM_MANAGEMENT_DEVICE
            || challenge.subject != self.config.device_id
            || challenge.cap_id != Some(cap_id)
        {
            return Err(invariant("管理 challenge 的 realm/subject/cap 不符"));
        }
        self.verify_challenge(&challenge, now, true)?;
        let signature = self
            .signing_key
            .sign(&auth_signing_input_v2(&challenge_bytes));
        let fields = self.post_cbor(
            self.vault_path("/authz/tokens"),
            vec![
                (0, cbor::bytes(challenge_bytes)),
                (1, cbor::bytes(signature.to_vec())),
            ],
        )?;
        let (token, expires_at, _, authz_rev, authority_rev) =
            self.parse_token_response(&fields, CHALLENGE_V2_REALM_MANAGEMENT_DEVICE, Some(cap_id))?;
        let auth_rev = cbor::uint_field(&fields, 4)?;
        if expires_at <= now {
            return Err(SyncError::RelayWire("管理 token 已过期"));
        }
        if authz_rev != self.replay.state().authz_rev
            || authority_rev != self.replay.state().authority_rev
        {
            return Err(invariant(
                "管理 token 的 authz/authority rev 与已验证回放不符",
            ));
        }
        let token = ManagementTokenV2 {
            token,
            cap_id,
            scopes: selected.cap.scopes,
            expires_at,
            authz_rev,
            authority_rev,
            auth_rev,
        };
        self.verify_management_token(&token, required, now)?;
        // 记录签发时绑定的 cap 到期（token 不应活过 cap）。
        if token.expires_at > selected.cap.expires_at {
            return Err(invariant("管理 token 的到期时间超过绑定 cap"));
        }
        self.management_token = Some(token.clone());
        Ok(token)
    }

    fn verify_challenge(
        &self,
        challenge: &ChallengeV2,
        now: u64,
        require_rev_binding: bool,
    ) -> Result<(), SyncError> {
        if challenge.instance_id != self.config.instance_id {
            return Err(invariant(
                "challenge 的 instance 与本地 pin 不符（跨 relay）",
            ));
        }
        if challenge.vault_id != self.config.vault_id {
            return Err(invariant("challenge 的 vault 与本地不符（跨租户）"));
        }
        let mut origin_input = Vec::with_capacity(32);
        origin_input.extend_from_slice(&challenge.instance_id);
        origin_input.extend_from_slice(&challenge.vault_id);
        if challenge.origin_hash != sha256(&origin_input) {
            return Err(invariant("challenge 的 origin 绑定不符"));
        }
        if challenge.expires_at <= now {
            return Err(invariant("challenge 已过期"));
        }
        if challenge.expires_at > now.saturating_add(CHALLENGE_MAX_AHEAD_SECS) {
            return Err(invariant("challenge 有效期超出允许窗口"));
        }
        // realm 3（管理 bearer）必须绑定**当前**回放视图；realm 1 的 token 只用于读控制链
        // 本身（此时还没有回放状态可比较，relay 会在每个请求上重验自己的 token revs）。
        if require_rev_binding
            && (challenge.authz_rev != self.replay.state().authz_rev
                || challenge.authority_rev != self.replay.state().authority_rev)
        {
            return Err(invariant(
                "challenge 的 authz/authority rev 与已验证回放不符（链陈旧或跨代）",
            ));
        }
        Ok(())
    }

    /// 每次使用前重验：token 两个 rev **等于**当前回放值，且 cap 仍有效。
    pub fn verify_management_token(
        &self,
        token: &ManagementTokenV2,
        required: CapScope,
        now: u64,
    ) -> Result<(), SyncError> {
        let state = self.replay.state();
        if token.expires_at <= now {
            return Err(SyncError::Unsupported("管理 token 已过期"));
        }
        if token.authz_rev != state.authz_rev || token.authority_rev != state.authority_rev {
            return Err(SyncError::Unsupported(
                "管理 token 的 authz/authority rev 已过期（必须重新鉴权）",
            ));
        }
        if !token.scopes.contains(required) {
            return Err(SyncError::Unsupported(
                "管理 token 绑定的 cap 不覆盖所需 scope",
            ));
        }
        let Some(entry) = state.cap(&token.cap_id) else {
            return Err(SyncError::Unsupported(
                "管理 token 绑定的 cap 不在已验证控制链上",
            ));
        };
        if !entry.cap.scopes.contains(required) {
            return Err(SyncError::Unsupported(
                "管理 token 绑定的 cap 不覆盖所需 scope",
            ));
        }
        if !state.cap_is_effective_at(entry, now) {
            return Err(SyncError::Unsupported(
                "管理 token 绑定的 cap 已失效（撤销/到期/authority 变更/主体非 ACTIVE）",
            ));
        }
        let _ = token.auth_rev;
        Ok(())
    }

    /// 带管理 bearer 的 v2 请求封装（S4d1b 的构造/提交走同一入口）。
    ///
    /// 每次调用：`refresh_control` →（必要时）realm3 鉴权 → 发送；403 `REVOKED` 时清缓存
    /// 并按新 rev 重试**一次**（不重放业务语义，调用方只应传只读/幂等请求）。
    pub fn management_request(
        &mut self,
        method: &'static str,
        path: &str,
        body: Vec<u8>,
        required: CapScope,
    ) -> Result<HttpResponse, SyncError> {
        for attempt in 0..2 {
            let token = self.management_bearer(required)?;
            let request = HttpRequest {
                method,
                path: path.to_owned(),
                headers: vec![
                    (
                        "authorization".to_owned(),
                        format!("Bearer {}", token.token()),
                    ),
                    ("content-type".to_owned(), CONTENT_TYPE_CBOR.to_owned()),
                ],
                body: body.clone(),
            };
            let response = self.send(&request)?;
            if (200..300).contains(&response.status) {
                return Ok(response);
            }
            if attempt == 0 && is_revoked_response(&response) {
                self.clear_auth_cache();
                continue;
            }
            return Err(crate::http_relay::error_from_response(&response));
        }
        Err(SyncError::Unsupported("管理请求在重新鉴权后仍被拒绝"))
    }

    // ------------------------------------------------------------ 控制提交（S4d1b）

    /// 已验证回放器（只读；`validate_pending_control`/`state` 都从这里取）。
    #[must_use]
    pub const fn replay(&self) -> &V2ControlReplay {
        &self.replay
    }

    /// 本地时钟当前秒（与回放/提交共用同一时钟源）。
    #[must_use]
    pub fn now_secs(&self) -> u64 {
        self.clock.now_secs()
    }

    /// 某 rev 的链 hash（只覆盖本次回放过的记录）。
    #[must_use]
    pub fn record_hash_at(&self, rev: u64) -> Option<[u8; 32]> {
        self.replay.record_hash_at(rev)
    }

    /// 提交一条**已满签**的控制记录到唯一提交路径（`POST /v2/vaults/{lowerhex32}/control/submit`）。
    ///
    /// 提交是**自证**的（ADR 0015 §1）：不带 bearer，管理 bearer 不能代替管理签名。
    /// 本方法只做"发一次 + 严格解析回执"，**不**做链位置预检/对账（S4d1b 的
    /// `approval_v1` 状态机在前后用已验证回放验证）；失败不改变本地 pin/缓存。
    pub fn submit_control_bytes(
        &mut self,
        signed: &SignedControlV2,
    ) -> Result<ControlReceiptV2, SyncError> {
        let operation = &signed.operation;
        if operation.vault_id != self.config.vault_id {
            return Err(invariant("待提交操作的 vault 与本地不符（跨租户）"));
        }
        if operation.instance_id != self.config.instance_id {
            return Err(invariant(
                "待提交操作的 instance 与本地 pin 不符（跨 relay）",
            ));
        }
        let bytes = signed.encode()?;
        let body = cbor::encode(vec![(0, cbor::bytes(bytes))])?;
        let response = self.send(&HttpRequest {
            method: "POST",
            path: canonical_submit_path(&self.config.vault_id),
            headers: vec![("content-type".to_owned(), CONTENT_TYPE_CBOR.to_owned())],
            body,
        })?;
        let fields = Self::decode_ok(&response)?;
        parse_control_receipt(&fields, operation)
    }
}

/// 严格解析控制提交回执（`{0..5}` 六字段，逐项与本地操作交叉校验）。
fn parse_control_receipt(
    fields: &cbor::Fields,
    operation: &OperationV2,
) -> Result<ControlReceiptV2, SyncError> {
    if fields.len() != 6 || fields.iter().any(|(key, _)| *key > 5) {
        return Err(invariant("控制提交回执字段集合不合法"));
    }
    let receipt = ControlReceiptV2 {
        control_rev: cbor::uint_field(fields, 0)?,
        head_hash: cbor::bytes_fixed::<32>(fields, 1)?,
        kind: u8::try_from(cbor::uint_field(fields, 2)?)
            .map_err(|_| invariant("控制提交回执的 kind 越界"))?,
        request_id: cbor::bytes_fixed::<16>(fields, 3)?,
        authz_rev: cbor::uint_field(fields, 4)?,
        authority_rev: cbor::uint_field(fields, 5)?,
    };
    if receipt.control_rev != operation.expected_control_rev.saturating_add(1) {
        return Err(invariant("控制提交回执的 rev 与操作的 CAS 绑定不符"));
    }
    if receipt.kind != operation.kind.as_u8() {
        return Err(invariant("控制提交回执的 kind 与操作不符"));
    }
    if receipt.request_id != operation.request_id {
        return Err(invariant("控制提交回执的 request_id 与操作不符"));
    }
    if receipt.head_hash == [0u8; 32] {
        return Err(invariant("控制提交回执的 head_hash 不得为全零"));
    }
    Ok(receipt)
}

fn is_revoked_response(response: &HttpResponse) -> bool {
    if response.status < 400 {
        return false;
    }
    match cbor::decode(&response.body) {
        Ok(fields) => matches!(cbor::text_field(&fields, 0).as_deref(), Ok("REVOKED")),
        Err(_) => false,
    }
}

/// 解析 `/control` 响应里的成员投影（键 3）：只取 `device_id` + `status`。
///
/// **展示专用**：任何字段缺失/类型不符的条目直接跳过（不 fail-stop，也不参与授权）。
/// relay 投影不是授权证据，UI 必须对 ACTIVE/REVOKED 以已验证控制链为准。
fn parse_relay_member_hints(fields: &cbor::Fields) -> Vec<RelayMemberHintV2> {
    let Ok(items) = cbor::array_field(fields, 3) else {
        return Vec::new();
    };
    let mut hints = Vec::with_capacity(items.len());
    for item in items {
        let Ok(entry) = cbor::value_map(&item) else {
            continue;
        };
        let (Ok(device_id), Ok(status)) = (
            cbor::bytes_fixed::<16>(&entry, 0),
            cbor::text_field(&entry, 2),
        ) else {
            continue;
        };
        hints.push(RelayMemberHintV2 { device_id, status });
    }
    hints
}

fn parse_control_commit(value: &CdeValue) -> Result<ControlCommitV2, SyncError> {
    let commit = cbor::value_map(value)?;
    if commit.iter().any(|(key, _)| *key > 4) {
        return Err(invariant("控制链提交含未知字段"));
    }
    Ok(ControlCommitV2 {
        rev: cbor::uint_field(&commit, 0)?,
        prev_hash: cbor::bytes_fixed(&commit, 1)?,
        record_hash: cbor::bytes_fixed(&commit, 2)?,
        signed_bytes: cbor::bytes_field(&commit, 3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::Secret32;
    use menghunote_sync_proto::{OperationV2, Rights, V2_WIRE_VERSION};

    const NOW: u64 = 1_800_000_000;
    const VAULT: VaultId = [0x5au8; 16];
    const INSTANCE: [u8; 16] = [0x11u8; 16];
    const DEV_A: DeviceId = [0x21u8; 16];
    const DEV_B: DeviceId = [0x22u8; 16];
    const DEV_C: DeviceId = [0x23u8; 16];
    const CAP_A: [u8; 16] = [0xC1u8; 16];
    const CAP_B: [u8; 16] = [0xC2u8; 16];
    const CAP_LIMITED: [u8; 16] = [0xC3u8; 16];
    const CAP_C: [u8; 16] = [0xC4u8; 16];
    const HPKE_A: [u8; 32] = [0x31u8; 32];
    const HPKE_B: [u8; 32] = [0x32u8; 32];
    const HPKE_C: [u8; 32] = [0x33u8; 32];

    fn key(seed: u8) -> DeviceSigningKey {
        DeviceSigningKey::from_seed(Secret32::from_array([seed; 32]))
    }

    fn pk(signing: &DeviceSigningKey) -> [u8; 32] {
        signing.public_key()
    }

    fn cert(
        device_id: DeviceId,
        sign_pk: [u8; 32],
        hpke_pk: [u8; 32],
        serial: u64,
        rights: u32,
        issued_authz_rev: u64,
    ) -> DeviceCertV2 {
        DeviceCertV2 {
            version: V2_WIRE_VERSION,
            instance_id: INSTANCE,
            vault_id: VAULT,
            device_id,
            sign_ed25519_pub: sign_pk,
            hpke_x25519_pub: hpke_pk,
            cert_serial: serial,
            data_rights: Rights::new(rights).expect("rights"),
            issued_authz_rev,
        }
    }

    fn grant(
        epoch: u64,
        recipient: DeviceId,
        hpke_pk: [u8; 32],
        authority_rev: u64,
        grant_id: [u8; 16],
        issued_authz_rev: u64,
    ) -> GrantPackageV2 {
        GrantPackageV2 {
            aad: proto_grant_aad(
                epoch,
                recipient,
                hpke_pk,
                authority_rev,
                grant_id,
                issued_authz_rev,
            ),
            enc: [0x77u8; 32],
            ciphertext: vec![1, 2, 3, 4],
        }
    }

    fn proto_grant_aad(
        epoch: u64,
        recipient: DeviceId,
        hpke_pk: [u8; 32],
        authority_rev: u64,
        grant_id: [u8; 16],
        issued_authz_rev: u64,
    ) -> menghunote_sync_proto::GrantAadV2 {
        menghunote_sync_proto::GrantAadV2 {
            version: V2_WIRE_VERSION,
            instance_id: INSTANCE,
            vault_id: VAULT,
            epoch,
            recipient_device_id: recipient,
            recipient_hpke_pub: hpke_pk,
            authority_rev,
            grant_id,
            issued_authz_rev,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn cap(
        cap_id: [u8; 16],
        device_id: DeviceId,
        sign_pk: [u8; 32],
        scopes: u32,
        issued_control_rev: u64,
        issued_authz_rev: u64,
        authority_rev: u64,
        not_before: u64,
        expires_at: u64,
    ) -> ManagementCapV2 {
        ManagementCapV2 {
            version: V2_WIRE_VERSION,
            instance_id: INSTANCE,
            vault_id: VAULT,
            cap_id,
            subject_device_id: device_id,
            subject_ed_pk: sign_pk,
            scopes: CapScope::new(scopes).expect("scopes"),
            not_before,
            expires_at,
            issued_control_rev,
            issued_authz_rev,
            issuer_authority_rev: authority_rev,
            delegable: false,
        }
    }

    fn policy() -> PolicyV2 {
        PolicyV2 {
            version: V2_WIRE_VERSION,
            highrisk_threshold: PolicyV2::HIGHRISK_THRESHOLD,
            min_quorum: PolicyV2::MIN_QUORUM,
            max_cap_ttl: menghunote_sync_proto::MAX_CAP_TTL_SECS_V2,
            max_invite_ttl: menghunote_sync_proto::MAX_INVITE_TTL_SECS_V2,
        }
    }

    fn sort_signers(mut signers: Vec<SignerV2>) -> Vec<SignerV2> {
        signers.sort_by(|left, right| {
            left.role
                .as_u8()
                .cmp(&right.role.as_u8())
                .then_with(|| left.device_id.cmp(&right.device_id))
                .then_with(|| left.cap_id.cmp(&right.cap_id))
                .then_with(|| left.pk.cmp(&right.pk))
        });
        signers
    }

    struct Chain {
        replay: V2ControlReplay,
        rev: u64,
        head: [u8; 32],
        signed_at: u64,
        manager: DeviceSigningKey,
        a: DeviceSigningKey,
        b: DeviceSigningKey,
        c: DeviceSigningKey,
        recovery: DeviceSigningKey,
        records: Vec<SignedControlV2>,
    }

    impl Chain {
        fn new() -> Self {
            let manager = key(0x01);
            let a = key(0x02);
            let b = key(0x03);
            let c = key(0x04);
            let recovery = key(0x05);
            let replay = V2ControlReplay::new(VAULT, INSTANCE).with_manager_pub(pk(&manager));
            Self {
                replay,
                rev: 0,
                head: [0u8; 32],
                signed_at: NOW - 600,
                manager,
                a,
                b,
                c,
                recovery,
                records: Vec::new(),
            }
        }

        fn apply(&mut self, signed: &SignedControlV2) -> Result<Option<ControlKindV2>, SyncError> {
            let bytes = signed.encode().expect("encode signed");
            let mut input = Vec::with_capacity(32 + bytes.len());
            input.extend_from_slice(&self.head);
            input.extend_from_slice(&bytes);
            let record_hash = sha256(&input);
            let commit = ControlCommitV2 {
                rev: self.rev + 1,
                prev_hash: self.head,
                record_hash,
                signed_bytes: bytes,
            };
            let kind = self.replay.apply_commit(&commit, NOW)?;
            self.rev += 1;
            self.head = record_hash;
            self.signed_at += 10;
            if kind.is_some() {
                self.records.push(signed.clone());
            }
            Ok(kind)
        }

        fn op(
            &mut self,
            kind: ControlKindV2,
            payload: Vec<u8>,
            signers: Vec<SignerV2>,
            expected_authz_rev: u64,
            authority_rev: u64,
        ) -> OperationV2 {
            OperationV2 {
                version: V2_WIRE_VERSION,
                instance_id: INSTANCE,
                vault_id: VAULT,
                kind,
                request_id: [self.rev as u8 + 1; 16].map(|value| value.wrapping_add(0x40)),
                expected_control_rev: self.rev,
                prev_control_hash: self.head,
                expected_authz_rev,
                authority_rev,
                signed_at: self.signed_at,
                not_after: self.signed_at + 300,
                canonical_path: canonical_submit_path(&VAULT),
                payload,
                signers: sort_signers(signers),
            }
        }

        fn sign(&self, operation: &OperationV2) -> SignedControlV2 {
            sign_by_pk(
                operation,
                &[&self.manager, &self.a, &self.b, &self.c, &self.recovery],
            )
        }
    }

    /// 按 signer 的 `pk` 找到对应私钥签名（顺序无关；poP/根签都按自己的 pk 匹配）。
    fn sign_by_pk(operation: &OperationV2, keys: &[&DeviceSigningKey]) -> SignedControlV2 {
        let control = control_signing_input(operation, proto_hash).expect("control input");
        let upgrade = upgrade_signing_input(operation, proto_hash).expect("upgrade input");
        let recover = recover_signing_input(operation, proto_hash).expect("recover input");
        let mut signatures = Vec::with_capacity(operation.signers.len());
        for signer in &operation.signers {
            let signing = keys
                .iter()
                .find(|signing| signing.public_key() == signer.pk)
                .unwrap_or_else(|| panic!("测试缺 {} 的私钥", crate::ids::hex_of(&signer.pk)));
            let input = match signer.role {
                SignerRole::Device => control.clone(),
                SignerRole::CreatorUpgrade => upgrade.clone(),
                SignerRole::OfflineRecovery => recover.clone(),
                SignerRole::TargetPop => {
                    pop_signing_input(operation, &signer.pk, proto_hash).expect("pop input")
                }
            };
            signatures.push(signing.sign(&input));
        }
        SignedControlV2 {
            operation: operation.clone(),
            signatures,
        }
    }

    fn device_signer(
        device_id: DeviceId,
        device_pk: [u8; 32],
        cap_id: Option<[u8; 16]>,
    ) -> SignerV2 {
        SignerV2 {
            role: SignerRole::Device,
            device_id: Some(device_id),
            cap_id,
            pk: device_pk,
        }
    }

    fn root_signer(role: SignerRole, sign_pk: [u8; 32]) -> SignerV2 {
        SignerV2 {
            role,
            device_id: None,
            cap_id: None,
            pk: sign_pk,
        }
    }

    fn pop_signer(device_id: DeviceId, sign_pk: [u8; 32]) -> SignerV2 {
        SignerV2 {
            role: SignerRole::TargetPop,
            device_id: Some(device_id),
            cap_id: None,
            pk: sign_pk,
        }
    }

    /// 建立 legacy→Upgrade 的起点（两台 ACTIVE：A/B，两张全 scope bootstrap cap）。
    fn upgrade_chain(chain: &mut Chain) -> Result<(), SyncError> {
        let manifest = vec![
            cert(DEV_A, pk(&chain.a.clone()), HPKE_A, 1, Rights::ALL, 1),
            cert(DEV_B, pk(&chain.b.clone()), HPKE_B, 1, Rights::ALL, 1),
        ];
        let caps = vec![
            cap(
                CAP_A,
                DEV_A,
                pk(&chain.a.clone()),
                CapScope::ALL,
                chain.rev + 1,
                1,
                1,
                NOW - 900,
                NOW + 86_400,
            ),
            cap(
                CAP_B,
                DEV_B,
                pk(&chain.b.clone()),
                CapScope::ALL,
                chain.rev + 1,
                1,
                1,
                NOW - 900,
                NOW + 86_400,
            ),
        ];
        let payload = UpgradePolicyPayloadV2 {
            old_manager_rev: 1,
            policy: policy(),
            fresh_recovery_pk: pk(&chain.recovery.clone()),
            bootstrap_caps: caps,
            active_manifest: manifest,
        }
        .encode()?;
        let signers = vec![
            device_signer(DEV_A, pk(&chain.a.clone()), None),
            device_signer(DEV_B, pk(&chain.b.clone()), None),
            root_signer(SignerRole::CreatorUpgrade, pk(&chain.manager.clone())),
            root_signer(SignerRole::OfflineRecovery, pk(&chain.recovery.clone())),
        ];
        let operation = chain.op(ControlKindV2::UpgradePolicy, payload, signers, 0, 1);
        let signed = chain.sign(&operation);
        chain.apply(&signed)?;
        Ok(())
    }

    fn two_sign(
        chain: &mut Chain,
        kind: ControlKindV2,
        payload: Vec<u8>,
        signers: &[(DeviceId, [u8; 16])],
    ) -> Result<SignedControlV2, SyncError> {
        let signer_v2 = signers
            .iter()
            .map(|(device_id, cap_id)| {
                let device_pk = match *device_id {
                    DEV_A => pk(&chain.a.clone()),
                    DEV_B => pk(&chain.b.clone()),
                    _ => pk(&chain.c.clone()),
                };
                device_signer(*device_id, device_pk, Some(*cap_id))
            })
            .collect();
        let operation = chain.op(
            kind,
            payload,
            signer_v2,
            chain.replay.state().authz_rev(),
            chain.replay.state().authority_rev(),
        );
        Ok(chain.sign(&operation))
    }

    fn assert_rejected<T>(result: Result<T, SyncError>, label: &str) {
        assert!(result.is_err(), "{label} 必须被拒绝");
    }

    // ---------------------------------------------------------------- 正例

    #[test]
    fn replays_full_management_chain_from_zero() {
        let mut chain = Chain::new();
        upgrade_chain(&mut chain).expect("upgrade");
        {
            let state = chain.replay.state();
            assert_eq!(state.rev(), 1);
            assert_eq!(state.authz_rev(), 1);
            assert_eq!(state.authority_rev(), 1);
            assert_eq!(state.authz_state(), Some(AuthzStateV2::Active));
            assert_eq!(state.active_device_count(), 2);
            assert_eq!(state.caps().count(), 2);
            assert_eq!(state.recovery_pub(), Some(pk(&chain.recovery.clone())));
        }

        // CapGrant：给 A 一张 INVITE-only cap（scope 是每位签发者 scope 的子集）。
        let limited = cap(
            CAP_LIMITED,
            DEV_A,
            pk(&chain.a.clone()),
            CapScope::INVITE,
            chain.rev + 1,
            2,
            1,
            NOW - 900,
            NOW + 3_600,
        );
        let signed = two_sign(
            &mut chain,
            ControlKindV2::CapGrant,
            CapGrantPayloadV2 {
                cap: limited.clone(),
            }
            .encode()
            .expect("cap grant"),
            &[(DEV_A, CAP_A), (DEV_B, CAP_B)],
        )
        .expect("op");
        chain.apply(&signed).expect("cap grant");
        assert_eq!(chain.replay.state().authz_rev(), 2);
        assert_eq!(chain.replay.state().caps().count(), 3);

        // CapRevoke 该 cap。
        let signed = two_sign(
            &mut chain,
            ControlKindV2::CapRevoke,
            CapRevokePayloadV2 {
                cap_id: CAP_LIMITED,
            }
            .encode()
            .expect("cap revoke"),
            &[(DEV_A, CAP_A), (DEV_B, CAP_B)],
        )
        .expect("op");
        chain.apply(&signed).expect("cap revoke");
        assert_eq!(chain.replay.state().authz_rev(), 3);
        assert!(chain
            .replay
            .state()
            .cap(&CAP_LIMITED)
            .expect("cap")
            .is_revoked());

        // InviteCreate / InviteCancel（一台 INVITE cap，只推进 control）。
        let invite_id = [0x51u8; 16];
        let payload = InviteCreatePayloadV2 {
            invite_id,
            token_hash: [0x61u8; 32],
            expires_at: chain.signed_at + 3_600,
        }
        .encode()
        .expect("invite payload");
        let operation = chain.op(
            ControlKindV2::InviteCreate,
            payload,
            vec![device_signer(DEV_A, pk(&chain.a.clone()), Some(CAP_A))],
            chain.replay.state().authz_rev(),
            chain.replay.state().authority_rev(),
        );
        let signed = chain.sign(&operation);
        chain.apply(&signed).expect("invite create");
        assert_eq!(chain.replay.state().invites().count(), 1);
        assert_eq!(chain.replay.state().authz_rev(), 3, "邀请只推进 control");
        let operation = chain.op(
            ControlKindV2::InviteCancel,
            InviteCancelPayloadV2 { invite_id }
                .encode()
                .expect("cancel payload"),
            vec![device_signer(DEV_A, pk(&chain.a.clone()), Some(CAP_A))],
            chain.replay.state().authz_rev(),
            chain.replay.state().authority_rev(),
        );
        let signed = chain.sign(&operation);
        chain.apply(&signed).expect("invite cancel");
        assert!(chain
            .replay
            .state()
            .invites()
            .next()
            .expect("invite")
            .cancelled_rev
            .is_some());
        assert_eq!(chain.replay.state().authz_rev(), 3);

        // DeviceApprove C：A/B APPROVE + 目标 PoP。
        let cert_c = cert(DEV_C, pk(&chain.c.clone()), HPKE_C, 1, Rights::ALL, 4);
        let payload = DeviceCredentialPayloadV2 {
            cert: cert_c.clone(),
            grants: vec![grant(1, DEV_C, HPKE_C, 1, [0x71u8; 16], 4)],
        }
        .encode()
        .expect("approve payload");
        let signers = vec![
            device_signer(DEV_A, pk(&chain.a.clone()), Some(CAP_A)),
            device_signer(DEV_B, pk(&chain.b.clone()), Some(CAP_B)),
            pop_signer(DEV_C, pk(&chain.c.clone())),
        ];
        let operation = chain.op(
            ControlKindV2::DeviceApprove,
            payload,
            signers,
            chain.replay.state().authz_rev(),
            chain.replay.state().authority_rev(),
        );
        let signed = chain.sign(&operation);
        chain.apply(&signed).expect("device approve");
        assert_eq!(chain.replay.state().active_device_count(), 3);
        assert_eq!(chain.replay.state().authz_rev(), 4);

        // 给 C 一张全 scope cap（A/B 两签），然后 A/C 两签撤销 B。
        let cap_c = cap(
            CAP_C,
            DEV_C,
            pk(&chain.c.clone()),
            CapScope::ALL,
            chain.rev + 1,
            5,
            1,
            NOW - 900,
            NOW + 86_400,
        );
        let signed = two_sign(
            &mut chain,
            ControlKindV2::CapGrant,
            CapGrantPayloadV2 { cap: cap_c }
                .encode()
                .expect("cap grant"),
            &[(DEV_A, CAP_A), (DEV_B, CAP_B)],
        )
        .expect("op");
        chain.apply(&signed).expect("cap grant to C");
        assert_eq!(chain.replay.state().authz_rev(), 5);

        let signed = two_sign(
            &mut chain,
            ControlKindV2::DeviceRevoke,
            DeviceRevokePayloadV2 {
                device_ids: vec![DEV_B],
            }
            .encode()
            .expect("revoke payload"),
            &[(DEV_A, CAP_A), (DEV_C, CAP_C)],
        )
        .expect("op");
        chain.apply(&signed).expect("device revoke");
        assert_eq!(chain.replay.state().authz_rev(), 6);
        assert!(!chain.replay.state().is_active_device(&DEV_B));
        assert!(chain.replay.state().cap(&CAP_B).expect("cap").is_revoked());
        assert_eq!(chain.replay.state().active_device_count(), 2);

        // RecoveryKeyChange：A/C POLICY 两签 + 新恢复钥 PoP；authority+1、authz+1、换 cap 代。
        let fresh_recovery = key(0x06);
        let payload = RecoveryKeyChangePayloadV2 {
            fresh_recovery_pk: pk(&fresh_recovery),
            bootstrap_caps: vec![
                cap(
                    [0xD1u8; 16],
                    DEV_A,
                    pk(&chain.a.clone()),
                    CapScope::ALL,
                    chain.rev + 1,
                    7,
                    2,
                    NOW - 900,
                    NOW + 86_400,
                ),
                cap(
                    [0xD2u8; 16],
                    DEV_C,
                    pk(&chain.c.clone()),
                    CapScope::ALL,
                    chain.rev + 1,
                    7,
                    2,
                    NOW - 900,
                    NOW + 86_400,
                ),
            ],
        }
        .encode()
        .expect("key change payload");
        let signers = vec![
            device_signer(DEV_A, pk(&chain.a.clone()), Some(CAP_A)),
            device_signer(DEV_C, pk(&chain.c.clone()), Some(CAP_C)),
            root_signer(SignerRole::OfflineRecovery, pk(&fresh_recovery)),
        ];
        let operation = chain.op(
            ControlKindV2::RecoveryKeyChange,
            payload,
            signers,
            chain.replay.state().authz_rev(),
            chain.replay.state().authority_rev(),
        );
        let signed = sign_by_pk(&operation, &[&chain.a, &chain.c, &fresh_recovery]);
        chain.apply(&signed).expect("recovery key change");
        assert_eq!(chain.replay.state().authority_rev(), 2);
        assert_eq!(chain.replay.state().authz_rev(), 7);
        assert_eq!(
            chain.replay.state().recovery_pub(),
            Some(pk(&fresh_recovery))
        );
        assert_eq!(
            chain.replay.state().authz_state(),
            Some(AuthzStateV2::Active)
        );

        // RecoveryReset：当前恢复根 + 新钥 PoP 各签一次；authority+1。
        let next_recovery = key(0x07);
        let payload = RecoveryResetPayloadV2 {
            policy: policy(),
            fresh_recovery_pk: pk(&next_recovery),
            bootstrap_caps: vec![
                cap(
                    [0xD3u8; 16],
                    DEV_A,
                    pk(&chain.a.clone()),
                    CapScope::ALL,
                    chain.rev + 1,
                    8,
                    3,
                    NOW - 900,
                    NOW + 86_400,
                ),
                cap(
                    [0xD4u8; 16],
                    DEV_C,
                    pk(&chain.c.clone()),
                    CapScope::ALL,
                    chain.rev + 1,
                    8,
                    3,
                    NOW - 900,
                    NOW + 86_400,
                ),
            ],
            device_adds: Vec::new(),
            revoke_ids: Vec::new(),
        }
        .encode()
        .expect("reset payload");
        let signers = vec![
            root_signer(SignerRole::OfflineRecovery, pk(&fresh_recovery)),
            root_signer(SignerRole::OfflineRecovery, pk(&next_recovery)),
        ];
        let operation = chain.op(
            ControlKindV2::RecoveryReset,
            payload,
            signers,
            chain.replay.state().authz_rev(),
            chain.replay.state().authority_rev(),
        );
        let signed = sign_by_pk(&operation, &[&fresh_recovery, &next_recovery]);
        chain.apply(&signed).expect("recovery reset");
        let state = chain.replay.state();
        assert_eq!(state.authority_rev(), 3);
        assert_eq!(state.authz_rev(), 8);
        assert_eq!(state.authz_state(), Some(AuthzStateV2::Active));
        assert_eq!(state.recovery_pub(), Some(pk(&next_recovery)));
        // 每个 rev 的链 hash 仍在 record_hashes 里，且逐条可复算。
        for rev in 1..=chain.rev {
            assert!(chain.replay.record_hash_at(rev).is_some(), "rev {rev}");
        }
    }

    #[test]
    fn paginated_apply_page_matches_single_pass() {
        let mut chain = Chain::new();
        upgrade_chain(&mut chain).expect("upgrade");
        let limited = cap(
            CAP_LIMITED,
            DEV_A,
            pk(&chain.a.clone()),
            CapScope::INVITE,
            chain.rev + 1,
            2,
            1,
            NOW - 900,
            NOW + 3_600,
        );
        let signed = two_sign(
            &mut chain,
            ControlKindV2::CapGrant,
            CapGrantPayloadV2 { cap: limited }
                .encode()
                .expect("payload"),
            &[(DEV_A, CAP_A), (DEV_B, CAP_B)],
        )
        .expect("op");
        chain.apply(&signed).expect("cap grant");
        let expected = chain.replay.state().clone();

        // 用页级入口从 0 重放（每页 1 条），逐页复算链 hash。
        let mut replay =
            V2ControlReplay::new(VAULT, INSTANCE).with_manager_pub(pk(&chain.manager.clone()));
        let mut prev = [0u8; 32];
        for (index, signed) in chain.records.iter().enumerate() {
            let bytes = signed.encode().expect("bytes");
            let mut input = Vec::new();
            input.extend_from_slice(&prev);
            input.extend_from_slice(&bytes);
            let hash = sha256(&input);
            let page = cbor::Fields::from(vec![
                (
                    0,
                    CdeValue::Array(vec![CdeValue::Map(vec![
                        (0, CdeValue::UInt(index as u64 + 1)),
                        (1, CdeValue::Bytes(prev.to_vec())),
                        (2, CdeValue::Bytes(hash.to_vec())),
                        (3, CdeValue::Bytes(bytes)),
                    ])]),
                ),
                (1, CdeValue::UInt(index as u64 + 1)),
            ]);
            replay.apply_page(&page, NOW).expect("apply page");
            prev = hash;
        }
        assert_eq!(replay.state(), &expected);
    }

    // ---------------------------------------------------------------- 负例：链/签名/租户

    #[test]
    fn rejects_tampered_signature_chain_and_tenant() {
        let mut chain = Chain::new();
        upgrade_chain(&mut chain).expect("upgrade");
        let signed = chain.records[0].clone();
        let good_bytes = signed.encode().expect("bytes");

        // 1) 改一字节签名 → 验签失败。
        let mut bad = signed.clone();
        bad.signatures[0][0] ^= 0x01;
        let result = apply_raw(&mut Chain::new(), &bad, 1, [0u8; 32], NOW);
        assert_rejected(result, "错误签名");

        // 2) 链 hash 不符（commit.record_hash 被替换）。
        let mut replay =
            V2ControlReplay::new(VAULT, INSTANCE).with_manager_pub(pk(&chain.manager.clone()));
        let commit = ControlCommitV2 {
            rev: 1,
            prev_hash: [0u8; 32],
            record_hash: [0xAAu8; 32],
            signed_bytes: good_bytes.clone(),
        };
        assert_rejected(replay.apply_commit(&commit, NOW), "record_hash 不符");

        // 3) prev_hash 不符。
        let commit = ControlCommitV2 {
            rev: 1,
            prev_hash: [0xBBu8; 32],
            record_hash: sha256(&[[0xBBu8; 32].as_slice(), good_bytes.as_slice()].concat()),
            signed_bytes: good_bytes.clone(),
        };
        assert_rejected(replay.apply_commit(&commit, NOW), "prev_hash 不符");

        // 4) rev 跳号。
        let mut good_hash_input = Vec::new();
        good_hash_input.extend_from_slice(&[0u8; 32]);
        good_hash_input.extend_from_slice(&good_bytes);
        let commit = ControlCommitV2 {
            rev: 2,
            prev_hash: [0u8; 32],
            record_hash: sha256(&good_hash_input),
            signed_bytes: good_bytes.clone(),
        };
        assert_rejected(replay.apply_commit(&commit, NOW), "rev 跳号");

        // 5) 跨 vault / 跨 instance：签名本身自洽（连 canonical_path 一起改），
        //    回放侧按本地 vault/instance 拒绝。
        for mutate in ["vault", "instance"] {
            let mut op = signed.operation.clone();
            if mutate == "vault" {
                op.vault_id = [0x99u8; 16];
                op.canonical_path = canonical_submit_path(&op.vault_id);
            } else {
                op.instance_id = [0x99u8; 16];
            }
            let resigned = sign_by_pk(&op, &[&chain.manager, &chain.a, &chain.b, &chain.recovery]);
            let result = apply_raw(&mut Chain::new(), &resigned, 1, [0u8; 32], NOW);
            assert_rejected(result, mutate);
        }
    }

    fn apply_raw(
        chain: &mut Chain,
        signed: &SignedControlV2,
        rev: u64,
        prev: [u8; 32],
        now: u64,
    ) -> Result<Option<ControlKindV2>, SyncError> {
        let bytes = signed.encode().expect("bytes");
        let mut input = Vec::new();
        input.extend_from_slice(&prev);
        input.extend_from_slice(&bytes);
        let commit = ControlCommitV2 {
            rev,
            prev_hash: prev,
            record_hash: sha256(&input),
            signed_bytes: bytes,
        };
        chain.replay.apply_commit(&commit, now)
    }

    #[test]
    fn rejects_unknown_upgrade_without_manager_anchor() {
        let mut chain = Chain::new();
        let mut replay = V2ControlReplay::new(VAULT, INSTANCE); // 无 manager_pub
        let signed = {
            upgrade_chain(&mut chain).expect("upgrade");
            chain.records[0].clone()
        };
        let bytes = signed.encode().expect("bytes");
        let mut input = Vec::new();
        input.extend_from_slice(&[0u8; 32]);
        input.extend_from_slice(&bytes);
        let commit = ControlCommitV2 {
            rev: 1,
            prev_hash: [0u8; 32],
            record_hash: sha256(&input),
            signed_bytes: bytes,
        };
        assert!(matches!(
            replay.apply_commit(&commit, NOW),
            Err(SyncError::NeedsTrust(_))
        ));
    }

    #[test]
    fn rejects_threshold_scope_and_rev_regressions() {
        let mut chain = Chain::new();
        upgrade_chain(&mut chain).expect("upgrade");

        // 单签（一台设备）不满足两签门限。
        let limited = cap(
            CAP_LIMITED,
            DEV_A,
            pk(&chain.a.clone()),
            CapScope::INVITE,
            chain.rev + 1,
            2,
            1,
            NOW - 900,
            NOW + 3_600,
        );
        let operation = {
            let signer = device_signer(DEV_A, pk(&chain.a.clone()), Some(CAP_A));
            chain.op(
                ControlKindV2::CapGrant,
                CapGrantPayloadV2 {
                    cap: limited.clone(),
                }
                .encode()
                .expect("payload"),
                vec![signer],
                chain.replay.state().authz_rev(),
                chain.replay.state().authority_rev(),
            )
        };
        let signed = chain.sign(&operation);
        let _ = &operation;
        assert_rejected(
            apply_raw(&mut Chain::new(), &signed, 2, chain.head, NOW),
            "单签",
        );

        // 同设备两张 cap 凑数：先合法授予 A 一张 INVITE cap，再用 (A,CAP_A)+(A,CAP_LIMITED)。
        let mut clone = Chain::new();
        upgrade_chain(&mut clone).expect("upgrade");
        let signed = two_sign(
            &mut clone,
            ControlKindV2::CapGrant,
            CapGrantPayloadV2 {
                cap: limited.clone(),
            }
            .encode()
            .expect("payload"),
            &[(DEV_A, CAP_A), (DEV_B, CAP_B)],
        )
        .expect("op");
        clone.apply(&signed).expect("grant limited");
        let extra = cap(
            [0xC5u8; 16],
            DEV_A,
            pk(&clone.a.clone()),
            CapScope::INVITE,
            clone.rev + 1,
            clone.replay.state().authz_rev() + 1,
            1,
            NOW - 900,
            NOW + 3_600,
        );
        let signers = vec![
            device_signer(DEV_A, pk(&clone.a.clone()), Some(CAP_A)),
            device_signer(DEV_A, pk(&clone.a.clone()), Some(CAP_LIMITED)),
        ];
        let operation = clone.op(
            ControlKindV2::CapGrant,
            CapGrantPayloadV2 { cap: extra }.encode().expect("payload"),
            signers,
            clone.replay.state().authz_rev(),
            clone.replay.state().authority_rev(),
        );
        let signed = clone.sign(&operation);
        let next_rev = clone.rev + 1;
        let head = clone.head;
        let error =
            apply_raw(&mut clone, &signed, next_rev, head, NOW).expect_err("同设备两 cap 必须拒绝");
        assert!(error.to_string().contains("两签"), "{error:?}");

        // 权限拼接：A 的 INVITE-only cap 不能签出 CAP_ADMIN cap。
        let escalate = cap(
            [0xC6u8; 16],
            DEV_A,
            pk(&clone.a.clone()),
            CapScope::CAP_ADMIN,
            clone.rev + 1,
            clone.replay.state().authz_rev() + 1,
            1,
            NOW - 60,
            NOW + 3_600,
        );
        let signers = vec![
            device_signer(DEV_A, pk(&clone.a.clone()), Some(CAP_LIMITED)),
            device_signer(DEV_B, pk(&clone.b.clone()), Some(CAP_B)),
        ];
        let operation = clone.op(
            ControlKindV2::CapGrant,
            CapGrantPayloadV2 { cap: escalate }
                .encode()
                .expect("payload"),
            signers,
            clone.replay.state().authz_rev(),
            clone.replay.state().authority_rev(),
        );
        let signed = clone.sign(&operation);
        let next_rev = clone.rev + 1;
        let head = clone.head;
        let error =
            apply_raw(&mut clone, &signed, next_rev, head, NOW).expect_err("scope 拼接必须拒绝");
        assert!(error.to_string().contains("scope"), "{error:?}");

        // 旧 authz rev（CAS 陈旧）。
        let revoke = CapRevokePayloadV2 {
            cap_id: CAP_LIMITED,
        }
        .encode()
        .expect("payload");
        let signers = vec![
            device_signer(DEV_A, pk(&clone.a.clone()), Some(CAP_A)),
            device_signer(DEV_B, pk(&clone.b.clone()), Some(CAP_B)),
        ];
        let mut operation = clone.op(
            ControlKindV2::CapRevoke,
            revoke,
            signers,
            clone.replay.state().authz_rev(),
            clone.replay.state().authority_rev(),
        );
        operation.expected_authz_rev = operation.expected_authz_rev.saturating_sub(1);
        let signed = clone.sign(&operation);
        let next_rev = clone.rev + 1;
        let head = clone.head;
        assert_rejected(
            apply_raw(&mut clone, &signed, next_rev, head, NOW),
            "旧 authz rev",
        );
    }

    // ---------------------------------------------------------------- cap 选择

    #[test]
    fn cap_selection_requires_a_single_effective_cap() {
        let mut chain = Chain::new();
        upgrade_chain(&mut chain).expect("upgrade");
        // A 只有 CAP_A（全 scope）→ 任何单 scope 都能覆盖。
        let state = chain.replay.state();
        let device_pk = pk(&chain.a.clone());
        assert_eq!(
            state
                .select_cap(
                    &DEV_A,
                    &device_pk,
                    CapScope::new(CapScope::INVITE).expect("s"),
                    NOW
                )
                .expect("cap")
                .cap
                .cap_id,
            CAP_A
        );
        // 错 device / 错 key。
        assert_rejected(
            state.select_cap(
                &DEV_B,
                &device_pk,
                CapScope::new(CapScope::INVITE).expect("s"),
                NOW,
            ),
            "错 device",
        );
        let other_pk = pk(&chain.b.clone());
        assert_rejected(
            state.select_cap(
                &DEV_A,
                &other_pk,
                CapScope::new(CapScope::INVITE).expect("s"),
                NOW,
            ),
            "错 key",
        );
        // 自然到期即时失效。
        assert_rejected(
            state.select_cap(
                &DEV_A,
                &device_pk,
                CapScope::new(CapScope::INVITE).expect("s"),
                NOW + 86_400 + 1,
            ),
            "过期",
        );
        // 撤销后不能选。
        let signed = two_sign(
            &mut chain,
            ControlKindV2::CapRevoke,
            CapRevokePayloadV2 { cap_id: CAP_A }
                .encode()
                .expect("payload"),
            &[(DEV_A, CAP_A), (DEV_B, CAP_B)],
        )
        .expect("op");
        chain.apply(&signed).expect("revoke A cap");
        let state = chain.replay.state();
        assert_rejected(
            state.select_cap(
                &DEV_A,
                &device_pk,
                CapScope::new(CapScope::INVITE).expect("s"),
                NOW,
            ),
            "已撤销",
        );
        // 错 authority：B 的 CAP_B 属于 authority=1，切到 2 后失效（用缓存伪造状态验证纯判定）。
        let mut moved = state.clone();
        moved.authority_rev = 2;
        assert_rejected(
            moved.select_cap(
                &DEV_B,
                &pk(&chain.b.clone()),
                CapScope::new(CapScope::INVITE).expect("s"),
                NOW,
            ),
            "错 authority",
        );
        // 未启用 V2。
        let fresh = ControlStateV2::new(VAULT, INSTANCE);
        assert_rejected(
            fresh.select_cap(
                &DEV_A,
                &device_pk,
                CapScope::new(CapScope::INVITE).expect("s"),
                NOW,
            ),
            "legacy",
        );
    }

    #[test]
    fn cap_selection_never_combines_two_caps() {
        let mut chain = Chain::new();
        upgrade_chain(&mut chain).expect("upgrade");
        // 给 A 一张只有 INVITE 的 cap。
        let limited = cap(
            CAP_LIMITED,
            DEV_A,
            pk(&chain.a.clone()),
            CapScope::INVITE,
            chain.rev + 1,
            2,
            1,
            NOW - 900,
            NOW + 3_600,
        );
        let signed = two_sign(
            &mut chain,
            ControlKindV2::CapGrant,
            CapGrantPayloadV2 { cap: limited }
                .encode()
                .expect("payload"),
            &[(DEV_A, CAP_A), (DEV_B, CAP_B)],
        )
        .expect("op");
        chain.apply(&signed).expect("grant limited");
        let mut state = chain.replay.state().clone();
        // 把全 scope cap 标成撤销，只留下 INVITE-only：需要 INVITE|APPROVE 时**不能**拼。
        state.caps.get_mut(&CAP_A).expect("cap").revoked_control_rev = Some(state.rev);
        let required = CapScope::new(CapScope::INVITE | CapScope::APPROVE).expect("scopes");
        let error = state
            .select_cap(&DEV_A, &pk(&chain.a.clone()), required, NOW)
            .expect_err("不允许拼 cap");
        assert!(error.to_string().contains("单张 cap"), "{error:?}");
    }

    // ---------------------------------------------------------------- pin/缓存

    fn fresh_vault() -> (tempfile::TempDir, Vault) {
        let base = std::env::var("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/tmp"));
        std::fs::create_dir_all(&base).expect("target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("management-v2-")
            .tempdir_in(&base)
            .expect("临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
        (dir, vault)
    }

    #[test]
    fn pin_and_cache_round_trip_and_detect_corruption() {
        let mut chain = Chain::new();
        upgrade_chain(&mut chain).expect("upgrade");
        let state = chain.replay.state();
        let cache = state.encode_cache().expect("cache");
        let decoded = ControlStateV2::decode_cache(&cache, VAULT, INSTANCE).expect("decode cache");
        assert_eq!(&decoded, state);

        let (_dir, vault) = fresh_vault();
        let pin = V2PinV1 {
            vault_id: VAULT,
            instance_id: INSTANCE,
            rev: state.rev(),
            head_hash: state.head_hash(),
            authz_rev: state.authz_rev(),
            authority_rev: state.authority_rev(),
            state_digest: sha256(&cache),
        };
        commit_v2_pin(&vault, None, &pin).expect("首次 pin");
        assert_eq!(load_v2_pin(&vault, VAULT).expect("load"), Some(pin));
        // 同 rev 换 hash = equivocation（由上层判定；这里验证 CAS 语义：旧 expected 冲突）。
        let stale = pin;
        let advanced = V2PinV1 {
            rev: stale.rev + 1,
            head_hash: [0xEEu8; 32],
            ..stale
        };
        commit_v2_pin(&vault, Some(&stale), &advanced).expect("前进");
        assert!(matches!(
            commit_v2_pin(&vault, Some(&stale), &pin),
            Err(SyncError::PinConflict)
        ));
        // 损坏 pin 不得当成"没有 pin"。
        vault
            .set_meta(META_CONTROL_PIN_V2, b"broken")
            .expect("写坏 pin");
        assert!(matches!(
            load_v2_pin(&vault, VAULT),
            Err(SyncError::PinCorrupt(_))
        ));
        // 损坏缓存：解码失败（调用方丢弃重建），不会误信。
        let mut broken_cache = cache.clone();
        let last = broken_cache.len() - 1;
        broken_cache[last] ^= 0xFF;
        assert!(ControlStateV2::decode_cache(&broken_cache, VAULT, INSTANCE).is_err());
        // 换了 vault 绑定同样拒绝。
        assert!(ControlStateV2::decode_cache(&cache, [0x99u8; 16], INSTANCE).is_err());
    }

    #[test]
    fn rollback_against_pin_is_rejected() {
        let mut chain = Chain::new();
        upgrade_chain(&mut chain).expect("upgrade");
        let (_dir, vault) = fresh_vault();
        // 起点：完全没有 pin/cache。
        struct NoopTransport;
        impl HttpTransport for NoopTransport {
            fn send(&mut self, _request: &HttpRequest) -> Result<HttpResponse, SyncError> {
                Err(SyncError::Transport("测试不应该发请求"))
            }
        }
        let config = V2ManagementConfig {
            vault_id: VAULT,
            instance_id: INSTANCE,
            device_id: DEV_A,
            manager_pub: pk(&chain.manager.clone()),
        };
        let signing = DeviceSigningKey::from_seed(Secret32::from_array([0x02; 32]));
        let client =
            V2ManagementClient::new(NoopTransport, "https://relay.example", config, signing)
                .with_store(vault.clone())
                .expect("with_store");
        assert_eq!(client.state().rev(), 0);
        // 手工把已验证 head（rev1）写进 pin/cache（等价首次 refresh 的提交）。
        let cache = chain.replay.state().encode_cache().expect("cache");
        let pin = V2PinV1 {
            vault_id: VAULT,
            instance_id: INSTANCE,
            rev: 1,
            head_hash: chain.replay.state().head_hash(),
            authz_rev: chain.replay.state().authz_rev(),
            authority_rev: chain.replay.state().authority_rev(),
            state_digest: sha256(&cache),
        };
        vault
            .set_meta(META_MANAGEMENT_CACHE_V2, &cache)
            .expect("cache");
        commit_v2_pin(&vault, None, &pin).expect("pin");
        // 新客户端应从缓存恢复（rev1）。
        let client = V2ManagementClient::new(
            NoopTransport,
            "https://relay.example",
            config,
            DeviceSigningKey::from_seed(Secret32::from_array([0x02; 32])),
        )
        .with_store(vault.clone())
        .expect("reload");
        assert_eq!(client.state().rev(), 1);
        assert_eq!(client.state().authz_rev(), 1);
        // 损坏缓存 + pin 保留 → 不误信缓存：回放从 0 开始（rev 归零），pin 仍存在。
        vault
            .set_meta(META_MANAGEMENT_CACHE_V2, b"corrupted")
            .expect("坏缓存");
        let mut client = V2ManagementClient::new(
            NoopTransport,
            "https://relay.example",
            config,
            DeviceSigningKey::from_seed(Secret32::from_array([0x02; 32])),
        )
        .with_store(vault.clone())
        .expect("reload");
        assert_eq!(client.state().rev(), 0);
        assert_eq!(client.pin().expect("pin 保留").rev, 1);
        // 回放水位低于 pin → ControlRollback。
        let error = client
            .verify_and_commit_pin(0, [0u8; 32])
            .expect_err("水位低于 pin 必须回滚拒绝");
        assert!(matches!(
            error,
            SyncError::ControlRollback {
                expected: 1,
                actual: 0
            }
        ));
    }

    #[test]
    fn incremental_replay_verifies_pin_prefix_and_advances() {
        let mut chain = Chain::new();
        upgrade_chain(&mut chain).expect("upgrade");
        let first_state = chain.replay.state().clone();
        // 第二条记录：cap grant。
        let limited = cap(
            CAP_LIMITED,
            DEV_A,
            pk(&chain.a.clone()),
            CapScope::INVITE,
            chain.rev + 1,
            2,
            1,
            NOW - 900,
            NOW + 3_600,
        );
        let signed = two_sign(
            &mut chain,
            ControlKindV2::CapGrant,
            CapGrantPayloadV2 { cap: limited }
                .encode()
                .expect("payload"),
            &[(DEV_A, CAP_A), (DEV_B, CAP_B)],
        )
        .expect("op");
        chain.apply(&signed).expect("cap grant");

        struct NoopTransport;
        impl HttpTransport for NoopTransport {
            fn send(&mut self, _request: &HttpRequest) -> Result<HttpResponse, SyncError> {
                Err(SyncError::Transport("测试不应该发请求"))
            }
        }
        let (_dir, vault) = fresh_vault();
        let cache = first_state.encode_cache().expect("cache");
        let pin = V2PinV1 {
            vault_id: VAULT,
            instance_id: INSTANCE,
            rev: 1,
            head_hash: first_state.head_hash(),
            authz_rev: 1,
            authority_rev: 1,
            state_digest: sha256(&cache),
        };
        vault
            .set_meta(META_MANAGEMENT_CACHE_V2, &cache)
            .expect("cache");
        commit_v2_pin(&vault, None, &pin).expect("pin");
        let config = V2ManagementConfig {
            vault_id: VAULT,
            instance_id: INSTANCE,
            device_id: DEV_A,
            manager_pub: pk(&chain.manager.clone()),
        };
        let mut client = V2ManagementClient::new(
            NoopTransport,
            "https://relay.example",
            config,
            DeviceSigningKey::from_seed(Secret32::from_array([0x02; 32])),
        )
        .with_store(vault.clone())
        .expect("load");
        assert_eq!(client.state().rev(), 1);
        // 从水位续：在本地 replay 上追加第二条（模拟 refresh 拉到的增量）。
        client.replay = V2ControlReplay::from_cached(first_state.clone());
        client.replay.manager_pub = Some(pk(&chain.manager.clone()));
        // 直接用真实链的增量记录：从 watermark=1 续，prev 必须等于 pin head。
        let bytes = chain.records[1].encode().expect("bytes");
        let mut input = Vec::new();
        input.extend_from_slice(&first_state.head_hash());
        input.extend_from_slice(&bytes);
        let commit = ControlCommitV2 {
            rev: 2,
            prev_hash: first_state.head_hash(),
            record_hash: sha256(&input),
            signed_bytes: bytes,
        };
        client.replay.apply_commit(&commit, NOW).expect("增量记录");
        client
            .verify_and_commit_pin(1, first_state.head_hash())
            .expect("增量回放后 pin 前进");
        let pin = load_v2_pin(&vault, VAULT).expect("load pin").expect("pin");
        assert_eq!(pin.rev, 2);
        assert_eq!(pin.authz_rev, 2);
        assert_eq!(pin.head_hash, chain.replay.state().head_hash());
    }

    // ---------------------------------------------------------------- challenge 校验

    #[test]
    fn challenge_validation_rejects_cross_tenant_and_stale_revs() {
        struct NoopTransport;
        impl HttpTransport for NoopTransport {
            fn send(&mut self, _request: &HttpRequest) -> Result<HttpResponse, SyncError> {
                Err(SyncError::Transport("测试不应该发请求"))
            }
        }
        let mut chain = Chain::new();
        upgrade_chain(&mut chain).expect("upgrade");
        let state = chain.replay.state();
        let config = V2ManagementConfig {
            vault_id: VAULT,
            instance_id: INSTANCE,
            device_id: DEV_A,
            manager_pub: pk(&chain.manager.clone()),
        };
        let mut client = V2ManagementClient::new(
            NoopTransport,
            "https://relay.example",
            config,
            DeviceSigningKey::from_seed(Secret32::from_array([0x02; 32])),
        );
        client.replay = V2ControlReplay::from_cached(state.clone());
        let mut origin_input = Vec::new();
        origin_input.extend_from_slice(&INSTANCE);
        origin_input.extend_from_slice(&VAULT);
        let origin_hash = sha256(&origin_input);
        let good = ChallengeV2 {
            version: V2_WIRE_VERSION,
            instance_id: INSTANCE,
            origin_hash,
            vault_id: VAULT,
            realm: CHALLENGE_V2_REALM_DEVICE,
            subject: DEV_A,
            challenge_id: [0x01u8; 16],
            nonce: [0x02u8; 32],
            expires_at: NOW + 60,
            auth_rev: 7,
            authz_rev: state.authz_rev(),
            cap_id: None,
            authority_rev: state.authority_rev(),
        };
        client.verify_challenge(&good, NOW, true).expect("正例");
        let mut bad = good.clone();
        bad.instance_id = [0x99u8; 16];
        assert_rejected(client.verify_challenge(&bad, NOW, true), "错 instance");
        let mut bad = good.clone();
        bad.vault_id = [0x99u8; 16];
        assert_rejected(client.verify_challenge(&bad, NOW, true), "错 vault");
        let mut bad = good.clone();
        bad.origin_hash = [0x99u8; 32];
        assert_rejected(client.verify_challenge(&bad, NOW, true), "错 origin");
        let mut bad = good.clone();
        bad.authz_rev = state.authz_rev() + 1;
        assert_rejected(client.verify_challenge(&bad, NOW, true), "错 authz rev");
        let mut bad = good.clone();
        bad.authority_rev = state.authority_rev() + 1;
        assert_rejected(client.verify_challenge(&bad, NOW, true), "错 authority rev");
        let mut bad = good.clone();
        bad.expires_at = NOW;
        assert_rejected(client.verify_challenge(&bad, NOW, true), "过期");
        let mut bad = good.clone();
        bad.expires_at = NOW + 10_000;
        assert_rejected(client.verify_challenge(&bad, NOW, true), "超窗");
    }
}

// @@PART4@@
