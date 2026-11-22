//! ADR 0014 **S1**：邀请链接（`mn-pair://join?...`）的生成、加入（PENDING）与批准。
//!
//! ## 为什么是邀请链接
//!
//! ADR 0014 §B：普通 Join 复用"恢复码"意味着新设备直接拿到**管理权**（恢复码可派生
//! 管理私钥）。邀请链接把两件事拆开：
//!
//! 1. **加入**只需要一段一次性 token（`T`，relay 只存 `SHA256(T)`）→ 设备只到 `PENDING`；
//! 2. **授权**仍由现有管理钥在创建端显式批准（§17.1 设备证书 + §17.2 HPKE grant）。
//!
//! 链接格式（与 ADR 0014 §B 草案一致，`pin` = 邀请方已保存的 relay instance 公开锚）：
//!
//! ```text
//! mn-pair://join?v=1&relay=<percent-encoded canonical origin>&vault=<hex32>&iid=<hex32>&pin=<hex32>#token=<base64url>
//! ```
//!
//! - token **只在 fragment**：不随查询串进 referrer/日志；relay 永不见明文 token；
//! - `local_dev` 只接受 `http://` + loopback 白名单；远端只接受 `https://`（复用
//!   [`crate::relay_policy::parse_relay_url`]，不另写一套 URL 规则）；
//! - `pin` 是加入端**唯一**的 relay 实例锚：兑换/认证/控制链全程必须一致，
//!   [`SyncError::InstanceMismatch`]，**不会**自动换 pin。
//!
//! ## 加入端 durable PENDING
//!
//! 兑换成功只产生 `PENDING` 设备。新设备的签名/HPKE 私钥、peer、invite_id、relay、
//! vault、pin 与阶段必须**先 durable**，否则丢回应/重启后只能重新兑换（`INVITE_USED`
//! 不可当成功，ADR 0014 §B）。本模块把这些材料经 [`crate::identity_wrap`]
//! （DPAPI / Linux 用户主钥）封装后写进**新实例自己的** SQLite meta
//! [`META_INVITE_PENDING`]：
//!
//! ```text
//! meta = b"MNIP" | version(u8) | vault_id(16) | device_id(16) | len(u32 BE) | protected CDE
//! ```
//!
//! - 明文头只放**公开 id**（它们是本地封装 entropy 的输入）；
//! - **token 不落盘**：兑换已经成功，后续检查/批准后 bootstrap 都不需要它；
//! - 阶段只进不退：`Redeemed → Approved → Bootstrapped`；清理只在
//!   [`InviteJoin::complete`]（app 写完公开配置之后），崩溃可对账。
//!
//! ## 批准与 bootstrap
//!
//! 创建端只用**现有管理钥**（`identity.manager_seed`）批准：从已验证成员视图读 pending
//! 设备的 `sign_pk`/`hpke_pk`，签 §17.1 证书并把当前 epoch 根封成 §17.2 grant（复用
//! [`crate::recovery_enroll`] 的签名/canonical body 实现，不复制一套）。
//!
//! 加入端在批准后用 pending 的签名钥走设备认证 → `GET devices/self/grants/latest` →
//! 用 pending 的 HPKE 私钥解 grant 得 root → [`crate::bootstrap::run_with_pins`]
//! （pin = 链接里的 relay instance）→ 提交 `sync.control_pin` → 构造
//! `LocalIdentityV1`（`manager_seed = None`）。任何一步失败都保留 pending 可重试，
//! **不半写身份/配置**（身份落盘与配置由 app 在 `Joined` 之后完成）。
//!
//! ## 信任边界（v1 明确记录）
//!
//! - 链接由创建端**带外**交给加入端；host/scheme/vault/iid/pin 严格校验，拒绝 userinfo、
//!   未知字段、额外 path、query 注入与自动换 pin；
//! - 首次接触时加入端无法把管理公钥锚到链外（链接不含管理公钥，ADR 0014 也不含）：
//!   grant 的签名按 relay 管理链里 `manager_rev` 对应公钥验证——它是**链内自洽**校验，
//!   不是外部信任证明；真正的授权仍是创建端在设备管理里比对指纹后批准**这台**设备
//!   （UI 必须展示双端指纹）；
//! - 完整 URL（含 token）不写日志、不进 `Debug`、不进公开配置：本模块的
//!   [`InviteLink`]/[`InviteLinkTarget`] 手写 `Debug` 只打印公开 id。

use sha2::{Digest, Sha256};

use crate::api_version::{RelayApi, RelayApiVersion};
use crate::bootstrap::{self, BootstrapPins, BootstrapReport};
use crate::cbor;
use crate::control_pin::commit_verified_head;
use crate::device::{device_fingerprint, DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use crate::dpapi;
use crate::error::SyncError;
use crate::hpke::open_epoch_grant;
use crate::http_relay::{Clock as _, HttpRelayClient, HttpRelayConfig, StdTcpTransport};
use crate::http_transport::HttpTransport;
use crate::identity::LocalIdentityV1;
use crate::identity_wrap;
use crate::keys::{RootSecretV1, Secret32};
use crate::random::{array, OsRandom};
use crate::relay::RelayClient as _;
use crate::relay_context::RelayContext;
use crate::relay_policy::{parse_relay_url, RelayMode};
use crate::{DeviceId, VaultId};
use core::fmt;
use menghunote_storage::{Value, Vault};
use menghunote_sync_proto::GrantAadV1;
use zeroize::Zeroizing;

/// 链接 scheme（区分大小写：URL scheme 会先小写化，见 parser）。
pub const INVITE_LINK_SCHEME: &str = "mn-pair";
/// 链接 host（`mn-pair://join`）。
pub const INVITE_LINK_JOIN_HOST: &str = "join";
/// 默认兑换 TTL：15 分钟（ADR 0014 §B）。
pub const DEFAULT_INVITE_TTL_SECS: u64 = 15 * 60;
/// TTL 上限：relay 的邀请有效期上限（24h）。
pub const MAX_INVITE_TTL_SECS: u64 = 24 * 60 * 60;

/// meta 键：加入端 durable PENDING（受保护 CDE；**不含 token**）。
pub const META_INVITE_PENDING: &str = "sync.invite_pending.v1";

/// pending 记录版本（CDE 键 0）。
const PENDING_VERSION: u64 = 1;
/// 外层 magic（meta 二进制头）。
const PENDING_MAGIC: [u8; 4] = *b"MNIP";
/// 外层版本。
const PENDING_OUTER_VERSION: u8 = 1;
/// 明文头长度 = magic4 + version1 + vault16 + device16 + len4。
const PENDING_HEADER_LEN: usize = 4 + 1 + 16 + 16 + 4;

// ---------------------------------------------------------------------------
// 对外类型
// ---------------------------------------------------------------------------

/// 创建端生成的一次性邀请链接。
///
/// **含 token**（在 URL fragment 里）：只在内存里展示/复制，不写日志/配置/Debug。
pub struct InviteLink {
    /// 完整 URL（含 fragment token；**不得**打印）。
    pub url: String,
    /// relay 侧邀请 id（公开标识，32 位 hex 可展示）。
    pub invite_id: [u8; 16],
    /// relay 确认的过期时刻（Unix 秒）。
    pub expires_at: u64,
}

impl InviteLink {
    /// 剩余有效期（秒；已过期 = 0）。
    #[must_use]
    pub fn expires_in(&self, now: u64) -> u64 {
        self.expires_at.saturating_sub(now)
    }

    /// TTL 文案（不含 token）：`有效至 12:34（剩余 14 分 59 秒）`。
    #[must_use]
    pub fn ttl_text(&self, now: u64) -> String {
        let left = self.expires_in(now);
        let minutes = left / 60;
        let seconds = left % 60;
        if left == 0 {
            "邀请已过期：请在设备面板重新生成".to_owned()
        } else {
            format!("邀请 {minutes} 分 {seconds} 秒内有效（一次性；链接含令牌，请私发）")
        }
    }

    /// 邀请 id 的短展示（前 8 位 hex；公开）。
    #[must_use]
    pub fn short_invite_id(&self) -> String {
        crate::ids::hex_of(&self.invite_id)
            .chars()
            .take(8)
            .collect()
    }
}

impl fmt::Debug for InviteLink {
    /// 绝不打印 URL/token：只给公开 id 与过期时刻（日志/断言可安全使用）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InviteLink")
            .field("invite_id", &crate::ids::hex_of(&self.invite_id))
            .field("expires_at", &self.expires_at)
            .field("url", &"<redacted: contains token>")
            .finish()
    }
}

/// 解析后的邀请链接（加入端）。
///
/// `token` 是秘密：手写 `Debug` 不打印；`relay` 是 canonical origin。
pub struct InviteLinkTarget {
    mode: RelayMode,
    relay: String,
    vault_id: VaultId,
    invite_id: [u8; 16],
    pin: [u8; 16],
    token: [u8; 32],
    /// 该 vault 的 relay API 面（ADR 0014 S3b）：链接 `api=` 字段；缺省 = v1。
    api_version: RelayApiVersion,
}

