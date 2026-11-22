//! 真实 relay 的 HTTP/CBOR 客户端（ADR 0006 §5；S7-9）。
//!
//! 控制面形状（与 `crates/relay/src/api.rs` 顶部 wire 约定一致）：
//!
//! - 所有控制面请求/响应 = `application/cbor` 的整数键 CDE map（`sync-proto::cde` 保证
//!   canonical：键升序、无重复、可选字段省略）；
//! - ID/hash/签名 = byte string；文本字段 = text；
//! - 签名包装 = `{0: Bytes(unsigned_body), 1: Bytes(signature)}`；
//! - 错误体 = `{0: code, 1: retryable, 2: current_epoch?, 3: current_control_rev?,
//!   4: bootstrap_hint?}`；
//! - 对象分片仍是 `application/octet-stream` 原样字节。
//!
//! 客户端职责：
//!
//! 1. challenge→token 认证（600s，提前 60s 刷新）；
//! 2. state/log/objects/updates/acks 全走 CBOR；
//! 3. **log 事件读签名**（事件 map 键 5），交给引擎按 ADR §2 验 `ObjectCommit`；
//! 4. `GET P/control?after_rev` 拉控制链 + 成员证书，**按 `control_rev` + `auth_rev`
//!    缓存/失效**（ADR 0010：独立吊销只推进 `auth_rev`、不动控制链，缓存必须跟着作废）；
//!    成员证书按 §17.1 用管理公钥验签；**active 集合只由已签控制链推导**
//!    （genesis/approve 加入事件 − begin/revoked-more 吊销），relay 的 `member.status`
//!    只做 fail-closed 否决、不能授权，见 [`parse_control`]。**消费 `/control` 必须带
//!    本地 [`ControlPin`]**（identity/`sync.pinned_*` 构造）；无 pin 只允许显式
//!    bootstrap/local-dev，否则 fail-stop（§35.4）。
//!
//! **传输层**：HTTPS 客户端在 [`crate::http_transport`]（rustls，校验证书链/主机名/有效期；
//! 明文 http 只允许显式开启的 loopback 开发，3xx 只跟随同 origin）。本文件只负责 wire
//! 语义，不碰 socket；生产部署 = relay 监听 `127.0.0.1` + 反向代理终止 TLS（见 relay 的
//! `serve --help`）。

use crate::base64url;
use crate::bootstrap::{control_head_hash, BootstrapPins};
use crate::cbor;
use crate::device::{verify_signature, DeviceSigningKey};
use crate::error::SyncError;
use crate::identity::LocalIdentityV1;
use crate::keys::RootSecretV1;
use crate::relay::{
    BootstrapBundle, BootstrapCheckpoint, BootstrapLease, CheckpointLease, CheckpointPublish,
    CheckpointRef, CheckpointStatus, EpochActivationLink, FrozenAssetPage, FrozenAssetRef,
    GrantLeafEvidence, ManagerChainEntry, ObjectSubmission, PublishedCheckpoint, RelayClient,
    RelayLogEvent, RelayLogKind, RelayLogPage, RelayState, RotationBegin, RotationView, SlotRecord,
    SlotUpload, UpdateOutcome, UpdateStatus, UpdateSubmission, VerifiedActivationT,
};
use crate::rotation::ManagerTransitionV1;
use crate::{DeviceId, ObjectId, VaultId};
use menghunote_sync_proto::{
    activation_t_signing_input, auth_signing_input, device_cert_signed_bytes,
    management_request_cde, management_signing_input, ActivationTV1, AuthRealm, Challenge,
    CheckpointCommit, ControlEvent, ControlKind, DeviceCertV1, EpochActivated, HttpMethod,
    ManifestV1, RotationAbortBodyV1, RotationBeginBodyV1, SignedControlRecord,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

// 传输层（含 TLS 门禁）已拆到 `http_transport`；这里重导出，保持既有 `http_relay::*` 路径可用。
pub use crate::http_transport::{
    HttpRequest, HttpResponse, HttpTransport, StdTcpTransport, TransportPolicy,
    ALLOW_PLAINTEXT_HTTP_ENV,
};

/// 控制面 Content-Type。
pub const CONTENT_TYPE_CBOR: &str = "application/cbor";
/// 对象分片 Content-Type。
pub const CONTENT_TYPE_OCTET: &str = "application/octet-stream";

/// 时间源（token 刷新用；测试可注入 FakeClock）。
pub trait Clock {
    /// 当前 Unix 秒。
    fn now_secs(&self) -> u64;
}

/// 系统时钟。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }
}

/// HTTP 客户端配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRelayConfig {
    /// relay 基址（`https://host[:port]`；本机开发可显式用 `http://127.0.0.1:port`）。
    ///
    /// 明文/证书/重定向策略由 [`StdTcpTransport`] 在构造时执行，本配置不绕过它们。
    pub base_url: String,
    /// vault 身份。
    pub vault_id: VaultId,
    /// 本机设备身份。
    pub device_id: DeviceId,
    /// 控制链信任锚：管理公钥（§17.1 验设备证书用）。
    ///
    /// genesis/bootstrapping 设备从恢复码派生后填入。**消费 `/control` 时缺失即
    /// fail-stop**（[`SyncError::Unsupported`]）：relay 的 rev1/管理链只能校验相等，
    /// 不能充当信任锚（ADR §35.2）。
    pub manager_pub: Option<[u8; 32]>,
}

#[derive(Debug, Clone)]
struct TokenState {
    token: String,
    expires_at: u64,
}

/// 控制链分页的安全上限：超过即 fail-stop，避免恶意 relay 用"每页 1 条 + 无限转发"
/// 拖死客户端（正常轮换历史远小于此值）。
const MAX_CONTROL_PAGES: usize = 4096;
/// 控制链最多接受的记录数（fail-stop，防分页内存放大）。
const MAX_CONTROL_COMMITS: u64 = 65_536;

/// 本地控制头 pin：`(relay instance, control rev, control hash)` 只允许前进或相等。
///
/// - `instance_id` 来自 `identity.pinned_instance` / `sync.pinned_instance`；
/// - `control_rev` 来自 `identity.control_rev` / `sync.pinned_control_rev`；
/// - `record_hash` 是 `/control` 链尾 `record_hash`（[`HttpRelayClient::control_head`]
///   的第二个返回值），用于识别"同 rev 替换 genesis/重写链"；
/// - `bootstrap_hash` 是 bootstrap 的 `control_head_hash(state, manager_chain)`
///   （`sync.pinned_control_hash`），公式不同，同一 rev 下单独比对。
///
/// 注意：两者都是 **同 rev 完整性**摘要；rev 前进时无法直接比较（rev 是摘要输入），
/// 因此前进场景只检查"旧 rev 的 record_hash 仍是新链的前缀"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlPin {
    /// 已 pin 的 relay instance id（换实例即 fail-stop）。
    pub instance_id: [u8; 16],
    /// 已认证的 control_rev（只允许前进或相等）。
    pub control_rev: u64,
    /// 已认证 rev 的链尾 `record_hash`（`control_head()` 第二个返回值）。
    pub record_hash: Option<[u8; 32]>,
    /// 已认证 rev 的 bootstrap 控制头摘要（`sync.pinned_control_hash`）。
    pub bootstrap_hash: Option<[u8; 32]>,
}

impl ControlPin {
    /// 只有 `(instance, rev)` 下界（identity 里没有 hash 时的形态）。
    #[must_use]
    pub fn new(instance_id: [u8; 16], control_rev: u64) -> Self {
        Self {
            instance_id,
            control_rev,
            record_hash: None,
            bootstrap_hash: None,
        }
    }

    /// 用本地身份构造 rev 下界 pin（identity 不含 control hash）。
    #[must_use]
    pub fn from_identity(identity: &LocalIdentityV1) -> Self {
        Self::new(identity.pinned_instance, identity.control_rev)
    }

    /// 用 bootstrap 持久化的 `sync.pinned_*` 构造；缺 instance/rev 时返回 `None`。
    #[must_use]
    pub fn from_bootstrap_pins(pins: &BootstrapPins) -> Option<Self> {
        Some(Self {
            instance_id: pins.instance_id?,
            control_rev: pins.control_rev?,
            record_hash: None,
            bootstrap_hash: pins.control_hash,
        })
    }

    /// 追加链尾 `record_hash`（同 rev 完整性）。
    #[must_use]
    pub fn with_record_hash(mut self, hash: [u8; 32]) -> Self {
        self.record_hash = Some(hash);
        self
    }

    /// 追加 bootstrap 控制头摘要（同 rev 完整性）。
    #[must_use]
    pub fn with_bootstrap_hash(mut self, hash: [u8; 32]) -> Self {
        self.bootstrap_hash = Some(hash);
        self
    }
}

/// 是否属于**显式本地开发**基址：`http://` + loopback。
///
/// 明文 loopback 本身已由 [`StdTcpTransport`] 的开发策略/环境变量显式放行，因此这里
/// 只把它当作"无 pin 消费 `/control`"的既有 dev 场景；生产 TLS 基址（`https://`）
/// 永远不会命中，必须显式注入 [`ControlPin`]。非 loopback 明文在传输层就会被拒绝。
fn is_local_dev_base_url(base_url: &str) -> bool {
    let Some(rest) = base_url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    // 去掉可选的 `:port`（IPv6 字面量本身含冒号，只有全数字端口才算）。
    let host = match authority.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            host
        }
        _ => authority,
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// 控制链成员的访问状态（ADR 0010 成员语义；**只读投影**，不是授权来源）。
///
/// 唯一的授权事实仍是 [`ControlState::active_certs`]（已签加入 − 已签吊销 − relay
/// `member.status` 否决）。本枚举只描述"该成员当前处于什么访问状态"，不参与任何
/// active 集合 / 成员摘要 / grant 规划。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemberStatus {
    /// 当前 active：与 `active_certs` **同谓词**（已签加入、未被已签吊销、relay status 未否决）。
    Active,
    /// 已签吊销（永久事实，优先于 relay status）、relay 标记 revoked，或无法证明 active
    /// 的成员（fail-closed：恶意 relay 把已签吊销证书 `status` 翻回 `active` 也不会显示 Active）。
    Revoked,
    /// relay 里尚未批准的**空证书占位**（`certificate` 为 X''），没有指纹/管理 rev。
    Pending,
    /// relay 标记 `expired`（不进入 active 集合；证书仍留在 `history_certs` 供 ≤S 验签）。
    Expired,
}

/// 已验证控制链的**只读成员视图**（ADR 0010 切片 2a-1；设备列表 UI 的数据源）。
///
/// 它是 [`ControlState`] 的投影：`status == Active` 当且仅当该设备进入 `active_certs`，
/// 不改变任何 active 集合、成员摘要或 grant 规划，也不新增/删除成员。
///
/// `sign_pk`/`hpke_pk` 是**公开**密钥；但 `Debug` 有意只打印
/// `device_id`/`status`/`fingerprint`，避免设备列表与日志意外带出完整密钥材料或证书字节。
#[derive(Clone, PartialEq, Eq)]
pub struct MemberView {
    /// 设备 ID（16 字节，公开标识）。
    pub device_id: DeviceId,
    /// 当前访问状态（见 [`MemberStatus`]）。
    pub status: MemberStatus,
    /// 设备指纹（32 位小写 hex）：成员证书可解析且 §17.1 验签通过时为 `Some`；
    /// pending 的空证书占位为 `None`。
    pub fingerprint: Option<String>,
    /// 设备 Ed25519 签名公钥（公开；`Debug` 不打印）。
    pub sign_pk: [u8; 32],
    /// 设备 X25519 HPKE 公钥（公开；`Debug` 不打印）。
    pub hpke_pk: [u8; 32],
    /// 证书里的 `issued_manager_rev`（历史证书保留其签发管理版本）；pending 空证书为 `None`。
    pub issued_manager_rev: Option<u64>,
}

impl core::fmt::Debug for MemberView {
    /// 只打印 `device_id`/`status`/`fingerprint`：不含 `sign_pk`/`hpke_pk` 与证书原字节。
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MemberView")
            .field("device_id", &crate::ids::hex_of(&self.device_id))
            .field("status", &self.status)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// `/control` 的验证后缓存。
#[derive(Debug, Clone)]
struct ControlState {
    /// relay instance id（`/state` 键 2）；与 pin 比对。
    instance_id: [u8; 16],
    control_rev: u64,
    epoch: u64,
    /// 拉取/验证这条控制链时 `/state` 报的 `auth_rev`（ADR 0010 缓存失效的锚点）。
    ///
    /// `auth_rev` 是全 vault 的鉴权版本（吊销/轮换推进），与 `control_rev` 无关：
    /// 独立吊销只推进 `auth_rev`，控制链/成员证书的**签名事实**不变，但 `active` 集合
    /// 的 relay 侧否决（`member.status`）变了。因此缓存必须同时按 `auth_rev` 失效，
    /// 不能只比较 `control_rev`/`epoch`——否则吊销后会用旧缓存把已移除设备当成 active。
    auth_rev: u64,
    /// 通过 §17.1 验签的**全部**成员证书（active + 已吊销/过期）。
    ///
    /// 历史吊销设备的 ≤S 记录仍需用它的旧证书验签，因此保留；它**不是** active 集合。
    history_certs: HashMap<DeviceId, DeviceCertV1>,
    /// 仅当前 active 成员证书（HPKE 集合/digest 的唯一来源）。
    ///
    /// 由**已签控制链**推导（genesis/approve 加入事件 − begin/revoked-more 吊销），
    /// relay 的 `member.status` 只做 fail-closed 否决，不能授权。
    active_certs: HashMap<DeviceId, DeviceCertV1>,
    /// §30.3 键 4：typed 控制记录 kind（旧记录为 `raw`）。
    kinds: Vec<(u64, String)>,
    /// active 成员证书的 digest 输入（device_id/cert/signature）。
    member_cert_inputs: Vec<menghunote_sync_proto::MemberCertDigestInputV1>,
    /// 已验证控制链链尾 `record_hash`（activation T 的 `prev_control_hash` 来源）。
    head_hash: [u8; 32],
    /// `rev → record_hash`：用于校验本地 pin 仍是新链的前缀。
    record_hashes: HashMap<u64, [u8; 32]>,
    /// 已验证的管理权转换链（bootstrap `control_head_hash` 的输入）。
    manager_chain: Vec<ManagerChainEntry>,
    /// 控制链里**复算通过**的 activation T（历史无 T 记录不会进入这里）。
    verified_activation_t: HashMap<[u8; 16], VerifiedActivationT>,
    /// activation T 所在控制记录的 rev（`T.expected_control_rev`，用于定位链上位置）。
    activation_revs: HashMap<[u8; 16], u64>,
    /// **只读**成员投影（`device_id` 稳定升序；ADR 0010 切片 2a-1 设备列表用）。
    ///
    /// 与 `active_certs`/`history_certs` 由同一轮 `/control` 解析得出，但**不参与**
    /// 授权判定：成员的 `Active` 当且仅当它进入 `active_certs`。
    members: Vec<MemberView>,
}

impl ControlState {
    /// 已验证控制链的只读成员投影（按 `device_id` 稳定升序）。
    ///
    /// 只是投影：active 授权仍只看 `active_certs`，本方法不改变任何成员事实。
    #[must_use]
    pub fn members(&self) -> &[MemberView] {
        &self.members
    }
}

/// 控制链复算出的**已签成员事实**：active 集合的唯一授权来源（评审 0002 P0-2）。
///
/// 事实只来自控制链本身：
/// - 加入 = rev 1 genesis 或 relay `approve` 追加的裸证书记录（`DeviceCertV1` CDE 原字节）；
/// - 移除 = 管理签名的 `rotation.begin`（`revoke[]`）与 `rotation.revoked-more`；
/// - **吊销是永久事实，按单调语义累积**：relay 协议不允许重新批准已吊销设备，因此
///   后续出现的"加入"记录（哪怕是合法证书的重放）不能把设备恢复成 active。
///   这也防住恶意 relay 把旧 approve 记录重放到吊销事件之后（链哈希自洽但顺序伪造）。
///
/// relay `/control` 的 `member.status` **不进入这里**：它不是授权事实，只能用于
/// 收窄（fail-closed）展示/过滤，见 [`parse_control`]。
#[derive(Debug, Default, Clone)]
struct SignedMemberFacts {
    /// 已签加入事件里的证书原字节（`device_id → certificate`）。
    joined_certs: HashMap<DeviceId, Vec<u8>>,
    /// 已签吊销事件命中的设备（永久，单调累积）。
    revoked: HashSet<DeviceId>,
}

impl SignedMemberFacts {
    /// 记录一次已签加入。**不解除**已签吊销：吊销是永久事实（ADR §8），
    /// 且链序可被恶意 relay 伪造，不能靠"最后一条事件"判定。
    fn record_join(&mut self, device_id: DeviceId, certificate: &[u8]) {
        self.joined_certs.insert(device_id, certificate.to_vec());
    }

    /// 记录一次已签吊销（永久）。
    fn record_revocation(&mut self, device_id: DeviceId) {
        self.revoked.insert(device_id);
    }

    /// 该设备当前是否由**已签事实**授权为 active：存在同字节的加入事件，
    /// 且历史上没有已签吊销事件。
    fn signed_active(&self, device_id: &DeviceId, certificate: &[u8]) -> bool {
        !self.revoked.contains(device_id)
            && self
                .joined_certs
                .get(device_id)
                .map(Vec::as_slice)
                .is_some_and(|joined| joined == certificate)
    }

    /// 该设备是否命中**已签吊销**（永久事实；与 relay 的 `member.status` 无关）。
    ///
    /// 只供成员视图做 fail-closed 投影：已签吊销是永久事实，relay 把 status 翻回
    /// `active` 也不能把设备显示成 Active（授权集合同理）。
    fn is_revoked(&self, device_id: &DeviceId) -> bool {
        self.revoked.contains(device_id)
    }
}

/// relay 的 HTTP 实现。
pub struct HttpRelayClient<T: HttpTransport, C: Clock = SystemClock> {
    transport: T,
    config: HttpRelayConfig,
    signing_key: DeviceSigningKey,
    clock: C,
    token: Option<TokenState>,
    control: Option<ControlState>,
    /// 本地控制链 pin（可选；由调用方从 identity/`sync.pinned_*` 注入）。
    ///
    /// **生产路径必须注入**：`None` 且不是显式 bootstrap/local-dev 时，
    /// [`Self::refresh_control`] 直接返回 [`SyncError::Unsupported`]，不会发请求。
    control_pin: Option<ControlPin>,
    /// 仅显式 bootstrap/local-dev：允许无 pin 消费 `/control`。
    ///
    /// 仍然强制 `manager_pub` 信任锚与逐条管理签名；生产 TLS 构造点不得打开。
    allow_unpinned_control: bool,
    /// 解密 MANIFEST 对象用的当前 epoch 根（可选；`checkpoint_manifest` 需要）。
    root: Option<RootSecretV1>,
}

impl<T: HttpTransport> HttpRelayClient<T, SystemClock> {
    /// 用系统时钟构造。
    pub fn new(transport: T, config: HttpRelayConfig, signing_key: DeviceSigningKey) -> Self {
        Self::with_clock(transport, config, signing_key, SystemClock)
    }
}

impl<T: HttpTransport, C: Clock> HttpRelayClient<T, C> {
    /// 注入当前 epoch 根，使 `checkpoint_manifest` 能按 manifest_oid 解密 MANIFEST。
    ///
    /// 不注入时 `checkpoint_manifest` 明确返回 `Unsupported`（而不是把 OID 当明文）。
    #[must_use]
    pub fn with_root(mut self, root: RootSecretV1) -> Self {
        self.root = Some(root);
        self
    }

    /// 指定时钟构造（测试 token 刷新）。
    pub fn with_clock(
        transport: T,
        config: HttpRelayConfig,
        signing_key: DeviceSigningKey,
        clock: C,
    ) -> Self {
        Self {
            transport,
            config,
            signing_key,
            clock,
            token: None,
            control: None,
            control_pin: None,
            allow_unpinned_control: false,
            root: None,
        }
    }

    /// 注入本地控制头 pin：此后每次拉取 `/control` 都必须从该 `(instance, rev, hash)`
    /// 单调前进到已认证 head；回退/同 rev 改内容/替换 genesis 一律 fail-stop。
    #[must_use]
    pub fn with_control_pin(mut self, pin: ControlPin) -> Self {
        self.control_pin = Some(pin);
        self
    }

    /// 用本地身份（`pinned_instance` / `control_rev`）构造并注入 pin。
    ///
    /// 这是生产构造点的首选入口：identity 里的值来自上一次成功验证，构成只进不退的
    /// 下界；`record_hash` 由 [`Self::control_head_pin`] 在验证成功后回写。
    #[must_use]
    pub fn with_identity_control_pin(mut self, identity: &LocalIdentityV1) -> Self {
        self.control_pin = Some(ControlPin::from_identity(identity));
        self
    }

    /// 用 bootstrap 持久化的 `sync.pinned_*` 构造并注入 pin。
    ///
    /// 缺 `instance_id`/`control_rev` 时返回明确错误（不得静默降级为无 pin）。
    pub fn with_bootstrap_control_pin(mut self, pins: &BootstrapPins) -> Result<Self, SyncError> {
        let pin = ControlPin::from_bootstrap_pins(pins).ok_or(SyncError::Unsupported(
            "sync.pinned_instance/control_rev 缺失：无法构造 ControlPin",
        ))?;
        self.control_pin = Some(pin);
        Ok(self)
    }

    /// **仅**首次 bootstrap 或本地开发：显式允许无 pin 消费 `/control`。
    ///
    /// 仍然强制 `manager_pub` 信任锚与逐条管理签名；生产 TLS 构造点必须改用
    /// [`Self::with_control_pin`] / [`Self::with_identity_control_pin`] /
    /// [`Self::with_bootstrap_control_pin`]。
    #[must_use]
    pub fn with_unpinned_bootstrap_control(mut self) -> Self {
        self.allow_unpinned_control = true;
        self
    }

    /// 已缓存的 control_rev（测试/诊断用）。
    pub fn cached_control_rev(&self) -> Option<u64> {
        self.control.as_ref().map(|control| control.control_rev)
    }

    /// 已缓存控制链对应的 `auth_rev`（测试/诊断用）。
    ///
    /// `None` = 没有已验证的控制成员缓存；`Some(rev)` = 这条缓存是在 `/state.auth_rev == rev`
    /// 时拉取/验证的。吊销后 `auth_rev` 前进而 `control_rev` 不变，`state()` 会比对这个值
    /// 并丢弃缓存（ADR 0010）。
    pub fn cached_auth_rev(&self) -> Option<u64> {
        self.control.as_ref().map(|control| control.auth_rev)
    }

    /// 已完成链校验的控制链链尾 `(rev, record_hash)`。
    ///
    /// activation T 的 `prev_control_hash` 必须取这里（而不是本地 journal 的旧 pin）；
    /// 缓存缺失时按 `/state` + `/control` 重新拉取并逐条验 hash/签名。
    pub fn control_head(&mut self) -> Result<(u64, [u8; 32]), SyncError> {
        // 先读 `/state`：control_rev/epoch 前进时丢弃旧缓存，避免把旧链尾当
        // activation T 的 `prev_control_hash`。
        let _ = self.state()?;
        if self.control.is_none() {
            self.refresh_control()?;
        }
        let control = self
            .control
            .as_ref()
            .ok_or(SyncError::RelayWire("control 缓存为空"))?;
        Ok((control.control_rev, control.head_hash))
    }

    /// 已认证缓存的完整控制头 pin（含 instance/rev/链尾 `record_hash`）。
    ///
    /// 调用方应把它写入本地持久化 pin（例如 identity 旁路或 rotation journal），下次
    /// 启动作为 [`Self::with_control_pin`] 的输入；本客户端不直接落盘（没有 Vault 句柄）。
    #[must_use]
    pub fn control_head_pin(&self) -> Option<ControlPin> {
        self.control.as_ref().map(|control| ControlPin {
            instance_id: control.instance_id,
            control_rev: control.control_rev,
            record_hash: Some(control.head_hash),
            bootstrap_hash: None,
        })
    }

    /// 本次 rotation 在 `/control` 中**复算通过**的 activation T 与旧/新管理双签。
    ///
    /// 历史控制记录不带 T 时返回 `None`：表示"无法复算"，绝不表示"验证通过"。
    ///
    /// 这是 inherent 便捷入口（设备侧直接调用）；`RelayClient` trait 的同名方法
    /// 委托到这里，二者语义完全一致（`engine.apply_epoch_change` 走 trait 路径）。
    pub fn verified_activation_t(
        &mut self,
        rotation_id: &[u8; 16],
    ) -> Result<Option<VerifiedActivationT>, SyncError> {
        self.verified_activation_t_cached(rotation_id)
    }

    /// [`Self::verified_activation_t`] 的实现主体。
    ///
    /// 单独成方法是**为了避免 trait 覆盖时的方法解析歧义**：trait 实现里直接调用
    /// `self.verified_activation_t(...)` 会命中 trait 方法本身（无限递归），而
    /// `HttpRelayClient::verified_activation_t` 路径又与 inherent 同名。
    fn verified_activation_t_cached(
        &mut self,
        rotation_id: &[u8; 16],
    ) -> Result<Option<VerifiedActivationT>, SyncError> {
        let _ = self.state()?;
        if self.control.is_none() {
            self.refresh_control()?;
        }
        Ok(self
            .control
            .as_ref()
            .and_then(|control| control.verified_activation_t.get(rotation_id).cloned()))
    }

    /// `/control` 中 `to_epoch > from_epoch` 的全部 activation 链材料（按控制链 rev 升序）。
    ///
    /// 这是离线跨多 epoch 的 B 侧授权链来源：每一环都钉在**已验证**控制链上
    /// （rev = `T.expected_control_rev`；`prev_control_hash` = 该 rev 前一条
    /// `record_hash`），并用 rotation view + primary/fallback 公开事实重建
    /// `ActivationCommitV1` 供消费方复算摘要。**不做**"只取 latest"的裁剪。
    ///
    /// 中间环不下载 baseline/grant（latest baseline 覆盖历史），只拉 view 与
    /// 两个 checkpoint 的公开元数据；任一环缺 view/checkpoint/位置摘要都直接报错，
    /// 由调用方转成 `Paused`（fail-closed）。
    pub fn epoch_activations(
        &mut self,
        from_epoch: u64,
    ) -> Result<Vec<EpochActivationLink>, SyncError> {
        self.collect_epoch_activations(from_epoch)
    }

    /// [`Self::epoch_activations`] 的实现主体（避免 trait 覆盖时的方法解析歧义）。
    fn collect_epoch_activations(
        &mut self,
        from_epoch: u64,
    ) -> Result<Vec<EpochActivationLink>, SyncError> {
        // 先读 `/state`：control_rev/epoch 前进时丢弃旧缓存，避免拿旧链当授权链。
        let _ = self.state()?;
        if self.control.is_none() {
            self.refresh_control()?;
        }
        let Some(control) = self.control.as_ref() else {
            return Err(SyncError::RelayWire("control 缓存为空"));
        };
        let instance_id = control.instance_id;
        // 按控制链 rev 升序，避免 HashMap 迭代顺序影响"链"的语义。
        let mut activations: Vec<(u64, [u8; 16], VerifiedActivationT)> = control
            .verified_activation_t
            .iter()
            .map(|(rotation_id, evidence)| {
                let rev = control.activation_revs.get(rotation_id).copied();
                (rev.unwrap_or(0), *rotation_id, evidence.clone())
            })
            .collect();
        if activations.iter().any(|(rev, _, _)| *rev == 0) {
            return Err(SyncError::RelayWire(
                "activation T 缺少控制链位置（rev），无法验证 epoch 链",
            ));
        }
        activations.sort_by_key(|(rev, _, _)| *rev);
        let record_hashes = control.record_hashes.clone();

        let mut links = Vec::new();
        for (rev, rotation_id, evidence) in activations {
            let activation_t = &evidence.activation_t;
            // 只返回本地 epoch 之后（不含已覆盖的旧环）的 activation。
            if activation_t.to_epoch <= from_epoch {
                continue;
            }
            // `rev - 1` 的 record_hash 是 T.prev_control_hash 的链上位置来源；
            // activation 不可能是 rev1（rev1 是 genesis），缺位置直接拒绝。
            let prev_control_hash = rev
                .checked_sub(1)
                .and_then(|prev_rev| record_hashes.get(&prev_rev).copied())
                .ok_or(SyncError::RelayWire(
                    "activation T 的前一条控制记录缺失（无法绑定 prev_control_hash）",
                ))?;
            let view = self.rotation(rotation_id)?;
            let primary_cid = view.primary_checkpoint.ok_or(SyncError::RelayWire(
                "activation 对应 rotation 缺 primary checkpoint",
            ))?;
            let fallback_cid = view.fallback_checkpoint.ok_or(SyncError::RelayWire(
                "activation 对应 rotation 缺 fallback checkpoint",
            ))?;
            let primary = self.checkpoint_status(primary_cid)?;
            let fallback = self.checkpoint_status(fallback_cid)?;
            let activation_commit = crate::rotation_apply::activation_commit_materials(
                self.config.vault_id,
                instance_id,
                &view,
                &primary,
                &fallback,
            )?;
            links.push(EpochActivationLink {
                control_rev: rev,
                prev_control_hash,
                view,
                evidence,
                activation_commit,
            });
        }
        Ok(links)
    }

