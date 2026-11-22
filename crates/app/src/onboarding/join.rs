//! Join（加入已有同步实例）的**后台执行体**（ADR 0011「生命周期与后台」/ S3）。
//!
//! ## 顺序（硬约束，与 Create 同构）
//!
//! ```text
//! Worker 线程
//!   1. DevLocal::join（恢复码自批设备 + bootstrap 导入基线；先联网后落地）
//!   2. DevIdentityStore::persist_protected（受保护身份落盘）
//!   3. 把 Vault + 公开事实交回 UI（Main 直接打开第一篇）
//! ```
//!
//! 与 Create 的差别只有两点：**没有一次性恢复码要展示**（用户自己输入的），以及
//! `join` 成功后**立即**写公开配置并进 Main（没有备份确认页）。身份仍必须先受保护
//! 落盘——"能开窗但身份没落盘"是 ADR 0009/0011 共同禁止的半状态。
//!
//! ## 失败分类（UI 据此给可操作提示）
//!
//! | 类别 | 触发 | 文案要点 |
//! | --- | --- | --- |
//! | [`JoinFailureKind::RecoveryCodeFormat`] | 恢复码 Bech32m/长度非法（**不联网**） | 格式非法、未建文件 |
//! | [`JoinFailureKind::Credentials`] | relay 拒绝恢复码/vault_id（管理身份对不上） | 核对恢复码与 vault_id |
//! | [`JoinFailureKind::NoBaseline`] | `/bootstrap` 409 `NO_VALID_BASELINE`、404、vault 不符 | 请创建者先发布初始基线 |
//! | [`JoinFailureKind::Unreachable`] | 连接被拒/超时/5xx | 确认 relay 已启动，可重试 |
//! | [`JoinFailureKind::Other`] | 本地存储/封装/其它 | 原样脱敏 |
//!
//! **所有分类都可重试**：Join 不是跨端事务，relay 侧可能已经建过设备（恢复码自批），
//! 重试只是再造一个新设备身份；本地失败路径会清掉本次新建的库/身份文件。
//!
//! ## 锁与清理
//!
//! 调用方在启动 worker **之前**取得实例目录锁并移交（与 Create 共用）；
//! 成功时锁随 [`JoinOutcome::lock`] 交回 UI（Main 继续持有），失败时 worker 退出、
//! 锁 Drop（删掉自己的锁文件），不留下"目录被锁死"的状态。
//!
//! 失败清理**只删本次新建的库文件（含 WAL/SHM/journal）与身份文件**：实例目录本身
//! 不删（可能是用户已有的空目录）。网络类失败根本不创建这些文件。
//!
//! ## 秘密
//!
//! 恢复码只以 [`Zeroizing<String>`] 存在于本次请求/线程内：
//! [`JoinRequest`]/[`JoinOutcome`] 都**不实现 `Debug`/`Clone`**，失败文案经
//! [`super::sanitize_error`] 替换本次恢复码并遮住 `mnrec1…` token，**不得**把 `{error:#}`
//! 直接显示给用户或写进日志。

use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use zeroize::Zeroizing;

use menghunote_storage::Vault;
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::SyncError;

use super::lock::InstanceLock;
use super::sanitize_error;

/// worker 线程名（诊断用；日志里出现时一眼能认出是谁）。
const WORKER_THREAD_NAME: &str = "menghunote-onboarding-join";

/// Join 的进度阶段（UI 只显示固定文案，不带任何秘密）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinStage {
    /// 用恢复码自批设备并从 relay 导入基线。
    Bootstrap,
    /// 把设备身份受保护落盘。
    PersistingIdentity,
}

impl JoinStage {
    /// 进度文案。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bootstrap => "正在用恢复码自批设备并导入基线（bootstrap）…",
            Self::PersistingIdentity => "正在持久化设备身份（受保护）…",
        }
    }
}

