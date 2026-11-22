//! 邀请链接加入（ADR 0014 **S1**）的后台执行体：兑换 PENDING → 等待批准 → bootstrap。
//!
//! ## 顺序（与恢复码 Join 同构，但**没有**恢复码）
//!
//! ```text
//! RedeemJob（worker 线程）：
//!   1. 解析链接（UI 已解析过一次；worker 用同一份 [`InviteLinkTarget`]）
//!   2. `InviteJoin::redeem`（真实 relay；成功只到 PENDING）
//!   3. 交回 Pending（库/身份**都还没创建**；pending 材料已受保护落库）→ 释放锁
//!
//! CheckJob（worker 线程）：
//!   1. 打开库、读 durable pending
//!   2. `InviteJoin::finish`：设备认证 → self grant → 解 root → bootstrap → control pin
//!   3. 身份受保护落盘（`DevIdentityStore`）
//!   4. 交回 Finished（app 写公开配置 → `InviteJoin::complete` → 进 Main）
//! ```
//!
//! ## 纪律
//!
//! - **不重复兑换**：已有同一邀请的 pending 时 `redeem` 走 resume 路径（纯本地，不联网）；
//! - **不自动批准**：创建端必须在设备面板显式批准；这里只轮询（用户点「检查状态」）；
//! - **不半写身份/配置**：身份落盘失败 → 清掉本次新建的库文件并交回失败；bootstrap 失败
//!   → 保留 pending 可重试，不动身份/配置；
//! - token 只在 [`InviteLinkTarget`] 里（不实现 `Debug`），兑换后不落盘、不进日志。
//!
//! ## 失败分类（UI 据此给可操作提示）
//!
//! | 类别 | 触发 |
//! | --- | --- |
//! | [`InviteFailureKind::LinkFormat`] | 链接格式/编码/版本非法（**不联网**） |
//! | [`InviteFailureKind::InviteUsed`] | relay `INVITE_USED`/`FORBIDDEN`（一次性 token 已用/错） |
//! | [`InviteFailureKind::Expired`] | relay `EXPIRED`（TTL 已过） |
//! | [`InviteFailureKind::Pin`] | relay instance 与链接 pin 不符（`InstanceMismatch`） |
//! | [`InviteFailureKind::Unreachable`] | 连接被拒/超时/5xx |
//! | [`InviteFailureKind::Tls`] | TLS 端点认证失败 |
//! | [`InviteFailureKind::Other`] | 本地存储/封装/其它（脱敏原文） |

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use menghunote_storage::Vault;
use menghunote_sync::api_version::RelayApiVersion;
use menghunote_sync::http_relay::Clock as _;
use menghunote_sync::http_transport::DEFAULT_TRANSPORT_TIMEOUTS;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::invite_link::{
    InviteFinish, InviteJoin, InviteJoinPhase, InviteLinkTarget, InvitePending,
};
use menghunote_sync::relay_context::RelayContext;
use menghunote_sync::relay_policy::RelayMode;
use menghunote_sync::SyncError;

use super::lock::InstanceLock;
use super::sanitize_error;
use crate::dev_sync::{classify_sync_error, FailureClass, SecurityCategory};

/// worker 线程名（诊断用）。
const REDEEM_THREAD_NAME: &str = "menghunote-onboarding-invite-redeem";
const CHECK_THREAD_NAME: &str = "menghunote-onboarding-invite-check";

/// 加入端阶段（UI 只显示固定文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteStage {
    /// 正在兑换邀请（relay 上只产生 PENDING）。
    Redeeming,
    /// 正在检查批准状态 / bootstrap。
    Checking,
    /// 正在持久化设备身份（受保护）。
    PersistingIdentity,
}

impl InviteStage {
    /// 进度文案。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Redeeming => "正在兑换邀请（relay 上只产生待批准设备）…",
            Self::Checking => "正在检查批准状态并导入基线（bootstrap）…",
            Self::PersistingIdentity => "正在持久化设备身份（受保护）…",
        }
    }
}