impl InviteLinkTarget {
    /// 严格解析邀请链接（唯一入口）。
    ///
    /// 拒绝：非 `mn-pair://join`、userinfo、额外 path、重复/未知 query 字段、
    /// `v=1` 之外版本、非 32 位 hex 的 vault/iid/pin、非 32B 的 token、
    /// 非 loopback 的 `http`、非 `https` 的远端。
    pub fn parse(raw: &str) -> Result<Self, InviteLinkError> {
        let text = raw.trim();
        if text.is_empty() {
            return Err(InviteLinkError::Empty);
        }
        let rest = text
            .strip_prefix("mn-pair://")
            .or_else(|| text.strip_prefix("MN-PAIR://"))
            .ok_or(InviteLinkError::Scheme)?;
        // fragment 只允许一个 `#`；token 只从 fragment 取。
        let (head, fragment) = rest.split_once('#').ok_or(InviteLinkError::Fragment)?;
        let (authority, query) = head.split_once('?').ok_or(InviteLinkError::Query)?;
        if !authority.eq_ignore_ascii_case(INVITE_LINK_JOIN_HOST) {
            return Err(InviteLinkError::Host);
        }
        if query.is_empty() {
            return Err(InviteLinkError::Query);
        }
        let token = parse_fragment(fragment)?;
        let fields = parse_query(query)?;
        let version = fields.version.ok_or(InviteLinkError::Missing("v"))?;
        if version != "1" {
            return Err(InviteLinkError::Version);
        }
        let relay_text = fields.relay.ok_or(InviteLinkError::Missing("relay"))?;
        let relay_plain = percent_decode(relay_text)?;
        let mode = if relay_plain.starts_with("https://") {
            RelayMode::RemoteTls
        } else if relay_plain.starts_with("http://") {
            RelayMode::LocalDev
        } else {
            return Err(InviteLinkError::RelayScheme);
        };
        let origin = parse_relay_url(&relay_plain, mode)
            .map_err(|error| InviteLinkError::Relay(error.to_string()))?;
        let vault_id = parse_hex32(fields.vault.ok_or(InviteLinkError::Missing("vault"))?)?;
        let invite_id = parse_hex32(fields.iid.ok_or(InviteLinkError::Missing("iid"))?)?;
        let pin = parse_hex32(fields.pin.ok_or(InviteLinkError::Missing("pin"))?)?;
        // ADR 0014 S3b：`api` 可选，缺省按 v1（旧链接逐字节兼容）；未知值明确拒绝。
        let api_version = match fields.api {
            None => RelayApiVersion::V1,
            Some(text) => parse_api_version(text)?,
        };
        Ok(Self {
            mode,
            relay: origin.as_str(),
            vault_id,
            invite_id,
            pin,
            token,
            api_version,
        })
    }

    /// 由 canonical 组件构造（测试/内部用；不做额外校验，调用方必须已解析过）。
    #[must_use]
    pub fn from_parts(
        mode: RelayMode,
        relay: impl Into<String>,
        vault_id: VaultId,
        invite_id: [u8; 16],
        pin: [u8; 16],
        token: [u8; 32],
        api_version: RelayApiVersion,
    ) -> Self {
        Self {
            mode,
            relay: relay.into(),
            vault_id,
            invite_id,
            pin,
            token,
            api_version,
        }
    }

    /// relay 接入模式（由链接 scheme 决定，只接受 http-loopback / https）。
    #[must_use]
    pub const fn mode(&self) -> RelayMode {
        self.mode
    }

    /// canonical relay origin（公开）。
    #[must_use]
    pub fn relay(&self) -> &str {
        &self.relay
    }

    /// 目标 vault id。
    #[must_use]
    pub const fn vault_id(&self) -> VaultId {
        self.vault_id
    }

    /// relay 邀请 id。
    #[must_use]
    pub const fn invite_id(&self) -> [u8; 16] {
        self.invite_id
    }

    /// 邀请方 pin 的 relay instance（公开锚）。
    #[must_use]
    pub const fn pin(&self) -> [u8; 16] {
        self.pin
    }

    /// 一次性 token（秘密；只在兑换请求里使用）。
    #[must_use]
    pub const fn token(&self) -> &[u8; 32] {
        &self.token
    }

    /// 该 vault 的 relay API 面（链接 `api=`；缺省 v1）。
    ///
    /// 加入端必须用它构造 `RelayContext`：v2 vault 的邀请必须在 `/v2/...` 上兑换/
    /// 认证/bootstrap，否则管理签名面与控制链历史不一致（fail-closed）。
    #[must_use]
    pub const fn api_version(&self) -> RelayApiVersion {
        self.api_version
    }

    /// vault id 的 32 位 hex（公开；供 UI 预填/展示）。
    #[must_use]
    pub fn vault_id_hex(&self) -> String {
        crate::ids::hex_of(&self.vault_id)
    }
}

impl fmt::Debug for InviteLinkTarget {
    /// token 不打印；其余是公开定位信息。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InviteLinkTarget")
            .field("mode", &self.mode)
            .field("relay", &self.relay)
            .field("vault_id", &crate::ids::hex_of(&self.vault_id))
            .field("invite_id", &crate::ids::hex_of(&self.invite_id))
            .field("pin", &crate::ids::hex_of(&self.pin))
            .field("api_version", &self.api_version)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// 邀请链接解析/校验失败原因（**不含 token/完整 URL**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteLinkError {
    /// 空输入。
    Empty,
    /// 不是 `mn-pair://`。
    Scheme,
    /// host 不是 `join`（或带了额外 path）。
    Host,
    /// 缺 fragment / 多余 `#`。
    Fragment,
    /// 缺 query。
    Query,
    /// 缺必需字段。
    Missing(&'static str),
    /// 重复字段。
    Duplicate(&'static str),
    /// 未知字段（拒绝 `?next=` 之类的恶意跳转）。
    Unknown(&'static str),
    /// 版本不是 1。
    Version,
    /// `api` 字段不是受支持的 relay API 面（只支持 1/2）。
    ApiVersion,
    /// fragment token 非法（长度/编码）。
    Token,
    /// 百分号编码非法。
    Percent,
    /// `vault`/`iid`/`pin` 不是 32 位 hex。
    Hex(&'static str),
    /// relay 不是受支持的 scheme（http/https）。
    RelayScheme,
    /// relay 被唯一权威 URL parser 拒绝（文案只含公开定位信息）。
    Relay(String),
}

impl fmt::Display for InviteLinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "邀请链接为空"),
            Self::Scheme => write!(f, "不是 mn-pair:// 邀请链接"),
            Self::Host => write!(f, "邀请链接的 host 必须是 join（不接受额外 path/跳转）"),
            Self::Fragment => write!(f, "邀请链接缺 fragment（#token=…）或含多余 #"),
            Self::Query => write!(f, "邀请链接缺 query（?v=1&relay=…）"),
            Self::Missing(field) => write!(f, "邀请链接缺字段 {field}=…"),
            Self::Duplicate(field) => write!(f, "邀请链接字段重复：{field}"),
            Self::Unknown(field) => write!(f, "邀请链接含未知字段 {field}（拒绝跳转/夹带）"),
            Self::Version => write!(f, "邀请链接版本不是 1"),
            Self::ApiVersion => {
                write!(f, "邀请链接的 api 字段非法（只支持 1=v1 / 2=v2）")
            }
            Self::Token => write!(f, "邀请链接令牌格式非法（应为 32 字节 base64url）"),
            Self::Percent => write!(f, "邀请链接含非法百分号编码"),
            Self::Hex(field) => write!(f, "邀请链接字段 {field} 不是 32 位十六进制"),
            Self::RelayScheme => write!(f, "邀请链接的 relay 必须是 http:// 或 https://"),
            Self::Relay(message) => write!(f, "邀请链接的 relay 不合法：{message}"),
        }
    }
}

impl std::error::Error for InviteLinkError {}

impl From<InviteLinkError> for SyncError {
    fn from(error: InviteLinkError) -> Self {
        Self::InviteLink(error.to_string())
    }
}

/// 加入端 pending 阶段（只进不退；`u64` 是 meta 里的稳定编码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InviteJoinPhase {
    /// 已兑换（relay 上 `PENDING`），等创建者批准。
    Redeemed,
    /// 已观察到设备 `ACTIVE`（grant 已可读），等 bootstrap 完成。
    Approved,
    /// bootstrap 已完成、身份材料已生成（等 app 写身份 + 公开配置后 [`InviteJoin::complete`]）。
    Bootstrapped,
}

impl InviteJoinPhase {
    const fn as_code(self) -> u64 {
        match self {
            Self::Redeemed => 1,
            Self::Approved => 2,
            Self::Bootstrapped => 3,
        }
    }

    const fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::Redeemed),
            2 => Some(Self::Approved),
            3 => Some(Self::Bootstrapped),
            _ => None,
        }
    }

    /// 稳定短标签（日志/诊断）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Redeemed => "redeemed",
            Self::Approved => "approved",
            Self::Bootstrapped => "bootstrapped",
        }
    }

    /// 面向用户的中文文案。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Redeemed => "已兑换，等待创建者批准",
            Self::Approved => "创建者已批准，正在导入基线",
            Self::Bootstrapped => "已加入，等待写入配置",
        }
    }
}

