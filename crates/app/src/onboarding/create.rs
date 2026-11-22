//! Create 的**后台执行体**（ADR 0011「生命周期与后台」/ ADR 0018 S4b）。
//!
//! ## 唯一入口：创建链接（v2 一次性票据）
//!
//! ADR 0018 S4b 起 App **只**支持「粘贴创建链接」（`mn-pair://create`）：
//! 管理凭证/管理员创建模式已移除，v1 setup secret 创建表单也不再显示。
//!
//! ```text
//! Worker 线程（一次性票据）：
//!   1. remote_init::create_vault_with_ticket（**固定 v2 面**：`POST /v2/vaults`；
//!      与既有库级入口同一套主体：本地材料 → 写前 pending → 回执校验/强 pin →
//!      身份回调落盘 → 初始 recovery 槽 → pending 收尾）
//!   2. 票据只在内存里用一次：不写 pending/配置/日志；失败不消费票据（relay 事务回滚）
//!   3. **自动发布初始基线**：用同一 `RelayContext`（v2）调
//!      `DevLocal::publish_initial_pair_with_identity`（一轮 sync + 两份同 epoch
//!      checkpoint；初始 recovery 槽已在第 1 步上传，不再重复上传）。失败**不是**
//!      创建失败：库/身份/恢复码全部保留，`baseline_published=false` + 脱敏 warning，
//!      由备份页说明、面板「重试发布基线」显式重试（不自动重试）
//!   4. 成功后走备份页/配置写入路径，配置 `api_version=2`
//! ```
//!
//! 链接的 relay 模式（`http`=local_dev / `https`=remote_tls）由链接本身决定，不在表单里
//! 另设模式选择；`remote_tls` 仍要求在表单显式确认"远程服务器/HTTPS"。
//!
//! 两条路径都保证"**身份落盘成功之后**才把 vault_id/恢复码交给 UI"。
//!
//! 「刚创建就复制邀请」曾经复现 B 侧 `NO_VALID_BASELINE`：Create 只做 genesis + 初始
//! 槽，**没有**基线 checkpoint，B 的 bootstrap 找不到两份 published。把发布接进
//! Create 之后，备份页复制邀请即可用；邀请生成入口另有一道本地 `published_baseline`
//! 前置检查（失败不发网络请求，避免 B 侧才暴露问题）。
//!
//! ## 失败清理
//!
//! 票据路径的失败清理由 `remote_init` 决定（ADR 0013 §3）：拿不到回执之前失败 → 清理
//! 本次新建的库文件（远端最多留下不可用空 vault）；拿到回执之后失败（含身份落盘失败）
//! → **保留**库/pending 与已落盘事实（不删用户数据），Error 页可重试/等启动对账。
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
use menghunote_sync::remote_init::create_vault_with_ticket;

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
    /// 自动发布初始基线（一轮 sync + 两份同 epoch checkpoint）。
    PublishingBaseline,
}

impl CreateStage {
    /// 进度文案。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Genesis => "正在用一次性票据创建 vault（genesis + 初始恢复槽）…",
            Self::PersistingIdentity => "正在持久化设备身份（受保护）…",
            Self::PublishingBaseline => "正在自动发布初始基线（同步 + 两份 checkpoint）…",
        }
    }
}