    /// §28.1：当前 active 成员证书集合 digest（SHA256(proto 字节输入)）。
    pub fn active_member_cert_digest(&mut self) -> Result<[u8; 32], SyncError> {
        if self.control.is_none() {
            self.refresh_control()?;
        }
        let Some(control) = &self.control else {
            return Err(SyncError::RelayWire("control 缓存为空"));
        };
        let input = menghunote_sync_proto::member_cert_digest(&control.member_cert_inputs);
        Ok(Sha256::digest(&input).into())
    }

    /// ADR 0010 切片 2a-1：已验证控制链的**只读成员视图**（设备列表 UI 用）。
    ///
    /// 返回按 `device_id` 稳定升序的 [`MemberView`]。语义与授权集合严格一致：
    /// - `Active` 当且仅当该设备进入 `active_certs`（已签加入 − 已签吊销 − relay status 否决），
    ///   因此**不会**把已签吊销、或 relay 单方面标 active 的设备显示成 Active；
    /// - `Revoked` 覆盖"relay status-only 撤销"（ADR 0010 默认的仅删凭证）与已签吊销；
    ///   历史证书保留，所以被吊销设备仍有指纹；
    /// - `Pending` 只来自 relay 的空证书占位，`fingerprint`/`issued_manager_rev` 为 `None`。
    ///
    /// 只读投影：不新增/删除成员，也不影响 active 集合、成员摘要与 grant 规划。
    /// 缓存缺失或 `auth_rev` 前进（独立吊销）时，先按既有 pin 规则重新拉取并验证
    /// `/control`，绝不把吊销前的旧成员缓存当作事实返回。
    pub fn members(&mut self) -> Result<Vec<MemberView>, SyncError> {
        // 有缓存时先读 `/state` 比对 `auth_rev`：独立吊销只推进 `auth_rev`（控制链签名
        // 事实与 `control_rev` 都不变），不看它就会把旧缓存里的 B 继续当成 active。
        // 无缓存时 `refresh_control` 自己会读 `/state`，这里不重复请求。
        if self.control.is_some() {
            let _ = self.state()?;
        }
        if self.control.is_none() {
            self.refresh_control()?;
        }
        let control = self
            .control
            .as_ref()
            .ok_or(SyncError::RelayWire("control 缓存为空"))?;
        Ok(control.members().to_vec())
    }

    /// §35.4：完整 staged/active grant leaf 集合（设备 realm 可读）。
    ///
    /// 对 `GET P/rotations/{r}/grants/leaves` 的结果做三重校验：
    /// 1. 每个 leaf 的 `signed_header_sha256` 与 `signed_header` 逐字节复算一致；
    /// 2. leaf 数量等于 relay 的 `grants_count`、且无重复设备；
    /// 3. 用全部 leaf 复算 `grants_digest`，必须与响应逐字节一致（不信任 relay 自报）。
    ///
    /// 返回 `Some(...)` 表示"完整 leaf manifest 已复算通过"；`None` 只由 trait 默认
    /// 实现（不支持该端点的客户端）返回，绝不表示验证通过。
    pub fn grant_leaf_evidence(
        &mut self,
        rotation_id: [u8; 16],
    ) -> Result<Option<Vec<GrantLeafEvidence>>, SyncError> {
        self.fetch_grant_leaf_evidence(&rotation_id)
    }

    /// [`Self::grant_leaf_evidence`] 的实现主体（避免 trait 方法解析歧义）。
    fn fetch_grant_leaf_evidence(
        &mut self,
        rotation_id: &[u8; 16],
    ) -> Result<Option<Vec<GrantLeafEvidence>>, SyncError> {
        let path = format!(
            "{}/rotations/{}/grants/leaves",
            self.vault_path(""),
            base64url::encode(rotation_id)
        );
        let fields = self.send_cbor("GET", path, None)?;
        let echoed_rotation: [u8; 16] = cbor::bytes_fixed(&fields, 5)
            .map_err(|_| SyncError::RelayWire("grant leaf manifest 的 rotation 字段非法"))?;
        if echoed_rotation != *rotation_id {
            return Err(SyncError::RelayWire("grant leaf manifest 的 rotation 不符"));
        }
        let state = cbor::text_field(&fields, 4)
            .map_err(|_| SyncError::RelayWire("grant leaf manifest 缺 rotation state"))?;
        if !matches!(state.as_str(), "staging" | "activated") {
            return Err(SyncError::RelayWire("grant leaf manifest 非 staged/active"));
        }
        let expected_count = cbor::uint_field(&fields, 2)
            .map_err(|_| SyncError::RelayWire("grant leaf manifest 缺 grants_count"))?;
        let expected_digest: [u8; 32] = cbor::bytes_fixed(&fields, 1)
            .map_err(|_| SyncError::RelayWire("grant leaf manifest 的 grants_digest 非法"))?;
        let leaves = cbor::array_field(&fields, 0)
            .map_err(|_| SyncError::RelayWire("grant leaf manifest 缺 leaves"))?;
        let mut evidence = Vec::with_capacity(leaves.len());
        let mut seen = HashSet::new();
        for leaf in leaves {
            let leaf =
                cbor::value_map(&leaf).map_err(|_| SyncError::RelayWire("grant leaf 不是 map"))?;
            let device_id: [u8; 16] = cbor::bytes_fixed(&leaf, 0)
                .map_err(|_| SyncError::RelayWire("grant leaf device_id 非法"))?;
            let stored_digest: [u8; 32] = cbor::bytes_fixed(&leaf, 1)
                .map_err(|_| SyncError::RelayWire("grant leaf signed_header digest 非法"))?;
            let signed_header = cbor::bytes_field(&leaf, 2)
                .map_err(|_| SyncError::RelayWire("grant leaf 缺 signed_header"))?;
            let computed: [u8; 32] = Sha256::digest(&signed_header).into();
            if computed != stored_digest {
                return Err(SyncError::RelayWire(
                    "grant leaf 的 signed_header digest 与 header 不符",
                ));
            }
            if !seen.insert(device_id) {
                return Err(SyncError::RelayWire("grant leaf manifest 含重复设备"));
            }
            evidence.push(GrantLeafEvidence {
                device_id,
                signed_header_sha256: computed,
            });
        }
        if u64::try_from(evidence.len()).ok() != Some(expected_count) {
            return Err(SyncError::RelayWire("grant leaf 数量与 grants_count 不符"));
        }
        if crate::rotation_apply::grant_set_digest(&evidence) != expected_digest {
            return Err(SyncError::RelayWire(
                "grant leaf manifest 与 grants_digest 不符",
            ));
        }
        Ok(Some(evidence))
    }

    /// rotation begin / 设备吊销会 bump `auth_rev` 使全部旧 token 失效；调用方必须清缓存重认证。
    ///
    /// 清的是内部的 token 与 control 两处缓存：`auth_rev` 是全 vault 的，旧 token 和它
    /// 对应的成员/链缓存必须一起作废（ADR 0010）。
    pub fn clear_auth_cache(&mut self) {
        self.token = None;
        self.control = None;
    }

    /// §30.3 键 4：已缓存的控制记录 `(rev, kind)`（旧记录为 `raw`）。
    #[must_use]
    pub fn control_kinds(&self) -> Vec<(u64, String)> {
        self.control
            .as_ref()
            .map(|control| control.kinds.clone())
            .unwrap_or_default()
    }

    /// 从 `GET /state` 键 14 读初始游标（relay task-86 的兜底；旧版没有该字段）。
    ///
    /// genesis 响应键 5 是首选来源；两者都没有时必须走 bootstrap，不能把它当成
    /// 网络错误反复重试。
    pub fn initial_cursor(&mut self) -> Result<String, SyncError> {
        let fields = self.send_cbor("GET", self.vault_path("/state"), None)?;
        cbor::opt_text_field(&fields, 14)?.ok_or(SyncError::Unsupported(
            "relay 未提供初始游标（genesis 键 5 / state 键 14）；必须先 bootstrap",
        ))
    }

    fn vault_path(&self, suffix: &str) -> String {
        format!(
            "/v1/vaults/{}{suffix}",
            base64url::encode(&self.config.vault_id)
        )
    }

    fn origin(&self) -> &str {
        self.config.base_url.trim_end_matches('/')
    }

    // ------------------------------------------------------------ 认证

    fn ensure_token(&mut self) -> Result<String, SyncError> {
        let now = self.clock.now_secs();
        if let Some(token) = &self.token {
            // 提前 60s 刷新（ADR §5：600s 有效）。
            if token.expires_at > now.saturating_add(60) {
                return Ok(token.token.clone());
            }
        }
        self.authenticate(now)?;
        self.token
            .as_ref()
            .map(|token| token.token.clone())
            .ok_or(SyncError::RelayWire("token 刷新后仍为空"))
    }

    fn authenticate(&mut self, now: u64) -> Result<(), SyncError> {
        let body = cbor::encode(vec![
            (0, cbor::uint(1)), // realm = device
            (1, cbor::bytes(self.config.device_id)),
        ])?;
        let response = self.transport.send(&HttpRequest {
            method: "POST",
            path: "/v1/auth/challenges".to_owned(),
            headers: cbor_headers(),
            body,
        })?;
        let fields = decode_ok(&response)?;
        let challenge_id: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
        let realm = cbor::uint_field(&fields, 1)?;
        if realm != u64::from(AuthRealm::Device as u8) {
            return Err(SyncError::Unsupported("relay 只支持 device realm"));
        }
        let subject: [u8; 16] = cbor::bytes_fixed(&fields, 2)?;
        if subject != self.config.device_id {
            return Err(SyncError::RelayWire("challenge subject 与设备不一致"));
        }
        let nonce: [u8; 32] = cbor::bytes_fixed(&fields, 3)?;
        let expires_at = cbor::uint_field(&fields, 4)?;
        let vault_id: [u8; 16] = cbor::bytes_fixed(&fields, 5)?;
        let instance_id: [u8; 16] = cbor::bytes_fixed(&fields, 6)?;
        let _server_time = cbor::uint_field(&fields, 7)?;
        if vault_id != self.config.vault_id {
            return Err(SyncError::RelayWire("challenge.vault_id 不符"));
        }
        if expires_at <= now {
            return Err(SyncError::RelayApi {
                code: "CHALLENGE_INVALID".to_owned(),
                retryable: true,
                current_epoch: None,
                current_control_rev: None,
                bootstrap_hint: None,
            });
        }
        let origin_sha256: [u8; 32] = Sha256::digest(self.origin().as_bytes()).into();
        let message = Challenge {
            pv: 1,
            relay_instance_id: instance_id,
            origin_sha256,
            vault_id,
            realm: AuthRealm::Device,
            subject: self.config.device_id,
            challenge_id,
            nonce,
            expires_at,
        };
        let signed = auth_signing_input(&message)?;
        let signature = self.signing_key.sign(&signed);
        let body = cbor::encode(vec![
            (0, cbor::bytes(challenge_id.to_vec())),
            (1, cbor::bytes(message.encode()?)),
            (2, cbor::bytes(signature)),
        ])?;
        let response = self.transport.send(&HttpRequest {
            method: "POST",
            path: "/v1/auth/tokens".to_owned(),
            headers: cbor_headers(),
            body,
        })?;
        let fields = decode_ok(&response)?;
        let token = cbor::text_field(&fields, 0)?;
        let expires_at = cbor::uint_field(&fields, 1)?;
        let _server_time = cbor::uint_field(&fields, 2)?;
        let realm = cbor::uint_field(&fields, 3)?;
        let _auth_rev = cbor::uint_field(&fields, 4)?;
        if realm != u64::from(AuthRealm::Device as u8) {
            return Err(SyncError::RelayWire("token realm 不符"));
        }
        if token.is_empty() {
            return Err(SyncError::RelayWire("token 为空"));
        }
        self.token = Some(TokenState { token, expires_at });
        Ok(())
    }

    // ------------------------------------------------------------ 请求

    fn send(
        &mut self,
        method: &'static str,
        path: String,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> Result<HttpResponse, SyncError> {
        let response = self.send_once(method, &path, content_type, body.clone())?;
        if !is_revoked_response(&response) {
            return Ok(response);
        }
        // ADR 0010：吊销推进的是**全 vault** 的 `auth_rev`，连调用者的旧 token 与
        // 控制成员缓存一起失效（relay 在鉴权阶段就拒绝，请求本身没有副作用）。
        // 这里清缓存后只重新认证重试**一次**：被吊销设备即使重认证也会继续 REVOKED，
        // 因此不会形成重认证环；活跃设备则能透明地跨过别人吊销带来的版本推进。
        self.clear_auth_cache();
        self.send_once(method, &path, content_type, body)
    }

    fn send_once(
        &mut self,
        method: &'static str,
        path: &str,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> Result<HttpResponse, SyncError> {
        let token = self.ensure_token()?;
        let mut headers = vec![("authorization".to_owned(), format!("Bearer {token}"))];
        if let Some(content_type) = content_type {
            headers.push(("content-type".to_owned(), content_type.to_owned()));
        }
        self.transport.send(&HttpRequest {
            method,
            path: path.to_owned(),
            headers,
            body,
        })
    }

    fn send_cbor(
        &mut self,
        method: &'static str,
        path: String,
        fields: Option<cbor::Fields>,
    ) -> Result<cbor::Fields, SyncError> {
        let body = fields.map(cbor::encode).transpose()?.unwrap_or_default();
        let response = self.send(method, path, Some(CONTENT_TYPE_CBOR), body)?;
        decode_ok(&response)
    }

    fn read_state(&mut self) -> Result<RelayState, SyncError> {
        let fields = self.send_cbor("GET", self.vault_path("/state"), None)?;
        let vault_id: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
        let log_id: [u8; 16] = cbor::bytes_fixed(&fields, 1)?;
        let instance_id: [u8; 16] = cbor::bytes_fixed(&fields, 2)?;
        let _state = cbor::text_field(&fields, 3)?;
        let epoch = cbor::uint_field(&fields, 4)?;
        let control_rev = cbor::uint_field(&fields, 5)?;
        let auth_rev = cbor::uint_field(&fields, 6)?;
        let head_seq = cbor::uint_field(&fields, 7)?;
        let gc_floor = cbor::uint_field(&fields, 8)?;
        let current_rotation_id = cbor::opt_bytes_field(&fields, 15)?
            .map(|bytes| {
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SyncError::RelayWire("state.current_rotation_id 长度非法"))
            })
            .transpose()?;
        if vault_id != self.config.vault_id {
            return Err(SyncError::RelayWire("state.vault_id 与本地不一致"));
        }
        Ok(RelayState {
            vault_id,
            log_id,
            instance_id,
            epoch,
            control_rev,
            auth_rev,
            head_seq,
            gc_floor,
            current_rotation_id,
        })
    }

    fn refresh_control(&mut self) -> Result<(), SyncError> {
        // §35.2/§35.4：消费 `/control` 前先做本地信任门禁，**失败时不发任何请求**。
        // 1. 管理公钥来自本地 identity/pin，是 rev1/管理链之外唯一的信任根；
        // 2. 无 pin 只允许显式 bootstrap 或明文 loopback 开发场景。
        let manager_pub = self.config.manager_pub.ok_or(SyncError::Unsupported(
            "控制链缺管理公钥信任锚：拒绝消费 /control（ADR §35.2）",
        ))?;
        if self.control_pin.is_none()
            && !self.allow_unpinned_control
            && !is_local_dev_base_url(&self.config.base_url)
        {
            return Err(SyncError::Unsupported(
                "控制链缺本地 ControlPin：拒绝消费 /control（仅显式 bootstrap/local-dev 可跳过）",
            ));
        }
        // 管理签名输入绑定 relay `instance_id`（`/control` 本身不返回它），先读 `/state`。
        let state = self.read_state()?;
        // instance pin 先查：换实例（哪怕控制链自洽）也不接受。
        if let Some(pin) = self.control_pin {
            if pin.instance_id != state.instance_id {
                return Err(SyncError::RelayWire("relay instance 与本地 pin 不符"));
            }
        }
        // relay `/control` 固定单页 256 条，只接受 `after_rev` 游标；逐页拉到 head。
        //
        // 始终从 `after_rev=0` 拉全链，而不是从本地 pin 的 rev 续读：这样既能用 pin 做
        // 下界/前缀锚点（检测 genesis 替换），也保留完整 `control_kinds`/历史 activation T；
        // pin 的单调性由 [`verify_control_pin`] 统一判定。
        let mut pages = Vec::new();
        let mut after_rev = 0u64;
        let mut head_rev: Option<u64> = None;
        loop {
            let path = format!("{}/control?after_rev={after_rev}", self.vault_path(""));
            let fields = self.send_cbor("GET", path, None)?;
            let page_head =
                cbor::uint_field(&fields, 1).map_err(|_| SyncError::RelayWire("control rev"))?;
            if let Some(expected) = head_rev {
                if page_head != expected {
                    return Err(SyncError::RelayWire(
                        "控制链分页的 control_rev 不一致（重排/替换）",
                    ));
                }
            } else {
                head_rev = Some(page_head);
            }
            let commits = cbor::array_field(&fields, 0)
                .map_err(|_| SyncError::RelayWire("control commits"))?;
            // 本页最后一条 rev；空页必须已经到 head，否则就是缺页。
            let page_last = match commits.last() {
                Some(commit) => {
                    let commit = cbor::value_map(commit)
                        .map_err(|_| SyncError::RelayWire("control commit map"))?;
                    cbor::uint_field(&commit, 0)
                        .map_err(|_| SyncError::RelayWire("control commit rev"))?
                }
                None => after_rev,
            };
            if page_last > page_head {
                return Err(SyncError::RelayWire("控制链分页的 rev 超过 control_rev"));
            }
            pages.push(fields);
            if page_last == page_head {
                break;
            }
            if page_last <= after_rev {
                return Err(SyncError::RelayWire("控制链分页未前进（缺页）"));
            }
            after_rev = page_last;
            if pages.len() > MAX_CONTROL_PAGES {
                return Err(SyncError::RelayWire("控制链分页超过安全上限"));
            }
        }
        let control = parse_control(
            &pages,
            &self.config.vault_id,
            &state.instance_id,
            &manager_pub,
            // 缓存锚点：这条控制链是在 `/state.auth_rev == state.auth_rev` 时验证的。
            state.auth_rev,
        )?;
        // `/state` 先于 `/control` 读取：并发追加只会让控制链更新，不能反而更旧。
        if control.control_rev < state.control_rev {
            return Err(SyncError::RelayWire("/control 的 control_rev 低于 /state"));
        }
        if let Some(pin) = self.control_pin {
            verify_control_pin(&pin, &state, &control)?;
        }
        self.control = Some(control);
        Ok(())
    }
}

/// 校验本地控制头 pin 的单调性（fail-stop，不做任何降级）。
///
/// 规则：
/// 1. instance 必须相等；
/// 2. 新 head 的 `control_rev` 不得低于 pin；
/// 3. 同 rev 时 `record_hash` / `bootstrap_hash`（若提供）必须逐字节相等；
/// 4. rev 前进时，pin rev 的 `record_hash` 必须仍是新链的前缀（防替换 genesis/重排）。
fn verify_control_pin(
    pin: &ControlPin,
    state: &RelayState,
    control: &ControlState,
) -> Result<(), SyncError> {
    if pin.instance_id != state.instance_id {
        return Err(SyncError::RelayWire("relay instance 与本地 pin 不符"));
    }
    if control.control_rev < pin.control_rev {
        return Err(SyncError::RelayWire("控制链低于本地 pin（回退）"));
    }
    if control.control_rev == pin.control_rev {
        if let Some(hash) = pin.record_hash {
            if control.head_hash != hash {
                return Err(SyncError::RelayWire(
                    "控制链同 rev 的 record_hash 与本地 pin 不符",
                ));
            }
        }
        if let Some(hash) = pin.bootstrap_hash {
            // bootstrap 摘要只覆盖 vault/instance/log/epoch/control_rev + 管理链；
            // `/state` 可能早于 `/control` 一瞬，按控制链自己的坐标复算。
            let mut pinned_state = state.clone();
            pinned_state.epoch = control.epoch;
            pinned_state.control_rev = control.control_rev;
            let computed = control_head_hash(&pinned_state, &control.manager_chain);
            if computed != hash {
                return Err(SyncError::RelayWire(
                    "控制链同 rev 的 bootstrap hash 与本地 pin 不符",
                ));
            }
        }
    } else if let Some(hash) = pin.record_hash {
        // 新链必须包含 pin 指向的旧链尾（append-only）；否则是替换 genesis/分叉。
        match control.record_hashes.get(&pin.control_rev) {
            Some(actual) if *actual == hash => {}
            _ => {
                return Err(SyncError::RelayWire(
                    "控制链未包含本地 pin 的 record_hash（替换/重排）",
                ))
            }
        }
    }
    Ok(())
}

impl<T: HttpTransport, C: Clock> RelayClient for HttpRelayClient<T, C> {
    fn state(&mut self) -> Result<RelayState, SyncError> {
        let state = self.read_state()?;
        let Some(control) = &self.control else {
            return Ok(state);
        };
        // 已认证的 head 只能前进：`/state` 更低即 relay 回滚，fail-stop。
        if state.control_rev < control.control_rev {
            return Err(SyncError::RelayWire(
                "relay control_rev 低于已认证缓存（回滚）",
            ));
        }
        // control_rev/epoch 前进 → 旧成员证书缓存失效（token 仍按自身 auth_rev 有效）。
        let rev_moved = state.control_rev > control.control_rev || state.epoch != control.epoch;
        // ADR 0010：**还必须看 `auth_rev`**。独立吊销只推进 `auth_rev`（`control_rev`
        // 与签名链完全不变），但 relay 的 `member.status` 已把目标改成 revoked；只比较
        // `control_rev` 会把旧缓存里的 active 集合/成员摘要继续当成事实。`auth_rev` 是
        // 全 vault 的鉴权版本，所以 token 与成员缓存必须一起作废、重新认证。
        let auth_moved = state.auth_rev != control.auth_rev;
        if rev_moved {
            self.control = None;
        }
        if auth_moved {
            self.clear_auth_cache();
        }
        Ok(state)
    }

    fn log(&mut self, cursor: Option<&str>, limit: u32) -> Result<RelayLogPage, SyncError> {
        let cursor = cursor.ok_or(SyncError::Unsupported(
            "relay 需要 bootstrap 提供的初始游标",
        ))?;
        let path = format!(
            "{}?after={}&limit={limit}",
            self.vault_path("/log"),
            percent_encode(cursor)
        );
        let fields = self.send_cbor("GET", path, None)?;
        let events = cbor::array_field(&fields, 0)?;
        let mut parsed = Vec::with_capacity(events.len());
        for event in events {
            let event = cbor::value_map(&event)?;
            let relay_seq = cbor::uint_field(&event, 0)?;
            let kind_text = cbor::text_field(&event, 1)?;
            let kind =
                RelayLogKind::parse(&kind_text).ok_or(SyncError::RelayWire("未知日志事件类型"))?;
            let object_id = match cbor::opt_bytes_field(&event, 2)? {
                Some(bytes) => Some(
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| SyncError::RelayWire("日志 object_id 不是 32 字节"))?,
                ),
                None => None,
            };
            let epoch = cbor::uint_field(&event, 3)?;
            let payload = cbor::bytes_field(&event, 4)?;
            let signature = match cbor::opt_bytes_field(&event, 5)? {
                Some(bytes) => Some(
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| SyncError::RelayWire("日志签名不是 64 字节"))?,
                ),
                None => None,
            };
            parsed.push(RelayLogEvent {
                relay_seq,
                kind,
                object_id,
                epoch,
                event: payload,
                signature,
            });
        }
        Ok(RelayLogPage {
            events: parsed,
            next_cursor: cbor::text_field(&fields, 1)?,
            through_seq: cbor::uint_field(&fields, 2)?,
            query_head: cbor::uint_field(&fields, 3)?,
            epoch: cbor::uint_field(&fields, 4)?,
            gc_floor: cbor::uint_field(&fields, 5)?,
        })
    }

    fn part(&mut self, object_id: &ObjectId, part_no: u32) -> Result<Vec<u8>, SyncError> {
        let path = format!(
            "/v1/vaults/{}/objects/{}/parts/{part_no}",
            base64url::encode(&self.config.vault_id),
            base64url::encode(object_id)
        );
        let response = self.send("GET", path, None, Vec::new())?;
        match response.status {
            200 => Ok(response.body),
            _ => Err(error_from_response(&response)),
        }
    }

    fn put_part(
        &mut self,
        object_id: &ObjectId,
        part_no: u32,
        envelope: &[u8],
    ) -> Result<(), SyncError> {
        let path = format!(
            "/v1/vaults/{}/objects/{}/parts/{part_no}",
            base64url::encode(&self.config.vault_id),
            base64url::encode(object_id)
        );
        let response = self.send("PUT", path, Some(CONTENT_TYPE_OCTET), envelope.to_vec())?;
        match response.status {
            200 | 201 => Ok(()),
            _ => Err(error_from_response(&response)),
        }
    }

    fn put_part_with_reservation(
        &mut self,
        object_id: &ObjectId,
        part_no: u32,
        envelope: &[u8],
        reservation: Option<&[u8; 16]>,
    ) -> Result<(), SyncError> {
        let Some(reservation) = reservation else {
            return self.put_part(object_id, part_no, envelope);
        };
        let path = format!(
            "/v1/vaults/{}/objects/{}/parts/{part_no}?reservation={}",
            base64url::encode(&self.config.vault_id),
            base64url::encode(object_id),
            base64url::encode(reservation)
        );
        let response = self.send("PUT", path, Some(CONTENT_TYPE_OCTET), envelope.to_vec())?;
        match response.status {
            200 | 201 => Ok(()),
            _ => Err(error_from_response(&response)),
        }
    }