/// 失败类别（每一类都对应用户能做的一件事）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteFailureKind {
    /// 链接格式/编码/版本非法（联网之前就被拒）。
    LinkFormat,
    /// 邀请已被使用/无效（`INVITE_USED`/`FORBIDDEN`/`ID_CONFLICT`）。
    InviteUsed,
    /// 邀请已过期（`EXPIRED`）。
    Expired,
    /// relay 实例与链接里的 pin 不符（不自动换 pin）。
    Pin,
    /// relay 不可达或超时（连接被拒、读超时、5xx）。
    Unreachable,
    /// TLS 端点认证失败。
    Tls,
    /// 其它（本地存储/身份封装/未预期错误）。
    Other,
}

impl InviteFailureKind {
    /// 面向用户的固定提示（不含秘密；后面拼脱敏技术细节）。
    #[must_use]
    pub const fn hint(self) -> &'static str {
        match self {
            Self::LinkFormat => {
                "邀请链接格式非法：请让创建者重新复制整段 mn-pair://join?... 链接（链接区分大小写）"
            }
            Self::InviteUsed => {
                "邀请已被使用或无效：一次性令牌只能用一次；请让创建者重新生成链接（已兑换的等待会被保留）"
            }
            Self::Expired => {
                "邀请已过期（默认 15 分钟）：请让创建者重新生成链接"
            }
            Self::Pin => {
                "relay 实例与邀请里的 pin 不一致（疑似换实例/中间人）：不会自动换 pin，请核对链接来源"
            }
            Self::Unreachable => {
                "relay 不可达或超时：请确认 relay 已启动、地址与模式匹配（local_dev 是 loopback 明文；remote_tls 是 HTTPS）"
            }
            Self::Tls => {
                "TLS 端点认证失败（证书链/主机名/有效期）：核对 relay 域名与系统时间；本程序不提供忽略证书"
            }
            Self::Other => "邀请加入失败",
        }
    }

    /// 是否建议直接重试（仅用于文案/测试断言）。
    #[must_use]
    pub const fn retryable(self) -> bool {
        // `InviteUsed`/`Expired`/`LinkFormat` 重试同一链接没有意义（需要新链接/先修正）。
        matches!(
            self,
            Self::Unreachable | Self::Tls | Self::Pin | Self::Other
        )
    }
}

/// 兑换请求（含**秘密**：链接里的 token）。
///
/// **不实现 `Debug`/`Clone`**：token 只在这一步使用，不复制、不打印。
pub struct InviteRedeemRequest {
    /// 已解析的链接（token 是秘密）。
    pub target: InviteLinkTarget,
    /// 目标实例目录里的库文件路径（必须尚不存在，或是本邀请的 pending 续跑）。
    pub vault_path: PathBuf,
    /// 实例目录锁（移交给 worker；Pending 事件里交回）。
    pub lock: InstanceLock,
    /// **测试专用**：remote_tls 初始化追加的显式信任根（DER；本地 TLS harness）。
    pub test_extra_roots: Vec<Vec<u8>>,
}

/// 检查/续跑请求（不需要 token：pending 已在库里）。
pub struct InviteCheckRequest {
    /// 目标实例目录里的库文件路径。
    pub vault_path: PathBuf,
    /// 实例目录锁（移交给 worker；终态事件里交回）。
    pub lock: InstanceLock,
    /// relay 模式（来自 pending；仅用于构造 context）。
    pub relay_mode: RelayMode,
    /// relay API 面（来自 pending；v2 邀请必须在 `/v2/...` 上检查/bootstrap）。
    pub api_version: RelayApiVersion,
    /// canonical relay origin（来自 pending）。
    pub relay_url: String,
    /// **测试专用**：显式信任根（DER）。
    pub test_extra_roots: Vec<Vec<u8>>,
}