/// Join 失败的类别（UI 文案与"下一步"由此决定）。
///
/// 数量刻意保持很少：每一类都对应**用户能做的一件事**；未知错误归到 [`Self::Other`]
/// 并原样展示脱敏细节，不臆造建议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinFailureKind {
    /// 恢复码格式非法（Bech32m 校验失败/长度不符；联网之前就被拒）。
    RecoveryCodeFormat,
    /// relay 拒绝：恢复码与 vault_id 不匹配（管理身份对不上）。
    Credentials,
    /// relay 上没有该 vault 的可用初始基线（未发布两份 checkpoint / 404 / vault 不符）。
    NoBaseline,
    /// relay 不可达或超时（连接被拒、读超时、5xx）。
    Unreachable,
    /// 其它（本地存储/身份封装/未预期错误）。
    Other,
}

impl JoinFailureKind {
    /// 面向用户的固定提示（不含秘密；后面会拼上脱敏技术细节）。
    #[must_use]
    pub const fn hint(self) -> &'static str {
        match self {
            Self::RecoveryCodeFormat => {
                "恢复码格式非法（应为 mnrec1… 的完整恢复码）：这一步没有联网、也没有创建任何文件，请核对后重试"
            }
            Self::Credentials => {
                "relay 拒绝了这次加入：恢复码或 vault_id 不匹配（恢复码区分大小写，vault_id 是 32 位十六进制）；请核对后重试"
            }
            Self::NoBaseline => {
                "relay 上没有可用的初始基线：若 vault_id 写错请核对；若实例刚创建，请创建者先在同步面板点「发布基线」（需要两份 checkpoint），再重试"
            }
            Self::Unreachable => {
                "relay 不可达或超时：请确认 relay 已启动、地址是 loopback 明文（例如 http://127.0.0.1:18080），然后重试"
            }
            Self::Other => "加入失败",
        }
    }

    /// 是否建议用户直接重试（本向导里全部允许重试；此方法只用于文案/测试断言）。
    #[must_use]
    pub const fn retryable(self) -> bool {
        true
    }
}

/// Join 请求（含**秘密**：恢复码）。
///
/// **不实现 `Debug`/`Clone`**：恢复码只在这一次加入里用，不复制、不打印。
pub struct JoinRequest {
    /// 目标库文件路径（实例目录里的 `vault.sqlite3`）。
    pub vault_path: std::path::PathBuf,
    /// relay 基地址（公开，已在校验阶段确认是 loopback）。
    pub relay_url: String,
    /// 服务器 vault id（16B；公开标识，成功后会以 32 位十六进制写进配置）。
    pub vault_id: [u8; 16],
    /// 用户输入的 vault id 文本（公开标识；只用于错误文案，原样是十六进制）。
    pub vault_id_hex: String,
    /// 创建者提供的恢复码（**秘密**；`Zeroizing` 在 drop 时清零）。
    pub recovery_code: Zeroizing<String>,
    /// 实例目录锁（移交给 worker，保证加入期间独占）。
    pub lock: InstanceLock,
}

/// Join 成功后的产物。
///
/// **不实现 `Debug`**：虽无恢复码，但 `vault`/`identity` 派生事实不应被随手打印。
pub struct JoinOutcome {
    /// 已打开的真实库（worker 里由 bootstrap 导入基线；`Vault` 是 `Send + Sync`）。
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
}

impl JoinOutcome {
    /// 公开事实（可写进配置的那部分；**不含**恢复码/root/私钥）。
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

/// Join 后台事件。
pub enum JoinEvent {
    /// 进度（固定文案，不含秘密）。
    Progress(JoinStage),
    /// 成功终态。
    Finished(Box<JoinOutcome>),
    /// 失败终态：类别 + **已脱敏**文案（调用方直接显示）。
    Failed {
        /// 失败类别（UI 据此决定是否回表单/给什么下一步）。
        kind: JoinFailureKind,
        /// 已脱敏的完整文案（调用方直接显示）。
        message: String,
    },
}

/// 在后台线程里跑的 Join 任务。
pub struct JoinJob {
    /// 事件接收端（非阻塞 `try_next`；测试有显式阻塞等待）。
    rx: Receiver<JoinEvent>,
    /// worker 线程句柄：**不 join**（ADR 0011「GUI 不 join 线程」）；持有着只为文档/诊断。
    _thread: thread::JoinHandle<()>,
}

impl std::fmt::Debug for JoinJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinJob").finish_non_exhaustive()
    }
}