/// Create 请求（含**秘密**：创建链接里的一次性票据）。
///
/// **不实现 `Debug`/`Clone`**：票据只在这一次创建里用，不复制、不打印。
pub struct CreateRequest {
    /// 目标库文件路径（实例目录里的 `vault.sqlite3`）。
    pub vault_path: std::path::PathBuf,
    /// relay 基地址（公开；`local_dev` 是 loopback 明文，`remote_tls` 是 HTTPS canonical origin）。
    pub relay_url: String,
    /// relay 接入模式（由创建链接 scheme 决定：`http`=local_dev，`https`=remote_tls）。
    pub relay_mode: RelayMode,
    /// 票据绑定的预留 vault id（创建必须使用同一个 id；错 vault 由 relay 统一 403）。
    pub vault_id: [u8; 16],
    /// 32B 票据明文（**只在内存**；由创建链接的 fragment 提供）。
    pub ticket: CreateTicket,
    /// 可选公开锚：创建链接里的 relay instance；回执不一致即失败（不自动换 pin）。
    pub expected_instance: Option<[u8; 16]>,
    /// 实例目录锁（移交给 worker，保证创建期间独占）。
    pub lock: InstanceLock,
    /// **测试专用**：远程初始化追加的显式信任根（DER；本地 TLS harness）。
    ///
    /// 生产恒为空（系统信任根）。只**增加**显式信任，不关闭证书链/SAN/有效期校验。
    pub test_extra_roots: Vec<Vec<u8>>,
    /// **测试专用**：自动发布初始基线改用故意不可达的 loopback 地址
    /// （`http://127.0.0.1:1`），模拟"vault 创建成功但基线发布失败"。
    ///
    /// 生产恒为 `false`。它只影响**发布**这一步：库/身份/恢复码仍真实创建并保留，
    /// 行为与真实网络故障完全一致（transport 连接失败）。
    pub test_baseline_unreachable: bool,
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
    /// 本次实例的 relay API 面（创建链接 = v2）。
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
    /// 初始基线是否已在 Create 内**自动发布**成功（两份同 epoch checkpoint）。
    ///
    /// `false` = 网络/权限/旧实例槽缺失等失败：实例仍可用（库/身份/恢复码都在），
    /// 但邀请生成会被本地前置检查明确挡住，直到用户显式「重试发布基线」。失败原因
    /// 在 [`Self::warning`] 里（已脱敏），**不吞**。
    pub baseline_published: bool,
    /// 非致命告警（例如身份已落盘但 pending 收尾失败：重启后启动对账会补；或初始基线
    /// 自动发布失败：可在面板重试）。
    ///
    /// **不影响**成功终态：实例已可用，不能因为一次收尾失败丢掉恢复码/重建实例。
    pub warning: Option<String>,
}

