//! ADR 0013 **S2**：relay 接入的**唯一** context 与客户端构造点。
//!
//! ## 为什么需要它
//!
//! S1 之前，每个操作（sync / publish / rotate / apply / members / revoke）各自用
//! `StdTcpTransport::new_local_dev*` 拼 transport、再手工装 `ControlPin`。散点带来两类
//! 风险（ADR 0013「前提校正」）：
//!
//! 1. 传输策略可能退化成环境敏感/明文默认（生产路径绝不能靠 `StdTcpTransport::new`）；
//! 2. **pin 只存在于进程内**，同 rev 换内容/换实例无人校验，也没有 durable 提交。
//!
//! [`RelayContext`] 把"模式 + canonical origin + 传输预算 + pin 策略 + 管理锚"收在一处，
//! [`RelayContext::open`] 是**唯一**的客户端构造入口：
//!
//! - `remote_tls`：必须已有 durable `sync.control_pin`（带 record hash）；
//!   缺 → [`SyncError::NeedsTrust`]、损坏 → [`SyncError::PinCorrupt`]、实例不符 →
//!   [`SyncError::InstanceMismatch`]，且这些检查都在**构造传输之前**，因此
//!   **一个请求都不会发**；
//! - `local_dev`（只在显式开发构建里可用）：durable pin 优先；没有则用受保护身份的
//!   `(pinned_instance, control_rev)` 下界 + `sync.pinned_*` 的 bootstrap 锚。仍然
//!   fail-closed：损坏的 pin 不当缺失、实例不符即拒。
//!
//! 生产传输一律 [`crate::relay_policy::production_transport`]（显式
//! `TransportPolicy::production()`），本地开发一律
//! [`crate::relay_policy::local_dev_transport`]；本模块**不调用**
//! `StdTcpTransport::new{,_local_dev,_local_dev_with}`。
//!
//! ## durable 提交时序（ADR 0013 §2）
//!
//! `open` 给客户端装一个 **committer**：`/control` 全链验签 + 分页/前缀连续性 +
//! pin 单调性都通过之后、把控制链放进 `self.control`（授权事实缓存）**之前**，
//! 先写 durable `sync.control_pin`。写失败/CAS 冲突 → 本次刷新失败、不缓存、不消费
//! 任何授权事实；写成功后业务步骤再失败也**不回退** pin。
//!
//! ## pending 守卫（ADR 0013 S2b）
//!
//! 初始化未完成时（`sync.init_pending` 阶段不是 `Done`），`open` 直接
//! [`SyncError::NeedsTrust`]：普通同步/管理操作**不得**在初始化对账之前启动。
//! 初始化内部需要 `open` 的唯一步骤是"上传初始 recovery 槽"，它走
//! [`RelayContext::open_for_init`]（`pub(crate)`，crate 外不可用）。
//!
//! ## 非目标
//!
//! 初始化（Create/Join 的 `NewGenesis / FreshRecovery / VerifiedMigration` 受限能力）
//! 属于 S2b 的 [`crate::init_session`]：本模块不提供 unpinned 初始化入口，`open` 也不会
//! 把缺失 pin 补成"信任"。app 侧（ADR 0013 S3a 起）按公开配置的 `relay_mode` 用
//! [`RelayContext::from_mode`] 构造 context；`remote_tls` 走 production TLS。

use std::ops::{Deref, DerefMut};

use menghunote_storage::Vault;

use crate::api_version::{RelayApi, RelayApiVersion};
use crate::bootstrap::BootstrapPins;
use crate::control_pin::{commit_verified_head, load_control_pin, VerifiedHead};
use crate::dev_local::root_of;
use crate::device::DeviceSigningKey;
use crate::error::SyncError;
use crate::http_relay::{
    ControlPin, HttpRelayClient, HttpRelayConfig, StdTcpTransport, VerifiedControlHead,
};
use crate::http_transport::{HttpTransport, TransportTimeouts, DEFAULT_TRANSPORT_TIMEOUTS};
use crate::identity::LocalIdentityV1;
use crate::init_journal;
use crate::relay_policy::{
    local_dev_transport, parse_relay_url, production_transport, RelayMode, RelayOrigin,
};

/// pin 策略（由模式决定；**不是**调用方可随时加的开关）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinPolicy {
    /// `remote_tls`：必须存在 durable `sync.control_pin`，且带 `record.hash`；
    /// 缺失/损坏/只有 `(instance, rev)` 下界一律停止（ADR 0013 §2）。
    DurableRequired,
    /// `local_dev`：durable pin 优先；没有则用身份的 `(instance, rev)` 下界 +
    /// bootstrap 锚（仍不接受损坏 pin，也不把 hash 挂到别的 rev 上）。
    LocalDevLowerBound,
}