/// PENDING 的公开事实（UI 展示；不含 token/私钥）。
#[derive(Debug, Clone)]
pub struct InvitePendingFacts {
    /// 库文件路径。
    pub vault_path: PathBuf,
    /// relay 模式。
    pub relay_mode: RelayMode,
    /// relay API 面（v1/v2；链接 `api=` 的持久副本）。
    pub api_version: RelayApiVersion,
    /// canonical relay origin。
    pub relay_url: String,
    /// 服务器 vault id（32 位 hex）。
    pub vault_id: String,
    /// 本机设备 id（32 位 hex）。
    pub device_id: String,
    /// 设备指纹（32 位 hex；与创建端确认框显示的一致）。
    pub fingerprint: String,
    /// 签名公钥短 hash（16 位 hex）。
    pub sign_short: String,
    /// HPKE 公钥短 hash（16 位 hex）。
    pub hpke_short: String,
    /// 邀请过期时刻（Unix 秒；只用于展示）。
    pub expires_at: u64,
    /// 当前阶段。
    pub phase: InviteJoinPhase,
    /// 是否复用已有 pending（true = 没有重复兑换）。
    pub resumed: bool,
}

/// 加入完成后的产物（身份已受保护落盘）。
pub struct InviteOutcome {
    /// 已打开的真实库（bootstrap 已导入基线）。
    pub vault: Vault,
    /// 实例目录锁（交回 UI，Main 继续持有）。
    pub lock: InstanceLock,
    /// 库文件路径。
    pub vault_path: PathBuf,
    /// relay 模式（写 ProfileV2）。
    pub relay_mode: RelayMode,
    /// relay API 面（写 ProfileV2；邀请链接 `api=` 决定，不回落 v1）。
    pub api_version: RelayApiVersion,
    /// canonical relay origin。
    pub relay_url: String,
    /// 服务器 vault id（32 位 hex）。
    pub vault_id: String,
    /// 本机设备 id（32 位 hex）。
    pub device_id: String,
    /// 持久身份映射出的 Loro peer。
    pub peer_id: u64,
    /// 持久身份的 key epoch。
    pub key_epoch: u64,
    /// 非致命告警（如：上次已完成 bootstrap 但配置未写入）。
    pub warning: Option<String>,
}

impl InviteOutcome {
    /// 公开事实（可写进配置的那部分；**不含**token/root/私钥）。
    #[must_use]
    pub fn public_facts(&self, instance_id: &str) -> super::layout::InstanceEntry {
        super::layout::InstanceEntry {
            instance_id: instance_id.to_owned(),
            relay_mode: self.relay_mode,
            api_version: self.api_version,
            relay_url: self.relay_url.clone(),
            vault_path: self.vault_path.to_string_lossy().into_owned(),
            vault_id: self.vault_id.clone(),
            device_id: self.device_id.clone(),
        }
    }
}

/// 兑换/检查的统一事件。
pub enum InviteEvent {
    /// 进度。
    Progress(InviteStage),
    /// 已兑换/恢复为 PENDING（等创建者批准；锁交回 UI）。
    Pending {
        /// 公开事实。
        facts: Box<InvitePendingFacts>,
        /// 交回的实例目录锁。
        lock: InstanceLock,
    },
    /// 检查完成但创建者还没批准（或设备已被移除；relay 不区分）。
    Waiting {
        /// 交回的实例目录锁。
        lock: InstanceLock,
    },
    /// 已完成（身份已落盘；app 需要写配置 + `InviteJoin::complete`）。
    Finished(Box<InviteOutcome>),
    /// 失败（已脱敏文案；锁已释放）。
    Failed {
        /// 类别。
        kind: InviteFailureKind,
        /// 已脱敏的完整文案。
        message: String,
        /// 安全类失败（TLS/NeedsTrust/Equivocation/InstanceMismatch/PinCorrupt）。
        security: Option<SecurityCategory>,
    },
}

/// 后台任务句柄（兑换或检查，二者不同时存在）。
pub struct InviteJob {
    rx: Receiver<InviteEvent>,
    _thread: thread::JoinHandle<()>,
}

impl std::fmt::Debug for InviteJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InviteJob").finish_non_exhaustive()
    }
}

