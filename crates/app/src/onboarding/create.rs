//! Create 的**后台执行体**（ADR 0011「生命周期与后台」/ S2）。
//!
//! ## 顺序（硬约束，不能交换）
//!
//! ```text
//! Worker 线程（local_dev）：
//!   1. DevLocal::create_deferring_identity（真实 genesis；pending 停在 PinCommitted）
//!   2. DevIdentityStore::persist_protected（受保护身份落盘）
//!   3. 推进 pending（IdentityWritten → Done）→ 把 Vault + 公开事实 + 恢复码交回 UI
//!
//! Worker 线程（remote_tls，ADR 0013 S3b）：
//!   1. remote_init::create_remote（production TLS：genesis → 身份回调落盘 →
//!      `IdentityWritten` → /state+/control 全链 + rev1 前缀 → 强 pin → 初始 recovery 槽）
//!   2. 身份回调就是 DevIdentityStore::persist_protected（原子 rename）
//!   3. 把 Vault + 公开事实 + 恢复码交回 UI
//!
//! Worker 线程（一次性票据，ADR 0014 S3b / S3a 的库级入口）：
//!   1. remote_init::create_vault_with_ticket（**固定 v2 面**：`POST /v2/vaults`；
//!      与 v1 Create **完全同一套**主体：本地材料 → 写前 pending → 回执校验/强 pin →
//!      身份回调落盘 → 初始 recovery 槽 → pending 收尾）
//!   2. 票据只在内存里用一次：不写 pending/配置/日志；失败不消费票据（relay 事务回滚）
//!   3. 成功后走**同一张**备份页/配置写入路径，配置 `api_version=2`
//! ```
//!
//! **v1 setup secret 与 v2 票据互斥**：请求里只有一个 [`CreateCredential`]，类型上
//! 不能同时带两种授权；workers 的清理/失败语义按各自的编排（票据路径由
//! `remote_init` 决定，与 v1 remote 相同）。
//!
//! 两条路径都保证"**身份落盘成功之后**才把 vault_id/恢复码交给 UI"。
//!
//! ## 失败清理（两条路径语义不同，见 ADR 0013 §3）
//!
//! - `local_dev`：任何失败都清掉本次新建的库/身份文件（既有 ADR 0009 语义）。
//! - `remote_tls`：拿不到回执之前失败 → 清理本次新建的库文件（远端最多留下不可用
//!   空 vault）；拿到回执之后失败（含身份落盘失败）→ **保留**库/pending 与已落盘事实
//!   （不删用户数据），Error 页可重试/等启动对账。app 自己不判断清理，交给 `remote_init`。
//!
//! **第 2 步成功之前，UI 不得展示 `vault_id` / 恢复码**：没有受保护身份就没有可用的
//! 实例，"看起来创建成功但身份没落盘"是 ADR 0009 明确禁止的半状态。
//!
//! ## 为什么在专用线程
//!
//! genesis 里有真实 HTTP、Argon2 与 crypto，全部在 UI 线程会冻结窗口（ADR 0011
//! 「网络、Argon2、crypto、身份落盘、配置写入全部在专用线程」）。GPUI 侧只经 channel
//! 收事件（见 [`CreateJob`] 与 `onboarding::app_root` 的轮询泵），**不 join 线程**。
//!
//! ## 锁
//!
//! 调用方在启动创建**之前**取得实例目录锁（[`super::lock::InstanceLock`]），并把锁
//! **移交给本 worker**：worker 存活期间独占该目录。成功时锁随
//! [`CreateOutcome::lock`] 交回 UI（Main 继续持有）；失败时 worker 退出、锁 Drop
//! （删掉自己的锁文件），不留下"目录被锁死"的状态。
//!
//! ## 失败清理
//!
//! 只清**本次新建**的库文件（含 WAL/SHM/journal 边车）与身份文件；**实例目录本身
//! 不删**（可能是用户已有的空目录）。relay 侧可能留下一个没有基线的空 vault，重试会
//! 建新的——这与 `DevLocal::create` 的既有失败语义一致（ADR 0011「非目标与风险」）。

use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use zeroize::Zeroizing;