/// 加入端的 durable pending（公开事实；秘密在受保护外层里）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvitePending {
    /// 目标 vault id。
    pub vault_id: VaultId,
    /// 本次本地生成的设备 id。
    pub device_id: DeviceId,
    /// Loro peer（u64，确定性映射自 16B 随机 peer）。
    pub peer_id: u64,
    /// relay 邀请 id。
    pub invite_id: [u8; 16],
    /// canonical relay origin。
    pub relay: String,
    /// relay 接入模式。
    pub mode: RelayMode,
    /// 该 vault 的 relay API 面（ADR 0014 S3b；链接 `api=` 的持久副本）。
    ///
    /// 检查/批准后 bootstrap 必须用**同一个面**（v2 vault 的 `check/finish` 不能回落
    /// `/v1/...`：管理签名路径与控制链历史都按面固定）。
    pub api_version: RelayApiVersion,
    /// 邀请方 pin 的 relay instance（不自动更换）。
    pub pin: [u8; 16],
    /// 本机设备的 Ed25519 签名公钥（公开；UI 双端短 hash 比对用）。
    pub sign_pk: [u8; 32],
    /// 本机设备的 X25519 HPKE 公钥（公开；UI 双端短 hash 比对用）。
    pub hpke_pk: [u8; 32],
    /// 邀请过期时刻（Unix 秒；过期后不可再兑换）。
    pub expires_at: u64,
    /// 兑换时刻（Unix 秒）。
    pub created_at: u64,
    /// 阶段。
    pub phase: InviteJoinPhase,
}

impl InvitePending {
    /// 剩余有效期（秒）。
    #[must_use]
    pub fn expires_in(&self, now: u64) -> u64 {
        self.expires_at.saturating_sub(now)
    }

    /// 是否已过期（只用于展示；兑换由 relay 判定）。
    #[must_use]
    pub fn expired(&self, now: u64) -> bool {
        self.expires_at <= now
    }

    /// 设备指纹（32 位 hex；与 relay 的公式一致，双端人工比对用）。
    #[must_use]
    pub fn fingerprint(&self) -> String {
        invite_device_fingerprint(
            &self.vault_id,
            &self.device_id,
            &self.sign_pk,
            &self.hpke_pk,
        )
    }

    /// 签名/HPKE 公钥的短 hash（各 16 位 hex；UI 双端比对用）。
    #[must_use]
    pub fn key_short_hashes(&self) -> (String, String) {
        (key_short_hash(&self.sign_pk), key_short_hash(&self.hpke_pk))
    }

    /// 短设备 id（前 8 位 hex）。
    #[must_use]
    pub fn short_device_id(&self) -> String {
        crate::ids::hex_of(&self.device_id)
            .chars()
            .take(8)
            .collect()
    }
}

/// [`InviteJoin::redeem`] 的结果：新开的库 + pending 事实。
pub struct InviteJoinStart {
    /// 本次新建（或复用的）本地库。
    pub vault: Vault,
    /// 库文件路径。
    pub vault_path: std::path::PathBuf,
    /// durable pending（已落库）。
    pub pending: InvitePending,
    /// `true` = 复用了已有 pending（**没有**重复兑换；重启/丢回应的续跑路径）。
    pub resumed: bool,
}

/// [`InviteJoin::check`] 的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteCheck {
    /// 仍是 `PENDING`（或已被移除；relay 对两者都回 `REVOKED`，不区分）。
    WaitingForApproval,
    /// 已 `ACTIVE`，可以 bootstrap。
    Approved,
}

/// [`InviteJoin::finish`] 的结论。
pub enum InviteFinish {
    /// 还没批准：保留 pending，稍后再查。
    WaitingForApproval,
    /// bootstrap 完成；app 必须落盘 `identity`、写公开配置后再 [`InviteJoin::complete`]。
    Joined(Box<InviteJoined>),
}

/// bootstrap 完成后的材料。
pub struct InviteJoined {
    /// 新设备身份（`manager_seed = None`；普通设备没有管理私钥）。
    pub identity: LocalIdentityV1,
    /// bootstrap 报告（覆盖水位/是否 fallback）。
    pub report: BootstrapReport,
    /// 本次加入的 pending 事实（清理前的快照）。
    pub pending: InvitePending,
}

impl fmt::Debug for InviteJoined {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InviteJoined")
            .field("identity", &self.identity)
            .field("covered_seq", &self.report.covered_seq)
            .field("phase", &self.pending.phase)
            .finish_non_exhaustive()
    }
}

/// 创建端批准一台 PENDING 设备的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedDevice {
    /// 目标设备 id。
    pub device_id: DeviceId,
    /// 设备指纹（32 位 hex，双端比对用）。
    pub fingerprint: String,
    /// 批准后 relay 的活动 epoch。
    pub epoch: u64,
    /// 批准后的 `control_rev`（批准会追加控制记录）。
    pub control_rev: u64,
    /// `false` = 目标此前已经是 `ACTIVE`（幂等：没有重复签名/追加）。
    pub changed: bool,
}

// ---------------------------------------------------------------------------
// 创建端：生成邀请链接
// ---------------------------------------------------------------------------

/// 生成一次性邀请链接（默认 TTL 15 分钟）。
///
/// 要求：受保护身份里有**管理私钥**（Join 设备明确失败）、relay instance 与
/// `identity.pinned_instance` 一致、control_rev 取 relay 当前 `/state`（不做本地猜测）。
/// token 32B 随机、只在返回值里；relay 只存 `SHA256(T)`；本地**不持久化** token。
pub fn create_invite_link(
    relay: &RelayContext,
    vault: &Vault,
    identity: &LocalIdentityV1,
) -> Result<InviteLink, SyncError> {
    create_invite_link_with(
        relay,
        vault,
        identity,
        DEFAULT_INVITE_TTL_SECS,
        crate::http_relay::SystemClock.now_secs(),
    )
}

/// 同 [`create_invite_link`]，显式给 TTL 与当前时间（测试/受控调用）。
pub fn create_invite_link_with(
    relay: &RelayContext,
    vault: &Vault,
    identity: &LocalIdentityV1,
    ttl_secs: u64,
    now: u64,
) -> Result<InviteLink, SyncError> {
    if ttl_secs == 0 {
        return Err(SyncError::InviteLink("邀请有效期必须大于 0".to_owned()));
    }
    if ttl_secs > MAX_INVITE_TTL_SECS {
        return Err(SyncError::InviteLink(format!(
            "邀请有效期上限 {MAX_INVITE_TTL_SECS} 秒（24 小时），收到 {ttl_secs}"
        )));
    }
    let Some(seed) = identity.manager_seed.clone() else {
        return Err(SyncError::Unsupported(
            "本机没有管理密钥（Join/邀请加入的设备）：不能生成邀请链接；请在有管理钥的设备上操作",
        ));
    };
    let manager = ManagementKey::from_seed(seed);
    if manager.public_key() != identity.pinned_manager {
        return Err(SyncError::LocalCorrupt(
            "身份的管理私钥与 pinned_manager 不一致（身份文件被换过或损坏）",
        ));
    }
    if identity.pinned_instance == [0u8; 16] {
        return Err(SyncError::NeedsTrust(
            "身份缺 pin 的 relay instance：不能生成邀请链接（先完成 Create/Join）",
        ));
    }
    // 与普通同步/管理操作同一道门禁：初始化未完成时不发管理请求。
    crate::init_journal::ensure_no_pending_sync(vault, identity.vault_id)?;

    let base = relay.origin_str();
    let api = relay.api();
    let mut transport = relay.transport()?;
    let (token, instance) = crate::recovery_enroll::management_token(
        &mut transport,
        &api,
        identity.vault_id,
        &manager,
        now,
    )?;
    if instance != identity.pinned_instance {
        return Err(SyncError::InstanceMismatch);
    }
    let state = state_view(&mut transport, &api, identity.vault_id, &token)?;
    if state.instance_id != identity.pinned_instance {
        return Err(SyncError::InstanceMismatch);
    }
    let expires_at = now.saturating_add(ttl_secs);
    let (invite_id, raw_token, confirmed_expiry) = crate::recovery_enroll::create_invite(
        &mut transport,
        &api,
        identity.vault_id,
        &manager,
        &token,
        state.control_rev,
        identity.pinned_instance,
        now,
        Some(expires_at),
    )?;
    let url = build_invite_url(
        &base,
        &identity.vault_id,
        &invite_id,
        &identity.pinned_instance,
        &raw_token,
        relay.api_version(),
    )?;
    Ok(InviteLink {
        url,
        invite_id,
        expires_at: confirmed_expiry,
    })
}

/// 组装链接（唯一编码入口；token 只进 fragment）。
///
/// ADR 0014 S3b：`api` 只在 **v2** vault 的链接里出现（`&api=2`）；v1 省略该字段，
/// 使既有 v1 链接逐字节不变（旧客户端/旧链接不需要任何迁移）。
fn build_invite_url(
    relay: &str,
    vault_id: &VaultId,
    invite_id: &[u8; 16],
    pin: &[u8; 16],
    token: &[u8; 32],
    api_version: RelayApiVersion,
) -> Result<String, SyncError> {
    let mut url = format!(
        "{INVITE_LINK_SCHEME}://{INVITE_LINK_JOIN_HOST}?v=1&relay={}&vault={}&iid={}&pin={}",
        percent_encode(relay),
        crate::ids::hex_of(vault_id),
        crate::ids::hex_of(invite_id),
        crate::ids::hex_of(pin),
    );
    if api_version != RelayApiVersion::V1 {
        url.push_str("&api=");
        url.push_str(&api_version.as_number().to_string());
    }
    url.push_str("#token=");
    url.push_str(&percent_encode(&crate::base64url::encode(token)));
    Ok(url)
}