impl InviteJob {
    /// 启动兑换（PENDING）后台任务。
    pub fn start_redeem(request: InviteRedeemRequest) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let InviteRedeemRequest {
            target,
            vault_path,
            lock,
            test_extra_roots,
        } = request;
        let thread = thread::Builder::new()
            .name(REDEEM_THREAD_NAME.to_owned())
            .spawn(move || {
                let events = tx;
                match run_redeem(&target, &vault_path, lock, &test_extra_roots, &events) {
                    Ok(()) => {}
                    Err(failure) => {
                        let _ = events.send(failure);
                    }
                }
            })?;
        Ok(Self {
            rx,
            _thread: thread,
        })
    }

    /// 启动检查/续跑后台任务。
    pub fn start_check(request: InviteCheckRequest) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let InviteCheckRequest {
            vault_path,
            lock,
            relay_mode,
            api_version,
            relay_url,
            test_extra_roots,
        } = request;
        let thread = thread::Builder::new()
            .name(CHECK_THREAD_NAME.to_owned())
            .spawn(move || {
                let events = tx;
                if let Err(failure) = run_check(
                    &vault_path,
                    lock,
                    relay_mode,
                    api_version,
                    &relay_url,
                    &test_extra_roots,
                    &events,
                ) {
                    let _ = events.send(failure);
                }
            })?;
        Ok(Self {
            rx,
            _thread: thread,
        })
    }

    /// 非阻塞收一条事件（生产泵）。
    pub fn try_next(&self) -> Option<InviteEvent> {
        self.rx.try_recv().ok()
    }

    /// 阻塞收一条事件（**测试专用**显式收敛点）。
    pub fn recv(&self, timeout: Duration) -> Option<InviteEvent> {
        self.rx.recv_timeout(timeout).ok()
    }
}

// ---------------------------------------------------------------------------
// worker 主体
// ---------------------------------------------------------------------------

fn context_for(
    mode: RelayMode,
    relay: &str,
    api_version: RelayApiVersion,
    roots: &[Vec<u8>],
) -> Result<RelayContext, SyncError> {
    let context = RelayContext::from_mode(mode, relay, DEFAULT_TRANSPORT_TIMEOUTS)?
        .with_api_version(api_version);
    if roots.is_empty() {
        Ok(context)
    } else {
        context.with_test_extra_roots(roots.iter())
    }
}

/// Linux 每用户主钥必须先初始化（与 `DevIdentityStore` 同一入口；Windows 是 no-op）。
fn ensure_user_key() -> Result<(), SyncError> {
    menghunote_sync::identity_wrap::initialize_user_key()
}

fn facts_of(
    pending: &InvitePending,
    vault_path: &std::path::Path,
    resumed: bool,
) -> InvitePendingFacts {
    let (sign_short, hpke_short) = pending.key_short_hashes();
    InvitePendingFacts {
        vault_path: vault_path.to_path_buf(),
        relay_mode: pending.mode,
        api_version: pending.api_version,
        relay_url: pending.relay.clone(),
        vault_id: menghunote_sync::ids::hex_of(&pending.vault_id),
        device_id: menghunote_sync::ids::hex_of(&pending.device_id),
        fingerprint: pending.fingerprint(),
        sign_short,
        hpke_short,
        expires_at: pending.expires_at,
        phase: pending.phase,
        resumed,
    }
}

fn run_redeem(
    target: &InviteLinkTarget,
    vault_path: &std::path::Path,
    lock: InstanceLock,
    roots: &[Vec<u8>],
    events: &mpsc::Sender<InviteEvent>,
) -> Result<(), InviteEvent> {
    let _ = events.send(InviteEvent::Progress(InviteStage::Redeeming));
    if let Err(error) = ensure_user_key() {
        return Err(failed(InviteFailureKind::Other, error, None));
    }
    let context = match context_for(target.mode(), target.relay(), target.api_version(), roots) {
        Ok(context) => context,
        Err(error) => return Err(failed(classify_invite_error(&error), error, None)),
    };
    let started = match InviteJoin::redeem(
        &context,
        target,
        vault_path,
        menghunote_sync::http_relay::SystemClock.now_secs(),
    ) {
        Ok(started) => started,
        Err(error) => {
            let kind = classify_invite_error(&error);
            return Err(failed(kind, error, None));
        }
    };
    // PENDING 已 durable 落库：库句柄不需要跨线程传递（检查时重开）。
    let facts = facts_of(&started.pending, &started.vault_path, started.resumed);
    drop(started.vault);
    let _ = events.send(InviteEvent::Pending {
        facts: Box::new(facts),
        lock,
    });
    Ok(())
}