use menghunote_storage::Vault;
use menghunote_sync::api_version::RelayApiVersion;
use menghunote_sync::create_link::CreateTicket;
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::error::SyncError;
use menghunote_sync::http_transport::DEFAULT_TRANSPORT_TIMEOUTS;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::relay_context::RelayContext;
use menghunote_sync::relay_policy::RelayMode;
use menghunote_sync::remote_init::{create_remote, create_vault_with_ticket};

use super::lock::InstanceLock;
use super::sanitize_error;
use crate::dev_sync::{classify_sync_error, FailureClass, SecurityCategory};

/// Create 泵的轮询间隔（UI 非阻塞看 channel；与 dev-sync 面板的 40ms 同量级）。
pub const CREATE_PUMP_INTERVAL: Duration = Duration::from_millis(40);

/// worker 线程名（诊断用；日志里出现时一眼能认出是谁）。
const WORKER_THREAD_NAME: &str = "menghunote-onboarding-create";

/// Create 的进度阶段（UI 只显示固定文案，不带任何秘密）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateStage {
    /// 联系 relay 做 genesis 并上传初始恢复槽。
    Genesis,
    /// 把设备身份受保护落盘。
    PersistingIdentity,
}

impl CreateStage {
    /// 进度文案。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Genesis => "正在创建 vault（genesis + 初始恢复槽）…",
            Self::PersistingIdentity => "正在持久化设备身份（受保护）…",
        }
    }
}

/// Create 的授权方式（**v1 与 v2 互斥**，类型上不能同时给两种）。
///
/// - [`Self::SetupSecret`]：既有 v1 Create（本地 `DevLocal` 或远端 `create_remote`）；
/// - [`Self::Ticket`]：ADR 0014 S3a 的一次性票据创建（固定 v2 面）。
///
/// 两种方式都只在**内存**里携带秘密；不实现 `Debug`/`Clone`。
pub enum CreateCredential {
    /// v1：服务器创建凭证（`Zeroizing` 在 drop 时清零）。
    SetupSecret(Zeroizing<String>),
    /// v2：一次性票据（`CreateTicket` 在 drop 时零化明文，`Debug` 脱敏）。
    Ticket {
        /// 票据绑定的预留 vault id（创建必须使用同一个 id；错 vault 由 relay 统一 403）。
        vault_id: [u8; 16],
        /// 32B 票据明文（**只在内存**）。
        ticket: CreateTicket,
        /// 可选公开锚：创建链接里的 relay instance；回执不一致即失败（不自动换 pin）。
        expected_instance: Option<[u8; 16]>,
    },
}

/// Create 请求（含**秘密**：setup secret 或创建票据）。
///
/// **不实现 `Debug`/`Clone`**：秘密只在这一次创建里用，不复制、不打印。
pub struct CreateRequest {
    /// 目标库文件路径（实例目录里的 `vault.sqlite3`）。
    pub vault_path: std::path::PathBuf,
    /// relay 基地址（公开；`local_dev` 是 loopback 明文，`remote_tls` 是 HTTPS canonical origin）。
    pub relay_url: String,
    /// relay 接入模式（ADR 0013 §1：模式是显式配置，不从 URL 猜）。
    ///
    /// 票据模式由创建链接决定（链接 scheme 即模式）；v2 面见 [`CreateCredential`]。
    pub relay_mode: RelayMode,
    /// 本次创建的授权方式（v1 setup secret / v2 一次性票据，二者互斥）。
    pub credential: CreateCredential,
    /// 实例目录锁（移交给 worker，保证创建期间独占）。
    pub lock: InstanceLock,
    /// **测试专用**：远程初始化追加的显式信任根（DER；本地 TLS harness）。
    ///
    /// 生产恒为空（系统信任根）。只**增加**显式信任，不关闭证书链/SAN/有效期校验。
    pub test_extra_roots: Vec<Vec<u8>>,
}