impl JoinJob {
    /// 启动后台 Join。
    ///
    /// 线程启动失败是**同步**错误（`std::io::Error`）：此时请求（含锁）已被 drop，
    /// 调用方可以安全地重试或退回表单。
    pub fn start(request: JoinRequest) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let JoinRequest {
            vault_path,
            relay_url,
            vault_id,
            vault_id_hex,
            recovery_code,
            lock,
        } = request;
        let thread = thread::Builder::new()
            .name(WORKER_THREAD_NAME.to_owned())
            .spawn(move || {
                let events = tx;
                match run_join(
                    &relay_url,
                    &vault_id,
                    &vault_id_hex,
                    &recovery_code,
                    &vault_path,
                    lock,
                    &events,
                ) {
                    Ok(outcome) => drop(events.send(JoinEvent::Finished(Box::new(outcome)))),
                    Err(failure) => {
                        drop(events.send(JoinEvent::Failed {
                            kind: failure.kind,
                            message: failure.message,
                        }));
                        // 失败时锁随局部变量 drop（删掉本次自己创建的锁文件）。
                        drop(failure.lock);
                    }
                }
            })?;
        Ok(Self {
            rx,
            _thread: thread,
        })
    }

    /// 非阻塞收一条事件（生产泵用）。
    pub fn try_next(&self) -> Option<JoinEvent> {
        // `Disconnected`（worker 已退出）与"暂时没有事件"同义：终态事件已在 channel
        // 里排在我们前面，排空后才可能断开。
        self.rx.try_recv().ok()
    }

    /// 阻塞收一条事件（**测试专用**的显式收敛点；不用于 UI 线程）。
    pub fn recv(&self, timeout: Duration) -> Option<JoinEvent> {
        self.rx.recv_timeout(timeout).ok()
    }
}

/// 失败时把锁**带回来**（供 worker 在发完事件后释放；避免在错误类型里丢所有权）。
struct Failure {
    kind: JoinFailureKind,
    message: String,
    lock: InstanceLock,
}

/// worker 线程主体：join → 身份落盘 → outcome。
fn run_join(
    relay_url: &str,
    vault_id: &[u8; 16],
    vault_id_hex: &str,
    recovery_code: &str,
    vault_path: &std::path::Path,
    lock: InstanceLock,
    events: &mpsc::Sender<JoinEvent>,
) -> Result<JoinOutcome, Failure> {
    let _ = events.send(JoinEvent::Progress(JoinStage::Bootstrap));
    let joined = match DevLocal::join(relay_url, vault_id, recovery_code, vault_path) {
        Ok(joined) => joined,
        Err(error) => {
            // 先把**底层**错误分类（`anyhow` 包装后就再也 downcast 不回去了）。
            let kind = classify_join_error(&error);
            let error = anyhow::Error::new(error).context(format!(
                "Join 失败（relay = {relay_url}，vault_id = {vault_id_hex}，路径 = {}）",
                vault_path.display()
            ));
            return Err(Failure {
                kind,
                message: failure_message(kind, &error, recovery_code),
                lock,
            });
        }
    };

    let _ = events.send(JoinEvent::Progress(JoinStage::PersistingIdentity));
    let store = crate::dev_sync::DevIdentityStore;
    if let Err(error) = store.persist_protected(vault_path, &joined.identity) {
        // 身份没落盘 = 本次加入无效：关库后清掉本次新建的库/身份文件（目录保留）。
        drop(joined.vault);
        cleanup_created_files(vault_path);
        let error = error.context(format!(
            "设备身份持久化失败（已清理本次新建的库文件/身份文件；实例目录保留：{}）",
            vault_path.display()
        ));
        return Err(Failure {
            kind: JoinFailureKind::Other,
            message: failure_message(JoinFailureKind::Other, &error, recovery_code),
            lock,
        });
    }

    Ok(JoinOutcome {
        vault: joined.vault,
        lock,
        vault_path: vault_path.to_path_buf(),
        vault_id: menghunote_sync::ids::hex_of(&joined.vault_id),
        device_id: menghunote_sync::ids::hex_of(&joined.identity.device_id),
        peer_id: joined.identity.loro_peer_id(),
        key_epoch: joined.identity.current_epoch,
    })
}