fn run_check(
    vault_path: &std::path::Path,
    lock: InstanceLock,
    relay_mode: RelayMode,
    api_version: RelayApiVersion,
    relay_url: &str,
    roots: &[Vec<u8>],
    events: &mpsc::Sender<InviteEvent>,
) -> Result<(), InviteEvent> {
    let _ = events.send(InviteEvent::Progress(InviteStage::Checking));
    if let Err(error) = ensure_user_key() {
        return Err(failed(InviteFailureKind::Other, error, None));
    }
    let context = match context_for(relay_mode, relay_url, api_version, roots) {
        Ok(context) => context,
        Err(error) => return Err(failed(classify_invite_error(&error), error, None)),
    };
    let vault = match Vault::open(vault_path) {
        Ok(vault) => vault,
        Err(error) => {
            return Err(failed(
                InviteFailureKind::Other,
                SyncError::Storage(error),
                None,
            ))
        }
    };
    let pending = match InviteJoin::load(&vault) {
        Ok(Some(pending)) => pending,
        Ok(None) => {
            return Err(text_failure(
                InviteFailureKind::Other,
                "本地没有邀请 pending：请重新粘贴邀请链接".to_owned(),
            ))
        }
        Err(error) => return Err(failed(InviteFailureKind::Other, error, None)),
    };

    // 上次已完成 bootstrap、只差配置：直接复用身份材料（不重复联网/不重复 bootstrap）。
    if pending.phase == InviteJoinPhase::Bootstrapped {
        return finish_from_bootstrapped(vault, vault_path, lock, pending, events);
    }

    let outcome = match InviteJoin::finish(
        &context,
        &vault,
        menghunote_sync::http_relay::SystemClock.now_secs(),
    ) {
        Ok(InviteFinish::WaitingForApproval) => {
            let _ = events.send(InviteEvent::Waiting { lock });
            return Ok(());
        }
        Ok(InviteFinish::Joined(joined)) => joined,
        Err(error) => {
            let kind = classify_invite_error(&error);
            let security = security_of(&error);
            return Err(failed(kind, error, security));
        }
    };
    let _ = events.send(InviteEvent::Progress(InviteStage::PersistingIdentity));
    let identity = &outcome.identity;
    if let Err(error) = crate::dev_sync::DevIdentityStore.persist_protected(vault_path, identity) {
        // 身份没落盘 = 本次加入无效：清掉本次新建的库文件（目录保留），pending 也没意义了。
        drop(vault);
        cleanup_created_files(vault_path);
        return Err(text_failure(
            InviteFailureKind::Other,
            format!("设备身份持久化失败（已清理本次新建的库文件）：{error:#}"),
        ));
    }
    finish_outcome(vault, vault_path, lock, pending, identity, None, events)
}