/// Create 成功后的产物。
///
/// **不实现 `Debug`**：`recovery_code` 是秘密材料。`vault_id`/`device_id` 是公开标识，
/// 只在 UI 的备份页与配置里出现。
pub struct CreateOutcome {
    /// 已打开的真实库（worker 里建出；`Vault` 是 `Send + Sync`）。
    pub vault: Vault,
    /// 实例目录锁（交回 UI，Main 继续持有）。
    pub lock: InstanceLock,
    /// 库文件路径。
    pub vault_path: std::path::PathBuf,
    /// 本次实例的 relay 模式（写 ProfileV2；remote_tls 不降级 local_dev）。
    pub relay_mode: RelayMode,
    /// 本次实例的 relay API 面（ADR 0014 S3b：v1 setup secret 创建 = v1；票据 = v2）。
    pub api_version: RelayApiVersion,
    /// canonical relay origin（公开；写 ProfileV2）。
    pub relay_url: String,
    /// 服务器 vault id（32 位十六进制）。
    pub vault_id: String,
    /// 本机设备 id（32 位十六进制）。
    pub device_id: String,
    /// 持久身份映射出的 Loro peer（唯一来源）。
    pub peer_id: u64,
    /// 持久身份的当前 key epoch。
    pub key_epoch: u64,
    /// 一次性恢复码（只在备份页展示；不落盘、不进日志/配置）。
    pub recovery_code: Zeroizing<String>,
    /// 非致命告警（例如身份已落盘但 pending 收尾失败：重启后启动对账会补）。
    ///
    /// **不影响**成功终态：实例已可用，不能因为一次收尾失败丢掉恢复码/重建实例。
    pub warning: Option<String>,
}

impl CreateOutcome {
    /// 公开事实（可写进配置的那部分；**不含**恢复码/setup secret/root）。
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

/// Create 后台事件。
pub enum CreateEvent {
    /// 进度（固定文案，不含秘密）。
    Progress(CreateStage),
    /// 成功终态。
    Finished(Box<CreateOutcome>),
    /// 失败终态：**已脱敏**的文案（调用方直接显示）+ 可选安全暂停类别。
    Failed {
        /// 已脱敏的完整文案（调用方直接显示）。
        message: String,
        /// 安全类失败（TLS/NeedsTrust/Equivocation/InstanceMismatch/PinCorrupt）；
        /// UI 按 `SecurityPaused` 同一分类显示，且**不自动重试/清 pin/重建**。
        security: Option<SecurityCategory>,
    },
}

/// 在后台线程里跑的 Create 任务。
pub struct CreateJob {
    /// 事件接收端（非阻塞 `try_next`；测试有显式阻塞等待）。
    rx: Receiver<CreateEvent>,
    /// worker 线程句柄：**不 join**（ADR 0011「GUI 不 join 线程」）；持有着只为文档/诊断。
    _thread: thread::JoinHandle<()>,
}

impl std::fmt::Debug for CreateJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateJob").finish_non_exhaustive()
    }
}

impl CreateJob {
    /// 启动后台 Create。
    ///
    /// 线程启动失败是**同步**错误（`std::io::Error`）：此时请求（含锁）已被 drop，
    /// 调用方可以安全地重试或退回表单。
    pub fn start(request: CreateRequest) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let CreateRequest {
            vault_path,
            relay_url,
            relay_mode,
            credential,
            lock,
            test_extra_roots,
        } = request;
        let thread = thread::Builder::new()
            .name(WORKER_THREAD_NAME.to_owned())
            .spawn(move || {
                let events = tx;
                let result = run_create(
                    &relay_url,
                    relay_mode,
                    credential,
                    &test_extra_roots,
                    &vault_path,
                    lock,
                    &events,
                );
                match result {
                    // 恢复码在 outcome 里；只有 UI 收下它才展示。
                    Ok(outcome) => drop(events.send(CreateEvent::Finished(Box::new(outcome)))),
                    Err(Failure {
                        error,
                        redact,
                        security,
                        lock,
                    }) => {
                        // 脱敏：setup secret 的任何回显都替换掉，恢复码形态的 token 也遮住。
                        // 票据路径没有可替文案的秘密（`CreateTicket` 的 `Debug` 本身脱敏）。
                        let secrets = redact
                            .as_ref()
                            .map_or_else(Vec::new, |secret| vec![secret.as_str()]);
                        let message = sanitize_error(&error, &secrets);
                        drop(events.send(CreateEvent::Failed { message, security }));
                        // 失败时锁随局部变量 drop（删掉本次自己创建的锁文件）。
                        drop(lock);
                    }
                }
            })?;
        Ok(Self {
            rx,
            _thread: thread,
        })
    }

    /// 非阻塞收一条事件（生产泵用）。
    pub fn try_next(&self) -> Option<CreateEvent> {
        // `Disconnected`（worker 已退出）也与"暂时没有事件"同义：终态事件已在 channel
        // 里排在我们前面，排空后才可能断开。
        self.rx.try_recv().ok()
    }

    /// 阻塞收一条事件（**测试专用**的显式收敛点；不用于 UI 线程）。
    pub fn recv(&self, timeout: Duration) -> Option<CreateEvent> {
        self.rx.recv_timeout(timeout).ok()
    }
}