    fn submit_updates(
        &mut self,
        items: Vec<UpdateSubmission>,
    ) -> Result<Vec<UpdateOutcome>, SyncError> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        if items.len() > 256 {
            return Err(SyncError::Unsupported("单批 update 超过 256"));
        }
        let entries = items
            .iter()
            .map(|item| {
                cbor::CdeValue::Map(vec![
                    (0, cbor::bytes(item.object_id)),
                    (1, cbor::bytes(item.signed_commit.clone())),
                    (2, cbor::bytes(item.signature)),
                ])
            })
            .collect();
        let fields = self.send_cbor(
            "POST",
            self.vault_path("/updates"),
            Some(vec![(0, cbor::array(entries))]),
        )?;
        let results = cbor::array_field(&fields, 0)?;
        let mut outcomes = Vec::with_capacity(results.len());
        for result in results {
            let result = cbor::value_map(&result)?;
            let object_id: [u8; 32] = cbor::bytes_fixed(&result, 0)?;
            let status_text = cbor::text_field(&result, 1)?;
            let relay_seq = cbor::opt_uint_field(&result, 2)?;
            let code = cbor::opt_text_field(&result, 3)?;
            let retryable = cbor::bool_field(&result, 4)?;
            let status = match status_text.as_str() {
                "accepted" => UpdateStatus::Accepted { relay_seq },
                "duplicate" => UpdateStatus::Duplicate,
                "failed" => UpdateStatus::Failed { code, retryable },
                _ => return Err(SyncError::RelayWire("未知 update 结果状态")),
            };
            outcomes.push(UpdateOutcome { object_id, status });
        }
        Ok(outcomes)
    }

    fn ack(&mut self, received_seq: u64, integrated_seq: u64, epoch: u64) -> Result<(), SyncError> {
        let body = cbor::encode(vec![
            (0, cbor::uint(received_seq)),
            (1, cbor::uint(integrated_seq)),
            (2, cbor::uint(epoch)),
        ])?;
        let response = self.send(
            "POST",
            self.vault_path("/acks"),
            Some(CONTENT_TYPE_CBOR),
            body,
        )?;
        match response.status {
            200 | 204 => Ok(()),
            _ => Err(error_from_response(&response)),
        }
    }

    fn device_cert(&mut self, device_id: &DeviceId) -> Result<Option<DeviceCertV1>, SyncError> {
        if self.control.is_none() {
            self.refresh_control()?;
        }
        // 返回所有**验签通过**的历史证书（含已吊销）：≤S 的旧对象仍要用它验生产者签名；
        // active 集合由 `active_member_hpke_keys` / `active_member_cert_digest` 单独给出。
        Ok(self
            .control
            .as_ref()
            .and_then(|control| control.history_certs.get(device_id))
            .cloned())
    }

    fn submit_object(&mut self, item: ObjectSubmission) -> Result<UpdateOutcome, SyncError> {
        let entry = cbor::CdeValue::Map(vec![
            (0, cbor::bytes(item.object_id)),
            (1, cbor::bytes(item.signed_commit)),
            (2, cbor::bytes(item.signature)),
        ]);
        let fields = self.send_cbor(
            "POST",
            self.vault_path("/updates"),
            Some(vec![(0, cbor::array(vec![entry]))]),
        )?;
        let results = cbor::array_field(&fields, 0)?;
        let result = results
            .first()
            .ok_or(SyncError::RelayWire("submit_object 响应缺少结果"))?;
        parse_update_outcome(&cbor::value_map(result)?)
    }

    fn put_key_slot(&mut self, upload: SlotUpload) -> Result<SlotRecord, SyncError> {
        let path = format!(
            "{}/key-slots/{}",
            self.vault_path(""),
            base64url::encode(&upload.slot_id)
        );
        let signed = cbor::CdeValue::Map(vec![
            (0, cbor::bytes(upload.signed_message)),
            (1, cbor::bytes(upload.signature)),
        ]);
        let fields = self.send_cbor(
            "PUT",
            path,
            Some(vec![
                (0, cbor::uint(upload.epoch)),
                (1, cbor::text(slot_kind_text(upload.kind))),
                (2, cbor::bytes(upload.descriptor)),
                (3, signed),
            ]),
        )?;
        let slot = cbor::value_map(&cbor::field(&fields, 0)?.clone())?;
        parse_slot_record(&slot)
    }

    fn list_key_slots(&mut self) -> Result<Vec<SlotRecord>, SyncError> {
        let fields = self.send_cbor("GET", format!("{}/key-slots", self.vault_path("")), None)?;
        let mut slots = Vec::new();
        for value in cbor::array_field(&fields, 0)? {
            slots.push(parse_slot_record(&cbor::value_map(&value)?)?);
        }
        Ok(slots)
    }

    fn activate_password_slot(
        &mut self,
        expected_slot_set_rev: u64,
        slot_id: ObjectId,
        signed_message: Vec<u8>,
        signature: [u8; 64],
    ) -> Result<u64, SyncError> {
        let signed = cbor::CdeValue::Map(vec![
            (0, cbor::bytes(signed_message)),
            (1, cbor::bytes(signature)),
        ]);
        let fields = self.send_cbor(
            "POST",
            format!("{}/key-slots/password/activate", self.vault_path("")),
            Some(vec![
                (0, cbor::uint(expected_slot_set_rev)),
                (1, cbor::bytes(slot_id)),
                (2, signed),
            ]),
        )?;
        cbor::uint_field(&fields, 0)
    }

    fn create_checkpoint(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
    ) -> Result<CheckpointLease, SyncError> {
        self.create_checkpoint_with_role(estimated_bytes, epoch, None)
    }

    fn create_checkpoint_with_role(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
        role: Option<&str>,
    ) -> Result<CheckpointLease, SyncError> {
        let mut body = vec![(0, cbor::uint(estimated_bytes))];
        if let Some(epoch) = epoch {
            body.push((1, cbor::uint(epoch)));
        }
        if let Some(role) = role {
            body.push((2, cbor::text(role)));
            // 评审 0003 P1-D：候选基线 create 必须携带幂等 `request_id`（relay 字段 3）。
            // 同 role/epoch 的重试派生同一值，relay 会返回原 cid/lease，而不是撞
            // "每 epoch role 唯一 CAS"。派生公式与 durable intent 共用同一入口。
            let request_id = crate::checkpoint::baseline_create_request_id(
                &self.config.vault_id,
                role,
                epoch.unwrap_or(0),
            );
            body.push((3, cbor::bytes(request_id.to_vec())));
        }
        let fields = self.send_cbor(
            "POST",
            format!("{}/checkpoints", self.vault_path("")),
            Some(body),
        )?;
        Ok(CheckpointLease {
            checkpoint_id: cbor::bytes_fixed(&fields, 0)?,
            epoch: cbor::uint_field(&fields, 1)?,
            covered_seq: cbor::uint_field(&fields, 2)?,
            reservation_id: cbor::bytes_fixed(&fields, 3)?,
            expires_at: cbor::uint_field(&fields, 4)?,
        })
    }

    fn add_checkpoint_refs(
        &mut self,
        checkpoint_id: menghunote_sync_proto::CheckpointId,
        refs: Vec<CheckpointRef>,
    ) -> Result<u64, SyncError> {
        let entries = refs
            .into_iter()
            .map(|reference| {
                cbor::CdeValue::Map(vec![
                    (0, cbor::bytes(reference.object_id)),
                    (1, cbor::text(reference.role.as_str())),
                ])
            })
            .collect();
        let path = format!(
            "{}/checkpoints/{}/refs",
            self.vault_path(""),
            base64url::encode(&checkpoint_id)
        );
        let fields = self.send_cbor("POST", path, Some(vec![(0, cbor::array(entries))]))?;
        cbor::uint_field(&fields, 0)
    }

    fn publish_checkpoint(
        &mut self,
        publish: CheckpointPublish,
    ) -> Result<PublishedCheckpoint, SyncError> {
        let path = format!(
            "{}/checkpoints/{}/publish",
            self.vault_path(""),
            base64url::encode(&publish.checkpoint_id)
        );
        let fields = self.send_cbor(
            "POST",
            path,
            Some(vec![
                (0, cbor::uint(publish.covered_seq)),
                (1, cbor::bytes(publish.manifest_oid)),
                (2, cbor::bytes(publish.refs_digest)),
                (3, cbor::bytes(publish.signed_commit)),
                (4, cbor::bytes(publish.signature)),
            ]),
        )?;
        Ok(PublishedCheckpoint {
            checkpoint_id: cbor::bytes_fixed(&fields, 0)?,
            epoch: cbor::uint_field(&fields, 1)?,
            covered_seq: cbor::uint_field(&fields, 2)?,
            published_at: cbor::uint_field(&fields, 3)?,
        })
    }

    fn bootstrap(&mut self) -> Result<BootstrapBundle, SyncError> {
        bootstrap_impl(self, false)
    }

    fn bootstrap_degraded(&mut self) -> Result<BootstrapBundle, SyncError> {
        bootstrap_impl(self, true)
    }

    fn renew_bootstrap_lease(&mut self, lease_id: [u8; 16]) -> Result<BootstrapLease, SyncError> {
        let fields = self.send_cbor(
            "PATCH",
            format!(
                "{}/bootstrap-leases/{}",
                self.vault_path(""),
                base64url::encode(&lease_id)
            ),
            None,
        )?;
        Ok(BootstrapLease {
            lease_id: cbor::bytes_fixed(&fields, 0)?,
            expires_at: cbor::uint_field(&fields, 1)?,
            floor_seq: cbor::uint_field(&fields, 2)?,
        })
    }

    fn begin_rotation(&mut self, begin: RotationBegin) -> Result<RotationView, SyncError> {
        let signed = cbor::CdeValue::Map(vec![
            (0, cbor::bytes(begin.signed_body)),
            (1, cbor::bytes(begin.signature.to_vec())),
        ]);
        let fields = self.send_cbor(
            "POST",
            self.vault_path("/rotations"),
            Some(vec![
                (0, signed),
                (1, cbor::bytes(begin.request_id.to_vec())),
                (2, cbor::uint(begin.expected_control_rev)),
                (3, cbor::uint(begin.expected_epoch)),
                (
                    4,
                    cbor::array(
                        begin
                            .revoke
                            .iter()
                            .map(|device| cbor::bytes(device.to_vec()))
                            .collect(),
                    ),
                ),
            ]),
        )?;
        parse_rotation_view(&fields)
    }

    fn rotation_for_request(
        &mut self,
        request_id: [u8; 16],
    ) -> Result<Option<RotationView>, SyncError> {
        let fields = self.send_cbor(
            "GET",
            format!(
                "{}/rotations/current?request_id={}",
                self.vault_path(""),
                base64url::encode(&request_id)
            ),
            None,
        )?;
        match cbor::field(&fields, 0) {
            Ok(_) => parse_rotation_view(&fields).map(Some),
            Err(_) => Ok(None),
        }
    }

    fn current_rotation(&mut self) -> Result<Option<RotationView>, SyncError> {
        let fields = self.send_cbor(
            "GET",
            format!("{}/rotations/current", self.vault_path("")),
            None,
        )?;
        match cbor::field(&fields, 0) {
            Ok(_) => parse_rotation_view(&fields).map(Some),
            Err(_) => Ok(None),
        }
    }

    fn rotation(&mut self, rotation_id: [u8; 16]) -> Result<RotationView, SyncError> {
        let fields = self.send_cbor(
            "GET",
            format!(
                "{}/rotations/{}",
                self.vault_path(""),
                base64url::encode(&rotation_id)
            ),
            None,
        )?;
        parse_rotation_view(&fields)
    }

    fn management_chain(&mut self) -> Result<Vec<ManagerChainEntry>, SyncError> {
        let fields = self.send_cbor(
            "GET",
            format!("{}/control?after_rev=0", self.vault_path("")),
            None,
        )?;
        cbor::array_field(&fields, 5)?
            .iter()
            .map(parse_manager_chain_entry)
            .collect()
    }

    fn active_member_hpke_keys(&mut self) -> Result<Vec<(DeviceId, [u8; 32])>, SyncError> {
        if self.control.is_none() {
            self.refresh_control()?;
        }
        let mut keys = self
            .control
            .as_ref()
            .map(|control| {
                control
                    .active_certs
                    .iter()
                    .map(|(device_id, cert)| (*device_id, cert.hpke_x25519_pub))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        keys.sort_by_key(|(device_id, _)| *device_id);
        Ok(keys)
    }

    fn control_kinds(&mut self) -> Result<Vec<(u64, String)>, SyncError> {
        if self.control.is_none() {
            self.refresh_control()?;
        }
        Ok(self
            .control
            .as_ref()
            .map(|control| control.kinds.clone())
            .unwrap_or_default())
    }

    fn self_grant_evidence(&mut self) -> Result<crate::relay::SelfGrantEvidence, SyncError> {
        let fields = self.send_cbor(
            "GET",
            format!("{}/devices/self/grants/latest", self.vault_path("")),
            None,
        )?;
        Ok(crate::relay::SelfGrantEvidence {
            epoch: cbor::uint_field(&fields, 0)?,
            device_id: cbor::bytes_fixed(&fields, 1)?,
            manager_rev: cbor::uint_field(&fields, 2)?,
            hpke_enc: cbor::bytes_fixed(&fields, 3)?,
            ciphertext: cbor::bytes_field(&fields, 4)?,
            signed_header: cbor::bytes_field(&fields, 5)?,
            signature: cbor::opt_bytes_field(&fields, 6)?.unwrap_or_default(),
            package_digest: cbor::opt_bytes_field(&fields, 10)?
                .map(|bytes| {
                    bytes
                        .try_into()
                        .map_err(|_| SyncError::RelayWire("grant digest 长度"))
                })
                .transpose()?,
        })
    }

    fn checkpoint_manifest(
        &mut self,
        checkpoint_id: &[u8; 16],
    ) -> Result<crate::relay::CheckpointManifest, SyncError> {
        let outer = self.send_cbor(
            "GET",
            format!(
                "{}/checkpoints/{}",
                self.vault_path(""),
                base64url::encode(checkpoint_id)
            ),
            None,
        )?;
        let detail = cbor::value_map(cbor::field(&outer, 0)?)?;
        // relay `api.rs:1859–1860`：`GET /checkpoints/{cid}` 键 4 是 MANIFEST 的
        // **object id（32B）**，不是 manifest 明文。明文在对象端点，且是 MNE1 密文。
        let manifest_oid: ObjectId = cbor::bytes_fixed(&detail, 4)
            .map_err(|_| SyncError::RelayWire("checkpoint 缺少 manifest_oid（键 4）"))?;
        let expected_digest: Option<[u8; 32]> = match cbor::opt_bytes_field(&detail, 6)? {
            Some(commit_bytes) => {
                let commit = CheckpointCommit::decode(&commit_bytes)?;
                if commit.checkpoint_id != *checkpoint_id
                    || commit.manifest_object_id != manifest_oid
                {
                    return Err(SyncError::RelayWire("checkpoint commit 与 GET 响应不绑定"));
                }
                Some(commit.manifest_digest)
            }
            None => None,
        };
        let root = self.root.clone().ok_or(SyncError::Unsupported(
            "checkpoint_manifest 需要当前 epoch 根解密 MANIFEST（HttpRelayClient::with_root）",
        ))?;
        let (manifest_bytes, object_digest) =
            crate::rotation_apply::fetch_manifest_object(self, &root, &manifest_oid)?;
        if let Some(expected) = expected_digest {
            if object_digest != expected {
                return Err(SyncError::Object(
                    "MANIFEST 对象摘要与 checkpoint commit 不符",
                ));
            }
        }
        let manifest = ManifestV1::decode(&manifest_bytes)?;
        if manifest.vault_id != self.config.vault_id
            || manifest.checkpoint_id != *checkpoint_id
            || manifest.key_epoch != root.key_epoch
        {
            return Err(SyncError::RelayWire(
                "MANIFEST 与 checkpoint/root 不绑定（vault/cid/key_epoch）",
            ));
        }
        Ok(crate::relay::CheckpointManifest {
            checkpoint_id: *checkpoint_id,
            manifest_bytes,
        })
    }

    fn reauthenticate(&mut self) -> Result<(), SyncError> {
        self.clear_auth_cache();
        Ok(())
    }

    fn rotation_assets(
        &mut self,
        rotation_id: [u8; 16],
        after: Option<[u8; 32]>,
        limit: u32,
    ) -> Result<FrozenAssetPage, SyncError> {
        let mut path = format!(
            "{}/rotations/{}/assets?limit={limit}",
            self.vault_path(""),
            base64url::encode(&rotation_id)
        );
        if let Some(after) = after {
            path.push_str(&format!("&after={}", base64url::encode(&after)));
        }
        let fields = self.send_cbor("GET", path, None)?;
        let assets = cbor::array_field(&fields, 0)?
            .iter()
            .map(parse_frozen_asset)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(FrozenAssetPage {
            assets,
            freeze_seq: cbor::uint_field(&fields, 1)?,
            asset_set_digest: cbor::opt_bytes_field(&fields, 2)?
                .map(|bytes| {
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| SyncError::RelayWire("asset_set_digest 长度非法"))
                })
                .transpose()?,
            total: cbor::uint_field(&fields, 3)?,
        })
    }

    fn checkpoint_status(
        &mut self,
        checkpoint_id: [u8; 16],
    ) -> Result<CheckpointStatus, SyncError> {
        let outer = self.send_cbor(
            "GET",
            format!(
                "{}/checkpoints/{}",
                self.vault_path(""),
                base64url::encode(&checkpoint_id)
            ),
            None,
        )?;
        let fields = cbor::value_map(cbor::field(&outer, 0)?)?;
        Ok(CheckpointStatus {
            checkpoint_id: cbor::bytes_fixed(&fields, 0)
                .map_err(|_| SyncError::RelayWire("checkpoint id 字段"))?,
            state: cbor::text_field(&fields, 1)
                .map_err(|_| SyncError::RelayWire("checkpoint state 字段"))?,
            role: cbor::opt_text_field(&fields, 2)
                .map_err(|_| SyncError::RelayWire("checkpoint role 字段"))?,
            covered_seq: cbor::uint_field(&fields, 3)
                .map_err(|_| SyncError::RelayWire("checkpoint covered 字段"))?,
            manifest_object_id: cbor::opt_bytes_field(&fields, 4)?
                .map(|bytes| {
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| SyncError::RelayWire("manifest oid 长度非法"))
                })
                .transpose()?,
            refs_digest: cbor::opt_bytes_field(&fields, 5)?
                .map(|bytes| {
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| SyncError::RelayWire("refs_digest 长度非法"))
                })
                .transpose()?,
            candidate_ready: cbor::bool_field(&fields, 9).unwrap_or(false),
            epoch: cbor::uint_field(&fields, 10).unwrap_or(0),
        })
    }

    /// 覆盖默认 `Ok(None)`：委托 inherent [`HttpRelayClient::verified_activation_t`]，
    /// 按已验证 `/control` 缓存复算 T 双签。缺失（历史记录无 T）仍是 `None`。
    fn verified_activation_t(
        &mut self,
        rotation_id: [u8; 16],
    ) -> Result<Option<VerifiedActivationT>, SyncError> {
        self.verified_activation_t_cached(&rotation_id)
    }

    /// 覆盖默认空链：从已验证 `/control` 缓存按 rev 升序产出每一环的完整材料。
    ///
    /// 与 [`HttpRelayClient::epoch_activations`] 语义完全一致（engine 走 trait 路径）。
    fn epoch_activations(
        &mut self,
        from_epoch: u64,
    ) -> Result<Vec<EpochActivationLink>, SyncError> {
        self.collect_epoch_activations(from_epoch)
    }

    /// §35.4：设备 realm 的完整 grant leaf manifest；HTTP 实现会复算
    /// `SHA256(signed_header)` 与 `grants_digest`，失败即 `RelayWire`。
    fn grant_leaf_evidence(
        &mut self,
        rotation_id: [u8; 16],
    ) -> Result<Option<Vec<GrantLeafEvidence>>, SyncError> {
        self.fetch_grant_leaf_evidence(&rotation_id)
    }
}

fn bootstrap_impl<T: HttpTransport, C: Clock>(
    client: &mut HttpRelayClient<T, C>,
    degraded: bool,
) -> Result<BootstrapBundle, SyncError> {
    let path = if degraded {
        format!("{}/bootstrap?degraded=1", client.vault_path(""))
    } else {
        format!("{}/bootstrap", client.vault_path(""))
    };
    let fields = client.send_cbor("POST", path, None)?;
    let primary = parse_bootstrap_checkpoint(cbor::field(&fields, 0)?)?;
    let fallback = parse_bootstrap_checkpoint(cbor::field(&fields, 1)?)?;
    let lease_id: [u8; 16] = cbor::bytes_fixed(&fields, 5)?;
    Ok(BootstrapBundle {
        primary,
        fallback,
        head_seq: cbor::uint_field(&fields, 2)?,
        gc_floor: cbor::uint_field(&fields, 3)?,
        epoch: cbor::uint_field(&fields, 4)?,
        degraded: cbor::bool_field(&fields, 7).unwrap_or(false),
        lease: BootstrapLease {
            lease_id,
            expires_at: cbor::uint_field(&fields, 6)?,
            floor_seq: 0,
        },
    })
}

fn parse_frozen_asset(value: &cbor::CdeValue) -> Result<FrozenAssetRef, SyncError> {
    let fields = cbor::value_map(value)?;
    Ok(FrozenAssetRef {
        object_id: cbor::bytes_fixed(&fields, 0)?,
        generation: cbor::uint_field(&fields, 2)?,
        key_epoch: cbor::uint_field(&fields, 3)?,
        part_count: u32::try_from(cbor::uint_field(&fields, 4)?)
            .map_err(|_| SyncError::RelayWire("frozen asset part_count 越界"))?,
        total_plain_len: cbor::uint_field(&fields, 5)?,
        object_digest: cbor::bytes_fixed(&fields, 6)?,
    })
}

fn parse_manager_chain_entry(value: &cbor::CdeValue) -> Result<ManagerChainEntry, SyncError> {
    let fields = cbor::value_map(value)?;
    Ok(ManagerChainEntry {
        rev: cbor::uint_field(&fields, 0)?,
        pubkey: cbor::bytes_fixed(&fields, 1)
            .map_err(|_| SyncError::RelayWire("management chain pubkey 长度非法"))?,
        transition: cbor::bytes_field(&fields, 2)?,
        old_rev: cbor::uint_field(&fields, 3)?,
        old_signature: cbor::opt_bytes_field(&fields, 4)?
            .map(|bytes| {
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SyncError::RelayWire("management old sig 长度非法"))
            })
            .transpose()?,
        new_signature: cbor::opt_bytes_field(&fields, 5)?
            .map(|bytes| {
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SyncError::RelayWire("management new sig 长度非法"))
            })
            .transpose()?,
    })
}

fn parse_rotation_view(fields: &cbor::Fields) -> Result<RotationView, SyncError> {
    let view = cbor::value_map(cbor::field(fields, 0)?)?;
    Ok(RotationView {
        rotation_id: cbor::bytes_fixed(&view, 0)?,
        state: cbor::text_field(&view, 1)?,
        from_epoch: cbor::uint_field(&view, 2)?,
        to_epoch: cbor::uint_field(&view, 3)?,
        freeze_seq: cbor::uint_field(&view, 4)?,
        revoked_count: cbor::uint_field(&view, 5)?,
        member_set_hash: cbor::opt_bytes_field(&view, 6)?
            .map(|bytes| {
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SyncError::RelayWire("rotation member_set_hash 长度非法"))
            })
            .transpose()?,
        grants_count: cbor::uint_field(&view, 7)?,
        grants_digest: cbor::opt_bytes_field(&view, 8)?
            .map(|bytes| {
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SyncError::RelayWire("rotation grants_digest 长度非法"))
            })
            .transpose()?,
        published_checkpoints: cbor::uint_field(&view, 9)?,
        control_rev: cbor::uint_field(&view, 10)?,
        auth_rev: cbor::uint_field(&view, 11)?,
        manager_rev: cbor::uint_field(&view, 12)?,
        from_epoch_revoked_once: cbor::uint_field(&view, 13)? != 0,
        freeze_asset_count: cbor::uint_field(&view, 15)?,
        freeze_asset_digest: opt_fixed32(&view, 16, "rotation asset_set_digest")?,
        member_cert_digest: opt_fixed32(&view, 17, "rotation member_cert_digest")?,
        primary_checkpoint: opt_fixed16(&view, 18, "rotation primary_checkpoint")?,
        fallback_checkpoint: opt_fixed16(&view, 19, "rotation fallback_checkpoint")?,
        activation_commit: cbor::opt_bytes_field(&view, 20)?,
        activated_at: cbor::opt_uint_field(&view, 21)?,
        activated_control_rev: cbor::opt_uint_field(&view, 22)?,
        candidate_count: cbor::array_field(&view, 23)?.len() as u64,
        candidate_ready: cbor::uint_field(&view, 26)? != 0,
        reservation_id: match view.iter().find(|(key, _)| *key == 25) {
            Some((_, cbor::CdeValue::Map(fields))) => Some(cbor::bytes_fixed(fields, 0)?),
            Some(_) => return Err(SyncError::RelayWire("rotation reservation 不是 map")),
            None => None,
        },
    })
}

fn opt_fixed32(
    fields: &cbor::Fields,
    key: u64,
    what: &'static str,
) -> Result<Option<[u8; 32]>, SyncError> {
    cbor::opt_bytes_field(fields, key)?
        .map(|bytes| {
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| SyncError::RelayWire(what))
        })
        .transpose()
}

fn opt_fixed16(
    fields: &cbor::Fields,
    key: u64,
    what: &'static str,
) -> Result<Option<[u8; 16]>, SyncError> {
    cbor::opt_bytes_field(fields, key)?
        .map(|bytes| {
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| SyncError::RelayWire(what))
        })
        .transpose()
}

fn parse_bootstrap_checkpoint(value: &cbor::CdeValue) -> Result<BootstrapCheckpoint, SyncError> {
    let fields = cbor::value_map(value)?;
    let manifest = cbor::opt_bytes_field(&fields, 3)?
        .ok_or(SyncError::RelayWire("bootstrap checkpoint 缺 manifest_oid"))?;
    let manifest_oid: ObjectId = manifest
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::RelayWire("bootstrap manifest_oid 长度非法"))?;
    let refs_digest = match cbor::opt_bytes_field(&fields, 4)? {
        None => None,
        Some(bytes) => Some(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| SyncError::RelayWire("bootstrap refs_digest 长度非法"))?,
        ),
    };
    let signature = match cbor::opt_bytes_field(&fields, 7)? {
        None => None,
        Some(bytes) => Some(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| SyncError::RelayWire("bootstrap checkpoint 签名长度非法"))?,
        ),
    };
    Ok(BootstrapCheckpoint {
        checkpoint_id: cbor::bytes_fixed(&fields, 0)?,
        epoch: cbor::uint_field(&fields, 1)?,
        covered_seq: cbor::uint_field(&fields, 2)?,
        manifest_oid,
        refs_digest,
        cursor: cbor::text_field(&fields, 5)?,
        signed_commit: cbor::opt_bytes_field(&fields, 6)?,
        signature,
    })
}

fn slot_kind_text(kind: menghunote_sync_proto::SlotKind) -> &'static str {
    match kind {
        menghunote_sync_proto::SlotKind::Password => "password",
        menghunote_sync_proto::SlotKind::Recovery => "recovery",
    }
}

fn parse_slot_kind(text: &str) -> Result<menghunote_sync_proto::SlotKind, SyncError> {
    match text {
        "password" => Ok(menghunote_sync_proto::SlotKind::Password),
        "recovery" => Ok(menghunote_sync_proto::SlotKind::Recovery),
        _ => Err(SyncError::RelayWire("未知槽类型")),
    }
}

fn parse_slot_record(fields: &cbor::Fields) -> Result<SlotRecord, SyncError> {
    Ok(SlotRecord {
        slot_id: cbor::bytes_fixed(fields, 0)?,
        epoch: cbor::uint_field(fields, 1)?,
        kind: parse_slot_kind(&cbor::text_field(fields, 2)?)?,
        descriptor: cbor::bytes_field(fields, 3)?,
        state: cbor::text_field(fields, 4)?,
        slot_set_rev: cbor::uint_field(fields, 5)?,
        retire_at: cbor::opt_uint_field(fields, 6)?,
    })
}

fn parse_update_outcome(fields: &cbor::Fields) -> Result<UpdateOutcome, SyncError> {
    let object_id: [u8; 32] = cbor::bytes_fixed(fields, 0)?;
    let status_text = cbor::text_field(fields, 1)?;
    let relay_seq = cbor::opt_uint_field(fields, 2)?;
    let code = cbor::opt_text_field(fields, 3)?;
    let retryable = cbor::bool_field(fields, 4)?;
    let status = match status_text.as_str() {
        "accepted" => UpdateStatus::Accepted { relay_seq },
        "duplicate" => UpdateStatus::Duplicate,
        "failed" => UpdateStatus::Failed { code, retryable },
        _ => return Err(SyncError::RelayWire("未知 update 结果状态")),
    };
    Ok(UpdateOutcome { object_id, status })
}

fn cbor_headers() -> Vec<(String, String)> {
    vec![("content-type".to_owned(), CONTENT_TYPE_CBOR.to_owned())]
}

/// 2xx 响应解 CBOR；非 2xx 解析错误体。
fn decode_ok(response: &HttpResponse) -> Result<cbor::Fields, SyncError> {
    if !(200..300).contains(&response.status) {
        return Err(error_from_response(response));
    }
    cbor::decode(&response.body)
}

/// 响应是否是 relay 的 `403 REVOKED`（`auth_rev` 前进导致的旧 token 失效，ADR 0010）。
///
/// 只认 CBOR 错误体的 `code == "REVOKED"`：其它 403（`FORBIDDEN`/`MANAGEMENT_REQUIRED`）
/// 不是"版本推进"，不能触发清缓存重认证。
fn is_revoked_response(response: &HttpResponse) -> bool {
    if response.status < 400 {
        return false;
    }
    match cbor::decode(&response.body) {
        Ok(fields) => matches!(cbor::text_field(&fields, 0).as_deref(), Ok("REVOKED")),
        Err(_) => false,
    }
}