/// 阶段已是 `Bootstrapped`（身份 + bootstrap 都完成过，只差 app 写配置）：
/// 直接读回受保护身份并交回 Finished，不重复联网、不重复 bootstrap。
fn finish_from_bootstrapped(
    vault: Vault,
    vault_path: &std::path::Path,
    lock: InstanceLock,
    pending: InvitePending,
    events: &mpsc::Sender<InviteEvent>,
) -> Result<(), InviteEvent> {
    let identity = match crate::dev_sync::DevIdentityStore.load(vault_path) {
        Ok(identity) => identity,
        Err(error) => {
            return Err(text_failure(
                InviteFailureKind::Other,
                format!(
                "该实例已完成加入（阶段 {}），但缺少本地身份文件：{error:#}；请重新粘贴邀请链接",
                pending.phase.as_str()
            ),
            ))
        }
    };
    if identity.device_id != pending.device_id || identity.vault_id != pending.vault_id {
        return Err(text_failure(
            InviteFailureKind::Other,
            "该实例的身份文件与本邀请的 pending 不一致：拒绝复用（请换一个新目录）".to_owned(),
        ));
    }
    finish_outcome(
        vault,
        vault_path,
        lock,
        pending,
        &identity,
        Some(
            "上次加入已完成 bootstrap 但公开配置未写入：本次直接补齐（没有重复兑换/bootstrap）"
                .to_owned(),
        ),
        events,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_outcome(
    vault: Vault,
    vault_path: &std::path::Path,
    lock: InstanceLock,
    pending: InvitePending,
    identity: &LocalIdentityV1,
    warning: Option<String>,
    events: &mpsc::Sender<InviteEvent>,
) -> Result<(), InviteEvent> {
    let outcome = InviteOutcome {
        vault,
        lock,
        vault_path: vault_path.to_path_buf(),
        relay_mode: pending.mode,
        api_version: pending.api_version,
        relay_url: pending.relay.clone(),
        vault_id: menghunote_sync::ids::hex_of(&pending.vault_id),
        device_id: menghunote_sync::ids::hex_of(&identity.device_id),
        peer_id: identity.loro_peer_id(),
        key_epoch: identity.current_epoch,
        warning,
    };
    let _ = events.send(InviteEvent::Finished(Box::new(outcome)));
    Ok(())
}

/// 清掉本次邀请加入新建的库文件（含边车）与身份文件；**不删实例目录**。
fn cleanup_created_files(vault_path: &std::path::Path) {
    let _ = std::fs::remove_file(vault_path);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = vault_path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(sidecar));
    }
    let _ = std::fs::remove_file(crate::dev_sync::DevIdentityStore::path_for(vault_path));
}

// ---------------------------------------------------------------------------
// 分类/文案
// ---------------------------------------------------------------------------

fn classify_invite_error(error: &SyncError) -> InviteFailureKind {
    match error {
        SyncError::InviteLink(_) | SyncError::ConfigInvalid(_) => InviteFailureKind::LinkFormat,
        SyncError::InstanceMismatch => InviteFailureKind::Pin,
        SyncError::RelayApi { code, .. } => match code.as_str() {
            "INVITE_USED" | "FORBIDDEN" | "ID_CONFLICT" => InviteFailureKind::InviteUsed,
            "EXPIRED" => InviteFailureKind::Expired,
            _ => InviteFailureKind::Other,
        },
        SyncError::Tls(_) => InviteFailureKind::Tls,
        SyncError::Transport(_) | SyncError::HttpStatus(_) => InviteFailureKind::Unreachable,
        _ => InviteFailureKind::Other,
    }
}

fn security_of(error: &SyncError) -> Option<SecurityCategory> {
    match classify_sync_error(error) {
        FailureClass::Security(category) => Some(category),
        FailureClass::Retryable | FailureClass::NeedsAttention => None,
    }
}

fn failed(
    kind: InviteFailureKind,
    error: SyncError,
    security: Option<SecurityCategory>,
) -> InviteEvent {
    let error = anyhow::Error::new(error);
    let message = format!("{}：{}", kind.hint(), sanitize_error(&error, &[]));
    InviteEvent::Failed {
        kind,
        message,
        security,
    }
}

fn text_failure(kind: InviteFailureKind, detail: String) -> InviteEvent {
    InviteEvent::Failed {
        kind,
        message: format!("{}：{detail}", kind.hint()),
        security: None,
    }
}

/// 邀请输入为空时的固定提示。
pub const INVITE_EMPTY_HINT: &str =
    "粘贴邀请链接（mn-pair://join?...&pin=…#token=…）后不需要恢复码";
