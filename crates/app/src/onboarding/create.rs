//! Create 的**后台执行体**（ADR 0011「生命周期与后台」/ S2）。
//!
//! ## 顺序（硬约束，不能交换）
//!
//! ```text
//! Worker 线程
//!   1. DevLocal::create（真实 genesis + 初始 recovery 槽上传；先联网后落地）
//!   2. DevIdentityStore::persist_protected（受保护身份落盘）
//!   3. 把 Vault + 公开事实 + 一次性恢复码交回 UI
//! ```
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
use menghunote_sync::dev_local::DevLocal;

use super::lock::InstanceLock;
use super::sanitize_error;

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

/// Create 请求（含**秘密**：setup secret）。
///
/// **不实现 `Debug`/`Clone`**：setup secret 只在这一次创建里用，不复制、不打印。
pub struct CreateRequest {
    /// 目标库文件路径（实例目录里的 `vault.sqlite3`）。
    pub vault_path: std::path::PathBuf,
    /// relay 基地址（公开，已在校验阶段确认是 loopback）。
    pub relay_url: String,
    /// 服务器创建凭证（**秘密**；`Zeroizing` 在 drop 时清零）。
    pub setup_secret: Zeroizing<String>,
    /// 实例目录锁（移交给 worker，保证创建期间独占）。
    pub lock: InstanceLock,
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
}

impl CreateOutcome {
    /// 公开事实（可写进配置的那部分；**不含**恢复码/setup secret/root）。
    #[must_use]
    pub fn public_facts(&self, instance_id: &str, relay_url: &str) -> super::layout::InstanceEntry {
        super::layout::InstanceEntry {
            instance_id: instance_id.to_owned(),
            relay_url: relay_url.to_owned(),
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
    /// 失败终态：**已脱敏**的文案（调用方直接显示）。
    Failed(String),
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
            setup_secret,
            lock,
        } = request;
        let thread = thread::Builder::new()
            .name(WORKER_THREAD_NAME.to_owned())
            .spawn(move || {
                let events = tx;
                let result = run_create(&relay_url, &setup_secret, &vault_path, lock, &events);
                match result {
                    // 恢复码在 outcome 里；只有 UI 收下它才展示。
                    Ok(outcome) => drop(events.send(CreateEvent::Finished(Box::new(outcome)))),
                    Err(Failure { error, lock }) => {
                        // 脱敏：setup secret 的任何回显都替换掉，恢复码形态的 token 也遮住。
                        let message = sanitize_error(&error, &[setup_secret.as_str()]);
                        drop(events.send(CreateEvent::Failed(message)));
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
    lock: InstanceLock,
}

/// worker 线程主体：create → 身份落盘 → outcome。
fn run_create(
    relay_url: &str,
    setup_secret: &str,
    vault_path: &std::path::Path,
    lock: InstanceLock,
    events: &mpsc::Sender<CreateEvent>,
) -> Result<CreateOutcome, Failure> {
    let _ = events.send(CreateEvent::Progress(CreateStage::Genesis));
    let created = match DevLocal::create(
        relay_url,
        setup_secret,
        vault_path,
        crate::dev_sync::setup::INSTANCE_LABEL,
    ) {
        Ok(created) => created,
        Err(error) => {
            return Err(Failure {
                error: anyhow::Error::new(error).context(format!(
                    "Create 失败（relay = {relay_url}，路径 = {}）",
                    vault_path.display()
                )),
                lock,
            })
        }
    };
    // setup secret 只在这一步用；`CreatedVault.setup_secret` 是回显字段，立即丢弃。
    drop(created.setup_secret);

    let _ = events.send(CreateEvent::Progress(CreateStage::PersistingIdentity));
    let store = crate::dev_sync::DevIdentityStore;
    if let Err(error) = store.persist_protected(vault_path, &created.identity) {
        // 身份没落盘 = 本次创建无效：关库后清掉本次新建的库/身份文件（目录保留）。
        drop(created.vault);
        cleanup_created_files(vault_path);
        return Err(Failure {
            error: error
                .context("设备身份持久化失败（已清理本次新建的库文件/身份文件；实例目录保留）"),
            lock,
        });
    }

    let peer_id = created.identity.loro_peer_id();
    let key_epoch = created.identity.current_epoch;
    Ok(CreateOutcome {
        vault: created.vault,
        lock,
        vault_path: vault_path.to_path_buf(),
        vault_id: menghunote_sync::ids::hex_of(&created.vault_id),
        device_id: menghunote_sync::ids::hex_of(&created.identity.device_id),
        peer_id,
        key_epoch,
        recovery_code: Zeroizing::new(created.recovery_code),
    })
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