// ---------------------------------------------------------------------------
// 加入端：durable PENDING 的兑换/检查/bootstrap
// ---------------------------------------------------------------------------

/// 加入端（邀请链接）的库级入口。
pub struct InviteJoin;

impl InviteJoin {
    /// 兑换邀请 → relay 上 `PENDING` 设备，并 durable 保存 pending 材料。
    ///
    /// 幂等：目标路径已有**同一邀请**的 pending 时复用（`resumed = true`），**不重复兑换**
    /// （`INVITE_USED` 不可当成功，ADR 0014 §B）。目录已有其它库 → 明确拒绝。
    ///
    /// 失败语义：本次新建的库文件被清掉（不留下半初始化库）；relay 侧可能留下一个
    /// 未批准的 `PENDING` 设备（一次性 token 已消费），重试需要**新建链接**。
    pub fn redeem(
        context: &RelayContext,
        target: &InviteLinkTarget,
        vault_path: &std::path::Path,
        now: u64,
    ) -> Result<InviteJoinStart, SyncError> {
        if vault_path.as_os_str().is_empty() {
            return Err(SyncError::LocalCorrupt("邀请加入需要明确的 vault 路径"));
        }
        // 先判 resume：已有同一邀请的 durable pending 时**纯本地**返回（不联网，
        // 也不需要调用方重新构造正确的 context——这正是丢回应/重启后的续跑路径）。
        if vault_path.exists() {
            let vault = Vault::open(vault_path)?;
            if let Some(pending) = Self::load(&vault)? {
                if pending.vault_id == target.vault_id()
                    && pending.invite_id == target.invite_id()
                    && pending.relay == target.relay()
                    && pending.pin == target.pin()
                {
                    return Ok(InviteJoinStart {
                        vault,
                        vault_path: vault_path.to_path_buf(),
                        pending,
                        resumed: true,
                    });
                }
                return Err(SyncError::LocalCorrupt(
                    "目标目录已有另一个同步实例：请换一个新目录（不覆盖已有库）",
                ));
            }
            return Err(SyncError::LocalCorrupt(
                "目标目录已有库/身份：邀请加入只接受空目录或本邀请的 PENDING 续跑",
            ));
        }
        // 只有真正要发请求的新兑换才要求 context 与链接一致（canonical origin + 模式）。
        if context.origin_str() != target.relay() {
            return Err(SyncError::ConfigInvalid(
                "relay context 与邀请链接的 canonical origin 不一致".to_owned(),
            ));
        }
        if context.mode != target.mode() {
            return Err(SyncError::ConfigInvalid(
                "relay context 模式与邀请链接不一致（http/https 不能混用）".to_owned(),
            ));
        }
        if context.api_version() != target.api_version() {
            return Err(SyncError::ConfigInvalid(
                "relay context 的 API 面与邀请链接不一致（v1/v2 不能在加入时混用）".to_owned(),
            ));
        }

        let vault = Vault::open(vault_path)?;
        let redeemed = Self::redeem_fresh(context, target, &vault, now);
        match redeemed {
            Ok(pending) => Ok(InviteJoinStart {
                vault,
                vault_path: vault_path.to_path_buf(),
                pending,
                resumed: false,
            }),
            Err(error) => {
                drop(vault);
                crate::dev_local::remove_vault_files(vault_path);
                Err(error)
            }
        }
    }

    fn redeem_fresh(
        context: &RelayContext,
        target: &InviteLinkTarget,
        vault: &Vault,
        now: u64,
    ) -> Result<InvitePending, SyncError> {
        let signing_key = DeviceSigningKey::generate(&mut OsRandom)?;
        let hpke_key = DeviceHpkeKey::generate(&mut OsRandom)?;
        let device_id: DeviceId = array(&mut OsRandom)?;
        let peer_bytes: [u8; 16] = array(&mut OsRandom)?;
        let mut peer_head = [0u8; 8];
        peer_head.copy_from_slice(&peer_bytes[..8]);
        let peer_id = u64::from_be_bytes(peer_head) & 0x7fff_ffff_ffff_ffff;

        let api = context.api();
        let mut transport = context.transport()?;
        crate::recovery_enroll::redeem_invite(
            &mut transport,
            &api,
            target.vault_id(),
            target.invite_id(),
            target.token(),
            device_id,
            &signing_key,
            &hpke_key,
        )?;
        let pending = InvitePending {
            vault_id: target.vault_id(),
            device_id,
            peer_id,
            invite_id: target.invite_id(),
            relay: target.relay().to_owned(),
            mode: target.mode(),
            api_version: target.api_version(),
            pin: target.pin(),
            sign_pk: signing_key.public_key(),
            hpke_pk: hpke_key.public_key(),
            // 链接本身不带 TTL：兑换成功后以本地 now 起算的保守展示时刻（relay 侧语义不变）。
            expires_at: now.saturating_add(DEFAULT_INVITE_TTL_SECS),
            created_at: now,
            phase: InviteJoinPhase::Redeemed,
        };
        let secrets = PendingSecrets {
            signing_seed: signing_key.seed().clone(),
            hpke_secret: hpke_key.secret().clone(),
        };
        save_pending(vault, &pending, &secrets)?;
        Ok(pending)
    }

    /// 读 durable pending（没有返回 `None`；损坏/未知版本是明确错误）。
    pub fn load(vault: &Vault) -> Result<Option<InvitePending>, SyncError> {
        let Some(blob) = vault.get_meta(META_INVITE_PENDING)? else {
            return Ok(None);
        };
        decode_pending(&blob).map(Some)
    }

    /// 检查创建者是否已批准（`PENDING` → `ACTIVE`）。
    ///
    /// 用 pending 的**签名私钥**走设备认证：relay 对 pending/revoked 都回 `REVOKED`
    /// （不区分，避免枚举），因此 `WaitingForApproval` 也包含"已被移除"；
    /// 认证回执的 vault/instance 必须与 pending 一致（**不自动换 pin**）。
    pub fn check(
        context: &RelayContext,
        vault: &Vault,
        now: u64,
    ) -> Result<InviteCheck, SyncError> {
        let (pending, secrets) = load_with_secrets(vault)?;
        verify_context(context, &pending)?;
        match authenticate(context, &pending, &secrets, now) {
            Ok(_) => {
                advance_phase(vault, &pending, &secrets, InviteJoinPhase::Approved)?;
                Ok(InviteCheck::Approved)
            }
            Err(error) if is_revoked(&error) => Ok(InviteCheck::WaitingForApproval),
            Err(error) => Err(error),
        }
    }

    /// 批准后 bootstrap：解 grant 得 root → 认证 bootstrap → 提交 control pin → 造身份。
    ///
    /// 失败保留 pending（阶段最多到 `Approved`），可重试；**不写身份文件/公开配置**
    /// （那是 app 的责任，`finish` 只返回材料）。
    pub fn finish(
        context: &RelayContext,
        vault: &Vault,
        now: u64,
    ) -> Result<InviteFinish, SyncError> {
        let (pending, secrets) = load_with_secrets(vault)?;
        verify_context(context, &pending)?;
        if pending.phase == InviteJoinPhase::Bootstrapped {
            return Err(SyncError::InviteLink(
                "该加入已经完成 bootstrap：请写入配置（或重启后再打开实例）".to_owned(),
            ));
        }
        let auth = match authenticate(context, &pending, &secrets, now) {
            Ok(auth) => auth,
            Err(error) if is_revoked(&error) => return Ok(InviteFinish::WaitingForApproval),
            Err(error) => return Err(error),
        };
        // 过期只影响**兑换**（relay 判定）；兑换已成功时批准后的设备认证不受本地过期影响。
        advance_phase(vault, &pending, &secrets, InviteJoinPhase::Approved)?;

        let hpke_key = DeviceHpkeKey::from_secret(secrets.hpke_secret.clone());
        // 1) 取 self grant 并解出 root（结构/收件人/epoch/管理签名逐项校验）。
        let (root, manager_pub) = fetch_and_open_grant(context, &pending, &hpke_key, &auth)?;
        // 2) 认证 bootstrap：pin 用链接里的 relay instance（换实例即拒绝）。
        let signing_seed = secrets.signing_seed.clone();
        let (report, state) = run_invite_bootstrap(
            context,
            vault,
            &pending,
            signing_seed,
            root.clone(),
            manager_pub,
        )?;
        if report.covered_seq > state.head_seq {
            return Err(SyncError::RelayWire("bootstrap 覆盖水位高于 relay head"));
        }
        // 3) 造身份（普通设备没有管理私钥）。
        let identity = LocalIdentityV1 {
            vault_id: pending.vault_id,
            device_id: pending.device_id,
            peer_id: crate::ids::PeerId::from_bytes(peer_bytes_of(pending.peer_id)),
            ed25519_seed: secrets.signing_seed.clone(),
            x25519_secret: secrets.hpke_secret.clone(),
            current_epoch: root.key_epoch,
            current_root: root.root.clone(),
            manager_seed: None,
            pinned_instance: pending.pin,
            pinned_manager: manager_pub,
            control_rev: state.control_rev,
            pending_rotation: None,
        };
        let bootstrapped = InvitePending {
            phase: InviteJoinPhase::Bootstrapped,
            ..pending.clone()
        };
        save_pending(vault, &bootstrapped, &secrets)?;
        Ok(InviteFinish::Joined(Box::new(InviteJoined {
            identity,
            report,
            pending: bootstrapped,
        })))
    }