/// 把 relay 的错误体映射成 [`SyncError::RelayApi`]。
pub(crate) fn error_from_response(response: &HttpResponse) -> SyncError {
    if let Ok(fields) = cbor::decode(&response.body) {
        let code = cbor::text_field(&fields, 0);
        let retryable = cbor::bool_field(&fields, 1);
        if let (Ok(code), Ok(retryable)) = (code, retryable) {
            return SyncError::RelayApi {
                code,
                retryable,
                current_epoch: cbor::opt_uint_field(&fields, 2).ok().flatten(),
                current_control_rev: cbor::opt_uint_field(&fields, 3).ok().flatten(),
                bootstrap_hint: cbor::opt_text_field(&fields, 4).ok().flatten(),
            };
        }
    }
    if response.status >= 500 {
        return SyncError::RelayApi {
            code: format!("HTTP_{}", response.status),
            retryable: true,
            current_epoch: None,
            current_control_rev: None,
            bootstrap_hint: None,
        };
    }
    SyncError::HttpStatus(response.status)
}

/// 解析 `/control`（可跨多页）：
///
/// 1. 管理公钥链（键 5）从**调用方提供的** `manager_pub` 信任锚起逐级验旧/新双签
///    （旧公钥永久保留）；`manager_pub` 缺失/管理链缺失一律 fail-stop，relay 的 rev1
///    只能与本地锚/链逐字节复核，**不得充当信任锚**（ADR §35.2）；
/// 2. 控制链逐条验证 `rev`/`prev_hash` 连续与 `SHA256(prev_hash || signed_record)`；
/// 3. **kind 只从 `signed_record` 内部（`SignedControlRecord` 键 0）读取**，外层键 4
///    只做一致性复核，绝不作为种类来源；
/// 4. 每条 typed 控制记录必须带管理签名，并按 kind 对应的请求 CDE（method/path/body）
///    重建 `management_signing_input` 后验签；`rotation.abort` 必须携带 typed
///    [`menghunote_sync_proto::RotationAborted`]，缺事件/无法重建 → fail-stop（§35.3）；
/// 5. 设备证书签名缺失或无效一律拒绝（`device_cert` 的信任根来自这条链）。
///
/// **分页（评审 0002 P0-2）**：relay `/control` 固定单页 256 条、用 `after_rev` 游标；
/// 本函数接受按游标顺序拉到的多页，逐页喂给 [`ControlVerifier`]，要求：
/// - 第一页必须从 rev=1 genesis 开始（`prev_hash` 全零），且 rev1 记录必须与
///   管理链 rev1 的 `transition` **逐字节相等**（替换 genesis 直接拒绝）；
/// - 每页的 `control_rev`/`epoch`/管理链完全一致；
/// - 每页内部与跨页的 `rev`、`prev_hash`、`record_hash` 连续；
/// - 最后一页链尾必须正好等于 `control_rev`（缺页/重排/截断直接拒绝）。
///
/// 兼容性：relay 的控制链还包含两类**旧裸记录**（rev 1 的创建设备证书、配对/槽激活的
/// signed message）。它们不是 `SignedControlRecord`，只记为 `raw`；其中**能解出
/// `DeviceCertV1` 且 vault 一致的**才是 genesis/approve 的已签加入事件，其余不构成
/// 任何授权事件（`epoch.activated` 必须是 typed 记录）。rev1 的 raw 记录不再有
/// "天然可信"例外：它必须与本地锚复算出的管理链 rev1 逐字节一致。
///
/// **active 成员集合（评审 0002 P0-2）**：只由已签控制链事实推导，relay 的
/// `member.status` **不是授权依据**：
/// 1. 初始成员来自 rev 1 genesis 的证书记录；
/// 2. 加入来自 genesis/approve 的裸证书记录（证书字节必须与成员条目逐字节一致）；
/// 3. 移除只认管理签名的 `rotation.begin` 的 `revoke[]` 与 `rotation.revoked-more`
///    的**已签 body**（事件字段只做一致性复核）；吊销永久累积，后续加入记录不能恢复；
/// 4. 无法证明 active 的设备一律排除（fail-closed）：没有已签加入、被已签吊销、
///    或 relay 声称 `revoked/expired/pending` 都不进 `active_certs`。
///
/// 证书签名（§17.1，按 `issued_manager_rev` 对应的历史管理公钥）仍是成员条目的
/// 准入前提；验签通过的吊销/过期证书保留在 `history_certs` 供 ≤S 历史验签，但绝不
/// 进入 active 集合。
///
/// **有效管理权（评审 0003 P0-A）**：[`ControlVerifier`] 逐条维护
/// [`EffectiveAuthority`]（当前有效管理 rev/pubkey + epoch）：
/// - 起点固定为 genesis 的 rev=1 管理公钥与 epoch=1；
/// - 管理链里更晚的 `ManagerTransitionV1` 即使旧/新双签有效，在对应 activation T
///   出现前也**不产生**任何授权（不得拿来验新记录、不得给新证书记入加入事件）；
/// - 每条 typed 控制记录必须由**当时有效**的 manager 签名（`manager_rev` 精确相等）；
/// - 每条 `epoch.activated` 的 `from_epoch` 必须等于当时有效 epoch；只有携带完整
///   activation T 并复算通过后，才把管理权推进到 `T.new_manager_rev/new_manager_pubkey`；
/// - 裸 `DeviceCertV1` 加入事件只在该证书的 `issued_manager_rev` 等于当时有效管理
///   rev 时才记入成员事实。历史证书仍按各自签发 rev 验签并保留在 `history_certs`，
///   但**不产生新的授权**。
///
/// **T.prev 绑定（评审 0003 P0-A）**：activation T 必须把自身钉在控制链的实际位置
/// 上——`T.expected_control_rev` 等于该记录 rev，`T.prev_control_hash` 等于 rev-1
/// 的实际 `record_hash`；`T.old_manager_rev`/旧管理签名公钥、`T.from_epoch` 与
/// `T.new_manager_pubkey`/管理转换也必须匹配该位置的有效 authority。任一不符
/// 一律 fail-stop，绝不进入成员集合 / HPKE keys / grant 规划。
fn parse_control(
    pages: &[cbor::Fields],
    vault_id: &VaultId,
    instance_id: &[u8; 16],
    manager_pub: &[u8; 32],
    auth_rev: u64,
) -> Result<ControlState, SyncError> {
    let first = pages
        .first()
        .ok_or(SyncError::RelayWire("控制链分页为空"))?;
    // rev1 genesis 必须从第一页第一条读出：它是管理链 rev1 的逐字节复核对象。
    let genesis = genesis_record(first)?;
    // 键 5：管理权转换链（rev 1 是 genesis 公钥，无 transition/签名）。
    let manager_chain = parse_manager_chain(first)?;
    let verified_chain =
        verify_manager_chain(&manager_chain, manager_pub, vault_id, instance_id, &genesis)?;
    let manager_keys = &verified_chain.keys;

    let mut verifier = ControlVerifier {
        manager_keys,
        manager_transitions: &verified_chain.transitions,
        vault_id,
        instance_id,
        genesis: &genesis,
        prev_hash: [0u8; 32],
        last_rev: 0,
        head_rev: None,
        epoch: None,
        kinds: Vec::new(),
        record_hashes: HashMap::new(),
        verified_activation_t: HashMap::new(),
        activation_revs: HashMap::new(),
        members: SignedMemberFacts::default(),
        // genesis 固定由管理链 rev1 授权；epoch 从 1 起（relay 初始化即 epoch=1）。
        authority: EffectiveAuthority {
            manager_rev: 1,
            manager_pub: *manager_pub,
            epoch: 1,
        },
    };
    for page in pages {
        // 管理链是信任锚：分页期间任何一页换了管理链都拒绝（不接受中途换 key）。
        if parse_manager_chain(page)? != manager_chain {
            return Err(SyncError::RelayWire("控制链分页的管理链不一致"));
        }
        verifier.verify_page(page)?;
    }
    let head_rev = verifier
        .head_rev
        .ok_or(SyncError::RelayWire("控制链分页缺少 head rev"))?;
    if verifier.last_rev != head_rev {
        return Err(SyncError::RelayWire("control_rev 与链尾不一致"));
    }

    let mut history_certs = HashMap::new();
    let mut active_certs = HashMap::new();
    let mut member_cert_inputs = Vec::new();
    // 只读成员投影（ADR 0010 切片 2a-1）：与授权集合同源解析，但不回写任何授权事实。
    let mut members = Vec::new();
    let last = pages.last().ok_or(SyncError::RelayWire("控制链分页为空"))?;
    for member in cbor::array_field(last, 3).map_err(|_| SyncError::RelayWire("control members"))? {
        let member =
            cbor::value_map(&member).map_err(|_| SyncError::RelayWire("control member map"))?;
        let device_id: [u8; 16] =
            cbor::bytes_fixed(&member, 0).map_err(|_| SyncError::RelayWire("control member id"))?;
        let certificate_bytes = cbor::bytes_field(&member, 1)
            .map_err(|_| SyncError::RelayWire("control member cert"))?;
        let status = cbor::text_field(&member, 2)
            .map_err(|_| SyncError::RelayWire("control member status"))?;
        // `expired` 为未来/外部 relay 预留（当前 relay schema 只有 pending/active/revoked）。
        // status 只做**否决**（fail-closed），不是授权来源：relay 把已吊销证书改回
        // active 也不会让它进入 active 集合，真正授权见下方的已签事实复算。
        if !matches!(
            status.as_str(),
            "active" | "revoked" | "expired" | "pending"
        ) {
            return Err(SyncError::RelayWire("control member status 未知"));
        }
        let sign_pk: [u8; 32] = cbor::bytes_fixed(&member, 3)
            .map_err(|_| SyncError::RelayWire("control member sign_pk"))?;
        let hpke_pk: [u8; 32] = cbor::bytes_fixed(&member, 4)
            .map_err(|_| SyncError::RelayWire("control member hpke_pk"))?;
        let signature = cbor::opt_bytes_field(&member, 5)?;
        // 待批准设备在 relay 里是空证书占位（`certificate` 列 X''、无签名）：既不是
        // active 成员也没有历史记录，跳过；不能让一个 pending 邀请拖垮整条控制链。
        // 其它状态（active/revoked/expired）的空证书是数据损坏：无法验历史，必须 fail-stop。
        if certificate_bytes.is_empty() {
            if signature.is_some() {
                return Err(SyncError::RelayWire("成员证书为空但带管理签名"));
            }
            if status == "pending" {
                // 成员视图保留这条 pending 记录（无指纹/无管理 rev），供设备列表显示
                // "待批准"；它仍然不构成任何授权事实。
                members.push(MemberView {
                    device_id,
                    status: MemberStatus::Pending,
                    fingerprint: None,
                    sign_pk,
                    hpke_pk,
                    issued_manager_rev: None,
                });
                continue;
            }
            return Err(SyncError::RelayWire("非待批准成员证书为空"));
        }
        let cert = DeviceCertV1::decode(&certificate_bytes)
            .map_err(|_| SyncError::RelayWire("成员证书不是合法 DeviceCertV1"))?;
        if cert.vault_id != *vault_id
            || cert.device_id != device_id
            || cert.sign_ed25519_pub != sign_pk
            || cert.hpke_x25519_pub != hpke_pk
        {
            return Err(SyncError::RelayWire("成员证书与成员字段不一致"));
        }
        // §17.1：证书必须由**签发时的管理公钥**（管理链上的 rev）外签。
        // active/吊销/待批准只要带真实证书就强制验签：吊销证书仍用于验证 ≤S 的历史
        // 生产者签名，未验签的证书不能进入 `history_certs`。
        let manager_key =
            manager_keys
                .get(&cert.issued_manager_rev)
                .ok_or(SyncError::RelayWire(
                    "成员证书的 issued_manager_rev 不在管理链上",
                ))?;
        let Some(signature) = signature else {
            return Err(SyncError::RelayWire(if status == "active" {
                "成员证书缺管理签名（§17.1）"
            } else {
                "历史成员证书缺管理签名（无法验证 ≤S 记录）"
            }));
        };
        let signature_bytes: [u8; 64] = signature
            .as_slice()
            .try_into()
            .map_err(|_| SyncError::RelayWire("证书签名不是 64 字节"))?;
        let signed_input = device_cert_signed_bytes(&cert)?;
        verify_signature(manager_key, &signed_input, &signature_bytes)?;
        history_certs.insert(device_id, cert.clone());
        // active 集合 = 已签加入 ∩ 未被已签吊销 ∩ relay status 未否决（fail-closed）。
        // 关键：`status == "active"` 只是必要条件，**不能**单独把设备变成 active；
        // 恶意 relay 把已吊销证书的 status 改回 active 时，`signed_active` 仍为 false。
        let signed_active = verifier
            .members
            .signed_active(&device_id, &certificate_bytes);
        // 成员视图是同一轮解析的**只读投影**：`Active` 与 `active_certs` 用同一个谓词，
        // 已签吊销（永久事实）优先于 relay status；其余无法证明 active 的一律 fail-closed
        // （不显示 Active，避免设备列表把恶意 relay 的 status 翻转当事实）。
        let member_status = if verifier.members.is_revoked(&device_id) || status == "revoked" {
            MemberStatus::Revoked
        } else if status == "pending" {
            // 带真实证书的 pending（当前 relay 不会产生）仍按待批准显示。
            MemberStatus::Pending
        } else if status == "expired" {
            // `expired` 按既有语义保留：证书进 `history_certs`，但不进 active 集合。
            MemberStatus::Expired
        } else if status == "active" && signed_active {
            MemberStatus::Active
        } else {
            MemberStatus::Revoked
        };
        members.push(MemberView {
            device_id,
            status: member_status,
            fingerprint: Some(
                crate::device::device_fingerprint(
                    vault_id,
                    &device_id,
                    &cert.sign_ed25519_pub,
                    &cert.hpke_x25519_pub,
                )
                .to_hex(),
            ),
            sign_pk,
            hpke_pk,
            issued_manager_rev: Some(cert.issued_manager_rev),
        });
        if status == "active" && signed_active {
            member_cert_inputs.push(menghunote_sync_proto::MemberCertDigestInputV1 {
                device_id,
                certificate: certificate_bytes,
                certificate_signature: Some(signature),
            });
            active_certs.insert(device_id, cert);
        }
        // 否则：证书留在 history_certs（供 ≤S 历史验签），但不进 active。
    }
    // 按 `device_id` 稳定排序（HashMap 的遍历序随机；设备列表/测试都需要确定性）。
    members.sort_by_key(|member| member.device_id);
    Ok(ControlState {
        instance_id: *instance_id,
        control_rev: head_rev,
        epoch: verifier
            .epoch
            .ok_or(SyncError::RelayWire("控制链分页缺少 epoch"))?,
        auth_rev,
        history_certs,
        active_certs,
        kinds: verifier.kinds,
        member_cert_inputs,
        head_hash: verifier.prev_hash,
        record_hashes: verifier.record_hashes,
        manager_chain,
        verified_activation_t: verifier.verified_activation_t,
        activation_revs: verifier.activation_revs,
        members,
    })
}

/// 分页控制链的校验状态机：跨页维护 `prev_hash`/`last_rev`/kind/activation 证据。
struct ControlVerifier<'a> {
    manager_keys: &'a HashMap<u64, [u8; 32]>,
    /// 管理链上每条 `ManagerTransitionV1` 的解码结果（`rev → transition`）。
    manager_transitions: &'a HashMap<u64, ManagerTransitionV1>,
    vault_id: &'a VaultId,
    instance_id: &'a [u8; 16],
    /// rev1 genesis 记录原字节（必须与管理链 rev1 的 `transition` 逐字节相等）。
    genesis: &'a [u8],
    prev_hash: [u8; 32],
    last_rev: u64,
    /// 第一页的 `control_rev`（relay 的 vault head）；后续页必须一致。
    head_rev: Option<u64>,
    /// 第一页的 `epoch`；后续页必须一致。
    epoch: Option<u64>,
    kinds: Vec<(u64, String)>,
    record_hashes: HashMap<u64, [u8; 32]>,
    verified_activation_t: HashMap<[u8; 16], VerifiedActivationT>,
    /// activation T 所在控制记录的 rev（`T.expected_control_rev`）。
    activation_revs: HashMap<[u8; 16], u64>,
    /// 从已验签控制链复算出的成员事实（active 集合的唯一授权来源）。
    members: SignedMemberFacts,
    /// 当前位置的**有效**管理权/epoch；逐条推进，见 [`EffectiveAuthority`]。
    authority: EffectiveAuthority,
}

/// 控制链上的**当前有效管理权**（评审 0003 P0-A）。
///
/// - `manager_rev`/`manager_pub`：从 genesis rev1 的信任锚开始。管理链里更晚的
///   `ManagerTransitionV1` 即使旧/新双签有效，也只是"候选管理权"，必须等到携带
///   完整 activation T 的 `epoch.activated` 复算通过后才推进（`T.old_manager_rev`
///   等于当前 rev、`T.new_manager_rev` 对应管理链条目、`T.prev_control_hash` 钉住
///   实际链尾）。这意味着历史管理私钥（旧 rev）不能给前缀之后的新对象授权。
/// - `epoch`：当前有效 epoch。每条 `epoch.activated` 的 `from_epoch` 必须与它相等；
///   验证后推进到 `to_epoch`（历史无 T 记录仍可推进 epoch，但**不能**推进管理权）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EffectiveAuthority {
    manager_rev: u32,
    manager_pub: [u8; 32],
    epoch: u64,
}

impl ControlVerifier<'_> {
    /// 验证一页：head/epoch 一致、页内 `rev`/hash 连续且签名有效。
    fn verify_page(&mut self, fields: &cbor::Fields) -> Result<(), SyncError> {
        let head = cbor::uint_field(fields, 1).map_err(|_| SyncError::RelayWire("control rev"))?;
        let epoch =
            cbor::uint_field(fields, 2).map_err(|_| SyncError::RelayWire("control epoch"))?;
        match self.head_rev {
            Some(expected) if expected != head => {
                return Err(SyncError::RelayWire(
                    "控制链分页的 control_rev 不一致（重排/替换）",
                ))
            }
            None => self.head_rev = Some(head),
            _ => {}
        }
        match self.epoch {
            Some(expected) if expected != epoch => {
                return Err(SyncError::RelayWire("控制链分页的 epoch 不一致"))
            }
            None => self.epoch = Some(epoch),
            _ => {}
        }
        let commits =
            cbor::array_field(fields, 0).map_err(|_| SyncError::RelayWire("control commits"))?;
        if commits.is_empty() {
            if self.last_rev != head {
                return Err(SyncError::RelayWire("控制链分页缺页（空页但未到 head）"));
            }
            return Ok(());
        }
        for commit in &commits {
            self.verify_commit(commit, head)?;
        }
        Ok(())
    }

    /// 验证并吸收一条 commit；所有失败都返回 [`SyncError::RelayWire`]（fail-stop）。
    fn verify_commit(&mut self, commit: &cbor::CdeValue, head: u64) -> Result<(), SyncError> {
        let commit =
            cbor::value_map(commit).map_err(|_| SyncError::RelayWire("control commit map"))?;
        let rev =
            cbor::uint_field(&commit, 0).map_err(|_| SyncError::RelayWire("control commit rev"))?;
        let stored_prev: [u8; 32] = cbor::bytes_fixed(&commit, 1)
            .map_err(|_| SyncError::RelayWire("control commit prev"))?;
        let stored_hash: [u8; 32] = cbor::bytes_fixed(&commit, 2)
            .map_err(|_| SyncError::RelayWire("control commit hash"))?;
        let signed_record = cbor::bytes_field(&commit, 3)
            .map_err(|_| SyncError::RelayWire("control commit signed"))?;
        if rev != self.last_rev.saturating_add(1) {
            return Err(SyncError::RelayWire("控制链 rev 不连续"));
        }
        if rev > head {
            return Err(SyncError::RelayWire("控制记录 rev 超过 control_rev"));
        }
        if rev > MAX_CONTROL_COMMITS {
            return Err(SyncError::RelayWire("控制链记录数超过安全上限"));
        }
        if stored_prev != self.prev_hash {
            return Err(SyncError::RelayWire("控制链 prev_hash 不连续"));
        }
        // relay 统一用 `SHA256(prev_hash || signed_record)`（genesis 也是全零 prev），
        // 不接受"自洽但不同公式"的 hash（自洽伪造链不能当授权链）。
        let mut input = Vec::with_capacity(64 + signed_record.len());
        input.extend_from_slice(&self.prev_hash);
        input.extend_from_slice(&signed_record);
        let computed: [u8; 32] = Sha256::digest(&input).into();
        if computed != stored_hash {
            return Err(SyncError::RelayWire("控制链 record_hash 不符"));
        }
        // rev 1 是 genesis：signed_record 就是创建设备证书 CDE，不是 typed 控制记录。
        // §35.2：不再有"无签名 genesis 天然可信"的 raw 例外——rev1 必须与调用方
        // 锚点/管理链逐字节一致，且不得伪装成 typed 记录（不得充当信任锚）。
        let kind = if rev == 1 {
            if stored_prev != [0u8; 32] {
                return Err(SyncError::RelayWire("rev1 genesis 的 prev_hash 必须全零"));
            }
            if signed_record != self.genesis {
                return Err(SyncError::RelayWire("rev1 genesis 与本地 pin/管理链不一致"));
            }
            // relay 对 raw 记录返回 `kind="raw"`（不是省略）；只允许这个显式 raw 标记，
            // 其他 typed kind 一律视为伪装。
            if let Some(kind) = cbor::opt_text_field(&commit, 4)? {
                if kind != "raw" {
                    return Err(SyncError::RelayWire("rev1 genesis 不得带 typed kind"));
                }
            }
            "raw".to_owned()
        } else {
            match SignedControlRecord::decode(&signed_record) {
                Ok(record) => {
                    // 外层 kind（键 4）只做复核；真实种类来自被签记录内部的键 0。
                    let outer_kind = cbor::opt_text_field(&commit, 4)?;
                    if let Some(outer_kind) = outer_kind {
                        if outer_kind != record.kind.as_str() {
                            return Err(SyncError::RelayWire("控制记录外层 kind 与签名记录不一致"));
                        }
                    }
                    let effects = verify_typed_control_record(
                        &record,
                        self.manager_keys,
                        self.manager_transitions,
                        self.authority,
                        self.prev_hash,
                        self.vault_id,
                        self.instance_id,
                        rev,
                    )?;
                    // 已签吊销是永久事实（ADR §8）：不因后续加入记录而恢复。
                    for device_id in effects.revoked {
                        self.members.record_revocation(device_id);
                    }
                    if let Some((rotation_id, evidence)) = effects.activation {
                        self.verified_activation_t.insert(rotation_id, evidence);
                        self.activation_revs.insert(rotation_id, rev);
                    }
                    // 只有携带完整 T 的 `epoch.activated` 才推进管理权；历史无 T 记录
                    // 最多推进 epoch（见 `EffectiveAuthority`）。
                    if let Some(authority) = effects.next_authority {
                        self.authority = authority;
                    }
                    record.kind.as_str().to_owned()
                }
                Err(_) => {
                    // 旧 relay 的裸 signed 记录（创建设备证书 / 配对 / 槽激活等）。
                    match cbor::opt_text_field(&commit, 4)? {
                        Some(outer_kind) if outer_kind != "raw" => {
                            return Err(SyncError::RelayWire(
                                "控制记录不是合法 SignedControlRecord",
                            ))
                        }
                        _ => "raw".to_owned(),
                    }
                }
            }
        };
        if kind == "raw" {
            self.record_raw_member_event(&signed_record)?;
        }
        self.record_hashes.insert(rev, stored_hash);
        self.prev_hash = stored_hash;
        self.last_rev = rev;
        self.kinds.push((rev, kind));
        Ok(())
    }

    /// 旧裸记录里只有**设备证书**构成已签加入事件（rev 1 genesis / relay `approve`
    /// 都追加 `DeviceCertV1` CDE 原字节）；槽激活等消息不是管理授权。
    ///
    /// 这里只记录"链上出现过该证书"；该设备是否真的 active 还要在 [`parse_control`]
    /// 里用成员条目外签通过的管理签名复核，并由已签吊销事件收窄。
    ///
    /// **有效授权（评审 0003 P0-A）**：加入事件本质上是"某把管理 key 为设备签发了
    /// 证书"，因此必须由**记录当时有效**的管理 rev 签发。历史 rev 的证书可以留在
    /// `history_certs` 供 ≤S 旧对象验签，但出现在链上时不产生任何新的成员授权——
    /// 这里直接 fail-stop，而不是"先收下再忽略"，避免后续任何消费路径误用。
    fn record_raw_member_event(&mut self, signed_record: &[u8]) -> Result<(), SyncError> {
        let Ok(cert) = DeviceCertV1::decode(signed_record) else {
            return Ok(());
        };
        if cert.vault_id != *self.vault_id {
            return Ok(());
        }
        if cert.issued_manager_rev != u64::from(self.authority.manager_rev) {
            return Err(SyncError::RelayWire(
                "成员加入证书的 issued_manager_rev 不是当前有效管理权",
            ));
        }
        self.members.record_join(cert.device_id, signed_record);
        Ok(())
    }
}

/// 从第一页读出 rev1 genesis 记录原字节。
///
/// `refresh_control` 始终从 `after_rev=0` 拉全链，因此第一页第一条必须是 rev=1、
/// `prev_hash` 全零；任何"从中间页开始"的链都不能当作完整控制链。
fn genesis_record(first: &cbor::Fields) -> Result<Vec<u8>, SyncError> {
    let commits =
        cbor::array_field(first, 0).map_err(|_| SyncError::RelayWire("control commits"))?;
    let commit = commits
        .first()
        .ok_or(SyncError::RelayWire("控制链缺少 rev1 genesis"))?;
    let commit = cbor::value_map(commit).map_err(|_| SyncError::RelayWire("control commit map"))?;
    let rev =
        cbor::uint_field(&commit, 0).map_err(|_| SyncError::RelayWire("control commit rev"))?;
    if rev != 1 {
        return Err(SyncError::RelayWire("控制链必须从 rev=1 genesis 开始"));
    }
    let prev: [u8; 32] =
        cbor::bytes_fixed(&commit, 1).map_err(|_| SyncError::RelayWire("control commit prev"))?;
    if prev != [0u8; 32] {
        return Err(SyncError::RelayWire("rev1 genesis 的 prev_hash 必须全零"));
    }
    cbor::bytes_field(&commit, 3).map_err(|_| SyncError::RelayWire("control commit signed"))
}

/// 解析 `/control` 键 5 的管理权转换链。
fn parse_manager_chain(fields: &cbor::Fields) -> Result<Vec<ManagerChainEntry>, SyncError> {
    cbor::opt_array_field(fields, 5)?
        .unwrap_or_default()
        .iter()
        .map(parse_manager_chain_entry)
        .collect()
}

/// 已复算通过的管理权转换链。
///
/// - `keys`：`rev → 管理公钥`（rev1 必为调用方信任锚）；
/// - `transitions`：`rev → ManagerTransitionV1`（rev1 无 transition，故从 rev2 起）。
///
/// 注意：管理链里存在某 rev 的公钥/转换**不等于**该管理权已经生效；生效只由控制链上
/// 携带完整 T 的 `epoch.activated` 推进（见 [`EffectiveAuthority`]）。
#[derive(Debug)]
struct VerifiedManagerChain {
    keys: HashMap<u64, [u8; 32]>,
    transitions: HashMap<u64, ManagerTransitionV1>,
}