impl PinPolicy {
    /// 按模式给出唯一策略。
    #[must_use]
    pub const fn for_mode(mode: RelayMode) -> Self {
        match mode {
            RelayMode::RemoteTls => Self::DurableRequired,
            RelayMode::LocalDev => Self::LocalDevLowerBound,
        }
    }
}

/// relay 接入上下文：模式 + canonical origin + 传输预算 + pin 策略 + 管理锚。
///
/// 所有同步操作都接受**同一个** context（URL 兼容入口只是先构造一个 `local_dev`
/// context 再委托），因此"生产策略 / durable pin / 预算"不会因操作不同而漂移。
#[derive(Clone)]
pub struct RelayContext {
    /// 接入模式（`local_dev` / `remote_tls`）。
    pub mode: RelayMode,
    /// canonical origin（`scheme://host[:port]`，已按唯一权威 parser 规范化）。
    pub origin: RelayOrigin,
    /// relay API 面（ADR 0014 S3a）：`/v1` 保旧 wire，`/v2` 只显式选择。
    ///
    /// 字段公开只为诊断/测试断言；构造入口默认 [`RelayApiVersion::V1`]，
    /// v2 必须经 [`RelayContext::with_api_version`] 显式开启（票据创建/新配置）。
    pub api_version: RelayApiVersion,
    /// 传输预算（连接/读写/整次请求）。
    pub timeouts: TransportTimeouts,
    /// pin 策略（由 `mode` 决定；字段公开只为诊断/测试断言）。
    pub pin_policy: PinPolicy,
    /// 管理权信任锚（ADR §35.2）。`None` = 取受保护身份里的 `pinned_manager`；
    /// 显式给出时必须与身份逐字节相等（S2b 的恢复码推导会用到这条）。
    pub manager_anchor: Option<[u8; 32]>,
    /// 额外信任根（DER；**只增加显式信任**，不关闭任何证书链/主机名/有效期校验）。
    ///
    /// 普通路径恒为空；唯一使用点是本地 TLS 测试 harness（`with_test_extra_roots`）。
    /// ADR 0013 §5 的"按实例导入私有 CA"将来复用同一注入点（仍需显式导入 + DER 副本）。
    extra_trust_roots: Vec<Vec<u8>>,
}

impl std::fmt::Debug for RelayContext {
    /// 不打印信任根的 DER（体积大且无诊断价值）；其余公开字段原样。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayContext")
            .field("mode", &self.mode)
            .field("origin", &self.origin)
            .field("api_version", &self.api_version)
            .field("timeouts", &self.timeouts)
            .field("pin_policy", &self.pin_policy)
            .field("manager_anchor", &self.manager_anchor)
            .field("extra_trust_roots", &self.extra_trust_roots.len())
            .finish()
    }
}

impl RelayContext {
    /// 本地开发 context：只接受 `http://localhost | 127.0.0.1 | [::1]`。
    ///
    /// 生产代码不得走到这里（ADR 0013 §1/§6：`local_dev` 只在显式开发构建中开放）。
    pub fn local_dev(relay_url: &str, timeouts: TransportTimeouts) -> Result<Self, SyncError> {
        let origin = parse_relay_url(relay_url, RelayMode::LocalDev)?;
        Ok(Self {
            mode: RelayMode::LocalDev,
            origin,
            api_version: RelayApiVersion::V1,
            timeouts,
            pin_policy: PinPolicy::LocalDevLowerBound,
            manager_anchor: None,
            extra_trust_roots: Vec::new(),
        })
    }

    /// 生产 context：只接受 `https://`（DNS/IP，无 userinfo/query/fragment/非根路径）。
    ///
    /// app 的 worker（ADR 0013 S3a 起）按公开配置的模式调它；`remote_tls` 的
    /// Create/Join 初始化（S3b 起）走 [`crate::remote_init`]，同样只接受这个 context。
    pub fn remote_tls(relay_url: &str, timeouts: TransportTimeouts) -> Result<Self, SyncError> {
        let origin = parse_relay_url(relay_url, RelayMode::RemoteTls)?;
        Ok(Self {
            mode: RelayMode::RemoteTls,
            origin,
            api_version: RelayApiVersion::V1,
            timeouts,
            pin_policy: PinPolicy::DurableRequired,
            manager_anchor: None,
            extra_trust_roots: Vec::new(),
        })
    }