/// 失败时把锁**带回来**（供 worker 在发完事件后释放；避免在错误类型里丢所有权）。
struct Failure {
    error: anyhow::Error,
    /// 需要从错误文案里抹掉的本次秘密（只有 v1 setup secret；票据路径为 `None`）。
    redact: Option<Zeroizing<String>>,
    /// 安全类失败（worker 在**包装成 anyhow 之前**按 `SyncError` 分类）。
    security: Option<SecurityCategory>,
    lock: InstanceLock,
}

/// 组装失败终态（携带可脱敏秘密与锁）。
fn failed(
    error: anyhow::Error,
    security: Option<SecurityCategory>,
    lock: InstanceLock,
    redact: Option<Zeroizing<String>>,
) -> Failure {
    Failure {
        error,
        redact,
        security,
        lock,
    }
}

/// 把 `SyncError` 映射成 UI 的安全暂停类别（与 autosync 的 `SecurityPaused` 同一映射）。
fn security_of(error: &SyncError) -> Option<SecurityCategory> {
    match classify_sync_error(error) {
        FailureClass::Security(category) => Some(category),
        FailureClass::Retryable | FailureClass::NeedsAttention => None,
    }
}

/// 票据创建失败的用户可操作分类（ADR 0014 S3b：403/409/429 明确区分）。
///
/// relay 对 403 统一不区分"不存在/过期/已用/错 vault"（不泄露票据状态），这里也只能
/// 转述这一事实；所有失败都**不建本地库/身份/配置**，也不把票据写进任何文件。
fn ticket_failure_hint(error: &SyncError) -> Option<&'static str> {
    match error {
        SyncError::RelayApi { code, .. } => match code.as_str() {
            "FORBIDDEN" => Some(
                "票据被拒绝（403 FORBIDDEN）：票据不存在/已过期/已被使用/不属于该 vault\
                 （relay 统一不区分）；本次没有创建本地库/身份/配置，票据也不会被保存",
            ),
            "ALREADY_INITIALIZED" => Some(
                "该 vault 已存在（409 ALREADY_INITIALIZED）：同一票据不能重复创建；\
                 relay 事务已回滚，票据额度没有被消费",
            ),
            "RATE_LIMITED" => {
                Some("relay 限流（429 RATE_LIMITED）：稍后再试；本次没有创建本地库，票据未被消费")
            }
            _ => None,
        },
        SyncError::InstanceMismatch => Some(
            "relay instance 与创建链接里的 pin 不一致（疑似换实例/中间人）：\
             不会自动换 pin，请核对链接来源与 relay 实例",
        ),
        _ => None,
    }
}

/// 票据创建的内部事实（不实现 `Debug`/`Clone`：`ticket` 是秘密）。
struct TicketCreate {
    vault_id: [u8; 16],
    ticket: CreateTicket,
    expected_instance: Option<[u8; 16]>,
}