/// 校验 `/control` 键 5 的管理权转换链，返回 `rev → 管理公钥` 与转换原文。
///
/// §35.2 的硬要求：
/// - `anchor` 必须来自调用方 identity/pin（已由 [`HttpRelayClient::refresh_control`]
///   强制为 `Some`），relay **不能**靠"缺少管理链"把 rev1 当成信任锚；
/// - 管理链 rev1 的 `transition` 必须与 rev1 控制记录**逐字节相等**（替换 genesis
///   或拿别的裸记录冒充 genesis 都直接拒绝）；
/// - 后续 rev 的旧/新双签必须能从 `anchor` 一路连到链尾。
fn verify_manager_chain(
    chain: &[ManagerChainEntry],
    anchor: &[u8; 32],
    vault_id: &VaultId,
    instance_id: &[u8; 16],
    genesis: &[u8],
) -> Result<VerifiedManagerChain, SyncError> {
    let mut keys = HashMap::new();
    let mut transitions = HashMap::new();
    // 旧 relay 可能不带管理链：此时无法复核 rev1，必须 fail-stop（不得退回"信任 rev1"）。
    if chain.is_empty() {
        return Err(SyncError::RelayWire(
            "控制链缺少管理权转换链（无法复核 genesis 信任锚）",
        ));
    }
    let first = chain
        .first()
        .ok_or(SyncError::RelayWire("控制链缺少管理权转换链"))?;
    if first.rev != 1 {
        return Err(SyncError::RelayWire("管理链必须从 rev=1 genesis 开始"));
    }
    if first.pubkey != *anchor {
        return Err(SyncError::RelayWire("管理链 rev=1 公钥与信任锚不符"));
    }
    if first.transition.as_slice() != genesis {
        return Err(SyncError::RelayWire(
            "管理链 rev=1 transition 与 rev1 控制记录不一致（genesis 被替换）",
        ));
    }
    keys.insert(1, first.pubkey);
    let mut prev_rev = 1u64;
    let mut prev_pub = *anchor;
    for entry in chain.iter().skip(1) {
        let transition = ManagerTransitionV1::decode(&entry.transition)?;
        if transition.vault_id != *vault_id
            || transition.instance_id != *instance_id
            || transition.old_rev != prev_rev
            || transition.new_rev != entry.rev
            || transition.new_rev != transition.old_rev.saturating_add(1)
            // restart 会保留原 from 并分配更高的候选 epoch（ADR 0006 §33），因此与
            // `ActivationTV1::validate` 同口径只要求 to > from，不要求恰好 +1。
            || transition.to_epoch <= transition.from_epoch
            || transition.new_pubkey != entry.pubkey
            || entry.old_rev != prev_rev
        {
            return Err(SyncError::RelayWire("管理转换链字段/rev 不连续"));
        }
        let old_signature = entry
            .old_signature
            .ok_or(SyncError::RelayWire("管理转换链缺旧管理签名"))?;
        let new_signature = entry
            .new_signature
            .ok_or(SyncError::RelayWire("管理转换链缺新管理签名"))?;
        verify_signature(&prev_pub, &entry.transition, &old_signature)?;
        verify_signature(&entry.pubkey, &entry.transition, &new_signature)?;
        transitions.insert(entry.rev, transition);
        keys.insert(entry.rev, entry.pubkey);
        prev_rev = entry.rev;
        prev_pub = entry.pubkey;
    }
    // 链必须从信任锚出发（rev 1 与锚一致）。
    if keys.get(&1) != Some(anchor) {
        return Err(SyncError::RelayWire("管理链缺 rev=1 或与信任锚不符"));
    }
    Ok(VerifiedManagerChain { keys, transitions })
}

/// 一条 typed 控制记录在**管理签名验签通过后**才允许消费的授权效果。
#[derive(Debug, Default)]
struct VerifiedControlEffects {
    /// `epoch.activated` 里复算通过的 activation T 证据（历史无 T 记录为 `None`）。
    activation: Option<([u8; 16], VerifiedActivationT)>,
    /// 已签吊销事件命中的设备：`rotation.begin` 的 `revoke[]` 与
    /// `rotation.revoked-more` 的**已签 body**（事件字段不参与管理签名）。
    revoked: Vec<DeviceId>,
    /// 本记录验证通过后推进到的有效管理权/epoch（仅 `epoch.activated` 产生；
    /// 无 T 的历史记录只推进 epoch，不推进管理 rev/pubkey）。
    next_authority: Option<EffectiveAuthority>,
}

/// 校验一条 typed 控制记录：expected_rev 连续、由**当时有效**的管理权签名，签名覆盖
/// `management_signing_input(instance, vault, request_id, expected_rev, SHA256(request CDE))`。
///
/// 返回的 [`VerifiedControlEffects`] 只在验签成功后产生；调用方据此复算 active 集合，
/// 并用 `next_authority` 推进 [`ControlVerifier::authority`]。
#[allow(clippy::too_many_arguments)]
fn verify_typed_control_record(
    record: &SignedControlRecord,
    manager_keys: &HashMap<u64, [u8; 32]>,
    manager_transitions: &HashMap<u64, ManagerTransitionV1>,
    authority: EffectiveAuthority,
    previous_record_hash: [u8; 32],
    vault_id: &VaultId,
    instance_id: &[u8; 16],
    rev: u64,
) -> Result<VerifiedControlEffects, SyncError> {
    if record.expected_control_rev.checked_add(1) != Some(rev) {
        return Err(SyncError::RelayWire(
            "控制记录 expected_control_rev 与链 rev 不连续",
        ));
    }
    // 评审 0003 P0-A：历史管理 key 即使仍在管理链上，也不是当前有效授权来源。
    if record.manager_rev != u64::from(authority.manager_rev) {
        return Err(SyncError::RelayWire(
            "控制记录 manager_rev 不是当前有效管理权",
        ));
    }
    let manager_key = manager_keys
        .get(&record.manager_rev)
        .ok_or(SyncError::RelayWire("控制记录 manager_rev 不在管理链上"))?;
    if *manager_key != authority.manager_pub {
        return Err(SyncError::RelayWire(
            "控制记录 manager_rev 与当前有效管理公钥不一致",
        ));
    }
    if record.signature.len() != 64 {
        return Err(SyncError::RelayWire("控制记录管理签名长度非法"));
    }
    let signature: [u8; 64] = record
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::RelayWire("控制记录管理签名长度非法"))?;
    let vault_path = format!("/v1/vaults/{}", base64url::encode(vault_id));
    let event = record.decoded_event()?;
    let mut effects = VerifiedControlEffects::default();
    let path = match record.kind {
        ControlKind::RotationBegin => {
            // 冻结时的吊销事实在**已签** canonical body 的 `revoke[]` 里；空 body 是
            // 历史/测试形态（等价于本次不吊销），签名覆盖 body，因此无法被 relay 截断。
            if !record.signed_body.is_empty() {
                let body = RotationBeginBodyV1::decode(&record.signed_body)
                    .map_err(|_| SyncError::RelayWire("rotation.begin body 无法解码"))?;
                effects.revoked = body.revoke;
            }
            format!("{vault_path}/rotations")
        }
        ControlKind::EpochActivated => {
            let Some(ControlEvent::EpochActivated(event)) = event else {
                return Err(SyncError::RelayWire("epoch.activated 缺 typed 事件"));
            };
            if event.vault_id != *vault_id
                || event.instance_id != *instance_id
                || event.control_rev != rev
                || event.to_epoch <= event.from_epoch
                || event.primary_checkpoint == event.fallback_checkpoint
            {
                return Err(SyncError::RelayWire("epoch.activated 事件字段不符"));
            }
            // 有效 epoch 必须首尾相接：不能用一条 from_epoch 对不上的激活记录
            // 把链"接"到另一个 epoch 上（含恶意 relay 重排/跳段）。
            if event.from_epoch != authority.epoch {
                return Err(SyncError::RelayWire(
                    "epoch.activated 的 from_epoch 与当前有效 epoch 不符",
                ));
            }
            // §34/P0-A：存在 T 就必须能复算并通过双签验证；历史记录不带 T 时只接受结构，
            // 但不产生“已验证”证据，也**不能**推进管理权（只推进 epoch）。
            let verified = verify_epoch_activated_t(
                &event,
                manager_keys,
                manager_transitions,
                authority,
                previous_record_hash,
                vault_id,
                instance_id,
            )?;
            effects.next_authority = Some(match verified {
                Some((rotation_id, evidence)) => {
                    let next = EffectiveAuthority {
                        manager_rev: evidence.activation_t.new_manager_rev,
                        manager_pub: evidence.activation_t.new_manager_pubkey,
                        epoch: evidence.activation_t.to_epoch,
                    };
                    effects.activation = Some((rotation_id, evidence));
                    next
                }
                None => EffectiveAuthority {
                    manager_rev: authority.manager_rev,
                    manager_pub: authority.manager_pub,
                    epoch: event.to_epoch,
                },
            });
            format!(
                "{vault_path}/rotations/{}/activate",
                base64url::encode(&event.rotation_id)
            )
        }
        ControlKind::RotationRestarted => {
            let Some(ControlEvent::RotationRestarted(event)) = event else {
                return Err(SyncError::RelayWire("rotation.restarted 缺 typed 事件"));
            };
            if event.old_rotation == event.new_rotation || event.new_to_epoch <= event.old_to_epoch
            {
                return Err(SyncError::RelayWire("rotation.restarted 事件字段不符"));
            }
            format!(
                "{vault_path}/rotations/{}/restart",
                base64url::encode(&event.old_rotation)
            )
        }
        ControlKind::RevocationsAdded => {
            let Some(ControlEvent::RevocationsAdded(event)) = event else {
                return Err(SyncError::RelayWire("rotation.revoked-more 缺 typed 事件"));
            };
            if event.devices.is_empty() {
                return Err(SyncError::RelayWire("rotation.revoked-more 事件为空"));
            }
            // 事件字段（含 devices）不在管理签名覆盖范围内：唯一已签事实是
            // canonical body `{0: revoke[]}`。设备列表只认 body，事件只做一致性复核。
            let revoked = parse_revoked_more_body(&record.signed_body)?;
            if revoked != event.devices {
                return Err(SyncError::RelayWire(
                    "rotation.revoked-more 事件与已签 body 不一致",
                ));
            }
            effects.revoked = revoked;
            format!(
                "{vault_path}/rotations/{}/revoke-more",
                base64url::encode(&event.rotation_id)
            )
        }
        // §35.3：abort 必须携带能把被签输入完整重建出来的 typed 事件。
        //
        // 事件里的 path/method 是管理签名覆盖的公开事实；`signed_body` 是
        // `{0: revoke[]}` 之外的 abort canonical body（空 map）。客户端不信任 relay
        // 自报字段：path 必须能由 event.rotation_id 规范化重建，body_digest 必须等于
        // `SHA256(record.signed_body)`，然后统一走下面的 `management_signing_input` 验签。
        // 任一字段缺失/不一致都 fail-stop，绝不把 abort 当成"已验证"。
        ControlKind::RotationAbort => {
            let Some(ControlEvent::RotationAborted(event)) = event else {
                return Err(SyncError::RelayWire(
                    "rotation.abort 缺 typed 事件（无法重建管理签名输入）",
                ));
            };
            if event.method != HttpMethod::Post {
                return Err(SyncError::RelayWire("rotation.abort 的 method 必须是 POST"));
            }
            // 被签 path 必须与 event.rotation_id 规范化一致（篡改 rid 或 path 都拒绝）。
            let expected_path = format!(
                "{vault_path}/rotations/{}/abort",
                base64url::encode(&event.rotation_id)
            );
            if event.path != expected_path {
                return Err(SyncError::RelayWire(
                    "rotation.abort 事件 path 与 rotation id 不符",
                ));
            }
            // abort 的 canonical body 只能是空 map（relay 也拒绝非空 body）。
            RotationAbortBodyV1::decode(&record.signed_body)
                .map_err(|_| SyncError::RelayWire("rotation.abort 的 canonical body 非空"))?;
            let body_digest: [u8; 32] = Sha256::digest(&record.signed_body).into();
            if event.body_digest != body_digest {
                return Err(SyncError::RelayWire(
                    "rotation.abort 事件的 body_digest 与已签 body 不符",
                ));
            }
            expected_path
        }
    };
    let body_digest: [u8; 32] = Sha256::digest(management_request_cde(
        HttpMethod::Post,
        &path,
        &record.signed_body,
    )?)
    .into();
    let input = management_signing_input(
        instance_id,
        vault_id,
        &record.request_id,
        record.expected_control_rev,
        &body_digest,
    )?;
    verify_signature(manager_key, &input, &signature)?;
    Ok(effects)
}

/// 解码 `rotation.revoked-more` 的已签 canonical body `{0: revoke[]}`。
///
/// 与客户端 `rotation_stage::revoke_more` 及 relay `CanonicalBody::parse(.., 0)` 对齐；
/// 只接受恰好一个键 0，避免用"另一个形状的自洽 body"绕过吊销语义。
fn parse_revoked_more_body(body: &[u8]) -> Result<Vec<DeviceId>, SyncError> {
    let fields = cbor::decode(body)
        .map_err(|_| SyncError::RelayWire("rotation.revoked-more body 无法解码"))?;
    if fields.len() != 1 || fields[0].0 != 0 {
        return Err(SyncError::RelayWire(
            "rotation.revoked-more body 不是 canonical {0: revoke[]}",
        ));
    }
    let values = cbor::array_field(&fields, 0)
        .map_err(|_| SyncError::RelayWire("rotation.revoked-more body 缺 revoke[]"))?;
    let mut revoked = Vec::with_capacity(values.len());
    for value in values {
        let bytes = cbor::value_bytes(&value)
            .map_err(|_| SyncError::RelayWire("rotation.revoked-more device 不是 bytes"))?;
        let device_id: DeviceId = bytes
            .as_slice()
            .try_into()
            .map_err(|_| SyncError::RelayWire("rotation.revoked-more device 不是 16 字节"))?;
        revoked.push(device_id);
    }
    if revoked.is_empty() {
        return Err(SyncError::RelayWire(
            "rotation.revoked-more body 的 revoke[] 为空",
        ));
    }
    Ok(revoked)
}