    /// 按模式构造 context（ADR 0013 §1：模式是显式配置，**不从 URL 猜**）。
    ///
    /// app 的 worker 用公开配置里的 `relay_mode` 调它；非法 URL/模式不符仍由唯一
    /// 权威 parser 拒绝（`remote_tls` 只接受 https，`local_dev` 只接受明文 loopback）。
    pub fn from_mode(
        mode: RelayMode,
        relay_url: &str,
        timeouts: TransportTimeouts,
    ) -> Result<Self, SyncError> {
        match mode {
            RelayMode::LocalDev => Self::local_dev(relay_url, timeouts),
            RelayMode::RemoteTls => Self::remote_tls(relay_url, timeouts),
        }
    }

    /// 显式选择 relay API 面（ADR 0014 S3a）。
    ///
    /// 缺省恒为 v1；**只有**票据创建（`/v2/vaults`）与 v2 新配置会传
    /// [`RelayApiVersion::V2`]。同一个 vault 的面在创建时固定，混用会让历史管理签名
    /// 验签失败（fail-closed，不会静默换面）。
    #[must_use]
    pub fn with_api_version(mut self, api_version: RelayApiVersion) -> Self {
        self.api_version = api_version;
        self
    }

    /// 本次 context 的 API 面。
    #[must_use]
    pub const fn api_version(&self) -> RelayApiVersion {
        self.api_version
    }

    /// 路径生成器（origin + 面；业务路径的唯一入口）。
    #[must_use]
    pub fn api(&self) -> RelayApi {
        RelayApi::new(self.origin.as_str(), self.api_version)
    }

    /// 显式指定管理权信任锚（必须与受保护身份一致；S2b 初始化路径用）。
    #[must_use]
    pub fn with_manager_anchor(mut self, anchor: [u8; 32]) -> Self {
        self.manager_anchor = Some(anchor);
        self
    }