/// 把 `DevLocal::join` 的底层错误映射成用户可操作的类别。
///
/// 只认**明确的** wire 错误码；不认识的一律 [`JoinFailureKind::Other`]（不臆造建议）。
/// `retryable` 只用于把 5xx/瞬时错误归到 [`JoinFailureKind::Unreachable`]。
#[must_use]
pub fn classify_join_error(error: &SyncError) -> JoinFailureKind {
    match error {
        // 恢复码 Bech32m/长度非法：在任何网络请求之前。
        SyncError::RecoveryCode(_) => JoinFailureKind::RecoveryCodeFormat,
        // 连接被拒/读超时/响应截断。
        SyncError::Transport(_) => JoinFailureKind::Unreachable,
        SyncError::RelayApi {
            code, retryable, ..
        } => match code.as_str() {
            "NO_VALID_BASELINE" | "NOT_FOUND" => JoinFailureKind::NoBaseline,
            // 恢复码错 → 管理 subject 对不上（MANAGEMENT_REQUIRED）；其余是鉴权类拒绝。
            "MANAGEMENT_REQUIRED"
            | "SIGNATURE_INVALID"
            | "UNAUTHORIZED"
            | "FORBIDDEN"
            | "CHALLENGE_INVALID"
            | "CHALLENGE_USED"
            | "INVALID_REQUEST" => JoinFailureKind::Credentials,
            _ if *retryable => JoinFailureKind::Unreachable,
            _ => JoinFailureKind::Other,
        },
        // 老 relay / 未实现端点：404 与"没有基线"给同一个可操作提示。
        SyncError::HttpStatus(404) => JoinFailureKind::NoBaseline,
        SyncError::HttpStatus(status) if *status >= 500 => JoinFailureKind::Unreachable,
        // 管理 realm 里没有 recovery 槽：实例没发布过基线（或不是 dev 实例）。
        SyncError::Object(message) if message.contains("recovery 槽") => {
            JoinFailureKind::NoBaseline
        }
        // relay 的 vault 与输入的 vault_id 不一致（challenge 响应不符）。
        SyncError::RelayWire(message) if message.contains("vault 不符") => {
            JoinFailureKind::NoBaseline
        }
        _ => JoinFailureKind::Other,
    }
}

/// 拼出最终展示文案：固定提示 + 脱敏技术细节。
///
/// **只用 `sanitize_error`**：恢复码正文与 `mnrec1…` token 不出现；超长错误被截断。
fn failure_message(kind: JoinFailureKind, error: &anyhow::Error, recovery_code: &str) -> String {
    let detail = sanitize_error(error, &[recovery_code]);
    match kind {
        JoinFailureKind::Other => format!("{}：{detail}", kind.hint()),
        _ => format!("{}（技术细节：{detail}）", kind.hint()),
    }
}