    /// 清掉 durable pending（app 写完身份与公开配置之后调用）。
    ///
    /// 只允许在 `Bootstrapped` 阶段清理（其余阶段清理等于丢掉可对账的加入事实）。
    pub fn complete(vault: &Vault) -> Result<(), SyncError> {
        let Some(pending) = Self::load(vault)? else {
            return Ok(());
        };
        if pending.phase != InviteJoinPhase::Bootstrapped {
            return Err(SyncError::InviteLink(format!(
                "加入尚未完成（阶段 {}）：不得清理 pending",
                pending.phase.as_str()
            )));
        }
        let affected = vault.exec(
            "DELETE FROM meta WHERE key = ?1",
            &[Value::Text(META_INVITE_PENDING.to_owned())],
        )?;
        if affected != 1 {
            return Err(SyncError::InviteLink(
                "清理邀请 pending 失败（记录不存在）".to_owned(),
            ));
        }
        Ok(())
    }
}

/// 邀请/批准路径的公共：解析 hex 的 pending 指纹（UI 展示用）。
///
/// 公式与 relay `pairing::fingerprint` 完全一致：
/// `SHA256(vault_id || device_id || sign_pk || hpke_pk)` 前 128 bit（32 位 hex）。
#[must_use]
pub fn invite_device_fingerprint(
    vault_id: &VaultId,
    device_id: &DeviceId,
    sign_pk: &[u8; 32],
    hpke_pk: &[u8; 32],
) -> String {
    device_fingerprint(vault_id, device_id, sign_pk, hpke_pk).to_hex()
}

/// 公钥的短 hash（十六进制前 16 位 = SHA-256 前 8 字节）：UI 双端比对用。
#[must_use]
pub fn key_short_hash(public_key: &[u8; 32]) -> String {
    let digest: [u8; 32] = Sha256::digest(public_key).into();
    crate::ids::hex_of(&digest[..8])
}

// ---------------------------------------------------------------------------
// 创建端：批准 PENDING 设备
// ---------------------------------------------------------------------------

/// 用**现有管理钥**批准一台 PENDING 设备（ADR 0014 S1 §3）。
///
/// 顺序：本地守卫（管理钥、pin）→ 已验证成员视图找目标 + 其 `sign_pk`/`hpke_pk` →
/// `/state` 的 control_rev/instance → 管理 realm token → §17.1 证书 + §17.2 grant 的
/// 管理签名（复用 [`crate::recovery_enroll`]）→ 逐字段校验响应。
///
/// 幂等：目标已是 `ACTIVE` 时返回 `changed = false`，**不重复签名、不追加控制记录**。
pub fn approve_pending_device(
    context: &RelayContext,
    vault: &Vault,
    identity: &LocalIdentityV1,
    target: DeviceId,
    now: u64,
) -> Result<ApprovedDevice, SyncError> {
    let Some(seed) = identity.manager_seed.clone() else {
        return Err(SyncError::Unsupported(
            "本机没有管理密钥（Join/邀请加入的设备）：不能批准设备；请用 Create 设备或最新恢复码",
        ));
    };
    let manager = ManagementKey::from_seed(seed);
    if manager.public_key() != identity.pinned_manager {
        return Err(SyncError::LocalCorrupt(
            "身份的管理私钥与 pinned_manager 不一致（身份文件被换过或损坏）",
        ));
    }
    crate::init_journal::ensure_no_pending_sync(vault, identity.vault_id)?;

    // 1) 已验证成员视图（不信任 relay 的 status 字面量做授权；pending 一定不是 active）。
    let mut pinned = context.open(vault, identity)?;
    let members = pinned.client_mut().members()?;
    let state = pinned.client_mut().state()?;
    if state.instance_id != identity.pinned_instance {
        return Err(SyncError::InstanceMismatch);
    }
    let Some(member) = members.iter().find(|member| member.device_id == target) else {
        return Err(SyncError::InviteLink(format!(
            "设备 {} 不在成员列表里（邀请可能未兑换/已过期）",
            crate::ids::hex_of(&target)
        )));
    };
    match member.status {
        crate::http_relay::MemberStatus::Pending => {}
        crate::http_relay::MemberStatus::Active => {
            return Ok(ApprovedDevice {
                device_id: target,
                fingerprint: invite_device_fingerprint(
                    &identity.vault_id,
                    &target,
                    &member.sign_pk,
                    &member.hpke_pk,
                ),
                epoch: state.epoch,
                control_rev: state.control_rev,
                changed: false,
            });
        }
        crate::http_relay::MemberStatus::Revoked => {
            return Err(SyncError::InviteLink(format!(
                "设备 {} 已被移除：已吊销设备不能重新批准，请重新生成邀请",
                crate::ids::hex_of(&target)
            )));
        }
        crate::http_relay::MemberStatus::Expired => {
            return Err(SyncError::InviteLink(format!(
                "设备 {} 的邀请已过期：请重新生成邀请",
                crate::ids::hex_of(&target)
            )));
        }
    }
    let sign_pk = member.sign_pk;
    let hpke_pk = member.hpke_pk;

    // 2) 管理 rev：与轮换/apply 的口径一致（旁路状态 → genesis）。
    let manager_rev = crate::dev_local_rotation::local_manager_rev(vault, identity)?;
    // 3) 管理 realm token（instance 必须与本地 pin 一致）。
    let api = context.api();
    let mut transport = context.transport()?;
    let (token, instance) = crate::recovery_enroll::management_token(
        &mut transport,
        &api,
        identity.vault_id,
        &manager,
        now,
    )?;
    if instance != identity.pinned_instance {
        return Err(SyncError::InstanceMismatch);
    }
    let root = crate::dev_local::root_of(identity);
    crate::recovery_enroll::approve_device(
        &mut transport,
        &api,
        identity.vault_id,
        &manager,
        &token,
        state.control_rev,
        identity.pinned_instance,
        target,
        sign_pk,
        hpke_pk,
        manager_rev,
        &root,
    )?;
    Ok(ApprovedDevice {
        device_id: target,
        fingerprint: invite_device_fingerprint(&identity.vault_id, &target, &sign_pk, &hpke_pk),
        epoch: state.epoch,
        // approve 会追加控制记录：rev 前进 1（响应里也回同一事实；以本地 +1 为准会重复
        // 请求，所以这里返回已知的前进值，UI 只用于展示）。
        control_rev: state.control_rev.saturating_add(1),
        changed: true,
    })
}

// ---------------------------------------------------------------------------
// 内部：pending 编解码
// ---------------------------------------------------------------------------

/// pending 里的秘密材料（签名/HPKE 私钥种子；不实现 Debug）。
struct PendingSecrets {
    signing_seed: Secret32,
    hpke_secret: Secret32,
}

fn save_pending(
    vault: &Vault,
    pending: &InvitePending,
    secrets: &PendingSecrets,
) -> Result<(), SyncError> {
    let plaintext = Zeroizing::new(encode_pending(pending, secrets)?);
    let entropy = dpapi::entropy_for(&pending.vault_id, &pending.device_id);
    let protected = identity_wrap::protect(&plaintext, &entropy)?;
    let mut blob = Vec::with_capacity(PENDING_HEADER_LEN + protected.len());
    blob.extend_from_slice(&PENDING_MAGIC);
    blob.push(PENDING_OUTER_VERSION);
    blob.extend_from_slice(&pending.vault_id);
    blob.extend_from_slice(&pending.device_id);
    let len = u32::try_from(protected.len())
        .map_err(|_| SyncError::InviteLink("pending 材料过长".to_owned()))?;
    blob.extend_from_slice(&len.to_be_bytes());
    blob.extend_from_slice(&protected);
    vault.set_meta(META_INVITE_PENDING, &blob)?;
    Ok(())
}

fn encode_pending(pending: &InvitePending, secrets: &PendingSecrets) -> Result<Vec<u8>, SyncError> {
    let mode = match pending.mode {
        RelayMode::LocalDev => 1,
        RelayMode::RemoteTls => 2,
    };
    cbor::encode(vec![
        (0, cbor::uint(PENDING_VERSION)),
        (1, cbor::bytes(pending.vault_id.to_vec())),
        (2, cbor::bytes(pending.device_id.to_vec())),
        (3, cbor::bytes(peer_bytes_of(pending.peer_id).to_vec())),
        (4, cbor::bytes(pending.invite_id.to_vec())),
        (5, cbor::bytes(secrets.signing_seed.expose_slice().to_vec())),
        (6, cbor::bytes(secrets.hpke_secret.expose_slice().to_vec())),
        (7, cbor::text(pending.relay.clone())),
        (8, cbor::uint(mode)),
        (9, cbor::bytes(pending.pin.to_vec())),
        (10, cbor::uint(pending.expires_at)),
        (11, cbor::uint(pending.created_at)),
        (12, cbor::uint(pending.phase.as_code())),
        (13, cbor::bytes(pending.sign_pk.to_vec())),
        (14, cbor::bytes(pending.hpke_pk.to_vec())),
        // ADR 0014 S3b：链接/加入的 API 面。旧记录没有该键 → 按 v1 解读（显式兼容）。
        (15, cbor::uint(pending.api_version.as_number().into())),
    ])
}

fn decode_pending(blob: &[u8]) -> Result<InvitePending, SyncError> {
    decode_pending_with_secrets(blob).map(|(pending, _)| pending)
}