/// worker 线程主体：v1（local_dev / remote_tls）与 v2 票据两条编排 → 身份落盘 → outcome。
fn run_create(
    relay_url: &str,
    relay_mode: RelayMode,
    credential: CreateCredential,
    test_extra_roots: &[Vec<u8>],
    vault_path: &std::path::Path,
    lock: InstanceLock,
    events: &mpsc::Sender<CreateEvent>,
) -> Result<CreateOutcome, Failure> {
    let _ = events.send(CreateEvent::Progress(CreateStage::Genesis));
    let label = crate::dev_sync::setup::INSTANCE_LABEL;
    let store = crate::dev_sync::DevIdentityStore;
    // v1 与 v2 **互斥**：两个分支只会有其中一个存在。
    let (setup_secret, ticket) = match credential {
        CreateCredential::SetupSecret(secret) => (Some(secret), None),
        CreateCredential::Ticket {
            vault_id,
            ticket,
            expected_instance,
        } => (
            None,
            Some(TicketCreate {
                vault_id,
                ticket,
                expected_instance,
            }),
        ),
    };

    // 0) 先按模式构造 context（非法 URL/模式在**建库之前**失败；测试可注入自签根）。
    //    票据路径在下面显式切到 v2（`create_vault_with_ticket` 只接受 v2）。
    let context = match RelayContext::from_mode(relay_mode, relay_url, DEFAULT_TRANSPORT_TIMEOUTS)
        .and_then(|context| {
            if test_extra_roots.is_empty() {
                Ok(context)
            } else {
                context.with_test_extra_roots(test_extra_roots.iter())
            }
        }) {
        Ok(context) => context,
        Err(error) => {
            return Err(failed(
                anyhow::Error::new(error).context(format!(
                    "Create 失败（模式 {relay_mode:?}，relay = {relay_url}，路径 = {}）",
                    vault_path.display()
                )),
                None,
                lock,
                setup_secret,
            ));
        }
    };
    let canonical_relay = context.origin_str();

    let created: CreatedData = match ticket {
        Some(ticket) => {
            // 票据端点就是 `/v2/vaults`：面由**创建方式**固定（不是 URL 猜的）。
            let context = context.with_api_version(RelayApiVersion::V2);
            let progress = events.clone();
            let created = create_vault_with_ticket(
                context,
                ticket.vault_id,
                &ticket.ticket,
                ticket.expected_instance,
                vault_path,
                label,
                move |identity: &LocalIdentityV1| {
                    let _ = progress.send(CreateEvent::Progress(CreateStage::PersistingIdentity));
                    store
                        .persist_protected(vault_path, identity)
                        .map_err(|error| SyncError::Io(format!("dev 身份持久化失败：{error:#}")))
                },
            );
            match created {
                Ok(created) => CreatedData {
                    vault: created.vault,
                    identity: created.identity,
                    vault_id: created.vault_id,
                    recovery_code: created.recovery_code,
                    warning: created.warning,
                    api_version: RelayApiVersion::V2,
                },
                Err(error) => {
                    let security = security_of(&error);
                    let hint = ticket_failure_hint(&error).unwrap_or(
                        "一次性票据 Create 失败（未建本地库/身份/配置；票据不会被保存；\
                         失败不消费票据，可用同一链接重试）",
                    );
                    return Err(failed(
                        anyhow::Error::new(error).context(format!(
                            "票据 Create 失败（模式 {relay_mode:?}，relay = {canonical_relay}，\
                             路径 = {}）：{hint}",
                            vault_path.display()
                        )),
                        security,
                        lock,
                        None,
                    ));
                }
            }
        }
        None => {
            let secret = setup_secret
                .as_ref()
                .map(|secret| secret.as_str())
                .unwrap_or_default();
            match relay_mode {
                RelayMode::LocalDev => {
                    // ADR 0013 S3a：pending 停在 `PinCommitted`；身份 rename 成功后再推进
                    // `IdentityWritten → Done`（失败留给启动对账，不在这里重建/删库）。
                    let created = match DevLocal::create_deferring_identity(
                        relay_url, secret, vault_path, label,
                    ) {
                        Ok(created) => created,
                        Err(error) => {
                            let security = security_of(&error);
                            return Err(failed(
                                anyhow::Error::new(error).context(format!(
                                    "Create 失败（relay = {relay_url}，路径 = {}）",
                                    vault_path.display()
                                )),
                                security,
                                lock,
                                setup_secret,
                            ));
                        }
                    };
                    // setup secret 只在这一步用；`CreatedVault.setup_secret` 是回显字段，立即丢弃。
                    drop(created.setup_secret);

                    let _ = events.send(CreateEvent::Progress(CreateStage::PersistingIdentity));
                    if let Err(error) = store.persist_protected(vault_path, &created.identity) {
                        // 身份没落盘 = 本次创建无效：关库后清掉本次新建的库/身份文件（目录保留）。
                        drop(created.vault);
                        cleanup_created_files(vault_path);
                        return Err(failed(
                            error.context(
                                "设备身份持久化失败（已清理本次新建的库文件/身份文件；实例目录保留）",
                            ),
                            None,
                            lock,
                            setup_secret,
                        ));
                    }
                    // 身份已原子落盘：推进 pending（IdentityWritten → Done → 清理）。失败**不删数据**。
                    let warning = match menghunote_sync::init_session::mark_identity_written(
                        &created.vault,
                    )
                    .and_then(|()| {
                        menghunote_sync::init_session::finish_identity_written(&created.vault)
                    }) {
                        Ok(()) => None,
                        Err(error) => Some(format!(
                            "身份已落盘，但初始化 pending 收尾失败（{error}）：实例保留可用，重启后会启动对账补齐；本次启动的同步可能被 pending 保护挡住"
                        )),
                    };
                    CreatedData {
                        vault: created.vault,
                        identity: created.identity,
                        vault_id: created.vault_id,
                        recovery_code: created.recovery_code,
                        warning,
                        api_version: RelayApiVersion::V1,
                    }
                }
                RelayMode::RemoteTls => {
                    // ADR 0013 S3b：受限初始化 + 身份回调（原子 rename 在 sync 的时序里发生）。
                    // 失败清理语义由 `remote_init` 负责：拿不到回执/身份前清本次新库，之后保留。
                    let progress = events.clone();
                    let created = create_remote(
                        context,
                        secret,
                        vault_path,
                        label,
                        move |identity: &LocalIdentityV1| {
                            let _ = progress
                                .send(CreateEvent::Progress(CreateStage::PersistingIdentity));
                            store
                                .persist_protected(vault_path, identity)
                                .map_err(|error| {
                                    SyncError::Io(format!("dev 身份持久化失败：{error:#}"))
                                })
                        },
                    );
                    match created {
                        Ok(created) => CreatedData {
                            vault: created.vault,
                            identity: created.identity,
                            vault_id: created.vault_id,
                            recovery_code: created.recovery_code,
                            warning: created.warning,
                            api_version: RelayApiVersion::V1,
                        },
                        Err(error) => {
                            let security = security_of(&error);
                            return Err(failed(
                                anyhow::Error::new(error).context(format!(
                                    "远端 Create 失败（模式 remote_tls，relay = {canonical_relay}，路径 = {}）",
                                    vault_path.display()
                                )),
                                security,
                                lock,
                                setup_secret,
                            ));
                        }
                    }
                }
            }
        }
    };

    let peer_id = created.identity.loro_peer_id();
    let key_epoch = created.identity.current_epoch;
    Ok(CreateOutcome {
        vault: created.vault,
        lock,
        vault_path: vault_path.to_path_buf(),
        relay_mode,
        api_version: created.api_version,
        relay_url: canonical_relay,
        vault_id: menghunote_sync::ids::hex_of(&created.vault_id),
        device_id: menghunote_sync::ids::hex_of(&created.identity.device_id),
        peer_id,
        key_epoch,
        recovery_code: Zeroizing::new(created.recovery_code),
        warning: created.warning,
    })
}

/// 两条编排的共同产物（避免在末尾对两套类型重复取值）。
struct CreatedData {
    vault: Vault,
    identity: LocalIdentityV1,
    vault_id: [u8; 16],
    recovery_code: String,
    warning: Option<String>,
    api_version: RelayApiVersion,
}

/// 清掉本次 Create 新建的库文件（含边车）与身份文件；**不删实例目录**。
fn cleanup_created_files(vault_path: &std::path::Path) {
    let _ = std::fs::remove_file(vault_path);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = vault_path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(sidecar));
    }
    let _ = std::fs::remove_file(crate::dev_sync::DevIdentityStore::path_for(vault_path));
}