impl CreateOutcome {
    /// 公开事实（可写进配置的那部分；**不含**恢复码/票据/root）。
    #[must_use]
    pub fn public_facts(&self, instance_id: &str) -> super::layout::InstanceEntry {
        super::layout::InstanceEntry {
            instance_id: instance_id.to_owned(),
            // 自定义显示名由 UI 层在拿到公开事实后写入（worker 不知道用户输入）。
            display_name: None,
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
            vault_id,
            ticket,
            expected_instance,
            lock,
            test_extra_roots,
            test_baseline_unreachable,
        } = request;
        let thread = thread::Builder::new()
            .name(WORKER_THREAD_NAME.to_owned())
            .spawn(move || {
                let events = tx;
                let result = run_create(
                    &relay_url,
                    relay_mode,
                    vault_id,
                    ticket,
                    expected_instance,
                    &test_extra_roots,
                    test_baseline_unreachable,
                    &vault_path,
                    lock,
                    &events,
                );
                match result {
                    // 恢复码在 outcome 里；只有 UI 收下它才展示。
                    Ok(outcome) => drop(events.send(CreateEvent::Finished(Box::new(outcome)))),
                    Err(Failure {
                        error,
                        security,
                        lock,
                    }) => {
                        // 票据路径没有可替文案的秘密（`CreateTicket` 的 `Debug` 本身脱敏）；
                        // 恢复码形态的 token 仍会被 `sanitize_error` 遮住。
                        let message = sanitize_error(&error, &[]);
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
    /// 安全类失败（worker 在**包装成 anyhow 之前**按 `SyncError` 分类）。
    security: Option<SecurityCategory>,
    lock: InstanceLock,
}

/// 组装失败终态（携带锁）。
fn failed(error: anyhow::Error, security: Option<SecurityCategory>, lock: InstanceLock) -> Failure {
    Failure {
        error,
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
                "创建链接被拒绝（403 FORBIDDEN）：票据不存在/已过期/已被使用/不属于该 vault\
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

/// worker 线程主体：创建链接票据 → v2 创建 → 身份落盘 → 自动发布初始基线 → outcome。
#[allow(clippy::too_many_arguments)]
fn run_create(
    relay_url: &str,
    relay_mode: RelayMode,
    vault_id: [u8; 16],
    ticket: CreateTicket,
    expected_instance: Option<[u8; 16]>,
    test_extra_roots: &[Vec<u8>],
    test_baseline_unreachable: bool,
    vault_path: &std::path::Path,
    lock: InstanceLock,
    events: &mpsc::Sender<CreateEvent>,
) -> Result<CreateOutcome, Failure> {
    let _ = events.send(CreateEvent::Progress(CreateStage::Genesis));
    let label = crate::dev_sync::INSTANCE_LABEL;
    let store = crate::dev_sync::DevIdentityStore;
    // 先按链接模式构造 context（非法 URL/模式在**建库之前**失败；测试可注入自签根）。
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
            ));
        }
    };
    let canonical_relay = context.origin_str();
    // 票据端点就是 `/v2/vaults`：面由**创建链接**固定（不是 URL 猜的）。
    let context = context.with_api_version(RelayApiVersion::V2);
    // 自动发布基线复用**同一个** context（`RelayContext` 只是不可变描述：clone 不会共享
    // 可变状态；强 pin 已在 genesis 里 durable 提交，`publish_*` 用 vault 里的 pin）。
    let publish_context = context.clone();
    let progress = events.clone();
    let created = create_vault_with_ticket(
        context,
        vault_id,
        &ticket,
        expected_instance,
        vault_path,
        label,
        move |identity: &LocalIdentityV1| {
            let _ = progress.send(CreateEvent::Progress(CreateStage::PersistingIdentity));
            store
                .persist_protected(vault_path, identity)
                .map_err(|error| SyncError::Io(format!("dev 身份持久化失败：{error:#}")))
        },
    );
    let created = match created {
        Ok(created) => created,
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
            ));
        }
    };
    // 自动发布初始基线（ADR 0017「创建后即可邀请」）：失败**不是**创建失败——库/身份/
    // 恢复码都必须交回 UI；失败只降级为 `baseline_published=false` + 脱敏 warning。
    let _ = events.send(CreateEvent::Progress(CreateStage::PublishingBaseline));
    let publish_target = if test_baseline_unreachable {
        // 只影响发布这一步的**真实**传输失败（连接拒绝），不改 Create 的任何成功事实。
        match RelayContext::from_mode(
            RelayMode::LocalDev,
            "http://127.0.0.1:1",
            DEFAULT_TRANSPORT_TIMEOUTS,
        ) {
            Ok(context) => context.with_api_version(RelayApiVersion::V2),
            Err(_) => publish_context,
        }
    } else {
        publish_context
    };
    let (baseline_published, baseline_warning) = match DevLocal::publish_initial_pair_with_identity(
        &publish_target,
        &created.vault,
        &created.identity,
    ) {
        Ok(report) => {
            // 发布核心只有真实读回/导入通过才会返回 Ok；失败细节留给 error 分支。
            debug_assert!(report.validated, "发布基线必须真实校验通过");
            (true, None)
        }
        Err(error) => {
            // 缺初始 recovery 槽是**不可由面板补救**的例外（K_slot_mne 只能由恢复码派生）：
            // 文案必须直接指向恢复码 CLI，不能诱导用户去点一个注定失败的重试按钮。
            let slot_missing = matches!(&error, SyncError::Unsupported(_));
            // 错误原文先脱敏（网络错误可能带 URL/路径；不含 token），再作为 warning：
            // 不吞失败——否则用户会在 B 侧才看到 NO_VALID_BASELINE。
            let error = anyhow::Error::new(error).context("自动发布初始基线失败");
            (
                false,
                Some(format!(
                    "初始基线未自动发布（{}）；实例已创建、恢复码仍有效：{}",
                    sanitize_error(&error, &[]),
                    if slot_missing {
                        "面板无法补传初始 recovery 槽；请保留恢复码，用 CLI \
                         `--dev-sync-publish` 补传一次后再邀请其他设备"
                    } else {
                        "进入主界面后先打开/新建一篇笔记，在同步面板点「重试发布基线」，\
                         成功前邀请链接无法生成"
                    }
                )),
            )
        }
    };
    let warning = match (created.warning, baseline_warning) {
        (Some(existing), Some(baseline)) => Some(format!("{existing}；{baseline}")),
        (existing, baseline) => existing.or(baseline),
    };
    let peer_id = created.identity.loro_peer_id();
    let key_epoch = created.identity.current_epoch;
    Ok(CreateOutcome {
        vault: created.vault,
        lock,
        vault_path: vault_path.to_path_buf(),
        relay_mode,
        api_version: RelayApiVersion::V2,
        relay_url: canonical_relay,
        vault_id: menghunote_sync::ids::hex_of(&created.vault_id),
        device_id: menghunote_sync::ids::hex_of(&created.identity.device_id),
        peer_id,
        key_epoch,
        recovery_code: Zeroizing::new(created.recovery_code),
        baseline_published,
        warning,
    })
}