fn decode_pending_with_secrets(blob: &[u8]) -> Result<(InvitePending, PendingSecrets), SyncError> {
    if blob.len() < PENDING_HEADER_LEN || blob[..4] != PENDING_MAGIC {
        return Err(SyncError::InviteLink(
            "邀请 pending 记录损坏（magic/长度）".to_owned(),
        ));
    }
    if blob[4] != PENDING_OUTER_VERSION {
        return Err(SyncError::InviteLink(format!(
            "邀请 pending 记录版本不支持：{}",
            blob[4]
        )));
    }
    let vault_id: [u8; 16] = blob[5..21]
        .try_into()
        .map_err(|_| SyncError::InviteLink("pending vault_id 长度非法".to_owned()))?;
    let device_id: [u8; 16] = blob[21..37]
        .try_into()
        .map_err(|_| SyncError::InviteLink("pending device_id 长度非法".to_owned()))?;
    let len = u32::from_be_bytes(
        blob[37..41]
            .try_into()
            .map_err(|_| SyncError::InviteLink("pending 长度非法".to_owned()))?,
    ) as usize;
    let protected = blob
        .get(PENDING_HEADER_LEN..)
        .ok_or_else(|| SyncError::InviteLink("pending 密文缺失".to_owned()))?;
    if protected.len() != len {
        return Err(SyncError::InviteLink(
            "pending 密文长度与头部不一致".to_owned(),
        ));
    }
    let plaintext =
        identity_wrap::unprotect(protected, &dpapi::entropy_for(&vault_id, &device_id))?;
    let fields = cbor::decode(&plaintext)?;
    let version = cbor::uint_field(&fields, 0)?;
    if version != PENDING_VERSION {
        return Err(SyncError::InviteLink(format!(
            "邀请 pending 内层版本不支持：{version}"
        )));
    }
    let pending_vault: [u8; 16] = cbor::bytes_fixed(&fields, 1)?;
    let pending_device: [u8; 16] = cbor::bytes_fixed(&fields, 2)?;
    if pending_vault != vault_id || pending_device != device_id {
        return Err(SyncError::InviteLink(
            "邀请 pending 头部与密文绑定的 ids 不一致".to_owned(),
        ));
    }
    let peer_bytes: [u8; 16] = cbor::bytes_fixed(&fields, 3)?;
    let mut head = [0u8; 8];
    head.copy_from_slice(&peer_bytes[..8]);
    let peer_id = u64::from_be_bytes(head) & 0x7fff_ffff_ffff_ffff;
    if peer_bytes_of(peer_id) != peer_bytes {
        return Err(SyncError::InviteLink(
            "邀请 pending peer 映射不稳定".to_owned(),
        ));
    }
    let signing_seed = cbor::bytes_fixed::<32>(&fields, 5)?;
    let hpke_secret = cbor::bytes_fixed::<32>(&fields, 6)?;
    let relay = cbor::text_field(&fields, 7)?;
    let mode_code = cbor::uint_field(&fields, 8)?;
    let mode = match mode_code {
        1 => RelayMode::LocalDev,
        2 => RelayMode::RemoteTls,
        _ => {
            return Err(SyncError::InviteLink(
                "邀请 pending 的 relay 模式未知".to_owned(),
            ))
        }
    };
    let pin: [u8; 16] = cbor::bytes_fixed(&fields, 9)?;
    let sign_pk: [u8; 32] = cbor::bytes_fixed(&fields, 13)?;
    let hpke_pk: [u8; 32] = cbor::bytes_fixed(&fields, 14)?;
    let expires_at = cbor::uint_field(&fields, 10)?;
    let created_at = cbor::uint_field(&fields, 11)?;
    let phase = InviteJoinPhase::from_code(cbor::uint_field(&fields, 12)?)
        .ok_or_else(|| SyncError::InviteLink("邀请 pending 阶段未知（拒绝猜测）".to_owned()))?;
    // ADR 0014 S3b：缺失（旧 pending）按 v1；未知数字明确拒绝（不静默降级）。
    let api_version = match cbor::opt_uint_field(&fields, 15)? {
        None => RelayApiVersion::V1,
        Some(value) => u32::try_from(value)
            .ok()
            .and_then(RelayApiVersion::from_number)
            .ok_or_else(|| {
                SyncError::InviteLink("邀请 pending 的 API 面未知（只支持 1/2）".to_owned())
            })?,
    };
    Ok((
        InvitePending {
            vault_id,
            device_id,
            peer_id,
            invite_id: cbor::bytes_fixed(&fields, 4)?,
            relay,
            mode,
            api_version,
            pin,
            sign_pk,
            hpke_pk,
            expires_at,
            created_at,
            phase,
        },
        PendingSecrets {
            signing_seed: Secret32::from_array(signing_seed),
            hpke_secret: Secret32::from_array(hpke_secret),
        },
    ))
}

fn load_with_secrets(vault: &Vault) -> Result<(InvitePending, PendingSecrets), SyncError> {
    let blob = vault.get_meta(META_INVITE_PENDING)?.ok_or_else(|| {
        SyncError::InviteLink("缺少邀请 pending（没有等待批准的加入；请先粘贴邀请链接）".to_owned())
    })?;
    let (pending, secrets) = decode_pending_with_secrets(&blob)?;
    // 防御性一致校验：公钥必须真的来自这份私钥（pending 被换过/损坏 → 拒绝）。
    if DeviceSigningKey::from_seed(secrets.signing_seed.clone()).public_key() != pending.sign_pk
        || DeviceHpkeKey::from_secret(secrets.hpke_secret.clone()).public_key() != pending.hpke_pk
    {
        return Err(SyncError::InviteLink(
            "邀请 pending 的公钥与私钥材料不一致（记录被篡改）".to_owned(),
        ));
    }
    Ok((pending, secrets))
}

fn advance_phase(
    vault: &Vault,
    pending: &InvitePending,
    secrets: &PendingSecrets,
    next: InviteJoinPhase,
) -> Result<(), SyncError> {
    if pending.phase >= next {
        return Ok(());
    }
    let updated = InvitePending {
        phase: next,
        ..pending.clone()
    };
    save_pending(vault, &updated, secrets)
}

// ---------------------------------------------------------------------------
// 内部：认证 / grant / bootstrap
// ---------------------------------------------------------------------------

fn verify_context(context: &RelayContext, pending: &InvitePending) -> Result<(), SyncError> {
    if context.mode != pending.mode || context.origin_str() != pending.relay {
        return Err(SyncError::ConfigInvalid(
            "relay context 与邀请 pending 的 relay/模式不一致".to_owned(),
        ));
    }
    if context.api_version() != pending.api_version {
        return Err(SyncError::ConfigInvalid(
            "relay context 的 API 面与邀请 pending 不一致（v1/v2 不能混用）".to_owned(),
        ));
    }
    Ok(())
}

fn is_revoked(error: &SyncError) -> bool {
    matches!(
        error,
        SyncError::RelayApi { code, .. } if code == "REVOKED"
    )
}

/// 设备认证（pending 的签名钥）；校验 vault 与 **pin 的 relay instance**。
fn authenticate(
    context: &RelayContext,
    pending: &InvitePending,
    secrets: &PendingSecrets,
    now: u64,
) -> Result<crate::recovery_enroll::DeviceAuth, SyncError> {
    let signing_key = DeviceSigningKey::from_seed(secrets.signing_seed.clone());
    let api = context.api();
    let mut transport = context.transport()?;
    let auth = crate::recovery_enroll::device_auth(
        &mut transport,
        &api,
        pending.vault_id,
        pending.device_id,
        &signing_key,
        now,
    )?;
    if auth.vault_id != pending.vault_id {
        return Err(SyncError::RelayWire("设备认证的 vault 与邀请不一致"));
    }
    if auth.instance_id != pending.pin {
        // ADR 0014 §B：不自动换 pin（换实例 = 拒绝，交给用户重新核对链接）。
        return Err(SyncError::InstanceMismatch);
    }
    Ok(auth)
}

/// `/state` 的只读视图（management 或 device bearer 均可）。
struct StateView {
    vault_id: [u8; 16],
    instance_id: [u8; 16],
    control_rev: u64,
}

fn state_view<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    token: &str,
) -> Result<StateView, SyncError> {
    let response = crate::recovery_enroll::send(
        transport,
        "GET",
        api.url(&api.vault_path(&vault_id, "/state")),
        Some(token),
        Vec::new(),
    )?;
    let fields = crate::recovery_enroll::decode(&response)?;
    let view = StateView {
        vault_id: cbor::bytes_fixed(&fields, 0)?,
        instance_id: cbor::bytes_fixed(&fields, 2)?,
        control_rev: cbor::uint_field(&fields, 5)?,
    };
    if view.vault_id != vault_id {
        return Err(SyncError::RelayWire("relay /state 的 vault 与请求不一致"));
    }
    Ok(view)
}