/// 清掉本次 Join 新建的库文件（含边车）与身份文件；**不删实例目录**。
fn cleanup_created_files(vault_path: &std::path::Path) {
    let _ = std::fs::remove_file(vault_path);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = vault_path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(sidecar));
    }
    let _ = std::fs::remove_file(crate::dev_sync::DevIdentityStore::path_for(vault_path));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay_api(code: &str, retryable: bool) -> SyncError {
        SyncError::RelayApi {
            code: code.to_owned(),
            retryable,
            current_epoch: None,
            current_control_rev: None,
            bootstrap_hint: None,
        }
    }

    #[test]
    fn error_classes_cover_the_actionable_cases() {
        assert_eq!(
            classify_join_error(&SyncError::RecoveryCode("Bech32m 校验失败")),
            JoinFailureKind::RecoveryCodeFormat
        );
        // 错恢复码在 relay 上表现为管理 subject 对不上。
        assert_eq!(
            classify_join_error(&relay_api("MANAGEMENT_REQUIRED", false)),
            JoinFailureKind::Credentials
        );
        assert_eq!(
            classify_join_error(&relay_api("SIGNATURE_INVALID", false)),
            JoinFailureKind::Credentials
        );
        // 未发布两份基线 / 端点不存在。
        assert_eq!(
            classify_join_error(&relay_api("NO_VALID_BASELINE", true)),
            JoinFailureKind::NoBaseline
        );
        assert_eq!(
            classify_join_error(&relay_api("NOT_FOUND", false)),
            JoinFailureKind::NoBaseline
        );
        assert_eq!(
            classify_join_error(&SyncError::HttpStatus(404)),
            JoinFailureKind::NoBaseline
        );
        assert_eq!(
            classify_join_error(&SyncError::Object(
                "relay 没有 recovery 槽（管理 realm 应可见 staging）"
            )),
            JoinFailureKind::NoBaseline
        );
        assert_eq!(
            classify_join_error(&SyncError::RelayWire(
                "management challenge 过期/vault 不符"
            )),
            JoinFailureKind::NoBaseline
        );
        // 不可达 / 5xx / 瞬时。
        assert_eq!(
            classify_join_error(&SyncError::Transport("连接被拒")),
            JoinFailureKind::Unreachable
        );
        assert_eq!(
            classify_join_error(&relay_api("HTTP_503", true)),
            JoinFailureKind::Unreachable
        );
        assert_eq!(
            classify_join_error(&SyncError::HttpStatus(502)),
            JoinFailureKind::Unreachable
        );
        // 不认识的一律 Other（不臆造建议）。
        assert_eq!(
            classify_join_error(&SyncError::Storage(
                menghunote_storage::StorageError::ChannelClosed {
                    context: "join 单测",
                }
            )),
            JoinFailureKind::Other
        );
        assert_eq!(
            classify_join_error(&relay_api("BUSY", false)),
            JoinFailureKind::Other
        );
    }

    #[test]
    fn failure_messages_never_echo_the_recovery_code() {
        let code = "mnrec1secretcode0123456789";
        let error = anyhow::anyhow!("relay 拒绝 code={code}（vault_id=00112233）");
        let message = failure_message(JoinFailureKind::Credentials, &error, code);
        assert!(
            !message.contains("secretcode"),
            "恢复码正文不得出现：{message}"
        );
        assert!(
            message.contains("恢复码或 vault_id 不匹配"),
            "必须给出可操作提示：{message}"
        );
        assert!(
            message.contains("00112233"),
            "公开的 vault_id 可以保留（诊断需要）：{message}"
        );
        // 不知道正文时也要遮住 mnrec1 形态的 token。
        let leaked = anyhow::anyhow!("bad token mnrec1abcdef0123");
        let message = failure_message(JoinFailureKind::Other, &leaked, "unrelated");
        assert!(!message.contains("mnrec1abcdef"), "{message}");
        assert!(message.contains("<恢复码已隐藏>"), "{message}");
    }

    #[test]
    fn every_class_carries_a_user_facing_hint() {
        for kind in [
            JoinFailureKind::RecoveryCodeFormat,
            JoinFailureKind::Credentials,
            JoinFailureKind::NoBaseline,
            JoinFailureKind::Unreachable,
            JoinFailureKind::Other,
        ] {
            assert!(!kind.hint().is_empty(), "{kind:?} 必须有文案");
            assert!(kind.retryable(), "{kind:?} 在向导里都允许重试");
        }
        assert!(
            JoinFailureKind::NoBaseline.hint().contains("发布基线"),
            "未发布基线必须指向「发布基线」按钮"
        );
        assert!(
            JoinFailureKind::Unreachable.hint().contains("重试"),
            "不可达必须说明可重试"
        );
    }

    #[test]
    fn stage_labels_are_secret_free() {
        for stage in [JoinStage::Bootstrap, JoinStage::PersistingIdentity] {
            let label = stage.label();
            assert!(!label.is_empty());
            assert!(!label.contains("mnrec1"), "{label}");
        }
    }
}