    /// **测试/私有 CA 注入**：给传输追加显式信任根（DER 编码的 CA 证书）。
    ///
    /// 只**增加**显式信任：证书链、主机名/IP SAN 与有效期仍由 rustls 校验；没有
    /// accept-invalid / 跳过校验的分支（ADR 0013 §5）。生产路径不设置（默认空）。
    ///
    /// 使用点：本地 TLS relay harness（`menghunote-dev-relay` 的 `start_tls`，见
    /// sync 的 `tests/remote_init.rs` 与 app 的 onboarding_ui 远端用例）；将来
    /// "按实例导入私有 CA"复用同一入口。
    #[doc(hidden)]
    pub fn with_test_extra_roots(
        mut self,
        roots: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> Result<Self, SyncError> {
        // 立即验一次 DER，坏证书在**构造期**就失败（不等到发请求）。
        let mut probe = rustls::RootCertStore::empty();
        for root in roots {
            let der = root.as_ref().to_vec();
            probe
                .add(rustls::pki_types::CertificateDer::from(der.clone()))
                .map_err(|error| {
                    SyncError::Tls(format!("注入的信任根不是合法 CA 证书：{error}"))
                })?;
            self.extra_trust_roots.push(der);
        }
        Ok(self)
    }

    /// 当前注入的信任根数量（诊断/测试断言用）。
    #[must_use]
    pub fn extra_trust_root_count(&self) -> usize {
        self.extra_trust_roots.len()
    }

    /// canonical origin 字符串（日志/指纹显示用，不含 secret）。
    #[must_use]
    pub fn origin_str(&self) -> String {
        self.origin.as_str()
    }

    /// 按模式构造**唯一**合法传输（`remote_tls` → production；`local_dev` → 开发策略）。
    ///
    /// 这是本 crate 里除 `relay_policy` 之外唯一允许创建 `StdTcpTransport` 的地方：
    /// 任何操作要发请求都必须经这里（或经 [`Self::open`] 的客户端）。
    pub fn transport(&self) -> Result<StdTcpTransport, SyncError> {
        let transport = match self.mode {
            RelayMode::RemoteTls => production_transport(&self.origin, Some(self.timeouts))?,
            RelayMode::LocalDev => local_dev_transport(&self.origin, Some(self.timeouts))?,
        };
        if self.extra_trust_roots.is_empty() {
            return Ok(transport);
        }
        // 只追加显式信任根；TLS 链/SAN/有效期校验逻辑不变（`with_extra_roots` 只扩根集）。
        transport.with_extra_roots(self.extra_trust_roots.iter().map(Vec::as_slice))
    }

    /// 构造一个已完成 pin 决议的客户端（**唯一**构造入口）。
    ///
    /// 顺序刻意是"先决议 pin、再建传输"：`remote_tls` 缺/坏 pin 时直接失败，
    /// 连 socket 都不会创建（更不会发请求）。
    pub fn open(
        &self,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<PinnedClient, SyncError> {
        // 初始化未完成 → 拒绝普通同步/管理操作（ADR 0013 §3）。这不是"信任降级"，
        // 而是"还有一笔未结清的初始化事务"：必须先对账完成/放弃。
        init_journal::ensure_no_pending_sync(vault, identity.vault_id)?;
        self.open_with(vault, identity, Self::transport)
    }

    /// **初始化内部**用的客户端构造：跳过 pending 守卫（crate 内唯一使用点是
    /// [`crate::dev_local::DevLocal::create`] 的初始 recovery 槽上传）。
    ///
    /// 它仍然是**带 pin 决议**的构造（没有 unpinned 分支）；`pub(crate)` 保证外部无法
    /// 用它绕过 pending 守卫。
    pub(crate) fn open_for_init(
        &self,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<PinnedClient, SyncError> {
        self.open_with(vault, identity, Self::transport)
    }

    /// [`Self::open`] 的泛型主体（测试注入传输/构造计数用）。
    ///
    /// `build_transport` 在 pin 决议**之后**调用：测试可以断言"坏 pin 时工厂从未被调用"。
    pub(crate) fn open_with<T: HttpTransport>(
        &self,
        vault: &Vault,
        identity: &LocalIdentityV1,
        build_transport: impl FnOnce(&Self) -> Result<T, SyncError>,
    ) -> Result<PinnedClient<T>, SyncError> {
        let manager_pub = self.resolve_manager(identity)?;
        let pin = self.resolve_pin(vault, identity)?;
        let transport = build_transport(self)?;
        let config = HttpRelayConfig {
            base_url: self.origin.as_str(),
            api_version: self.api_version,
            vault_id: identity.vault_id,
            device_id: identity.device_id,
            manager_pub: Some(manager_pub),
        };
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let committer = self.committer(vault.clone(), identity.vault_id);
        let client = HttpRelayClient::new(transport, config, signing_key)
            .with_root(root_of(identity))
            .with_control_pin(pin)
            .with_control_committer(committer);
        Ok(PinnedClient {
            client,
            vault_id: identity.vault_id,
        })
    }

    /// 管理锚：显式给出时必须与受保护身份一致；全 0 视为缺失（fail-closed）。
    fn resolve_manager(&self, identity: &LocalIdentityV1) -> Result<[u8; 32], SyncError> {
        let anchor = self.manager_anchor.unwrap_or(identity.pinned_manager);
        if anchor != identity.pinned_manager {
            return Err(SyncError::NeedsTrust(
                "context 的管理锚与受保护身份不一致：拒绝用未受信的锚消费 /control",
            ));
        }
        if anchor == [0u8; 32] {
            return Err(SyncError::NeedsTrust(
                "缺少管理权信任锚：拒绝消费 /control（ADR §35.2）",
            ));
        }
        Ok(anchor)
    }

    /// 决议本次客户端使用的本地 pin（与 [`PinPolicy`] 一一对应）。
    ///
    /// 全部路径都先 `load_control_pin`：**损坏/版本未知/绑定不符**是明确错误，
    /// 绝不降级成"没有 pin"（ADR 0013 §2）。
    fn resolve_pin(
        &self,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<ControlPin, SyncError> {
        let stored = load_control_pin(vault, identity.vault_id)?;
        match self.pin_policy {
            PinPolicy::DurableRequired => {
                let stored = stored.ok_or(SyncError::NeedsTrust(
                    "生产路径缺 durable sync.control_pin：拒绝以 unpinned 方式消费 /control",
                ))?;
                if stored.instance_id != identity.pinned_instance {
                    return Err(SyncError::InstanceMismatch);
                }
                if stored.record.hash.is_none() {
                    return Err(SyncError::NeedsTrust(
                        "durable pin 只有 (instance, rev) 下界、缺 record hash：无法证明同 rev 历史完整性",
                    ));
                }
                if stored.record.rev < identity.control_rev {
                    return Err(SyncError::NeedsTrust(
                        "durable pin 落后于受保护身份的 control_rev：拒绝退回 identity-only",
                    ));
                }
                Ok(stored.to_control_pin())
            }
            PinPolicy::LocalDevLowerBound => {
                let meta_anchor = BootstrapPins::from_vault(vault)?.control_anchor()?;
                let Some(stored) = stored else {
                    let mut pin = ControlPin::from_identity(identity);
                    if let Some(anchor) = meta_anchor {
                        pin = pin.with_bootstrap_anchor(anchor);
                    }
                    return Ok(pin);
                };
                if stored.instance_id != identity.pinned_instance {
                    return Err(SyncError::InstanceMismatch);
                }
                let durable_anchor = stored.bootstrap;
                // durable pin 是**已提交**的验证结果，优先于 `sync.pinned_*` 这个
                // bootstrap 提示（ADR 0013 §2「已有 record 永远优先于弱初始化路径」）。
                // 同 rev 却内容不同 = 本地绑定矛盾，必须拒绝；不同 rev 时只能取 durable
                // （在 `open` 阶段没有管理链，无法证明提示锚的历史前缀）。
                let anchor = match (durable_anchor, meta_anchor) {
                    (Some(durable), Some(meta)) if durable.rev == meta.rev && durable != meta => {
                        return Err(SyncError::Equivocation(
                            "durable control_pin 的 bootstrap 锚与 sync.pinned_* 不一致",
                        ))
                    }
                    (Some(durable), _) => Some(durable),
                    (None, meta) => meta,
                };
                if stored.record.rev < identity.control_rev {
                    // 身份 rev 前进：旧 record hash 不能挂到更高的 rev 上（同 rev 完整性
                    // 只在 rev 相等时有意义）；bootstrap 锚独立携带自己的 rev，必须保留。
                    let mut pin = ControlPin::new(identity.pinned_instance, identity.control_rev);
                    if let Some(anchor) = anchor {
                        pin = pin.with_bootstrap_anchor(anchor);
                    }
                    return Ok(pin);
                }
                let mut pin = stored.to_control_pin();
                if pin.bootstrap.is_none() {
                    if let Some(anchor) = anchor {
                        pin = pin.with_bootstrap_anchor(anchor);
                    }
                }
                Ok(pin)
            }
        }
    }

    /// durable 提交闭包：控制链验证通过后、安装授权缓存前写 `sync.control_pin`。
    fn committer(&self, vault: Vault, vault_id: [u8; 16]) -> crate::http_relay::ControlCommitter {
        Box::new(move |head: VerifiedControlHead| {
            // expected 必须**重读**：进程内 pin 可能已过期，CAS 判定的对象只能是库里的。
            let expected = load_control_pin(&vault, vault_id)?;
            let mut verified = VerifiedHead::from_verified_control(
                vault_id,
                head.instance_id,
                head.rev,
                head.hash,
            );
            if let Some(anchor) = head.bootstrap {
                // 再按锚自己的历史视图验证一次（`with_bootstrap` 内含摘要复算）；
                // 上游 `verify_control_pin` 已经验过，这里是第二道，不是唯一一道。
                verified = verified.with_bootstrap(anchor, &head.manager_chain)?;
            }
            commit_verified_head(&vault, expected.as_ref(), verified)
        })
    }
}

/// 已完成 pin 决议的 relay 客户端。
///
/// 只是构造期的证明与便利：durable 提交钩子已经装进内部的 [`HttpRelayClient`]，
/// 取走客户端（[`Self::into_client`]）不会丢钩子。
pub struct PinnedClient<T: HttpTransport = StdTcpTransport> {
    client: HttpRelayClient<T>,
    vault_id: [u8; 16],
}

impl<T: HttpTransport> PinnedClient<T> {
    /// 只读客户端（诊断/测试）。
    pub fn client(&self) -> &HttpRelayClient<T> {
        &self.client
    }

    /// 可变客户端（调用方的操作主体）。
    pub fn client_mut(&mut self) -> &mut HttpRelayClient<T> {
        &mut self.client
    }

    /// 取出内部客户端（durable 提交钩子随之保留）。
    #[must_use]
    pub fn into_client(self) -> HttpRelayClient<T> {
        self.client
    }

    /// 该客户端绑定的 vault id。
    #[must_use]
    pub const fn vault_id(&self) -> [u8; 16] {
        self.vault_id
    }
}

impl<T: HttpTransport> Deref for PinnedClient<T> {
    type Target = HttpRelayClient<T>;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl<T: HttpTransport> DerefMut for PinnedClient<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.client
    }
}

/// 同步操作的 relay 目标：`&str`（local_dev 兼容入口）或显式 [`RelayContext`]。
///
/// 兼容语义：`&str` 目标用调用点给的 legacy 预算
/// （`sync_once_with(...).transport`）构造一个 `local_dev` context；显式 context
/// **自带预算**，legacy 预算参数被忽略（避免两处预算冲突）。
pub trait RelayTarget {
    /// 得到本次操作使用的 context。
    fn relay_context(
        &self,
        legacy_timeouts: Option<TransportTimeouts>,
    ) -> Result<RelayContext, SyncError>;
}

impl RelayTarget for str {
    fn relay_context(
        &self,
        legacy_timeouts: Option<TransportTimeouts>,
    ) -> Result<RelayContext, SyncError> {
        RelayContext::local_dev(self, legacy_timeouts.unwrap_or(DEFAULT_TRANSPORT_TIMEOUTS))
    }
}

impl RelayTarget for String {
    fn relay_context(
        &self,
        legacy_timeouts: Option<TransportTimeouts>,
    ) -> Result<RelayContext, SyncError> {
        self.as_str().relay_context(legacy_timeouts)
    }
}

impl RelayTarget for RelayContext {
    fn relay_context(
        &self,
        _legacy_timeouts: Option<TransportTimeouts>,
    ) -> Result<RelayContext, SyncError> {
        Ok(self.clone())
    }
}

impl<T: RelayTarget + ?Sized> RelayTarget for &T {
    fn relay_context(
        &self,
        legacy_timeouts: Option<TransportTimeouts>,
    ) -> Result<RelayContext, SyncError> {
        (**self).relay_context(legacy_timeouts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::LocalIdentityV1;
    use crate::keys::Secret32;
    use crate::relay_policy::RelayMode;
    use std::cell::Cell;

    /// 测试用"传输工厂计数"：只想知道 pin 决议失败时有没有碰过 socket 构造。
    #[derive(Default)]
    struct SpyTransport {
        sent: usize,
    }

    impl HttpTransport for SpyTransport {
        fn send(
            &mut self,
            _request: &crate::http_relay::HttpRequest,
        ) -> Result<crate::http_relay::HttpResponse, SyncError> {
            self.sent += 1;
            Err(SyncError::Transport("spy 不联网"))
        }
    }

    fn fresh_vault() -> (tempfile::TempDir, Vault) {
        let base = std::env::var("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/tmp"));
        std::fs::create_dir_all(&base).expect("target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("relay-context-")
            .tempdir_in(&base)
            .expect("临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
        (dir, vault)
    }

    fn identity() -> LocalIdentityV1 {
        LocalIdentityV1 {
            vault_id: [0x5a; 16],
            device_id: [0x04; 16],
            peer_id: crate::ids::PeerId::from_bytes([0x06; 16]),
            ed25519_seed: Secret32::from_array([0x09; 32]),
            x25519_secret: Secret32::from_array([0x0a; 32]),
            current_epoch: 1,
            current_root: Secret32::from_array([0x0b; 32]),
            manager_seed: None,
            pinned_instance: [0x22; 16],
            pinned_manager: [0x07; 32],
            control_rev: 1,
            pending_rotation: None,
        }
    }

    const LOCAL: &str = "http://127.0.0.1:18080";
    const REMOTE: &str = "https://relay.example";

    #[test]
    fn mode_to_policy_is_fixed_and_origin_is_canonical() {
        let local = RelayContext::local_dev("http://LOCALHOST:18080", DEFAULT_TRANSPORT_TIMEOUTS)
            .expect("local");
        assert_eq!(local.mode, RelayMode::LocalDev);
        assert_eq!(local.pin_policy, PinPolicy::LocalDevLowerBound);
        assert_eq!(local.origin_str(), "http://localhost:18080");
        assert_eq!(local.api_version(), RelayApiVersion::V1, "缺省必须是 v1");
        let remote =
            RelayContext::remote_tls("https://Relay.Example:443/", DEFAULT_TRANSPORT_TIMEOUTS)
                .expect("remote");
        assert_eq!(remote.pin_policy, PinPolicy::DurableRequired);
        assert_eq!(remote.origin_str(), "https://relay.example");
        assert_eq!(remote.api_version(), RelayApiVersion::V1);
        // 模式与 URL 互斥：不能把 https 塞进 local_dev，也不能把 loopback 明文当生产。
        assert!(RelayContext::local_dev(REMOTE, DEFAULT_TRANSPORT_TIMEOUTS).is_err());
        assert!(RelayContext::remote_tls(LOCAL, DEFAULT_TRANSPORT_TIMEOUTS).is_err());
    }

    #[test]
    fn transport_policy_is_mode_locked() {
        let local = RelayContext::local_dev(LOCAL, DEFAULT_TRANSPORT_TIMEOUTS).expect("local");
        let transport = local.transport().expect("local transport");
        assert_eq!(
            transport.policy(),
            crate::http_transport::TransportPolicy::localhost_development()
        );
        let remote = RelayContext::remote_tls(REMOTE, DEFAULT_TRANSPORT_TIMEOUTS).expect("remote");
        let transport = remote.transport().expect("production transport");
        assert_eq!(
            transport.policy(),
            crate::http_transport::TransportPolicy::production()
        );
    }

    /// `remote_tls` 缺 pin：在**构造传输之前**就失败（不发请求的前置条件）。
    #[test]
    fn remote_tls_without_a_durable_pin_never_builds_a_transport() {
        let (_dir, vault) = fresh_vault();
        let ctx = RelayContext::remote_tls(REMOTE, DEFAULT_TRANSPORT_TIMEOUTS).expect("remote");
        let builds = Cell::new(0u32);
        let result = ctx.open_with(&vault, &identity(), |_| {
            builds.set(builds.get() + 1);
            Ok(SpyTransport::default())
        });
        let error = match result {
            Ok(_) => panic!("缺 durable pin 必须 NeedsTrust"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");
        assert_eq!(builds.get(), 0, "pin 决议失败时不得构造传输");
        assert!(
            vault
                .get_meta(crate::control_pin::META_CONTROL_PIN)
                .expect("meta")
                .is_none(),
            "拒绝路径不得写入 pin"
        );
    }

    /// 损坏 pin 必须报 `PinCorrupt`，**不能**当缺失退回 NeedsTrust/unpinned。
    #[test]
    fn corrupt_pin_is_pin_corrupt_not_missing() {
        let (_dir, vault) = fresh_vault();
        vault
            .set_meta(crate::control_pin::META_CONTROL_PIN, b"not-a-pin")
            .expect("写坏 pin");
        let ctx = RelayContext::remote_tls(REMOTE, DEFAULT_TRANSPORT_TIMEOUTS).expect("remote");
        let builds = Cell::new(0u32);
        let error = match ctx.open_with(&vault, &identity(), |_| {
            builds.set(builds.get() + 1);
            Ok(SpyTransport::default())
        }) {
            Ok(_) => panic!("损坏 pin 必须拒绝"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::PinCorrupt(_)), "{error:?}");
        assert_eq!(builds.get(), 0, "损坏 pin 时不得构造传输");

        // local_dev 同样不把损坏 pin 当缺失（fail-closed 与模式无关）。
        let local = RelayContext::local_dev(LOCAL, DEFAULT_TRANSPORT_TIMEOUTS).expect("local");
        let error = match local.open_with(&vault, &identity(), |_| Ok(SpyTransport::default())) {
            Ok(_) => panic!("local_dev 的损坏 pin 同样必须拒绝"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::PinCorrupt(_)), "{error:?}");
    }

    /// 只有 `(instance, rev)` 下界的 durable pin 在生产路径上不足以放行。
    #[test]
    fn remote_tls_rejects_a_lower_bound_only_pin() {
        let (_dir, vault) = fresh_vault();
        crate::control_pin::commit_verified_head(
            &vault,
            None,
            VerifiedHead::from_verified_control([0x5a; 16], [0x22; 16], 1, [0x11; 32]),
        )
        .expect("先写一个带 hash 的 pin");
        // 回退成"只有下界"的形态（模拟历史实例/降级写）：必须拒绝。
        let bytes = crate::control_pin::encode_pin_v1(&crate::control_pin::PinV1 {
            vault_id: [0x5a; 16],
            instance_id: [0x22; 16],
            record: crate::control_pin::PinAnchorV1 { rev: 1, hash: None },
            bootstrap: None,
        })
        .expect("编码");
        vault
            .set_meta(crate::control_pin::META_CONTROL_PIN, &bytes)
            .expect("写 lower-bound pin");
        let ctx = RelayContext::remote_tls(REMOTE, DEFAULT_TRANSPORT_TIMEOUTS).expect("remote");
        let error = match ctx.open_with(&vault, &identity(), |_| Ok(SpyTransport::default())) {
            Ok(_) => panic!("只有下界的 pin 不得放行生产路径"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");
    }

    /// 有 durable pin 时 `open` 成功，且 local_dev/remote_tls 都能各用各的策略。
    #[test]
    fn open_with_a_durable_pin_builds_a_pinned_client() {
        let (_dir, vault) = fresh_vault();
        crate::control_pin::commit_verified_head(
            &vault,
            None,
            VerifiedHead::from_verified_control([0x5a; 16], [0x22; 16], 1, [0x11; 32]),
        )
        .expect("写 durable pin");
        for ctx in [
            RelayContext::remote_tls(REMOTE, DEFAULT_TRANSPORT_TIMEOUTS).expect("remote"),
            RelayContext::local_dev(LOCAL, DEFAULT_TRANSPORT_TIMEOUTS).expect("local"),
        ] {
            let pinned = ctx
                .open_with(&vault, &identity(), |_| Ok(SpyTransport::default()))
                .expect("有 durable pin 必须可构造");
            assert_eq!(pinned.vault_id(), [0x5a; 16]);
            assert_eq!(pinned.client().cached_control_rev(), None, "构造时不联网");
        }
    }

    /// instance 绑定不符：两种模式都必须在发请求前拒绝。
    #[test]
    fn instance_mismatch_is_rejected_before_any_request() {
        let (_dir, vault) = fresh_vault();
        crate::control_pin::commit_verified_head(
            &vault,
            None,
            VerifiedHead::from_verified_control([0x5a; 16], [0x99; 16], 1, [0x11; 32]),
        )
        .expect("写 durable pin");
        let ctx = RelayContext::remote_tls(REMOTE, DEFAULT_TRANSPORT_TIMEOUTS).expect("remote");
        let builds = Cell::new(0u32);
        let error = match ctx.open_with(&vault, &identity(), |_| {
            builds.set(builds.get() + 1);
            Ok(SpyTransport::default())
        }) {
            Ok(_) => panic!("换实例必须拒绝"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::InstanceMismatch), "{error:?}");
        assert_eq!(builds.get(), 0);
    }

    /// 管理锚：显式给出的锚与身份不一致 → NeedsTrust；全 0 → NeedsTrust。
    #[test]
    fn manager_anchor_must_match_the_identity() {
        let (_dir, vault) = fresh_vault();
        crate::control_pin::commit_verified_head(
            &vault,
            None,
            VerifiedHead::from_verified_control([0x5a; 16], [0x22; 16], 1, [0x11; 32]),
        )
        .expect("写 durable pin");
        let mismatched = RelayContext::remote_tls(REMOTE, DEFAULT_TRANSPORT_TIMEOUTS)
            .expect("remote")
            .with_manager_anchor([0x77; 32]);
        let error = match mismatched.open_with(&vault, &identity(), |_| Ok(SpyTransport::default()))
        {
            Ok(_) => panic!("管理锚不符必须拒绝"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");

        let mut zero = identity();
        zero.pinned_manager = [0u8; 32];
        let ctx = RelayContext::local_dev(LOCAL, DEFAULT_TRANSPORT_TIMEOUTS).expect("local");
        let error = match ctx.open_with(&vault, &zero, |_| Ok(SpyTransport::default())) {
            Ok(_) => panic!("全 0 管理锚必须拒绝"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");
    }

    /// `RelayTarget`：`&str` 走 local_dev 兼容入口、显式 context 原样返回。
    #[test]
    fn relay_target_accepts_urls_and_explicit_contexts() {
        let url: &str = LOCAL;
        let from_url = url.relay_context(None).expect("legacy url");
        assert_eq!(from_url.mode, RelayMode::LocalDev);
        assert_eq!(from_url.timeouts, DEFAULT_TRANSPORT_TIMEOUTS);
        let ctx = RelayContext::remote_tls(REMOTE, DEFAULT_TRANSPORT_TIMEOUTS).expect("remote");
        let explicit = ctx
            .relay_context(Some(DEFAULT_TRANSPORT_TIMEOUTS))
            .expect("ctx");
        assert_eq!(
            explicit.mode,
            RelayMode::RemoteTls,
            "显式 context 不被 legacy 预算改写"
        );
        assert_eq!(explicit.origin_str(), "https://relay.example");
        let owned = String::from(LOCAL);
        assert_eq!(
            owned.relay_context(None).expect("String 目标").mode,
            RelayMode::LocalDev
        );
        // `&String` / `&&str` 之类的双层引用也要能透传（调用点形态不同）。
        assert_eq!(
            owned.relay_context(None).expect("String 目标").mode,
            RelayMode::LocalDev
        );
    }
}