/// 取 self grant、逐项校验并解出 root；同时给出 grant 对应的管理公钥。
fn fetch_and_open_grant(
    context: &RelayContext,
    pending: &InvitePending,
    hpke_key: &DeviceHpkeKey,
    auth: &crate::recovery_enroll::DeviceAuth,
) -> Result<(RootSecretV1, [u8; 32]), SyncError> {
    let api = context.api();
    let mut transport = context.transport()?;
    let response = crate::recovery_enroll::send(
        &mut transport,
        "GET",
        api.url(&api.vault_path(&pending.vault_id, "/devices/self/grants/latest")),
        Some(&auth.token),
        Vec::new(),
    )?;
    let fields = crate::recovery_enroll::decode(&response)?;
    let epoch = cbor::uint_field(&fields, 0)?;
    let grant_device: [u8; 16] = cbor::bytes_fixed(&fields, 1)?;
    let manager_rev = cbor::uint_field(&fields, 2)?;
    let enc: [u8; 32] = cbor::bytes_fixed(&fields, 3)?;
    let ciphertext = cbor::bytes_field(&fields, 4)?;
    let signed_header = cbor::bytes_field(&fields, 5)?;
    let signature = cbor::opt_bytes_field(&fields, 6)?.unwrap_or_default();
    if grant_device != pending.device_id {
        return Err(SyncError::RelayWire("self grant 的收件设备不符"));
    }
    if signature.len() != 64 {
        return Err(SyncError::RelayWire("self grant 缺管理签名"));
    }
    if signed_header.len() < 64 {
        return Err(SyncError::RelayWire("self grant signed_header 长度非法"));
    }
    let aad_len = signed_header.len() - 64;
    let aad = GrantAadV1::decode(&signed_header[..aad_len])?;
    let signed_enc: [u8; 32] = signed_header[aad_len..aad_len + 32]
        .try_into()
        .map_err(|_| SyncError::RelayWire("self grant enc 长度非法"))?;
    let signed_hash: [u8; 32] = signed_header[aad_len + 32..]
        .try_into()
        .map_err(|_| SyncError::RelayWire("self grant 密文摘要长度非法"))?;
    let ct_hash: [u8; 32] = Sha256::digest(&ciphertext).into();
    if signed_enc != enc
        || signed_hash != ct_hash
        || aad.pv != 1
        || aad.vault_id != pending.vault_id
        || aad.epoch != epoch
        || aad.recipient_device_id != pending.device_id
        || aad.recipient_x25519_pub != hpke_key.public_key()
        || aad.manager_rev != manager_rev
    {
        return Err(SyncError::RelayWire(
            "self grant 与邀请/本地设备不一致（拒绝解封）",
        ));
    }
    // 管理链：取 `manager_rev` 对应的公钥验证 grant 外层签名。
    let chain = management_chain(&mut transport, &api, &pending.vault_id, &auth.token)?;
    let manager_pub = chain
        .iter()
        .find(|entry| entry.0 == manager_rev)
        .map(|entry| entry.1)
        .ok_or(SyncError::RelayWire("管理链缺少 grant 的 manager_rev"))?;
    let signed_input = aad.grant_signed_bytes(&enc, &ct_hash)?;
    let mut signature64 = [0u8; 64];
    signature64.copy_from_slice(&signature[..64]);
    crate::device::verify_signature(&manager_pub, &signed_input, &signature64)?;
    // 解封 root。
    let root = open_epoch_grant(hpke_key.secret(), &enc, &aad, &ciphertext)?;
    if root.vault_id != pending.vault_id {
        return Err(SyncError::Object("grant 解出的 root 与邀请 vault 不符"));
    }
    Ok((root, manager_pub))
}

/// 管理链 `(rev, pubkey)`（按 rev 升序；relay 的 `/control` 键 5）。
fn management_chain<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: &[u8; 16],
    token: &str,
) -> Result<Vec<(u64, [u8; 32])>, SyncError> {
    let response = crate::recovery_enroll::send(
        transport,
        "GET",
        api.url(&api.vault_path(vault_id, "/control?after_rev=0")),
        Some(token),
        Vec::new(),
    )?;
    let fields = crate::recovery_enroll::decode(&response)?;
    let mut chain = Vec::new();
    for entry in cbor::array_field(&fields, 5)? {
        let fields = cbor::value_map(&entry)?;
        chain.push((
            cbor::uint_field(&fields, 0)?,
            cbor::bytes_fixed::<32>(&fields, 1)?,
        ));
    }
    Ok(chain)
}

/// 认证 bootstrap（pin = 链接里的 instance），提交 durable control pin。
fn run_invite_bootstrap(
    context: &RelayContext,
    vault: &Vault,
    pending: &InvitePending,
    signing_seed: Secret32,
    root: RootSecretV1,
    manager_pub: [u8; 32],
) -> Result<(BootstrapReport, crate::relay::RelayState), SyncError> {
    let signing_key = DeviceSigningKey::from_seed(signing_seed);
    let transport: StdTcpTransport = context.transport()?;
    let config = HttpRelayConfig {
        base_url: context.origin_str(),
        api_version: context.api_version(),
        vault_id: pending.vault_id,
        device_id: pending.device_id,
        manager_pub: Some(manager_pub),
    };
    let mut client =
        HttpRelayClient::new(transport, config, signing_key).with_unpinned_bootstrap_control();
    let pins = BootstrapPins {
        instance_id: Some(pending.pin),
        ..BootstrapPins::default()
    };
    let report = bootstrap::run_with_pins(
        vault,
        &mut client,
        pending.vault_id,
        pending.peer_id,
        &root,
        &pins,
    )?;
    // 用同一验证视图提交 record pin（管理链自洽校验；无既有 pin 时写入）。
    let head = client.verified_head()?;
    commit_verified_head(vault, None, head)?;
    let state = client.state()?;
    Ok((report, state))
}

// ---------------------------------------------------------------------------
// 内部：URL 编解码
// ---------------------------------------------------------------------------

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn percent_decode(value: &str) -> Result<String, InviteLinkError> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let high = *bytes.get(index + 1).ok_or(InviteLinkError::Percent)?;
                let low = *bytes.get(index + 2).ok_or(InviteLinkError::Percent)?;
                let high = hex_value(high).ok_or(InviteLinkError::Percent)?;
                let low = hex_value(low).ok_or(InviteLinkError::Percent)?;
                out.push(high << 4 | low);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| InviteLinkError::Percent)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_hex32(text: &str) -> Result<[u8; 16], InviteLinkError> {
    if text.len() != 32 {
        return Err(InviteLinkError::Hex("vault/iid/pin"));
    }
    let mut out = [0u8; 16];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let high = hex_value(chunk[0]).ok_or(InviteLinkError::Hex("vault/iid/pin"))?;
        let low = hex_value(chunk[1]).ok_or(InviteLinkError::Hex("vault/iid/pin"))?;
        out[index] = high << 4 | low;
    }
    Ok(out)
}

struct QueryFields<'a> {
    version: Option<&'a str>,
    relay: Option<&'a str>,
    vault: Option<&'a str>,
    iid: Option<&'a str>,
    pin: Option<&'a str>,
    api: Option<&'a str>,
}

fn parse_query(query: &str) -> Result<QueryFields<'_>, InviteLinkError> {
    let mut fields = QueryFields {
        version: None,
        relay: None,
        vault: None,
        iid: None,
        pin: None,
        api: None,
    };
    for pair in query.split('&') {
        if pair.is_empty() {
            return Err(InviteLinkError::Query);
        }
        let (key, value) = pair.split_once('=').ok_or(InviteLinkError::Query)?;
        if value.is_empty() {
            return Err(InviteLinkError::Missing("query 字段值"));
        }
        let slot = match key {
            "v" => &mut fields.version,
            "relay" => &mut fields.relay,
            "vault" => &mut fields.vault,
            "iid" => &mut fields.iid,
            "pin" => &mut fields.pin,
            // ADR 0014 S3b：`api` 是可选的面（缺省 v1）；未知名称仍拒绝。
            "api" => &mut fields.api,
            // 未知字段一律拒绝（拒绝恶意跳转/夹带）；文案只回显一个固定占位。
            _other => return Err(InviteLinkError::Unknown("未知字段")),
        };
        if slot.is_some() {
            return Err(InviteLinkError::Duplicate("query 字段"));
        }
        *slot = Some(value);
    }
    Ok(fields)
}

/// 解析 `api=` 字段：只接受已知的面（`1`/`2` 或 `v1`/`v2`），未知值明确拒绝。
fn parse_api_version(text: &str) -> Result<RelayApiVersion, InviteLinkError> {
    RelayApiVersion::parse(text).ok_or(InviteLinkError::ApiVersion)
}

fn parse_fragment(fragment: &str) -> Result<[u8; 32], InviteLinkError> {
    if fragment.contains('#') {
        return Err(InviteLinkError::Fragment);
    }
    let (key, value) = fragment.split_once('=').ok_or(InviteLinkError::Token)?;
    if key != "token" {
        return Err(InviteLinkError::Token);
    }
    let decoded = percent_decode(value)?;
    let bytes = crate::base64url::decode(&decoded).map_err(|_| InviteLinkError::Token)?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| InviteLinkError::Token)
}