/// §34 + 评审 0003 P0-A：校验 `/control` 里 `epoch.activated` 携带的 activation T。
///
/// 覆盖下列绑定，任一不符即 fail-stop：
/// 1. T 三字段同时存在（历史记录可全缺，但**缺 T 不等于验证通过**，返回 `None`）；
/// 2. `ActivationTV1::decode` 成功（20 字段、键序、长度、单调性）；
/// 3. **链上位置**：`T.expected_control_rev == rev`，且 `T.prev_control_hash` 等于
///    该记录**实际前一条**（rev-1）的 `record_hash`——恶意 relay 换掉前缀并重算
///    自洽 hash 时，被签 T 仍指向旧前缀，必须拒绝；
/// 4. **有效 authority**：`T.old_manager_rev`（及其旧签名公钥）与 `T.from_epoch`
///    必须等于该位置当前有效的管理 rev/公钥与 epoch；`T.new_manager_rev/pubkey`
///    必须与管理链上对应 `ManagerTransitionV1` 的字段/epoch/rotation 完全一致；
/// 5. 旧/新管理公钥分别复算 `MN/v1/activation-t\\0 || CDE(T)` 双签。
#[allow(clippy::too_many_arguments)]
fn verify_epoch_activated_t(
    event: &EpochActivated,
    manager_keys: &HashMap<u64, [u8; 32]>,
    manager_transitions: &HashMap<u64, ManagerTransitionV1>,
    authority: EffectiveAuthority,
    previous_record_hash: [u8; 32],
    vault_id: &VaultId,
    instance_id: &[u8; 16],
) -> Result<Option<([u8; 16], VerifiedActivationT)>, SyncError> {
    let (Some(t_bytes), Some(old_signature), Some(new_signature)) = (
        &event.activation_t,
        &event.old_t_signature,
        &event.new_t_signature,
    ) else {
        // P0-A：未生产无兼容负担，任何 epoch.activated 缺完整 T/双签都不能保留旧
        // authority 继续授权；否则恶意 relay 可删 event 的 13–15 字段降级 effective
        // manager，再用已退役 key 追加 attacker 证书。
        return Err(SyncError::RelayWire(
            "epoch.activated 缺 activation T/双签（不允许无 T 降级）",
        ));
    };
    let activation_t = ActivationTV1::decode(t_bytes)
        .map_err(|_| SyncError::RelayWire("epoch.activated 的 activation T 无法解码"))?;
    let matches = activation_t.vault_id == *vault_id
        && activation_t.instance_id == *instance_id
        && activation_t.rotation_id == event.rotation_id
        && activation_t.from_epoch == event.from_epoch
        && activation_t.to_epoch == event.to_epoch
        && activation_t.freeze_seq == event.covered_seq
        && activation_t.primary_cid == event.primary_checkpoint
        && activation_t.fallback_cid == event.fallback_checkpoint
        && activation_t.grants_digest == event.grants_digest
        && activation_t.expected_control_rev == event.control_rev;
    if !matches {
        return Err(SyncError::RelayWire(
            "epoch.activated 的 activation T 与事件/本地事实不一致",
        ));
    }
    // ① T 必须钉在**实际**控制链位置上：precondition 是 rev-1 的真实 record_hash。
    //    只验 relay 自洽 hash 不够——恶意 relay 可以换掉前缀后重算整条 hash 链。
    if activation_t.expected_control_rev != event.control_rev {
        return Err(SyncError::RelayWire(
            "activation T 的 expected_control_rev 与记录 rev 不符",
        ));
    }
    if activation_t.prev_control_hash != previous_record_hash {
        return Err(SyncError::RelayWire(
            "activation T 的 prev_control_hash 与链上实际前一条记录不符",
        ));
    }
    // ② 旧管理权/f_epoch 必须是**该位置当前有效**的 authority，而不是管理链上
    //    "曾经/将来"的任意一条。
    if activation_t.from_epoch != authority.epoch {
        return Err(SyncError::RelayWire(
            "activation T 的 from_epoch 与当前有效 epoch 不符",
        ));
    }
    if u64::from(activation_t.old_manager_rev) != u64::from(authority.manager_rev) {
        return Err(SyncError::RelayWire(
            "activation T 的 old_manager_rev 不是当前有效管理权",
        ));
    }
    let old_key = manager_keys
        .get(&u64::from(activation_t.old_manager_rev))
        .ok_or(SyncError::RelayWire(
            "activation T 的旧管理 rev 不在管理链上",
        ))?;
    if *old_key != authority.manager_pub {
        return Err(SyncError::RelayWire(
            "activation T 的旧管理公钥与当前有效管理权不一致",
        ));
    }
    // ③ 新管理权只能通过这条 T 推进：T 必须与管理链里**同一 rev** 的双签转换
    //    （rotation/epoch/rev/pubkey/covered_seq）完全一致，不能拿另一份转换拼接。
    let transition = manager_transitions
        .get(&u64::from(activation_t.new_manager_rev))
        .ok_or(SyncError::RelayWire(
            "activation T 的新管理 rev 缺少对应管理权转换",
        ))?;
    let transition_matches = transition.vault_id == *vault_id
        && transition.instance_id == *instance_id
        && transition.rotation_id == activation_t.rotation_id
        && transition.from_epoch == activation_t.from_epoch
        && transition.to_epoch == activation_t.to_epoch
        && transition.old_rev == u64::from(activation_t.old_manager_rev)
        && transition.new_rev == u64::from(activation_t.new_manager_rev)
        && transition.new_pubkey == activation_t.new_manager_pubkey
        && transition.covered_seq == activation_t.freeze_seq;
    if !transition_matches {
        return Err(SyncError::RelayWire(
            "activation T 与管理权转换的 rotation/epoch/rev/pubkey 不一致",
        ));
    }
    let new_key = manager_keys
        .get(&u64::from(activation_t.new_manager_rev))
        .ok_or(SyncError::RelayWire(
            "activation T 的新管理 rev 不在管理链上",
        ))?;
    if activation_t.new_manager_pubkey != *new_key {
        return Err(SyncError::RelayWire(
            "activation T 的新管理公钥与管理链不符",
        ));
    }
    let old_signature: [u8; 64] = old_signature
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::RelayWire("activation T 旧管理签名长度非法"))?;
    let new_signature: [u8; 64] = new_signature
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::RelayWire("activation T 新管理签名长度非法"))?;
    let signing_input = activation_t_signing_input(&activation_t)
        .map_err(|_| SyncError::RelayWire("activation T 签名输入无法重建"))?;
    verify_signature(old_key, &signing_input, &old_signature)?;
    verify_signature(new_key, &signing_input, &new_signature)?;
    Ok(Some((
        event.rotation_id,
        VerifiedActivationT {
            activation_t,
            old_t_signature: old_signature.to_vec(),
            new_t_signature: new_signature.to_vec(),
        },
    )))
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRng;
    use std::cell::Cell;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct MockTransport {
        responses: VecDeque<HttpResponse>,
        requests: Vec<HttpRequest>,
    }

    impl MockTransport {
        fn with_responses(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: responses.into(),
                requests: Vec::new(),
            }
        }
    }

    impl HttpTransport for MockTransport {
        fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
            self.requests.push(request.clone());
            self.responses
                .pop_front()
                .ok_or(SyncError::Transport("测试没有更多预设响应"))
        }
    }

    struct FakeClock(Cell<u64>);

    impl FakeClock {
        fn new(now: u64) -> Self {
            Self(Cell::new(now))
        }

        fn set(&self, now: u64) {
            self.0.set(now);
        }
    }

    impl Clock for FakeClock {
        fn now_secs(&self) -> u64 {
            self.0.get()
        }
    }

    fn cbor_response(fields: cbor::Fields) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: cbor::encode(fields).expect("测试响应编码"),
        }
    }

    fn auth_responses(expires_at: u64) -> Vec<HttpResponse> {
        vec![
            cbor_response(vec![
                (0, cbor::bytes([0x01; 16])),
                (1, cbor::uint(1)),
                (2, cbor::bytes([0x04; 16])),
                (3, cbor::bytes([0x02; 32])),
                (4, cbor::uint(expires_at - 100)),
                (5, cbor::bytes([0x5a; 16])),
                (6, cbor::bytes([0x03; 16])),
                (7, cbor::uint(expires_at - 600)),
            ]),
            cbor_response(vec![
                (0, cbor::text("token-1")),
                (1, cbor::uint(expires_at)),
                (2, cbor::uint(expires_at - 600)),
                (3, cbor::uint(1)),
                (4, cbor::uint(1)),
            ]),
        ]
    }

    fn state_fields(control_rev: u64, head_seq: u64) -> cbor::Fields {
        state_fields_auth(control_rev, head_seq, 1)
    }

    /// 与 [`state_fields`] 相同，但显式给 `auth_rev`（ADR 0010 缓存失效测试用）。
    fn state_fields_auth(control_rev: u64, head_seq: u64, auth_rev: u64) -> cbor::Fields {
        vec![
            (0, cbor::bytes([0x5a; 16])),
            (1, cbor::bytes([0x11; 16])),
            (2, cbor::bytes([0x22; 16])),
            (3, cbor::text("active")),
            (4, cbor::uint(1)),
            (5, cbor::uint(control_rev)),
            (6, cbor::uint(auth_rev)),
            (7, cbor::uint(head_seq)),
            (8, cbor::uint(0)),
            (9, cbor::uint(0)),
            (10, cbor::uint(0)),
            (11, cbor::uint(1024)),
            (12, cbor::uint(2048)),
            (13, cbor::uint(2048)),
        ]
    }

    struct TestDevice {
        id: DeviceId,
        cert: DeviceCertV1,
    }

    fn test_device(seed: u8) -> TestDevice {
        let id = [seed; 16];
        let sign = DeviceSigningKey::generate(&mut TestRng::new(u64::from(seed))).expect("密钥");
        let hpke = crate::device::DeviceHpkeKey::generate(&mut TestRng::new(u64::from(seed)))
            .expect("hpke");
        let cert = DeviceCertV1 {
            vault_id: [0x5a; 16],
            device_id: id,
            sign_ed25519_pub: sign.public_key(),
            hpke_x25519_pub: hpke.public_key(),
            cert_serial: 1,
            issued_manager_rev: 1,
            rights: menghunote_sync_proto::Rights(menghunote_sync_proto::Rights::ALL),
        };
        TestDevice { id, cert }
    }

    fn member_fields(
        device: &TestDevice,
        signed: bool,
        manager: &DeviceSigningKey,
    ) -> cbor::CdeValue {
        member_fields_with_status(device, "active", signed, manager)
    }

    fn member_fields_with_status(
        device: &TestDevice,
        status: &str,
        signed: bool,
        manager: &DeviceSigningKey,
    ) -> cbor::CdeValue {
        let mut fields = vec![
            (0, cbor::bytes(device.id)),
            (1, cbor::bytes(device.cert.encode().expect("证书编码"))),
            (2, cbor::text(status)),
            (3, cbor::bytes(device.cert.sign_ed25519_pub)),
            (4, cbor::bytes(device.cert.hpke_x25519_pub)),
        ];
        if signed {
            let signature = manager.sign(&device_cert_signed_bytes(&device.cert).expect("待签"));
            fields.push((5, cbor::bytes(signature)));
        }
        cbor::CdeValue::Map(fields)
    }

    /// relay 的 pending 成员条目：空证书占位（`certificate` X''、无管理签名）。
    fn pending_member_fields(device: &TestDevice) -> cbor::CdeValue {
        cbor::CdeValue::Map(vec![
            (0, cbor::bytes(device.id)),
            (1, cbor::bytes(Vec::new())),
            (2, cbor::text("pending")),
            (3, cbor::bytes(device.cert.sign_ed25519_pub)),
            (4, cbor::bytes(device.cert.hpke_x25519_pub)),
        ])
    }

    /// 控制链构造器：模拟 relay `vault::initialize` + `control::append`。
    ///
    /// rev 1 是 genesis（signed_record = 创建设备证书 CDE 的字节）；rev 2+ 是 typed
    /// `SignedControlRecord`，管理签名按 relay `management::run` 的公式生成。
    struct ControlChainBuilder {
        manager: DeviceSigningKey,
        vault_id: [u8; 16],
        instance_id: [u8; 16],
        prev: [u8; 32],
        rev: u64,
        commits: Vec<cbor::CdeValue>,
        manager_chain: Vec<cbor::CdeValue>,
        next_request: u8,
    }

    impl ControlChainBuilder {
        fn new(manager: DeviceSigningKey) -> Self {
            let vault_id = [0x5a; 16];
            let genesis = cbor::encode(vec![(0, cbor::bytes(vault_id))]).expect("genesis");
            Self::with_genesis(manager, genesis)
        }

        /// 用指定 genesis 字节构造（负例：同 rev 但替换 genesis）。
        fn with_genesis(manager: DeviceSigningKey, genesis: Vec<u8>) -> Self {
            let vault_id = [0x5a; 16];
            let instance_id = [0x22; 16];
            let mut chain = Self {
                manager: manager.clone(),
                vault_id,
                instance_id,
                prev: [0u8; 32],
                rev: 0,
                commits: Vec::new(),
                manager_chain: vec![cbor::CdeValue::Map(vec![
                    (0, cbor::uint(1)),
                    (1, cbor::bytes(manager.public_key())),
                    (2, cbor::bytes(genesis.clone())),
                    (3, cbor::uint(1)),
                ])],
                next_request: 0x30,
            };
            chain.push_raw(genesis);
            chain
        }

        /// 用**真实设备证书**作为 genesis（首台设备的已签加入事件）。
        fn with_genesis_cert(manager: DeviceSigningKey, device: &TestDevice) -> Self {
            Self::with_genesis(manager, device.cert.encode().expect("genesis 证书编码"))
        }

        fn push_raw(&mut self, record: Vec<u8>) -> [u8; 32] {
            self.append_commit(None, record)
        }

        /// 追加一条已签加入事件（relay `approve` 把设备证书 CDE 原字节作为裸记录）。
        fn push_join(&mut self, device: &TestDevice) -> [u8; 32] {
            self.push_raw(device.cert.encode().expect("加入证书编码"))
        }

        fn append_commit(&mut self, outer_kind: Option<&str>, record: Vec<u8>) -> [u8; 32] {
            let mut input = Vec::with_capacity(64 + record.len());
            input.extend_from_slice(&self.prev);
            input.extend_from_slice(&record);
            let hash: [u8; 32] = Sha256::digest(&input).into();
            self.rev += 1;
            let mut entry = vec![
                (0, cbor::uint(self.rev)),
                (1, cbor::bytes(self.prev)),
                (2, cbor::bytes(hash)),
                (3, cbor::bytes(record)),
            ];
            if let Some(kind) = outer_kind {
                entry.push((4, cbor::text(kind)));
            }
            self.commits.push(cbor::CdeValue::Map(entry));
            self.prev = hash;
            hash
        }

        fn sign_record(
            &self,
            kind: ControlKind,
            body: &[u8],
            path: &str,
            event: Option<Vec<u8>>,
        ) -> Vec<u8> {
            let request_id = [self.next_request; 16];
            let expected_control_rev = self.rev;
            let body_digest: [u8; 32] =
                Sha256::digest(management_request_cde(HttpMethod::Post, path, body).expect("cde"))
                    .into();
            let input = management_signing_input(
                &self.instance_id,
                &self.vault_id,
                &request_id,
                expected_control_rev,
                &body_digest,
            )
            .expect("sign input");
            let signature = self.manager.sign(&input);
            SignedControlRecord {
                kind,
                request_id,
                manager_rev: 1,
                expected_control_rev,
                signed_body: body.to_vec(),
                signature: signature.to_vec(),
                event,
            }
            .encode()
            .expect("record")
        }

        fn push_typed(
            &mut self,
            kind: ControlKind,
            body: &[u8],
            path: &str,
            event: Option<Vec<u8>>,
        ) -> [u8; 32] {
            self.next_request = self.next_request.wrapping_add(1);
            let record = self.sign_record(kind, body, path, event);
            self.append_commit(Some(kind.as_str()), record)
        }

        /// 用给定签名替换真实管理签名（伪造链负例）。
        fn push_typed_with_signature(
            &mut self,
            kind: ControlKind,
            body: &[u8],
            _path: &str,
            event: Option<Vec<u8>>,
            signature: Vec<u8>,
        ) -> [u8; 32] {
            self.next_request = self.next_request.wrapping_add(1);
            let request_id = [self.next_request; 16];
            let expected_control_rev = self.rev;
            let record = SignedControlRecord {
                kind,
                request_id,
                manager_rev: 1,
                expected_control_rev,
                signed_body: body.to_vec(),
                signature,
                event,
            }
            .encode()
            .expect("record");
            self.append_commit(Some(kind.as_str()), record)
        }

        /// 外层 kind 与被签记录内部的 kind 不一致（篡改负例）。
        fn push_begin_with_outer_kind(&mut self, outer_kind: &str) -> [u8; 32] {
            self.next_request = self.next_request.wrapping_add(1);
            let path = format!("/v1/vaults/{}/rotations", base64url::encode(&self.vault_id));
            let record = self.sign_record(ControlKind::RotationBegin, &[], &path, None);
            self.append_commit(Some(outer_kind), record)
        }

        fn push_begin(&mut self) -> [u8; 32] {
            let path = format!("/v1/vaults/{}/rotations", base64url::encode(&self.vault_id));
            self.push_typed(ControlKind::RotationBegin, &[], &path, None)
        }

        /// 追加一条带**已签 revoke[]** 的 `rotation.begin`（冻结时吊销）。
        fn push_begin_revoking(&mut self, revoke: &[DeviceId]) -> [u8; 32] {
            let path = format!("/v1/vaults/{}/rotations", base64url::encode(&self.vault_id));
            let body = RotationBeginBodyV1 {
                expected_epoch: 1,
                expected_control_rev: self.rev,
                request_id: [0u8; 16],
                revoke: revoke.to_vec(),
            }
            .encode()
            .expect("begin body");
            self.push_typed(ControlKind::RotationBegin, &body, &path, None)
        }

        /// 追加一条 `rotation.revoked-more`：已签 canonical body `{0: revoke[]}`，
        /// 事件字段与 body 保持一致。
        fn push_revoked_more(&mut self, rotation_id: [u8; 16], revoke: &[DeviceId]) -> [u8; 32] {
            let body = cbor::encode(vec![(
                0,
                cbor::array(revoke.iter().map(|device| cbor::bytes(*device)).collect()),
            )])
            .expect("revoke-more body");
            let event = menghunote_sync_proto::RevocationsAdded {
                rotation_id,
                devices: revoke.to_vec(),
                restarted: false,
                new_rotation: None,
            }
            .encode()
            .expect("revoke-more event");
            let path = format!(
                "/v1/vaults/{}/rotations/{}/revoke-more",
                base64url::encode(&self.vault_id),
                base64url::encode(&rotation_id)
            );
            self.push_typed(ControlKind::RevocationsAdded, &body, &path, Some(event))
        }

        /// `POST P/rotations/{r}/abort` 的规范 path（与 relay/客户端一致）。
        fn abort_path(&self, rotation_id: [u8; 16]) -> String {
            format!(
                "/v1/vaults/{}/rotations/{}/abort",
                base64url::encode(&self.vault_id),
                base64url::encode(&rotation_id)
            )
        }

        /// §35.3：abort typed 事件（rid/method/path/body digest）。
        fn abort_event(&self, rotation_id: [u8; 16], body_digest: [u8; 32]) -> Vec<u8> {
            menghunote_sync_proto::RotationAborted {
                rotation_id,
                method: HttpMethod::Post,
                path: self.abort_path(rotation_id),
                body_digest,
            }
            .encode()
            .expect("abort event")
        }

        /// 追加一条**正确签名**的 `rotation.abort`（canonical body 为空 map）。
        fn push_abort(&mut self, rotation_id: [u8; 16]) -> [u8; 32] {
            let body = cbor::encode(Vec::new()).expect("空 canonical body");
            let body_digest: [u8; 32] = Sha256::digest(&body).into();
            let path = self.abort_path(rotation_id);
            let event = self.abort_event(rotation_id, body_digest);
            self.push_typed(ControlKind::RotationAbort, &body, &path, Some(event))
        }

        /// 追加一条签名覆盖正确 path/body、但**事件字节被替换**的 abort（负例）。
        fn push_abort_with_event(&mut self, rotation_id: [u8; 16], event: Vec<u8>) -> [u8; 32] {
            let body = cbor::encode(Vec::new()).expect("空 canonical body");
            let path = self.abort_path(rotation_id);
            self.push_typed(ControlKind::RotationAbort, &body, &path, Some(event))
        }

        /// 追加一条 abort，但管理签名由**错误的管理 key**产生（负例）。
        fn push_abort_signed_by(
            &mut self,
            rotation_id: [u8; 16],
            signer: &DeviceSigningKey,
        ) -> [u8; 32] {
            self.next_request = self.next_request.wrapping_add(1);
            let request_id = [self.next_request; 16];
            let expected_control_rev = self.rev;
            let body = cbor::encode(Vec::new()).expect("空 canonical body");
            let body_digest: [u8; 32] = Sha256::digest(&body).into();
            let path = self.abort_path(rotation_id);
            let request_digest: [u8; 32] = Sha256::digest(
                management_request_cde(HttpMethod::Post, &path, &body).expect("request cde"),
            )
            .into();
            let input = management_signing_input(
                &self.instance_id,
                &self.vault_id,
                &request_id,
                expected_control_rev,
                &request_digest,
            )
            .expect("signing input");
            let record = SignedControlRecord {
                kind: ControlKind::RotationAbort,
                request_id,
                manager_rev: 1,
                expected_control_rev,
                signed_body: body,
                signature: signer.sign(&input).to_vec(),
                event: Some(self.abort_event(rotation_id, body_digest)),
            }
            .encode()
            .expect("record");
            self.append_commit(Some("rotation.abort"), record)
        }

        /// 替换 rev1 genesis 记录并重算 record_hash（只支持只有一条 commit 的链）。
        fn replace_genesis_record(&mut self, record: Vec<u8>) {
            assert_eq!(self.rev, 1, "替换 genesis 的负例只支持单条链");
            let zero = [0u8; 32];
            let hash: [u8; 32] =
                Sha256::digest([zero.as_slice(), record.as_slice()].concat().as_slice()).into();
            let cbor::CdeValue::Map(fields) = &mut self.commits[0] else {
                panic!("commit 必须是 map");
            };
            for (key, value) in fields.iter_mut() {
                match *key {
                    1 => *value = cbor::bytes(zero),
                    2 => *value = cbor::bytes(hash),
                    3 => *value = cbor::bytes(record.clone()),
                    _ => {}
                }
            }
            self.prev = hash;
        }

        fn push_activated(&mut self, rotation_id: [u8; 16], to_epoch: u64) -> [u8; 32] {
            let event = menghunote_sync_proto::EpochActivated {
                vault_id: self.vault_id,
                instance_id: self.instance_id,
                rotation_id,
                from_epoch: to_epoch - 1,
                to_epoch,
                covered_seq: 6,
                primary_checkpoint: [0x41; 16],
                fallback_checkpoint: [0x42; 16],
                activation_commit: [0x43; 32],
                member_set_hash: [0x44; 32],
                grants_digest: [0x45; 32],
                control_rev: self.rev + 1,
                auth_rev: 3,
                activation_t: None,
                old_t_signature: None,
                new_t_signature: None,
            }
            .encode()
            .expect("event");
            let path = format!(
                "/v1/vaults/{}/rotations/{}/activate",
                base64url::encode(&self.vault_id),
                base64url::encode(&rotation_id)
            );
            self.push_typed(ControlKind::EpochActivated, &[], &path, Some(event))
        }

        /// 管理链追加 rev2（`1→2` 转换由旧 key 双签；新 key 由测试提供）。
        fn push_manager_rev2(&mut self, new_manager: &DeviceSigningKey, rotation_id: [u8; 16]) {
            let transition = crate::rotation::ManagerTransitionV1 {
                vault_id: self.vault_id,
                instance_id: self.instance_id,
                rotation_id,
                from_epoch: 1,
                to_epoch: 2,
                old_rev: 1,
                new_rev: 2,
                new_pubkey: new_manager.public_key(),
                covered_seq: 6,
            }
            .encode()
            .expect("manager transition");
            let old_signature = self.manager.sign(&transition);
            let new_signature = new_manager.sign(&transition);
            self.manager_chain.push(cbor::CdeValue::Map(vec![
                (0, cbor::uint(2)),
                (1, cbor::bytes(new_manager.public_key())),
                (2, cbor::bytes(transition)),
                (3, cbor::uint(1)),
                (4, cbor::bytes(old_signature.to_vec())),
                (5, cbor::bytes(new_signature.to_vec())),
            ]));
        }

        /// 追加一条携带完整 activation T + 旧/新管理双签的 `epoch.activated`。
        ///
        /// `tamper`：0 = 旧签名翻 1 bit、1 = 新签名翻 1 bit、2 = 用错误新 key 签名。
        fn push_activated_with_t(
            &mut self,
            new_manager: &DeviceSigningKey,
            rotation_id: [u8; 16],
            to_epoch: u64,
            tamper: Option<u8>,
        ) -> [u8; 32] {
            let from_epoch = to_epoch.saturating_sub(1);
            let control_rev = self.rev + 1;
            let material = crate::rotation_slots::ActivationTMaterial {
                vault_id: self.vault_id,
                instance_id: self.instance_id,
                rotation_id,
                from_epoch,
                to_epoch,
                freeze_seq: 6,
                old_manager_rev: 1,
                new_manager_rev: 2,
                new_manager_public: new_manager.public_key(),
                prev_control_hash: self.prev,
                expected_control_rev: control_rev,
                primary_checkpoint: [0x41; 16],
                primary_commit_digest: [0xA1; 32],
                fallback_checkpoint: [0x42; 16],
                fallback_commit_digest: [0xB2; 32],
                slot_set_digest: [0x38; 32],
                member_cert_digest: [0x39; 32],
                grants_digest: [0x45; 32],
                old_asset_set_digest: [0x3B; 32],
                new_asset_set_digest: [0x3C; 32],
            };
            let activation_t =
                crate::rotation_slots::build_activation_t(&material).expect("activation T");
            let t_signing_input = menghunote_sync_proto::activation_t_signing_input(&activation_t)
                .expect("T signing input");
            let mut old_signature = self.manager.sign(&t_signing_input).to_vec();
            let mut new_signature = new_manager.sign(&t_signing_input).to_vec();
            match tamper {
                Some(0) => old_signature[0] ^= 0x01,
                Some(1) => new_signature[0] ^= 0x01,
                Some(2) => {
                    let wrong =
                        DeviceSigningKey::generate(&mut TestRng::new(0xBAD)).expect("错误新 key");
                    new_signature = wrong.sign(&t_signing_input).to_vec();
                }
                _ => {}
            }
            let event = menghunote_sync_proto::EpochActivated {
                vault_id: self.vault_id,
                instance_id: self.instance_id,
                rotation_id,
                from_epoch,
                to_epoch,
                covered_seq: 6,
                primary_checkpoint: [0x41; 16],
                fallback_checkpoint: [0x42; 16],
                activation_commit: [0x43; 32],
                member_set_hash: [0x44; 32],
                grants_digest: [0x45; 32],
                control_rev,
                auth_rev: 3,
                activation_t: Some(activation_t.encode().expect("T encode")),
                old_t_signature: Some(old_signature),
                new_t_signature: Some(new_signature),
            }
            .encode()
            .expect("event");
            let path = format!(
                "/v1/vaults/{}/rotations/{}/activate",
                base64url::encode(&self.vault_id),
                base64url::encode(&rotation_id)
            );
            self.push_typed(ControlKind::EpochActivated, &[], &path, Some(event))
        }

        fn fields(&self, members: Vec<cbor::CdeValue>) -> cbor::Fields {
            self.page_fields(&self.commits, members)
        }

        /// 构造一页 `/control` 响应（只带给定 commit 子集，head/管理链取整条链）。
        fn page_fields(
            &self,
            commits: &[cbor::CdeValue],
            members: Vec<cbor::CdeValue>,
        ) -> cbor::Fields {
            vec![
                (0, cbor::array(commits.to_vec())),
                (1, cbor::uint(self.rev)),
                (2, cbor::uint(1)),
                (3, cbor::array(members)),
                (4, cbor::array(Vec::new())),
                (5, cbor::array(self.manager_chain.clone())),
            ]
        }
    }

    fn config(manager_pub: Option<[u8; 32]>) -> HttpRelayConfig {
        HttpRelayConfig {
            base_url: "http://127.0.0.1:8080".to_owned(),
            vault_id: [0x5a; 16],
            device_id: [0x04; 16],
            manager_pub,
        }
    }

    fn client_with(
        responses: Vec<HttpResponse>,
        clock: FakeClock,
        manager_pub: Option<[u8; 32]>,
    ) -> HttpRelayClient<MockTransport, FakeClock> {
        let signing = DeviceSigningKey::generate(&mut TestRng::new(9)).expect("签名密钥");
        HttpRelayClient::with_clock(
            MockTransport::with_responses(responses),
            config(manager_pub),
            signing,
            clock,
        )
    }

    /// 指定 base_url 的配置（生产 https / 本地 http 的 pin 门禁用例）。
    fn config_at(base_url: &str, manager_pub: Option<[u8; 32]>) -> HttpRelayConfig {
        let mut config = config(manager_pub);
        config.base_url = base_url.to_owned();
        config
    }

    fn client_at(
        responses: Vec<HttpResponse>,
        clock: FakeClock,
        manager_pub: Option<[u8; 32]>,
        base_url: &str,
    ) -> HttpRelayClient<MockTransport, FakeClock> {
        let signing = DeviceSigningKey::generate(&mut TestRng::new(9)).expect("签名密钥");
        HttpRelayClient::with_clock(
            MockTransport::with_responses(responses),
            config_at(base_url, manager_pub),
            signing,
            clock,
        )
    }

    /// 解出 CDE map、改字段、重编码（篡改事件负例）。
    fn mutate_event(bytes: &[u8], mutate: impl FnOnce(&mut Vec<(u64, cbor::CdeValue)>)) -> Vec<u8> {
        let mut entries = cbor::decode(bytes).expect("event decode");
        mutate(&mut entries);
        cbor::encode(entries).expect("event re-encode")
    }

    #[test]
    fn checkpoint_create_with_role_sends_deterministic_request_id() {
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(vec![
            (0, cbor::bytes([0x71; 16].to_vec())),
            (1, cbor::uint(2)),
            (2, cbor::uint(5)),
            (3, cbor::bytes([0xCE; 16].to_vec())),
            (4, cbor::uint(1000)),
            (5, cbor::text("primary")),
        ]));
        // 第二次 create 的响应给一个不同 cid：测试只关心**请求**里的 request_id 是否稳定
        // （真实 relay 会在同 id 命中时回原 cid，不会走到这里）。
        responses.push(cbor_response(vec![
            (0, cbor::bytes([0x72; 16].to_vec())),
            (1, cbor::uint(2)),
            (2, cbor::uint(5)),
            (3, cbor::bytes([0xCE; 16].to_vec())),
            (4, cbor::uint(1000)),
            (5, cbor::text("primary")),
        ]));
        let mut client = client_with(responses, FakeClock::new(900), None);
        let lease = client
            .create_checkpoint_with_role(4096, Some(2), Some("primary"))
            .expect("create checkpoint");
        assert_eq!(lease.checkpoint_id, [0x71; 16]);
        client
            .create_checkpoint_with_role(4096, Some(2), Some("primary"))
            .expect("retry create checkpoint");
        let posts: Vec<&HttpRequest> = client
            .transport
            .requests
            .iter()
            .filter(|request| request.method == "POST" && request.path.ends_with("/checkpoints"))
            .collect();
        assert_eq!(posts.len(), 2, "两次 create 各发一个 POST");
        let mut request_ids = Vec::new();
        for post in posts {
            let fields = cbor::decode(&post.body).expect("create body CDE");
            let request_id: [u8; 16] = cbor::bytes_fixed(&fields, 3).expect("request_id");
            request_ids.push(request_id);
        }
        assert_eq!(
            request_ids[0],
            crate::checkpoint::baseline_create_request_id(&[0x5a; 16], "primary", 2),
            "HTTP create 必须发送 role/epoch 派生的幂等 request_id"
        );
        assert_eq!(
            request_ids[1], request_ids[0],
            "同一次 prepare 的重试必须复用同一 request_id"
        );
    }

    #[test]
    fn percent_encodes_query() {
        assert_eq!(percent_encode("abc-_."), "abc-_.");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("a+b/c="), "a%2Bb%2Fc%3D");
    }

    #[test]
    fn rotation_view_reads_section_30_3_fields() {
        let view = cbor::CdeValue::Map(vec![
            (0, cbor::bytes([0x41; 16].to_vec())),
            (1, cbor::text("frozen")),
            (2, cbor::uint(1)),
            (3, cbor::uint(2)),
            (4, cbor::uint(17)),
            (5, cbor::uint(1)),
            (7, cbor::uint(2)),
            (8, cbor::bytes([0x42; 32].to_vec())),
            (9, cbor::uint(0)),
            (10, cbor::uint(3)),
            (11, cbor::uint(4)),
            (12, cbor::uint(5)),
            (13, cbor::uint(0)),
            (15, cbor::uint(7)),
            (16, cbor::bytes([0x43; 32].to_vec())),
            (17, cbor::bytes([0x44; 32].to_vec())),
            (18, cbor::bytes([0x45; 16].to_vec())),
            (19, cbor::bytes([0x46; 16].to_vec())),
            (20, cbor::bytes(vec![0x47])),
            (21, cbor::uint(9)),
            (22, cbor::uint(10)),
            (23, cbor::array(vec![cbor::CdeValue::Map(vec![]); 2])),
            (26, cbor::uint(1)),
        ]);
        let fields = match cbor::CdeValue::Map(vec![(0, view)]) {
            cbor::CdeValue::Map(fields) => fields,
            _ => unreachable!(),
        };
        let parsed = parse_rotation_view(&fields).expect("rotation view");
        assert_eq!(parsed.control_rev, 3);
        assert_eq!(parsed.auth_rev, 4);
        assert_eq!(parsed.manager_rev, 5);
        assert_eq!(parsed.freeze_asset_count, 7);
        assert_eq!(parsed.freeze_asset_digest, Some([0x43; 32]));
        assert_eq!(parsed.member_cert_digest, Some([0x44; 32]));
        assert_eq!(parsed.primary_checkpoint, Some([0x45; 16]));
        assert_eq!(parsed.fallback_checkpoint, Some([0x46; 16]));
        assert_eq!(parsed.activated_at, Some(9));
        assert_eq!(parsed.activated_control_rev, Some(10));
        assert_eq!(parsed.candidate_count, 2);
        assert!(parsed.candidate_ready);
    }

    #[test]
    fn state_authenticates_reuses_token_and_refreshes_before_expiry() {
        let clock = FakeClock::new(900);
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(1, 9)));
        responses.push(cbor_response(state_fields(1, 9)));
        responses.extend(auth_responses(1500));
        responses.push(cbor_response(state_fields(1, 9)));
        let mut client = client_with(responses, clock, Some([0x33; 32]));

        let state = client.state().unwrap();
        assert_eq!(state.head_seq, 9);
        assert_eq!(state.control_rev, 1);
        assert_eq!(client.transport.requests.len(), 3);
        assert_eq!(client.transport.requests[0].path, "/v1/auth/challenges");
        assert_eq!(client.transport.requests[1].path, "/v1/auth/tokens");
        let challenge_body = cbor::decode(&client.transport.requests[0].body).unwrap();
        assert_eq!(challenge_body[0], (0, cbor::uint(1)));
        assert_eq!(challenge_body[1], (1, cbor::bytes([0x04; 16])));
        assert!(client.transport.requests[2].path.ends_with("/state"));
        assert!(client.transport.requests[2]
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer token-1"));
        assert!(client.transport.requests[2]
            .headers
            .iter()
            .any(|(name, _)| name == "content-type"));

        // 未到期：不重新认证。
        client.state().unwrap();
        assert_eq!(client.transport.requests.len(), 4);

        // 剩余 <60s：刷新（challenge/token 各一次）。
        client.clock.set(1150);
        client.state().unwrap();
        assert_eq!(client.transport.requests.len(), 7);
        assert_eq!(client.transport.requests[4].path, "/v1/auth/challenges");
        assert_eq!(client.transport.requests[5].path, "/v1/auth/tokens");
    }

    #[test]
    fn log_reads_signature_and_control_cache_serves_certs() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(77)).expect("管理密钥");
        let device_a = test_device(0x0a);
        let device_b = test_device(0x0b);
        let object_id = [0x77; 32];
        let commit = menghunote_sync_proto::ObjectCommit {
            vault_id: [0x5a; 16],
            object_id,
            message_id: [0x01; 16],
            object_type: menghunote_sync_proto::ObjectType::Update,
            generation: 1,
            key_epoch: 1,
            sender_seq: 1,
            part_count: 1,
            total_len: 10,
            object_digest: [0x02; 32],
        };
        let log_fields = vec![
            (
                0,
                cbor::array(vec![cbor::CdeValue::Map(vec![
                    (0, cbor::uint(1)),
                    (1, cbor::text("update")),
                    (2, cbor::bytes(object_id)),
                    (3, cbor::uint(1)),
                    (4, cbor::bytes(commit.encode().unwrap())),
                    (5, cbor::bytes([0x44; 64])),
                ])]),
            ),
            (1, cbor::text("cursor-1")),
            (2, cbor::uint(1)),
            (3, cbor::uint(1)),
            (4, cbor::uint(1)),
            (5, cbor::uint(0)),
        ];

        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_begin();
        let control = chain.fields(vec![member_fields(&device_a, true, &manager)]);
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(log_fields));
        // refresh_control 先读 /state（管理签名输入绑定 instance_id），再读 /control。
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(control));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));

        client.state().unwrap();
        let page = client.log(Some("cursor-0"), 10).unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].signature, Some([0x44; 64]));
        assert_eq!(page.events[0].event, commit.encode().unwrap());
        assert_eq!(page.next_cursor, "cursor-1");

        let cert = client.device_cert(&device_a.id).unwrap().expect("证书");
        assert_eq!(cert.sign_ed25519_pub, device_a.cert.sign_ed25519_pub);
        assert_eq!(client.cached_control_rev(), Some(2));
        assert!(client.device_cert(&device_b.id).unwrap().is_none());

        // 控制链哈希、管理签名与 §17.1 证书签名都已校验通过（否则上面的 device_cert 会失败）。
    }

    #[test]
    fn control_cache_invalidates_when_control_rev_moves() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(78)).expect("管理密钥");
        let device_a = test_device(0x1a);
        let device_b = test_device(0x1b);

        let chain_rev1 = ControlChainBuilder::new(manager.clone());
        let mut chain_rev2 = ControlChainBuilder::new(manager.clone());
        chain_rev2.push_begin();
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(1, 0)));
        responses.push(cbor_response(state_fields(1, 0)));
        responses.push(cbor_response(
            chain_rev1.fields(vec![member_fields(&device_a, true, &manager)]),
        ));
        responses.push(cbor_response(state_fields(2, 0)));
        responses.push(cbor_response(state_fields(2, 0)));
        responses.push(cbor_response(chain_rev2.fields(vec![
            member_fields(&device_a, true, &manager),
            member_fields(&device_b, true, &manager),
        ])));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));

        client.state().unwrap();
        assert!(client.device_cert(&device_a.id).unwrap().is_some());
        assert_eq!(client.transport.requests.len(), 5); // auth×2 + state + state + control
        client.state().unwrap();
        assert_eq!(client.transport.requests.len(), 6);
        assert_eq!(
            client.cached_control_rev(),
            None,
            "control_rev 前进必须让缓存失效"
        );
        assert!(client.device_cert(&device_b.id).unwrap().is_some());
        assert_eq!(client.cached_control_rev(), Some(2));
    }

    #[test]
    fn control_cache_invalidates_when_auth_rev_moves_without_control_rev() {
        // ADR 0010：独立吊销只推进 `auth_rev`，`control_rev`/控制链不变。
        // 只比较 control_rev 会把吊销前的 active 集合继续当成事实，因此必须同时看 auth_rev，
        // 并且在 auth_rev 前进时连 token 一起作废（全 vault 鉴权版本）。
        let manager = DeviceSigningKey::generate(&mut TestRng::new(81)).expect("管理密钥");
        let device_a = test_device(0x1a);
        let chain = ControlChainBuilder::new(manager.clone());
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(1, 0)));
        responses.push(cbor_response(state_fields(1, 0)));
        responses.push(cbor_response(
            chain.fields(vec![member_fields(&device_a, true, &manager)]),
        ));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));

        client.state().unwrap();
        assert!(client.device_cert(&device_a.id).unwrap().is_some());
        assert_eq!(client.cached_control_rev(), Some(1));
        assert_eq!(client.cached_auth_rev(), Some(1));

        // 同 control_rev、auth_rev=2：`state()` 必须清掉成员缓存与 token。
        client
            .transport
            .responses
            .push_back(cbor_response(state_fields_auth(1, 0, 2)));
        // 清缓存后重新取成员：先重认证（旧 token 已作废）+ 重新读 state/control。
        client.transport.responses.extend(auth_responses(1500));
        client
            .transport
            .responses
            .push_back(cbor_response(state_fields_auth(1, 0, 2)));
        client.transport.responses.push_back(cbor_response(
            chain.fields(vec![member_fields(&device_a, true, &manager)]),
        ));

        let state = client.state().unwrap();
        assert_eq!(state.auth_rev, 2, "/state 的 auth_rev 必须被观察到");
        assert_eq!(state.control_rev, 1, "独立吊销不改变 control_rev");
        assert_eq!(
            client.cached_control_rev(),
            None,
            "auth_rev 前进必须清控制缓存"
        );
        assert_eq!(
            client.cached_auth_rev(),
            None,
            "auth_rev 前进必须清 token 锚点"
        );
        assert!(client.device_cert(&device_a.id).unwrap().is_some());
        assert_eq!(client.cached_control_rev(), Some(1));
        assert_eq!(
            client.cached_auth_rev(),
            Some(2),
            "刷新后的缓存锚点必须是新 auth_rev"
        );
    }

    #[test]
    fn state_rejects_control_rev_rollback_against_authenticated_cache() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(80)).expect("管理密钥");
        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_begin();
        let members = vec![member_fields(&test_device(0x2a), true, &manager)];
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(chain.fields(members)));
        // 缓存已认证 rev=2；随后 `/state` 回退到 rev=1 → fail-stop。
        responses.push(cbor_response(state_fields(1, 1)));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        RelayClient::control_kinds(&mut client).expect("先认证 rev=2");
        assert!(matches!(
            client.state(),
            Err(SyncError::RelayWire(
                "relay control_rev 低于已认证缓存（回滚）"
            ))
        ));
    }

    #[test]
    fn control_chain_hash_and_certificate_signature_are_verified() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(79)).expect("管理密钥");
        let device = test_device(0x2a);

        // record_hash 被篡改 → RelayWire。
        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_begin();
        let mut fields = chain.fields(vec![member_fields(&device, true, &manager)]);
        let commits = cbor::array_field(&fields, 0).unwrap();
        let mut last = cbor::value_map(commits.last().unwrap()).unwrap();
        let mut hash = cbor::bytes_field(&last, 2).unwrap();
        hash[0] ^= 0x01;
        last[2].1 = cbor::bytes(hash);
        let mut tampered = commits;
        let last_index = tampered.len() - 1;
        tampered[last_index] = cbor::CdeValue::Map(last);
        fields[0].1 = cbor::array(tampered);
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(1, 1)));
        responses.push(cbor_response(fields));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            client.device_cert(&device.id),
            Err(SyncError::RelayWire("控制链 record_hash 不符"))
        ));

        // 证书签名被篡改 → Signature。
        let chain = ControlChainBuilder::new(manager.clone());
        let mut fields = chain.fields(vec![member_fields(&device, true, &manager)]);
        let members = cbor::array_field(&fields, 3).unwrap();
        let mut member = cbor::value_map(&members[0]).unwrap();
        let mut signature = cbor::bytes_field(&member, 5).unwrap();
        signature[0] ^= 0x01;
        member[5].1 = cbor::bytes(signature);
        fields[3].1 = cbor::array(vec![cbor::CdeValue::Map(member)]);
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(1, 1)));
        responses.push(cbor_response(fields));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            client.device_cert(&device.id),
            Err(SyncError::Signature)
        ));
    }

    #[test]
    fn control_rejects_missing_certificate_signature() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(81)).expect("管理密钥");
        let device = test_device(0x3a);
        let chain = ControlChainBuilder::new(manager.clone());
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(1, 1)));
        responses.push(cbor_response(
            chain.fields(vec![member_fields(&device, false, &manager)]),
        ));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            client.device_cert(&device.id),
            Err(SyncError::RelayWire("成员证书缺管理签名（§17.1）"))
        ));
    }

    #[test]
    fn control_rejects_wrong_manager_public_key() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(82)).expect("管理密钥");
        let device = test_device(0x4a);
        let chain = ControlChainBuilder::new(manager.clone());
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(1, 1)));
        responses.push(cbor_response(
            chain.fields(vec![member_fields(&device, true, &manager)]),
        ));
        // 信任锚不是管理链 rev=1 的公钥 → 整条控制链不可信。
        let mut client = client_with(responses, FakeClock::new(900), Some([0x99; 32]));
        assert!(matches!(
            client.device_cert(&device.id),
            Err(SyncError::RelayWire("管理链 rev=1 公钥与信任锚不符"))
        ));
    }

    #[test]
    fn control_rejects_self_consistent_forged_chain() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(83)).expect("管理密钥");
        let rotation_id = [0x21; 16];
        let event = menghunote_sync_proto::EpochActivated {
            vault_id: [0x5a; 16],
            instance_id: [0x22; 16],
            rotation_id,
            from_epoch: 1,
            to_epoch: 2,
            covered_seq: 6,
            primary_checkpoint: [0x41; 16],
            fallback_checkpoint: [0x42; 16],
            activation_commit: [0x43; 32],
            member_set_hash: [0x44; 32],
            grants_digest: [0x45; 32],
            control_rev: 2,
            auth_rev: 3,
            // 该负例只测管理签名伪造，不带 T（历史记录形态）。
            activation_t: None,
            old_t_signature: None,
            new_t_signature: None,
        }
        .encode()
        .expect("event");
        let path = format!(
            "/v1/vaults/{}/rotations/{}/activate",
            base64url::encode(&[0x5a; 16]),
            base64url::encode(&rotation_id)
        );
        let mut chain = ControlChainBuilder::new(manager.clone());
        // hash 链完全自洽（rev/prev/hash 都对），但签名不是管理 key 产生的。
        chain.push_typed_with_signature(
            ControlKind::EpochActivated,
            &[],
            &path,
            Some(event),
            vec![0xab; 64],
        );
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(chain.fields(vec![member_fields(
            &test_device(0x5a),
            true,
            &manager,
        )])));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            RelayClient::control_kinds(&mut client),
            Err(SyncError::RelayWire(
                "epoch.activated 缺 activation T/双签（不允许无 T 降级）"
            ))
        ));
    }

    #[test]
    fn control_verifies_activation_t_double_signature_and_exposes_evidence() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(91)).expect("管理密钥");
        let new_manager = DeviceSigningKey::generate(&mut TestRng::new(92)).expect("新管理密钥");
        let rotation_id = [0x27; 16];
        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_manager_rev2(&new_manager, rotation_id);
        chain.push_activated_with_t(&new_manager, rotation_id, 2, None);
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(chain.fields(vec![member_fields(
            &test_device(0x5a),
            true,
            &manager,
        )])));
        // `verified_activation_t` / `control_head` 都会先读一次 `/state` 检查缓存前进。
        // 三份：inherent 入口、trait 入口、`control_head`。
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(state_fields(2, 1)));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        RelayClient::control_kinds(&mut client).expect("完整 T 双签必须通过 /control 解析");
        let evidence = client
            .verified_activation_t(&rotation_id)
            .expect("control 缓存")
            .expect("携带 T 的事件必须产生已验证证据");
        assert_eq!(evidence.activation_t.rotation_id, rotation_id);
        assert_eq!(evidence.activation_t.from_epoch, 1);
        assert_eq!(evidence.activation_t.to_epoch, 2);
        assert_eq!(evidence.activation_t.freeze_seq, 6);
        assert_eq!(evidence.activation_t.expected_control_rev, 2);
        assert_eq!(evidence.activation_t.primary_cid, [0x41; 16]);
        assert_eq!(evidence.activation_t.fallback_cid, [0x42; 16]);
        let t_input = menghunote_sync_proto::activation_t_signing_input(&evidence.activation_t)
            .expect("T signing input");
        for (label, public, signature) in [
            (
                "旧管理 key 对 T 的签名",
                manager.public_key(),
                evidence.old_t_signature.as_slice(),
            ),
            (
                "新管理 key 对 T 的签名",
                new_manager.public_key(),
                evidence.new_t_signature.as_slice(),
            ),
        ] {
            verify_signature(
                &public,
                &t_input,
                &signature.try_into().expect("64B T 签名"),
            )
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        }
        // trait 覆盖 = 委托 inherent 入口：engine 的 `apply_epoch_change` 走这条路径，
        // 必须拿到与设备侧直接调用完全一致的证据。
        let trait_evidence = RelayClient::verified_activation_t(&mut client, rotation_id)
            .expect("trait 入口")
            .expect("trait 入口也必须产出已验证 T");
        assert_eq!(trait_evidence, evidence, "trait 入口不得弱化/改写证据");
        let (rev, hash) = client.control_head().expect("控制链链尾");
        assert_eq!(rev, 2);
        assert_ne!(hash, [0u8; 32]);
    }

    /// `RotationView` → `GET /rotations/{r}` 响应里的键值（与 relay `rotation_fields` 对齐）。
    fn rotation_view_value(view: &RotationView) -> cbor::CdeValue {
        let mut fields = vec![
            (0, cbor::bytes(view.rotation_id)),
            (1, cbor::text(view.state.clone())),
            (2, cbor::uint(view.from_epoch)),
            (3, cbor::uint(view.to_epoch)),
            (4, cbor::uint(view.freeze_seq)),
            (5, cbor::uint(view.revoked_count)),
            (7, cbor::uint(view.grants_count)),
            (8, cbor::bytes(view.grants_digest.expect("grants_digest"))),
            (9, cbor::uint(view.published_checkpoints)),
            (10, cbor::uint(view.control_rev)),
            (11, cbor::uint(view.auth_rev)),
            (12, cbor::uint(view.manager_rev)),
            (13, cbor::uint(u64::from(view.from_epoch_revoked_once))),
            (15, cbor::uint(view.freeze_asset_count)),
            (
                16,
                cbor::bytes(view.freeze_asset_digest.expect("freeze_asset_digest")),
            ),
            (
                17,
                cbor::bytes(view.member_cert_digest.expect("member_cert_digest")),
            ),
            (
                18,
                cbor::bytes(view.primary_checkpoint.expect("primary checkpoint")),
            ),
            (
                19,
                cbor::bytes(view.fallback_checkpoint.expect("fallback checkpoint")),
            ),
            (
                20,
                cbor::bytes(view.activation_commit.clone().expect("activation commit")),
            ),
            (21, cbor::uint(view.activated_at.expect("activated_at"))),
            (
                22,
                cbor::uint(view.activated_control_rev.expect("activated_control_rev")),
            ),
            (23, cbor::array(Vec::new())),
            (26, cbor::uint(u64::from(view.candidate_ready))),
        ];
        if let Some(hash) = view.member_set_hash {
            fields.push((6, cbor::bytes(hash)));
        }
        fields.sort_by_key(|(key, _)| *key);
        cbor::CdeValue::Map(fields)
    }

    /// `GET /checkpoints/{cid}` 的 detail map（`checkpoint_status` 只读这些键）。
    fn checkpoint_detail_value(
        checkpoint_id: [u8; 16],
        covered_seq: u64,
        manifest_oid: [u8; 32],
        epoch: u64,
    ) -> cbor::CdeValue {
        cbor::CdeValue::Map(vec![
            (0, cbor::bytes(checkpoint_id)),
            (1, cbor::text("published")),
            (3, cbor::uint(covered_seq)),
            (4, cbor::bytes(manifest_oid)),
            (9, cbor::uint(1)),
            (10, cbor::uint(epoch)),
        ])
    }

    #[test]
    fn epoch_activations_binds_control_position_and_rebuilds_commit() {
        // 每个环必须带：控制链位置（rev = T.expected_control_rev；prev_control_hash =
        // 前一条 record_hash）+ 由 view/checkpoint 重建的 ActivationCommitV1。
        let manager = DeviceSigningKey::generate(&mut TestRng::new(95)).expect("管理密钥");
        let new_manager = DeviceSigningKey::generate(&mut TestRng::new(96)).expect("新管理密钥");
        let rotation_id = [0x29; 16];
        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_manager_rev2(&new_manager, rotation_id);
        // activation 记录前的链尾（rev2 的 record_hash），T.prev_control_hash 必须等于它。
        let prev_control_hash = chain.prev;
        let activation_hash = chain.push_activated_with_t(&new_manager, rotation_id, 2, None);
        assert_ne!(activation_hash, [0u8; 32]);

        let vault_id = [0x5a; 16];
        let instance_id = [0x22; 16];
        let primary_cid = [0x41; 16];
        let fallback_cid = [0x42; 16];
        let primary_status = CheckpointStatus {
            checkpoint_id: primary_cid,
            state: "published".to_owned(),
            role: None,
            covered_seq: 6,
            manifest_object_id: Some([0x51; 32]),
            refs_digest: Some([0x52; 32]),
            candidate_ready: true,
            epoch: 2,
        };
        let fallback_status = CheckpointStatus {
            checkpoint_id: fallback_cid,
            state: "published".to_owned(),
            role: None,
            covered_seq: 6,
            manifest_object_id: Some([0x53; 32]),
            refs_digest: Some([0x54; 32]),
            candidate_ready: true,
            epoch: 2,
        };
        let mut view = RotationView {
            rotation_id,
            state: "activated".to_owned(),
            from_epoch: 1,
            to_epoch: 2,
            freeze_seq: 6,
            revoked_count: 0,
            member_set_hash: Some([0x44; 32]),
            grants_count: 1,
            grants_digest: Some([0x45; 32]),
            published_checkpoints: 2,
            control_rev: 2,
            auth_rev: 3,
            manager_rev: 2,
            from_epoch_revoked_once: false,
            freeze_asset_count: 0,
            // builder 的 T.old_asset_set_digest = [0x3B; 32]。
            freeze_asset_digest: Some([0x3B; 32]),
            member_cert_digest: Some([0x39; 32]),
            primary_checkpoint: Some(primary_cid),
            fallback_checkpoint: Some(fallback_cid),
            activation_commit: None,
            activated_at: Some(1_700_000_000),
            activated_control_rev: Some(2),
            candidate_count: 2,
            candidate_ready: true,
            reservation_id: None,
        };
        let commit = crate::rotation_apply::activation_commit_materials(
            vault_id,
            instance_id,
            &view,
            &primary_status,
            &fallback_status,
        )
        .expect("commit materials");
        let commit_hash: [u8; 32] = Sha256::digest(commit.encode().expect("commit encode")).into();
        view.activation_commit = Some(commit_hash.to_vec());

        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(chain.fields(vec![member_fields(
            &test_device(0x5a),
            true,
            &manager,
        )])));
        // epoch_activations: /state → /rotations/{r} → /checkpoints/{primary} → fallback。
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(vec![(0, rotation_view_value(&view))]));
        responses.push(cbor_response(vec![(
            0,
            checkpoint_detail_value(primary_cid, 6, [0x51; 32], 2),
        )]));
        responses.push(cbor_response(vec![(
            0,
            checkpoint_detail_value(fallback_cid, 6, [0x53; 32], 2),
        )]));
        // 过滤分支只需再读一次 /state。
        responses.push(cbor_response(state_fields(2, 1)));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        RelayClient::control_kinds(&mut client).expect("控制链必须通过");

        // engine 走 trait 入口；HttpRelayClient 的实现与 inherent 入口委托同一 collector。
        let links = RelayClient::epoch_activations(&mut client, 1).expect("activation 链");
        assert_eq!(links.len(), 1);
        let link = &links[0];
        assert_eq!(link.control_rev, 2, "rev 必须来自已验证控制链");
        assert_eq!(
            link.prev_control_hash, prev_control_hash,
            "prev_control_hash 必须绑定链上位置"
        );
        assert_eq!(link.evidence.activation_t.expected_control_rev, 2);
        assert_eq!(link.view.rotation_id, rotation_id);
        assert_eq!(link.activation_commit, commit, "commit 必须由公开事实重建");
        assert_eq!(link.view.activation_commit, Some(commit_hash.to_vec()));
        // 本地 epoch 之后的过滤：from_epoch=2 时不再返回 to=2 的旧环。
        let filtered = client.epoch_activations(2).expect("filtered chain");
        assert!(filtered.is_empty(), "不得返回本地 epoch 已覆盖的旧环");
    }

    #[test]
    fn control_rejects_activation_t_missing_or_tampered_double_signature() {
        for (label, tamper) in [
            ("旧管理签名被篡改", 0u8),
            ("新管理签名被篡改", 1),
            ("换错的新管理 key 签名", 2),
        ] {
            let manager = DeviceSigningKey::generate(&mut TestRng::new(93)).expect("管理密钥");
            let new_manager =
                DeviceSigningKey::generate(&mut TestRng::new(94)).expect("新管理密钥");
            let rotation_id = [0x28; 16];
            let mut chain = ControlChainBuilder::new(manager.clone());
            chain.push_manager_rev2(&new_manager, rotation_id);
            chain.push_activated_with_t(&new_manager, rotation_id, 2, Some(tamper));
            let mut responses = auth_responses(1200);
            responses.push(cbor_response(state_fields(2, 1)));
            responses.push(cbor_response(chain.fields(vec![member_fields(
                &test_device(0x5a),
                true,
                &manager,
            )])));
            let mut client =
                client_with(responses, FakeClock::new(900), Some(manager.public_key()));
            assert!(
                matches!(
                    RelayClient::control_kinds(&mut client),
                    Err(SyncError::Signature)
                ),
                "{label} 必须以 Signature 拒绝"
            );
        }
    }

    #[test]
    fn control_rejects_activation_without_t() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(95)).expect("管理密钥");
        let rotation_id = [0x29; 16];
        let mut chain = ControlChainBuilder::new(manager.clone());
        // 无 T 的历史激活形态：现在必须直接拒绝，不能保留旧 authority 继续授权。
        chain.push_activated(rotation_id, 2);
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(chain.fields(vec![member_fields(
            &test_device(0x5a),
            true,
            &manager,
        )])));
        responses.push(cbor_response(state_fields(2, 1)));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            RelayClient::control_kinds(&mut client),
            Err(SyncError::RelayWire(
                "epoch.activated 缺 activation T/双签（不允许无 T 降级）"
            ))
        ));
    }

    #[test]
    fn control_rejects_outer_kind_tamper() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(84)).expect("管理密钥");
        let mut chain = ControlChainBuilder::new(manager.clone());
        // 被签记录内部是 rotation.begin，外层未签名 kind 被改成 rotation.abort。
        chain.push_begin_with_outer_kind("rotation.abort");
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(1, 1)));
        responses.push(cbor_response(chain.fields(vec![member_fields(
            &test_device(0x5b),
            true,
            &manager,
        )])));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            RelayClient::control_kinds(&mut client),
            Err(SyncError::RelayWire("控制记录外层 kind 与签名记录不一致"))
        ));
    }

    #[test]
    fn control_reads_kind_from_signed_record_internal_bytes() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(85)).expect("管理密钥");
        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_begin();
        let rotation_id = [0x31; 16];
        let _ = rotation_id;
        chain.push_begin();
        let mut fields = chain.fields(vec![member_fields(&test_device(0x5c), true, &manager)]);
        // 删掉所有外层 kind（键 4）：kind 必须仍能从 SignedControlRecord 内部读出。
        let commits = cbor::array_field(&fields, 0).unwrap();
        let stripped = commits
            .into_iter()
            .map(|commit| {
                let mut fields = cbor::value_map(&commit).unwrap();
                fields.retain(|(key, _)| *key != 4);
                cbor::CdeValue::Map(fields)
            })
            .collect::<Vec<_>>();
        fields[0].1 = cbor::array(stripped);
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(3, 1)));
        responses.push(cbor_response(fields));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        let kinds = RelayClient::control_kinds(&mut client).expect("kinds");
        assert_eq!(
            kinds,
            vec![
                (1, "raw".to_owned()),
                (2, "rotation.begin".to_owned()),
                (3, "rotation.begin".to_owned()),
            ]
        );
    }

    #[test]
    fn control_pagination_verifies_each_page_to_head() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(101)).expect("管理密钥");
        let mut chain = ControlChainBuilder::new(manager.clone());
        for _ in 0..299 {
            chain.push_begin();
        }
        let members = vec![member_fields(&test_device(0x6a), true, &manager)];
        let page0 = chain.page_fields(&chain.commits[..256], members.clone());
        let page1 = chain.page_fields(&chain.commits[256..], members);
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(300, 1)));
        responses.push(cbor_response(page0));
        responses.push(cbor_response(page1));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));

        let kinds = RelayClient::control_kinds(&mut client).expect("分页控制链");
        assert_eq!(kinds.len(), 300);
        assert_eq!(kinds.first(), Some(&(1, "raw".to_owned())));
        assert_eq!(kinds.last(), Some(&(300, "rotation.begin".to_owned())));
        assert_eq!(client.cached_control_rev(), Some(300));
        let control_paths: Vec<&str> = client
            .transport
            .requests
            .iter()
            .filter(|request| request.path.contains("/control"))
            .map(|request| request.path.as_str())
            .collect();
        assert_eq!(
            control_paths,
            vec![
                "/v1/vaults/WlpaWlpaWlpaWlpaWlpaWg/control?after_rev=0",
                "/v1/vaults/WlpaWlpaWlpaWlpaWlpaWg/control?after_rev=256",
            ]
        );
    }

    #[test]
    fn control_rejects_lower_pin_and_same_rev_genesis_replacement() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(102)).expect("管理密钥");
        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_begin();
        let members = vec![member_fields(&test_device(0x6b), true, &manager)];
        let head = chain.fields(members.clone());

        // 低 pin：relay head=2，本地 pin=3 → fail-stop。
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(head.clone()));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()))
            .with_control_pin(ControlPin::new([0x22; 16], 3));
        assert!(matches!(
            RelayClient::control_kinds(&mut client),
            Err(SyncError::RelayWire("控制链低于本地 pin（回退）"))
        ));

        // 同 rev 但 genesis 被替换（不同链尾 record_hash）→ fail-stop。
        let head_hash: [u8; 32] = {
            let commits = cbor::array_field(&head, 0).expect("commits");
            let last = cbor::value_map(commits.last().expect("最后一条")).expect("map");
            cbor::bytes_fixed(&last, 2).expect("record hash")
        };
        let other_genesis =
            cbor::encode(vec![(0, cbor::bytes([0xee; 16]))]).expect("另一份 genesis");
        let mut other = ControlChainBuilder::with_genesis(manager.clone(), other_genesis);
        other.push_begin();
        assert_eq!(other.rev, 2);
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(other.fields(members)));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()))
            .with_control_pin(ControlPin::new([0x22; 16], 2).with_record_hash(head_hash));
        assert!(matches!(
            RelayClient::control_kinds(&mut client),
            Err(SyncError::RelayWire(
                "控制链同 rev 的 record_hash 与本地 pin 不符"
            ))
        ));
    }

    #[test]
    fn control_pin_compares_bootstrap_control_hash_at_same_rev() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(103)).expect("管理密钥");
        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_begin();
        let members = vec![member_fields(&test_device(0x6c), true, &manager)];
        let fields = chain.fields(members);
        let manager_chain = parse_manager_chain(&fields).expect("管理链");
        let state = RelayState {
            vault_id: [0x5a; 16],
            log_id: [0x11; 16],
            instance_id: [0x22; 16],
            epoch: 1,
            control_rev: 2,
            auth_rev: 1,
            head_seq: 0,
            gc_floor: 0,
            current_rotation_id: None,
        };
        let bootstrap_hash = crate::bootstrap::control_head_hash(&state, &manager_chain);
        let pins = crate::bootstrap::BootstrapPins {
            instance_id: Some([0x22; 16]),
            control_rev: Some(2),
            control_hash: Some(bootstrap_hash),
            ..Default::default()
        };
        let pin = ControlPin::from_bootstrap_pins(&pins).expect("bootstrap pin");

        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(fields.clone()));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()))
            .with_control_pin(pin);
        RelayClient::control_kinds(&mut client).expect("bootstrap pin 与同 rev 控制头一致");
        assert_eq!(
            client.control_head_pin().map(|pin| pin.control_rev),
            Some(2)
        );

        // 同 rev 但 bootstrap 摘要不符（管理链/坐标被改）→ fail-stop。
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(fields));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()))
            .with_control_pin(ControlPin::new([0x22; 16], 2).with_bootstrap_hash([0x99; 32]));
        assert!(matches!(
            RelayClient::control_kinds(&mut client),
            Err(SyncError::RelayWire(
                "控制链同 rev 的 bootstrap hash 与本地 pin 不符"
            ))
        ));
    }

    #[test]
    fn checkpoint_manifest_downloads_manifest_object_by_oid() {
        use crate::keys::{RootSecretV1, Secret32};
        use menghunote_sync_proto::ObjectType;
        let root = RootSecretV1::new([0x5a; 16], 1, Secret32::from_array([0x33; 32]));
        let checkpoint_id = [0x63; 16];
        let manifest = ManifestV1 {
            vault_id: [0x5a; 16],
            key_epoch: 1,
            checkpoint_id,
            covered_relay_seq: 6,
            min_reader: 0,
            structure_doc_id: "structure".to_owned(),
            docs: Vec::new(),
            assets: Vec::new(),
        };
        let plaintext = manifest.encode().expect("manifest");
        let manifest_oid = [0x91; 32];
        let (parts, digest) = crate::upload::encrypt_local_parts(
            &root,
            ObjectType::Manifest,
            &manifest_oid,
            [0x92; 16],
            [0x04; 16],
            0,
            0,
            &plaintext,
            &[],
        )
        .expect("encrypt");
        assert_eq!(parts.len(), 1);
        let commit = CheckpointCommit {
            vault_id: [0x5a; 16],
            checkpoint_id,
            epoch: 1,
            covered_relay_seq: 6,
            manifest_object_id: manifest_oid,
            manifest_digest: digest,
            refs_digest: [0x11; 32],
            producer_device_id: [0x04; 16],
            validation_version: 1,
        };
        let mut responses = auth_responses(1200);
        // GET /checkpoints/{cid}：键 4 是 manifest_oid（不是明文），键 6 是 CheckpointCommit。
        responses.push(cbor_response(vec![(
            0,
            cbor::CdeValue::Map(vec![
                (0, cbor::bytes(checkpoint_id)),
                (1, cbor::text("published")),
                (3, cbor::uint(6)),
                (4, cbor::bytes(manifest_oid)),
                (6, cbor::bytes(commit.encode().expect("commit"))),
            ]),
        )]));
        responses.push(HttpResponse {
            status: 200,
            body: parts[0].1.clone(),
        });
        let mut client =
            client_with(responses, FakeClock::new(900), Some([0x33; 32])).with_root(root.clone());
        let got = client
            .checkpoint_manifest(&checkpoint_id)
            .expect("manifest");
        assert_eq!(got.checkpoint_id, checkpoint_id);
        assert_eq!(got.manifest_bytes, plaintext);
        assert!(client
            .transport
            .requests
            .last()
            .expect("part 请求")
            .path
            .contains("/objects/"));
    }

    #[test]
    fn error_body_maps_code_epoch_and_retryable() {
        let error = HttpResponse {
            status: 409,
            body: cbor::encode(vec![
                (0, cbor::text("EPOCH")),
                (1, cbor::boolean(false)),
                (2, cbor::uint(3)),
                (3, cbor::uint(7)),
                (4, cbor::text("bootstrap")),
            ])
            .unwrap(),
        };
        let err = error_from_response(&error);
        match err {
            SyncError::RelayApi {
                code,
                retryable,
                current_epoch,
                current_control_rev,
                bootstrap_hint,
            } => {
                assert_eq!(code, "EPOCH");
                assert!(!retryable);
                assert_eq!(current_epoch, Some(3));
                assert_eq!(current_control_rev, Some(7));
                assert_eq!(bootstrap_hint.as_deref(), Some("bootstrap"));
            }
            other => panic!("期望 RelayApi，实际 {other:?}"),
        }

        // 5xx 无错误体 → 可重试内部错误。
        let err = error_from_response(&HttpResponse {
            status: 503,
            body: Vec::new(),
        });
        assert!(matches!(
            err,
            SyncError::RelayApi {
                retryable: true,
                ..
            }
        ));
        // 4xx 无错误体 → HTTP 状态。
        assert!(matches!(
            error_from_response(&HttpResponse {
                status: 404,
                body: b"not cbor".to_vec()
            }),
            SyncError::HttpStatus(404)
        ));
    }

    /// 单设备 active 集合的期望 `member_cert_digest`（含管理签名）。
    fn active_digest_for(manager: &DeviceSigningKey, device: &TestDevice) -> [u8; 32] {
        let signature =
            manager.sign(&device_cert_signed_bytes(&device.cert).expect("证书签名输入"));
        let input = vec![menghunote_sync_proto::MemberCertDigestInputV1 {
            device_id: device.id,
            certificate: device.cert.encode().expect("证书编码"),
            certificate_signature: Some(signature.to_vec()),
        }];
        Sha256::digest(menghunote_sync_proto::member_cert_digest(&input)).into()
    }

    #[test]
    fn control_active_set_ignores_relay_status_flip_of_revoked_cert() {
        // 评审 0002 P0-2 残留：relay 把**已签吊销**的证书 status 改回 active 时，
        // active 集合必须仍按已签事实排除它（status 不是授权来源）。
        let manager = DeviceSigningKey::generate(&mut TestRng::new(201)).expect("管理密钥");
        let genesis = test_device(0xa1);
        let revoked = test_device(0xa2);
        let mut chain = ControlChainBuilder::with_genesis_cert(manager.clone(), &genesis);
        chain.push_join(&revoked);
        // 已签 `rotation.begin` 的 revoke[] 是永久吊销事实。
        chain.push_begin_revoking(&[revoked.id]);
        assert_eq!(chain.rev, 3);

        // 恶意 relay：member.status 把已吊销证书改回 active。
        let members = vec![
            member_fields(&genesis, true, &manager),
            member_fields(&revoked, true, &manager),
        ];
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(chain.rev, 0)));
        responses.push(cbor_response(chain.fields(members)));
        // `members()` 自己会先读一次 /state 比对 auth_rev（ADR 0010 缓存失效锚点）。
        responses.push(cbor_response(state_fields(chain.rev, 0)));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));

        RelayClient::control_kinds(&mut client).expect("控制链必须通过");
        // 吊销证书仍留在 history_certs（≤S 历史验签需要）。
        assert_eq!(
            client.device_cert(&revoked.id).unwrap(),
            Some(revoked.cert.clone()),
            "已签吊销证书必须保留在 history_certs"
        );
        // active 只含 genesis 设备。
        assert_eq!(
            RelayClient::active_member_hpke_keys(&mut client).expect("active keys"),
            vec![(genesis.id, genesis.cert.hpke_x25519_pub)],
            "status 改回 active 不能覆盖已签吊销"
        );
        assert_eq!(
            client.active_member_cert_digest().expect("active digest"),
            active_digest_for(&manager, &genesis),
            "已签吊销设备不得进入 active digest"
        );

        // 成员视图同样 fail-closed：已签吊销优先于 relay status，历史证书保留 → 指纹仍在。
        let views = client.members().expect("成员视图必须成功");
        let view_of = |device: DeviceId| {
            views
                .iter()
                .find(|view| view.device_id == device)
                .expect("成员视图缺少设备")
        };
        assert_eq!(view_of(genesis.id).status, MemberStatus::Active);
        assert_eq!(
            view_of(revoked.id).status,
            MemberStatus::Revoked,
            "relay 把已签吊销证书的 status 翻回 active 不得显示为 Active"
        );
        assert!(
            view_of(revoked.id).fingerprint.is_some(),
            "已签吊销设备的历史证书保留，指纹仍可解析"
        );
    }

    /// 成员视图的状态投影：Active 与 active_certs 同谓词、pending 无指纹、
    /// expired 按既有语义保留、Debug 不泄漏密钥材料。
    #[test]
    fn member_view_projects_statuses_and_hides_key_material() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(211)).expect("管理密钥");
        let genesis = test_device(0xc1);
        let expired = test_device(0xc2);
        let pending = test_device(0xc3);
        let mut chain = ControlChainBuilder::with_genesis_cert(manager.clone(), &genesis);
        chain.push_join(&expired);

        let members = vec![
            member_fields(&genesis, true, &manager),
            member_fields_with_status(&expired, "expired", true, &manager),
            pending_member_fields(&pending),
        ];
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(chain.rev, 0)));
        responses.push(cbor_response(chain.fields(members)));
        // `members()` 自己会先读一次 /state 比对 auth_rev。
        responses.push(cbor_response(state_fields(chain.rev, 0)));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));

        let views = client.members().expect("成员视图必须成功");
        assert_eq!(
            views.iter().map(|view| view.device_id).collect::<Vec<_>>(),
            vec![genesis.id, expired.id, pending.id],
            "成员视图必须按 device_id 稳定升序"
        );

        // genesis：Active + 指纹 + 签发管理版本。
        assert_eq!(views[0].status, MemberStatus::Active);
        assert_eq!(
            views[0].fingerprint.as_deref(),
            Some(
                crate::device::device_fingerprint(
                    &[0x5a; 16],
                    &genesis.id,
                    &genesis.cert.sign_ed25519_pub,
                    &genesis.cert.hpke_x25519_pub,
                )
                .to_hex()
                .as_str()
            )
        );
        assert_eq!(views[0].issued_manager_rev, Some(1));

        // expired：relay 标记过期，不进 active 集合，但证书仍解析（有指纹）。
        assert_eq!(views[1].status, MemberStatus::Expired);
        assert!(views[1].fingerprint.is_some());
        assert_eq!(views[1].issued_manager_rev, Some(1));

        // pending：空证书占位 → 无指纹 / 无管理 rev，但公钥字段照常投影。
        assert_eq!(views[2].status, MemberStatus::Pending);
        assert_eq!(views[2].fingerprint, None);
        assert_eq!(views[2].issued_manager_rev, None);
        assert_eq!(views[2].sign_pk, pending.cert.sign_ed25519_pub);
        assert_eq!(views[2].hpke_pk, pending.cert.hpke_x25519_pub);

        // 视图不改变 active 集合：pending/expired 都不进。
        assert_eq!(
            RelayClient::active_member_hpke_keys(&mut client).expect("active 集合"),
            vec![(genesis.id, genesis.cert.hpke_x25519_pub)],
            "成员视图不得影响 active 集合"
        );

        // Debug 只打 device_id/status/fingerprint，不含完整 sign_pk/hpke_pk。
        let debug = format!("{:?}", views[0]);
        assert!(debug.contains("MemberView") && debug.contains("Active"));
        assert!(debug.contains(&crate::ids::hex_of(&genesis.id)));
        assert!(debug.contains(&views[0].fingerprint.clone().expect("genesis 指纹")));
        assert!(!debug.contains(&crate::ids::hex_of(&genesis.cert.sign_ed25519_pub)));
        assert!(!debug.contains(&crate::ids::hex_of(&genesis.cert.hpke_x25519_pub)));
        let debug = format!("{:?}", views[2]);
        assert!(debug.contains("Pending"));
        assert!(!debug.contains(&crate::ids::hex_of(&pending.cert.sign_ed25519_pub)));
        assert!(!debug.contains(&crate::ids::hex_of(&pending.cert.hpke_x25519_pub)));
    }

    #[test]
    fn control_signed_revocation_is_permanent_against_join_replay() {
        // 吊销是永久事实：恶意 relay 在吊销事件之后重放旧的合法 approve 记录
        // （链哈希自洽、证书与签名都合法）也不能把设备恢复为 active。
        let manager = DeviceSigningKey::generate(&mut TestRng::new(205)).expect("管理密钥");
        let genesis = test_device(0xe1);
        let revoked = test_device(0xe2);
        let mut chain = ControlChainBuilder::with_genesis_cert(manager.clone(), &genesis);
        chain.push_join(&revoked);
        chain.push_begin_revoking(&[revoked.id]);
        // 重放：吊销之后又追加同一份合法证书的"加入"记录。
        chain.push_join(&revoked);
        assert_eq!(chain.rev, 4);

        let members = vec![
            member_fields(&genesis, true, &manager),
            member_fields(&revoked, true, &manager),
        ];
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(chain.rev, 0)));
        responses.push(cbor_response(chain.fields(members)));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));

        RelayClient::control_kinds(&mut client).expect("控制链必须通过");
        assert_eq!(
            RelayClient::active_member_hpke_keys(&mut client).expect("active keys"),
            vec![(genesis.id, genesis.cert.hpke_x25519_pub)],
            "重放旧的加入事件不能解除已签吊销"
        );
        assert_eq!(
            client.device_cert(&revoked.id).unwrap(),
            Some(revoked.cert.clone()),
            "历史证书仍可读"
        );
    }

    #[test]
    fn control_active_set_requires_signed_join_event() {
        // active 集合来自已签加入事件：member 条目即使带有效管理签名，若控制链上没有
        // genesis/approve 的裸证书记录，也不能把它变成 active（伪造条目不生效）。
        let manager = DeviceSigningKey::generate(&mut TestRng::new(202)).expect("管理密钥");
        let genesis = test_device(0xb1);
        let forged = test_device(0xb2);
        let chain = ControlChainBuilder::with_genesis_cert(manager.clone(), &genesis);

        let members = vec![
            member_fields(&genesis, true, &manager),
            member_fields(&forged, true, &manager),
        ];
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(chain.rev, 0)));
        responses.push(cbor_response(chain.fields(members)));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));

        RelayClient::control_kinds(&mut client).expect("控制链必须通过");
        assert_eq!(
            client.device_cert(&forged.id).unwrap(),
            Some(forged.cert.clone()),
            "证书本身可读（历史验签），但不等于是已签加入事件"
        );
        assert_eq!(
            RelayClient::active_member_hpke_keys(&mut client).expect("active keys"),
            vec![(genesis.id, genesis.cert.hpke_x25519_pub)],
            "没有已签加入事件的成员条目不得生效"
        );
        assert_eq!(
            client.active_member_cert_digest().expect("active digest"),
            active_digest_for(&manager, &genesis)
        );
    }

    #[test]
    fn control_revoked_more_removes_member_and_grant_plan_skips_it() {
        // `rotation.revoked-more` 的吊销事实只认已签 canonical body；被吊销设备保留
        // 历史证书，但 active 集合 / HPKE keys / digest / grant 计划都不得包含它。
        let manager = DeviceSigningKey::generate(&mut TestRng::new(203)).expect("管理密钥");
        let genesis = test_device(0xc1);
        let revoked = test_device(0xc2);
        let rotation_id = [0x62; 16];
        let mut chain = ControlChainBuilder::with_genesis_cert(manager.clone(), &genesis);
        chain.push_join(&revoked);
        chain.push_revoked_more(rotation_id, &[revoked.id]);
        assert_eq!(chain.rev, 3);

        let members = vec![
            member_fields(&genesis, true, &manager),
            member_fields_with_status(&revoked, "revoked", true, &manager),
        ];
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(chain.rev, 0)));
        responses.push(cbor_response(chain.fields(members)));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));

        let kinds = RelayClient::control_kinds(&mut client).expect("控制链必须通过");
        assert_eq!(
            kinds.last(),
            Some(&(3, "rotation.revoked-more".to_owned())),
            "吊销事件必须被识别为已签控制记录"
        );
        assert_eq!(
            client.device_cert(&revoked.id).unwrap(),
            Some(revoked.cert.clone()),
            "历史吊销证书可用于 ≤S 验签"
        );
        assert_eq!(
            RelayClient::active_member_hpke_keys(&mut client).expect("active keys"),
            vec![(genesis.id, genesis.cert.hpke_x25519_pub)]
        );
        assert_eq!(
            client.active_member_cert_digest().expect("active digest"),
            active_digest_for(&manager, &genesis)
        );

        // plan_grants 只从 active 集合取收件人：被吊销设备不得拿到候选根。
        use crate::device::ManagementKey;
        use crate::keys::{RootSecretV1, Secret32};
        let root = RootSecretV1::new([0x5a; 16], 2, Secret32::from_array([0x77; 32]));
        let manager_key = ManagementKey::from_seed(Secret32::from_array([0x78; 32]));
        let leaves =
            crate::rotation_slots::plan_all_active_grants(&mut client, &root, &manager_key, 1)
                .expect("grant plan");
        assert_eq!(
            leaves.iter().map(|leaf| leaf.device_id).collect::<Vec<_>>(),
            vec![genesis.id],
            "plan_grants 不得给被吊销设备封候选根"
        );
    }

    #[test]
    fn control_rejects_forged_status_active_member_without_chain_join() {
        // relay 单方面把 member entry 标 active 但控制链上没有它的加入事件：
        // active 集合必须为空（fail-closed），fetch 不能把它当成员。
        let manager = DeviceSigningKey::generate(&mut TestRng::new(204)).expect("管理密钥");
        let forged = test_device(0xd1);
        let chain = ControlChainBuilder::new(manager.clone());
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(chain.rev, 0)));
        responses.push(cbor_response(
            chain.fields(vec![member_fields(&forged, true, &manager)]),
        ));
        // `members()` 自己会先读一次 /state 比对 auth_rev。
        responses.push(cbor_response(state_fields(chain.rev, 0)));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        RelayClient::control_kinds(&mut client).expect("控制链通过");
        assert!(
            RelayClient::active_member_hpke_keys(&mut client)
                .expect("active keys")
                .is_empty(),
            "没有已签加入事件的 member.status=active 不得成为 active"
        );

        // 成员视图同样 fail-closed：无法证明 active 的成员不得显示为 Active。
        let views = client.members().expect("成员视图");
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].device_id, forged.id);
        assert_eq!(
            views[0].status,
            MemberStatus::Revoked,
            "没有已签加入事件的 active 成员不得在视图里显示为 Active"
        );
        assert!(views[0].fingerprint.is_some(), "证书验签通过 → 指纹可解析");
    }

    #[test]
    fn updates_put_part_and_ack_use_cbor_layout() {
        let object_id = [0x77; 32];
        let commit = menghunote_sync_proto::ObjectCommit {
            vault_id: [0x5a; 16],
            object_id,
            message_id: [0x01; 16],
            object_type: menghunote_sync_proto::ObjectType::Update,
            generation: 1,
            key_epoch: 1,
            sender_seq: 1,
            part_count: 1,
            total_len: 10,
            object_digest: [0x02; 32],
        };
        let mut responses = auth_responses(1200);
        responses.push(HttpResponse {
            status: 201,
            body: cbor::encode(vec![
                (0, cbor::bytes(object_id)),
                (1, cbor::uint(0)),
                (2, cbor::bytes([0x09; 32])),
                (3, cbor::boolean(false)),
            ])
            .unwrap(),
        });
        responses.push(cbor_response(vec![(
            0,
            cbor::array(vec![
                cbor::CdeValue::Map(vec![
                    (0, cbor::bytes(object_id)),
                    (1, cbor::text("accepted")),
                    (2, cbor::uint(5)),
                    (4, cbor::boolean(false)),
                ]),
                cbor::CdeValue::Map(vec![
                    (0, cbor::bytes([0x78; 32])),
                    (1, cbor::text("failed")),
                    (3, cbor::text("EPOCH")),
                    (4, cbor::boolean(false)),
                ]),
            ]),
        )]));
        responses.push(HttpResponse {
            status: 204,
            body: Vec::new(),
        });
        let signed_commit = commit.encode().unwrap();
        let mut client = client_with(responses, FakeClock::new(900), None);

        client.put_part(&object_id, 0, &[0xaa; 4]).unwrap();
        let outcomes = client
            .submit_updates(vec![UpdateSubmission {
                object_id,
                signed_commit: signed_commit.clone(),
                commit,
                signature: [0u8; 64],
            }])
            .unwrap();
        assert_eq!(
            outcomes[0].status,
            UpdateStatus::Accepted { relay_seq: Some(5) }
        );
        assert!(matches!(
            outcomes[1].status,
            UpdateStatus::Failed {
                code: Some(_),
                retryable: false
            }
        ));
        client.ack(1, 1, 1).unwrap();

        // 请求布局与 relay 一致：CBOR map；update item = {0,1,2}。
        assert_eq!(
            client.transport.requests[2]
                .headers
                .iter()
                .find(|(name, _)| name == "content-type")
                .map(|(_, value)| value.as_str()),
            Some(CONTENT_TYPE_OCTET)
        );
        let update_body = cbor::decode(&client.transport.requests[3].body).unwrap();
        let items = cbor::array_field(&update_body, 0).unwrap();
        let item = cbor::value_map(&items[0]).unwrap();
        assert_eq!(item.len(), 3);
        assert_eq!(cbor::bytes_field(&item, 0).unwrap(), object_id.to_vec());
        assert_eq!(cbor::bytes_field(&item, 1).unwrap(), signed_commit);
        assert_eq!(cbor::bytes_field(&item, 2).unwrap(), vec![0u8; 64]);
        let ack_body = cbor::decode(&client.transport.requests[4].body).unwrap();
        assert_eq!(ack_body[0], (0, cbor::uint(1)));
        assert_eq!(ack_body[1], (1, cbor::uint(1)));
        assert_eq!(ack_body[2], (2, cbor::uint(1)));
    }

    #[test]
    fn control_consumption_requires_pin_and_anchor_without_network() {
        // 只有"明文 http + 严格 loopback"才算显式本地开发；https 与伪 loopback 都不算。
        assert!(is_local_dev_base_url("http://127.0.0.1:8080"));
        assert!(is_local_dev_base_url("http://localhost:1/"));
        assert!(is_local_dev_base_url("http://[::1]:9000"));
        assert!(!is_local_dev_base_url("https://127.0.0.1:8080"));
        assert!(!is_local_dev_base_url("http://127.0.0.1.evil.example"));
        assert!(!is_local_dev_base_url("http://relay.example"));

        let manager = DeviceSigningKey::generate(&mut TestRng::new(96)).expect("管理密钥");

        // 生产 https + 无 pin：明确 Unsupported，且**一个请求都不能发**。
        let mut client = client_at(
            Vec::new(),
            FakeClock::new(900),
            Some(manager.public_key()),
            "https://relay.example",
        );
        let error = RelayClient::control_kinds(&mut client).expect_err("无 pin 必须拒绝");
        assert!(
            matches!(&error, SyncError::Unsupported(message) if message.contains("ControlPin")),
            "{error:?}"
        );
        assert!(
            client.transport.requests.is_empty(),
            "门禁必须在任何请求之前"
        );

        // 有 pin 但缺管理锚：同样 fail-stop（relay 的 rev1 不能充当信任锚）。
        let mut client = client_at(
            Vec::new(),
            FakeClock::new(900),
            None,
            "https://relay.example",
        )
        .with_control_pin(ControlPin::new([0x22; 16], 1));
        let error = RelayClient::control_kinds(&mut client).expect_err("缺锚必须拒绝");
        assert!(
            matches!(&error, SyncError::Unsupported(message) if message.contains("管理公钥")),
            "{error:?}"
        );
        assert!(client.transport.requests.is_empty());

        // bootstrap pins → ControlPin helper：缺 instance/rev 必须明确失败，不缺则成功。
        let empty_pins = crate::bootstrap::BootstrapPins {
            instance_id: None,
            log_id: None,
            epoch: None,
            control_rev: None,
            control_hash: None,
            highwater: None,
        };
        let error = client_at(
            Vec::new(),
            FakeClock::new(900),
            Some(manager.public_key()),
            "https://relay.example",
        )
        .with_bootstrap_control_pin(&empty_pins)
        .err()
        .expect("缺 pin 字段必须拒绝");
        assert!(
            matches!(&error, SyncError::Unsupported(message) if message.contains("ControlPin")),
            "{error:?}"
        );
        let full_pins = crate::bootstrap::BootstrapPins {
            instance_id: Some([0x22; 16]),
            control_rev: Some(2),
            ..empty_pins
        };
        assert!(client_at(
            Vec::new(),
            FakeClock::new(900),
            Some(manager.public_key()),
            "https://relay.example",
        )
        .with_bootstrap_control_pin(&full_pins)
        .is_ok());

        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_begin();
        let members = vec![member_fields(&test_device(0x70), true, &manager)];

        // 有 pin（https）：正常消费完整控制链。
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(chain.fields(members.clone())));
        let mut client = client_at(
            responses,
            FakeClock::new(900),
            Some(manager.public_key()),
            "https://relay.example",
        )
        .with_control_pin(ControlPin::new([0x22; 16], 1));
        assert_eq!(
            RelayClient::control_kinds(&mut client)
                .expect("有 pin 必须通过")
                .len(),
            2
        );

        // 显式 bootstrap（https、无 pin）：允许，但仍强制管理锚与逐条验签。
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(2, 1)));
        responses.push(cbor_response(chain.fields(members)));
        let mut client = client_at(
            responses,
            FakeClock::new(900),
            Some(manager.public_key()),
            "https://relay.example",
        )
        .with_unpinned_bootstrap_control();
        assert_eq!(
            RelayClient::control_kinds(&mut client)
                .expect("显式 bootstrap 必须通过")
                .len(),
            2
        );
    }

    #[test]
    fn control_rejects_replaced_genesis_missing_chain_and_typed_genesis() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(97)).expect("管理密钥");
        let members = vec![member_fields(&test_device(0x71), true, &manager)];

        // 1. 替换 rev1 记录：管理链 rev1 的 transition 仍是原 genesis → 逐字节不等即拒绝。
        let mut chain = ControlChainBuilder::new(manager.clone());
        let other = cbor::encode(vec![(0, cbor::bytes([0xee; 16]))]).expect("另一份 genesis");
        chain.replace_genesis_record(other);
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(1, 0)));
        responses.push(cbor_response(chain.fields(members.clone())));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            RelayClient::control_kinds(&mut client),
            Err(SyncError::RelayWire(
                "管理链 rev=1 transition 与 rev1 控制记录不一致（genesis 被替换）"
            ))
        ));

        // 2. 缺管理链（旧 relay 形态）：不得退回"信任 rev1"。
        let chain = ControlChainBuilder::new(manager.clone());
        let mut fields = chain.fields(members.clone());
        fields.retain(|(key, _)| *key != 5);
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(1, 0)));
        responses.push(cbor_response(fields));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            RelayClient::control_kinds(&mut client),
            Err(SyncError::RelayWire(
                "控制链缺少管理权转换链（无法复核 genesis 信任锚）"
            ))
        ));

        // 3. rev1 伪装成 typed 记录（无签名 genesis 不得当信任锚）。
        let mut chain = ControlChainBuilder::new(manager.clone());
        let cbor::CdeValue::Map(commit) = &mut chain.commits[0] else {
            panic!("commit 必须是 map");
        };
        commit.push((4, cbor::text("rotation.begin")));
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(1, 0)));
        responses.push(cbor_response(chain.fields(members)));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            RelayClient::control_kinds(&mut client),
            Err(SyncError::RelayWire("rev1 genesis 不得带 typed kind"))
        ));
    }

    #[test]
    fn abort_control_record_verifies_reconstructed_signing_input() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(98)).expect("管理密钥");
        let rotation_id = [0x28; 16];
        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_begin();
        chain.push_abort(rotation_id);
        let members = vec![member_fields(&test_device(0x72), true, &manager)];
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(3, 1)));
        responses.push(cbor_response(chain.fields(members)));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));

        let kinds = RelayClient::control_kinds(&mut client)
            .expect("abort 的 rid/path/body_digest 必须能重建签名输入并验签");
        assert_eq!(kinds.last(), Some(&(3, "rotation.abort".to_owned())));
    }

    #[test]
    fn abort_control_record_rejects_tampered_missing_or_foreign_signature() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(99)).expect("管理密钥");
        let rotation_id = [0x29; 16];
        let members = vec![member_fields(&test_device(0x73), true, &manager)];
        let empty_body = cbor::encode(Vec::new()).expect("空 canonical body");
        let body_digest: [u8; 32] = Sha256::digest(&empty_body).into();

        fn expect_error(
            manager: &DeviceSigningKey,
            chain: ControlChainBuilder,
            members: Vec<cbor::CdeValue>,
            predicate: impl FnOnce(&SyncError) -> bool,
            label: &str,
        ) {
            let rev = chain.rev;
            let mut responses = auth_responses(1200);
            responses.push(cbor_response(state_fields(rev, 1)));
            responses.push(cbor_response(chain.fields(members)));
            let mut client =
                client_with(responses, FakeClock::new(900), Some(manager.public_key()));
            let error = RelayClient::control_kinds(&mut client).expect_err(label);
            assert!(predicate(&error), "{label}: {error:?}");
        }

        // 篡改 rotation id：event.path 不再由 rid 规范化重建。
        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_begin();
        let event = chain.abort_event(rotation_id, body_digest);
        let event = mutate_event(&event, |entries| {
            entries[0].1 = cbor::bytes([0xEE; 16]);
        });
        chain.push_abort_with_event(rotation_id, event);
        expect_error(
            &manager,
            chain,
            members.clone(),
            |error| {
                matches!(
                    error,
                    SyncError::RelayWire("rotation.abort 事件 path 与 rotation id 不符")
                )
            },
            "篡改 rid",
        );

        // 篡改 path。
        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_begin();
        let event = chain.abort_event(rotation_id, body_digest);
        let event = mutate_event(&event, |entries| {
            entries[2].1 = cbor::text("/v1/vaults/1/rotations/2/abort");
        });
        chain.push_abort_with_event(rotation_id, event);
        expect_error(
            &manager,
            chain,
            members.clone(),
            |error| {
                matches!(
                    error,
                    SyncError::RelayWire("rotation.abort 事件 path 与 rotation id 不符")
                )
            },
            "篡改 path",
        );

        // 篡改 body digest。
        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_begin();
        let event = chain.abort_event(rotation_id, body_digest);
        let event = mutate_event(&event, |entries| {
            entries[3].1 = cbor::bytes([0u8; 32]);
        });
        chain.push_abort_with_event(rotation_id, event);
        expect_error(
            &manager,
            chain,
            members.clone(),
            |error| {
                matches!(
                    error,
                    SyncError::RelayWire("rotation.abort 事件的 body_digest 与已签 body 不符")
                )
            },
            "篡改 body_digest",
        );

        // 缺 typed 事件（旧 relay 形态）：无法重建 → fail-stop，绝不"跳过验签"。
        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_begin();
        let path = chain.abort_path(rotation_id);
        chain.push_typed(ControlKind::RotationAbort, &empty_body, &path, None);
        expect_error(
            &manager,
            chain,
            members.clone(),
            |error| {
                matches!(
                    error,
                    SyncError::RelayWire("rotation.abort 缺 typed 事件（无法重建管理签名输入）")
                )
            },
            "缺事件",
        );

        // 缺签名。
        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_begin();
        let path = chain.abort_path(rotation_id);
        let event = chain.abort_event(rotation_id, body_digest);
        chain.push_typed_with_signature(
            ControlKind::RotationAbort,
            &empty_body,
            &path,
            Some(event),
            Vec::new(),
        );
        expect_error(
            &manager,
            chain,
            members.clone(),
            |error| matches!(error, SyncError::RelayWire("控制记录管理签名长度非法")),
            "缺签名",
        );

        // 错管理 key：签名 64B 但无法用管理链 rev1 公钥验证 → Signature。
        let wrong = DeviceSigningKey::generate(&mut TestRng::new(100)).expect("错误管理 key");
        let mut chain = ControlChainBuilder::new(manager.clone());
        chain.push_begin();
        chain.push_abort_signed_by(rotation_id, &wrong);
        expect_error(
            &manager,
            chain,
            members,
            |error| matches!(error, SyncError::Signature),
            "错管理 key",
        );
    }

    #[test]
    fn grant_leaf_evidence_recomputes_headers_and_grants_digest() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(101)).expect("管理密钥");
        let rotation_id = [0x2a; 16];

        fn leaf(device: [u8; 16], header: &[u8]) -> cbor::CdeValue {
            cbor::CdeValue::Map(vec![
                (0, cbor::bytes(device)),
                (1, cbor::bytes(Sha256::digest(header).to_vec())),
                (2, cbor::bytes(header.to_vec())),
            ])
        }

        fn leaf_digest(leaves: &[([u8; 16], cbor::CdeValue)]) -> [u8; 32] {
            let mut ordered = leaves.to_vec();
            ordered.sort_by_key(|(device, _)| *device);
            let mut input = Vec::new();
            for (device, value) in ordered {
                let cbor::CdeValue::Map(fields) = value else {
                    panic!("leaf 必须是 map");
                };
                let digest = fields
                    .iter()
                    .find(|(key, _)| *key == 1)
                    .map(|(_, value)| match value {
                        cbor::CdeValue::Bytes(bytes) => bytes.clone(),
                        _ => panic!("digest 必须是 bytes"),
                    })
                    .expect("digest");
                input.extend_from_slice(&device);
                input.extend_from_slice(&digest);
            }
            Sha256::digest(&input).into()
        }

        fn response(
            rotation_id: [u8; 16],
            leaves: Vec<([u8; 16], cbor::CdeValue)>,
            digest: [u8; 32],
            count: u64,
        ) -> cbor::Fields {
            vec![
                (
                    0,
                    cbor::array(leaves.iter().map(|(_, leaf)| leaf.clone()).collect()),
                ),
                (1, cbor::bytes(digest)),
                (2, cbor::uint(count)),
                (3, cbor::uint(2)),
                (4, cbor::text("staging")),
                (5, cbor::bytes(rotation_id)),
            ]
        }

        // 正例：两枚 leaf，设备顺序故意倒序，客户端必须用 sorted(device) 复算。
        let a = [0xa1; 16];
        let b = [0xa2; 16];
        let leaves = vec![(b, leaf(b, b"header-b")), (a, leaf(a, b"header-a"))];
        let digest = leaf_digest(&leaves);
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(response(
            rotation_id,
            leaves.clone(),
            digest,
            2,
        )));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        let evidence = RelayClient::grant_leaf_evidence(&mut client, rotation_id)
            .expect("leaf manifest")
            .expect("HTTP 客户端必须返回 Some");
        assert_eq!(evidence.len(), 2);
        assert_eq!(evidence[0].device_id, b);
        assert_eq!(
            evidence[0].signed_header_sha256,
            <[u8; 32]>::from(Sha256::digest(b"header-b"))
        );
        assert_eq!(
            crate::rotation_apply::grant_set_digest(&evidence),
            digest,
            "客户端复算必须与 relay grants_digest 一致"
        );

        // 负例：leaf header 与自报 digest 不一致。
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(response(
            rotation_id,
            vec![(
                a,
                cbor::CdeValue::Map(vec![
                    (0, cbor::bytes(a)),
                    (1, cbor::bytes([0u8; 32])),
                    (2, cbor::bytes(b"header-a".to_vec())),
                ]),
            )],
            digest,
            1,
        )));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            RelayClient::grant_leaf_evidence(&mut client, rotation_id),
            Err(SyncError::RelayWire(
                "grant leaf 的 signed_header digest 与 header 不符"
            ))
        ));

        // 负例：leaf 数量与 grants_count 不符。
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(response(
            rotation_id,
            leaves.clone(),
            digest,
            3,
        )));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            RelayClient::grant_leaf_evidence(&mut client, rotation_id),
            Err(SyncError::RelayWire("grant leaf 数量与 grants_count 不符"))
        ));

        // 负例：重复设备。
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(response(
            rotation_id,
            vec![(a, leaf(a, b"header-a")), (a, leaf(a, b"header-a"))],
            leaf_digest(&[(a, leaf(a, b"header-a")), (a, leaf(a, b"header-a"))]),
            2,
        )));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            RelayClient::grant_leaf_evidence(&mut client, rotation_id),
            Err(SyncError::RelayWire("grant leaf manifest 含重复设备"))
        ));

        // 负例：复算 digest 与 relay 自报不符。
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(response(rotation_id, leaves, [0u8; 32], 2)));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            RelayClient::grant_leaf_evidence(&mut client, rotation_id),
            Err(SyncError::RelayWire(
                "grant leaf manifest 与 grants_digest 不符"
            ))
        ));
    }
}