fn peer_bytes_of(peer_id: u64) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&peer_id.to_be_bytes());
    out
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceHpkeKey;

    const RELAY: &str = "http://127.0.0.1:18080";
    const VAULT: [u8; 16] = [0x5a; 16];
    const IID: [u8; 16] = [0x11; 16];
    const PIN: [u8; 16] = [0x22; 16];

    fn token() -> [u8; 32] {
        crate::random::array(&mut OsRandom).expect("token")
    }

    #[test]
    fn url_roundtrips_and_token_stays_in_fragment() {
        let token = token();
        let url =
            build_invite_url(RELAY, &VAULT, &IID, &PIN, &token, RelayApiVersion::V1).expect("url");
        assert!(!url.contains("api="), "v1 链接不带 api 字段（逐字节兼容）");
        let (head, fragment) = url.split_once('#').expect("fragment");
        assert!(
            !head.contains(&crate::base64url::encode(&token)),
            "token 不能出现在 fragment 之前"
        );
        assert_eq!(
            fragment,
            format!("token={}", crate::base64url::encode(&token))
        );
        // relay 被 percent-encoded（`://` 不是安全字符）。
        assert!(
            head.contains("relay=http%3A%2F%2F127.0.0.1%3A18080"),
            "{head}"
        );
        let target = InviteLinkTarget::parse(&url).expect("parse");
        assert_eq!(target.mode(), RelayMode::LocalDev);
        assert_eq!(target.relay(), "http://127.0.0.1:18080");
        assert_eq!(target.vault_id(), VAULT);
        assert_eq!(target.invite_id(), IID);
        assert_eq!(target.pin(), PIN);
        assert_eq!(target.token(), &token);
    }

    /// ADR 0014 S3b：v2 vault 的邀请链接必须带 `api=2`；缺省/`api=1` 都是 v1。
    #[test]
    fn api_field_marks_v2_links_and_defaults_to_v1() {
        let token = token();
        let v2 = build_invite_url(RELAY, &VAULT, &IID, &PIN, &token, RelayApiVersion::V2)
            .expect("v2 url");
        assert!(v2.contains("&api=2#token="), "{v2}");
        let target = InviteLinkTarget::parse(&v2).expect("parse v2");
        assert_eq!(target.api_version(), RelayApiVersion::V2);
        // 规范化重建仍是同一串（v2 字段位置固定）。
        let rebuilt = build_invite_url(RELAY, &VAULT, &IID, &PIN, &token, target.api_version())
            .expect("rebuild");
        assert_eq!(rebuilt, v2);

        // 缺省 → v1；显式 api=1 也是 v1。
        let v1 = build_invite_url(RELAY, &VAULT, &IID, &PIN, &token, RelayApiVersion::V1)
            .expect("v1 url");
        assert_eq!(
            InviteLinkTarget::parse(&v1).expect("parse").api_version(),
            RelayApiVersion::V1
        );
        let explicit_v1 = v1.replace("#token=", "&api=1#token=");
        assert_eq!(
            InviteLinkTarget::parse(&explicit_v1)
                .expect("parse api=1")
                .api_version(),
            RelayApiVersion::V1
        );

        // 未知 api 明确拒绝（不是"猜一个面"），且不回显 token。
        let encoded = crate::base64url::encode(&token);
        for bad in ["3", "v3", "", "two"] {
            let broken = format!(
                "{}&api={bad}#token={encoded}",
                &v2[..v2.find("&api=").expect("api")]
            );
            let error = InviteLinkTarget::parse(&broken).expect_err("未知 api 必须拒绝");
            assert!(!error.to_string().contains(&encoded), "{error}");
        }
    }

    #[test]
    fn remote_links_parse_only_https_and_canonicalize() {
        let token = token();
        let url = build_invite_url(
            "https://Relay.Example:443/",
            &VAULT,
            &IID,
            &PIN,
            &token,
            RelayApiVersion::V1,
        )
        .expect("url");
        let target = InviteLinkTarget::parse(&url).expect("parse");
        assert_eq!(target.mode(), RelayMode::RemoteTls);
        assert_eq!(target.relay(), "https://relay.example");
        // 明文 loopback 只能 local_dev；https 进 remote_tls。
        let (head, fragment) = url.split_once('#').expect("fragment");
        let swapped = head.replace(
            "https%3A%2F%2FRelay.Example%3A443",
            "http%3A%2F%2F127.0.0.1%3A18080",
        );
        let swapped = format!("{swapped}#{fragment}");
        assert_eq!(
            InviteLinkTarget::parse(&swapped).expect("local").mode(),
            RelayMode::LocalDev,
            "http loopback 必须按 local_dev 解析"
        );
    }

    #[test]
    fn rejects_malicious_shapes_without_echoing_token() {
        let token = crate::base64url::encode(&token());
        let base = format!(
            "mn-pair://join?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}&iid={}&pin={}",
            crate::ids::hex_of(&VAULT),
            crate::ids::hex_of(&IID),
            crate::ids::hex_of(&PIN)
        );
        for (label, value) in [
            ("空", String::new()),
            ("scheme", format!("https://join?v=1#token={token}")),
            ("host", format!("mn-pair://evil?v=1&relay=x#token={token}")),
            ("path", format!("mn-pair://join/x?v=1&relay=x#token={token}")),
            ("userinfo", format!("mn-pair://user:pass@join?v=1&relay=x#token={token}")),
            ("unknown", format!("mn-pair://join?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&next=https%3A%2F%2Fevil&vault={}&iid={}&pin={}#token={token}",
                crate::ids::hex_of(&VAULT), crate::ids::hex_of(&IID), crate::ids::hex_of(&PIN))),
            ("version", format!("{base}&v=2#token={token}")),
            ("dup", format!("mn-pair://join?v=1&v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}&iid={}&pin={}#token={token}",
                crate::ids::hex_of(&VAULT), crate::ids::hex_of(&IID), crate::ids::hex_of(&PIN))),
            ("hex", format!("{base}&vault=zz#token={token}")),
            ("token", format!("{base}#token=short")),
            ("fragment", format!("{base}#token={token}#extra")),
        ] {
            let error = InviteLinkTarget::parse(&value).err().unwrap_or_else(|| panic!("{label} 必须拒绝"));
            let text = error.to_string();
            assert!(!text.contains(&token), "{label} 错误文案泄露了 token：{text}");
            assert!(!text.contains("//join?"), "{label} 错误文案回显了链接：{text}");
        }
    }

    #[test]
    fn percent_encoding_roundtrips_canonical_origins() {
        for origin in [
            "http://127.0.0.1:18080",
            "https://relay.example",
            "https://[::1]:8443",
        ] {
            let encoded = percent_encode(origin);
            assert!(!encoded.contains("://"), "{encoded}");
            assert_eq!(percent_decode(&encoded).expect("decode"), origin);
        }
        // 非法百分号编码必须拒绝（不静默当字面量）。
        assert!(percent_decode("abc%zz").is_err());
        assert!(percent_decode("abc%2").is_err());
    }

    #[test]
    fn debug_never_prints_the_token() {
        let token = token();
        let url =
            build_invite_url(RELAY, &VAULT, &IID, &PIN, &token, RelayApiVersion::V1).expect("url");
        let target = InviteLinkTarget::parse(&url).expect("parse");
        let text = format!("{target:?}");
        assert!(!text.contains(&crate::base64url::encode(&token)), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        let link = InviteLink {
            url,
            invite_id: IID,
            expires_at: 1234,
        };
        let text = format!("{link:?}");
        assert!(!text.contains("mn-pair"), "{text}");
        assert!(text.contains("<redacted: contains token>"), "{text}");
        assert!(text.contains("1234"));
    }

    #[test]
    fn key_short_hash_and_fingerprint_are_stable_and_distinct() {
        let sign = DeviceSigningKey::generate(&mut OsRandom).expect("sign");
        let hpke = DeviceHpkeKey::generate(&mut OsRandom).expect("hpke");
        let device: DeviceId = [0x33; 16];
        let fingerprint =
            invite_device_fingerprint(&VAULT, &device, &sign.public_key(), &hpke.public_key());
        assert_eq!(fingerprint.len(), 32);
        assert_eq!(
            fingerprint,
            device_fingerprint(&VAULT, &device, &sign.public_key(), &hpke.public_key()).to_hex()
        );
        let short = key_short_hash(&sign.public_key());
        assert_eq!(short.len(), 16);
        assert_ne!(short, key_short_hash(&hpke.public_key()));
    }

    #[test]
    fn ttl_text_is_public_and_zero_clamps() {
        let link = InviteLink {
            url: String::new(),
            invite_id: IID,
            expires_at: 1000,
        };
        assert!(!link.ttl_text(940).contains("mn-pair"));
        assert!(link.ttl_text(940).contains("1 分 0 秒"));
        assert!(link.ttl_text(2000).contains("已过期"));
        assert_eq!(link.short_invite_id().len(), 8);
    }

    #[test]
    fn ttl_bounds_are_enforced_before_any_network() {
        // 无管理密钥：明确失败（不联网、不建库）。
        crate::test_support::ensure_local_wrap();
        let identity = LocalIdentityV1::generate_dev(VAULT, [0x44; 16]).expect("dev");
        let base = std::env::var("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/tmp"));
        std::fs::create_dir_all(&base).expect("target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("invite-ttl-")
            .tempdir_in(&base)
            .expect("临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
        let context =
            RelayContext::local_dev(RELAY, crate::http_transport::DEFAULT_TRANSPORT_TIMEOUTS)
                .expect("context");
        assert!(matches!(
            create_invite_link_with(&context, &vault, &identity, 0, 1),
            Err(SyncError::InviteLink(_))
        ));
        assert!(matches!(
            create_invite_link_with(&context, &vault, &identity, MAX_INVITE_TTL_SECS + 1, 1),
            Err(SyncError::InviteLink(_))
        ));
        assert!(matches!(
            create_invite_link_with(&context, &vault, &identity, 60, 1),
            Err(SyncError::Unsupported(_))
        ));
    }
}
