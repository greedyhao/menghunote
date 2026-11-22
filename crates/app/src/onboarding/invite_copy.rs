//! ADR 0017 **S5b**：备份页「复制邀请链接」的后台执行体。
//!
//! ## 为什么单独一个 job
//!
//! 刚创建完的实例停在**备份页**：`AppShell`/dev-sync worker 都还没构造（它们属于
//! Main），但 ADR 0017 §3 要求"创建成功后直接复制邀请链接给其他设备"。因此这里复用
//! 同一套 worker 纪律（专用线程 + channel + 明确终态），只做一件事：
//!
//! ```text
//! CopyInviteJob（worker 线程）：
//!   1. DevLocal::published_baseline（**本地、不发网络请求**）：没有基线 →
//!      `InviteCopyFailure::NoBaseline`（B 的 bootstrap 必然 NO_VALID_BASELINE，
//!      不能把失败留到加入端）
//!   2. DevIdentityStore::load（受保护身份；含管理私钥）
//!   3. RelayContext::from_mode（模式/面来自 CreateOutcome；不猜、不降级）
//!   4. sync::invite_link::create_invite_link（v2 链接带 api=2、默认 15 分钟）
//!   5. 交回 InviteLink（**含 token，只在内存**）→ UI 复制到剪贴板
//! ```
//!
//! ## 纪律
//!
//! - **不要求活动文档**：邀请是管理操作（ADR 0017 §3），只需要 vault + 身份 + 管理
//!   私钥；这里不碰编辑器/闸门。
//! - **前置检查在联网之前**：`published_baseline` 是 Create/发布写下的 durable 本地
//!   事实。缺失时明确失败（"创建端尚未发布初始基线…"），不向 relay 发任何请求，也
//!   不会让 B 侧先拿到一个注定 `NO_VALID_BASELINE` 的链接。
//! - token 只在 [`InviteLink`]（`Debug` 脱敏）；本模块不写配置/日志/库，也不把 URL
//!   放进错误文案；失败只交回脱敏后的可操作原因。
//! - 不自动重试：一次点击一次请求；失败后用户可再点（每次都会生成新链接，旧的作废）。
//! - 网络/签名都在 worker：UI 线程不阻塞（ADR 0011「网络/crypto 全部在专用线程」）。

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use menghunote_storage::Vault;
use menghunote_sync::api_version::RelayApiVersion;
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::http_transport::DEFAULT_TRANSPORT_TIMEOUTS;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::invite_link::{create_invite_link, InviteLink};
use menghunote_sync::relay_context::RelayContext;
use menghunote_sync::relay_policy::RelayMode;
use menghunote_sync::SyncError;

use super::sanitize_error;
use crate::dev_sync::DevIdentityStore;

/// worker 线程名（诊断用；日志里出现时一眼能认出是谁）。
const WORKER_THREAD_NAME: &str = "menghunote-onboarding-copy-invite";

/// 失败类别（每类对应一句用户能执行的话）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteCopyFailure {
    /// 本机没有管理密钥（Join 设备）：不能生成邀请。
    NoManagerKey,
    /// **创建端尚未发布初始基线**（Create 的自动发布失败/旧实例从未发布）。
    ///
    /// 单独一类而不是并进 `NotReady`：用户可以照提示显式「重试发布基线」；在基线出现
    /// 之前生成链接只会让加入端在 bootstrap 阶段撞 `NO_VALID_BASELINE`。
    NoBaseline,
    /// 身份缺 pin / 初始化 pending 未收尾（不能发管理请求）。
    NotReady,
    /// relay instance 与本地 pin 不一致（不自动换 pin）。
    InstanceMismatch,
    /// relay 不可达/超时/传输策略拒绝。
    Unreachable,
    /// TLS 端点认证失败。
    Tls,
    /// relay 明确拒绝（业务错误码）。
    Relay,
    /// 其它（本地存储/身份封装/未预期错误）。
    Other,
}

impl InviteCopyFailure {
    /// 面向用户的固定提示（不含 token/URL；技术细节由调用方追加脱敏文本）。
    #[must_use]
    pub const fn hint(self) -> &'static str {
        match self {
            Self::NoManagerKey => {
                "本机没有管理密钥（Join/邀请加入的设备）：不能生成邀请链接；请用创建端设备操作"
            }
            Self::NoBaseline => {
                "创建端尚未发布初始基线，请先在设备面板点「发布基线」/重试；\
                 基线出现前生成链接会让加入端在 bootstrap 阶段失败（NO_VALID_BASELINE）"
            }
            Self::NotReady => {
                "本机初始化尚未收尾或缺少 pin：暂不能生成邀请链接（不会自动重建/换 pin）"
            }
            Self::InstanceMismatch => {
                "relay instance 与本地 pin 不一致（疑似换实例/中间人）：不会自动换 pin，请核对 relay"
            }
            Self::Unreachable => {
                "relay 不可达（连接被拒/超时/模式与地址不符）：请确认 relay 已启动后再试"
            }
            Self::Tls => "TLS 端点认证失败：证书链/主机名/有效期不通过（不提供忽略校验）",
            Self::Relay => "relay 拒绝了生成邀请的请求（业务错误码见下方细节）",
            Self::Other => "生成邀请链接失败（没有写配置/日志，token 也不会落盘）",
        }
    }
}

impl InviteCopyFailure {
    /// 把 `SyncError` 映射成可操作类别（只读分类，不改错误本身）。
    #[must_use]
    pub fn classify(error: &SyncError) -> Self {
        match error {
            SyncError::Unsupported(_) => Self::NoManagerKey,
            SyncError::NeedsTrust(_) | SyncError::PinCorrupt(_) => Self::NotReady,
            SyncError::InstanceMismatch | SyncError::Equivocation(_) => Self::InstanceMismatch,
            SyncError::Transport(_) | SyncError::TransportPolicy(_) => Self::Unreachable,
            SyncError::Tls(_) => Self::Tls,
            SyncError::RelayApi { .. } => Self::Relay,
            _ => Self::Other,
        }
    }
}

/// 备份页复制邀请的进度（固定文案，不含 token）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyInviteStage {
    /// 读身份 + 向 relay 申请一次性邀请。
    Requesting,
}

impl CopyInviteStage {
    /// 进度文案。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Requesting => "正在生成一次性邀请链接（15 分钟默认；只在本机内存里）…",
        }
    }
}

/// 复制邀请链接的请求（`vault` 是已打开库的**共享句柄**：worker 只读，不落盘）。
pub struct CopyInviteRequest {
    /// 已打开库（备份页持有的同一实例的克隆；存储操作在工作线程串行）。
    pub vault: Vault,
    /// 库文件路径（读受保护身份用）。
    pub vault_path: PathBuf,
    /// 实例的 relay 模式（写链接/选传输）。
    pub relay_mode: RelayMode,
    /// canonical relay origin。
    pub relay_url: String,
    /// 该实例的 relay API 面（v2 链接带 `api=2`）。
    pub api_version: RelayApiVersion,
    /// **测试专用**：额外信任根（与 Create/Join 同一注入点；生产为空）。
    pub test_extra_roots: Vec<Vec<u8>>,
}

/// 复制邀请链接的终态事件（`InviteLink` 含 token，只在内存）。
pub enum CopyInviteEvent {
    /// 进度（固定文案）。
    Progress(CopyInviteStage),
    /// 成功：`InviteLink`（`Debug` 脱敏；调用方复制后即可 drop）。
    Finished(Box<InviteLink>),
    /// 失败：已脱敏文案 + 可操作类别。
    Failed {
        /// 已脱敏的完整文案（调用方直接显示）。
        message: String,
        /// 失败类别（UI 显示固定提示）。
        kind: InviteCopyFailure,
    },
}

/// 后台复制邀请任务（持有 channel 与线程；**不 join**，与 Create/Join 同一纪律）。
pub struct CopyInviteJob {
    rx: Receiver<CopyInviteEvent>,
    _thread: thread::JoinHandle<()>,
}

impl std::fmt::Debug for CopyInviteJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopyInviteJob").finish_non_exhaustive()
    }
}

impl CopyInviteJob {
    /// 启动后台任务；线程启动失败是**同步**错误（请求已被 drop，可安全重试）。
    pub fn start(request: CopyInviteRequest) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name(WORKER_THREAD_NAME.to_owned())
            .spawn(move || {
                let _ = tx.send(CopyInviteEvent::Progress(CopyInviteStage::Requesting));
                match run_copy_invite(&request) {
                    Ok(link) => {
                        drop(tx.send(CopyInviteEvent::Finished(Box::new(link))));
                    }
                    Err((error, kind)) => {
                        let message = sanitize_error(&error, &[]);
                        drop(tx.send(CopyInviteEvent::Failed { message, kind }));
                    }
                }
            })?;
        Ok(Self {
            rx,
            _thread: thread,
        })
    }

    /// 非阻塞收一条事件（生产泵用）。
    pub fn try_next(&self) -> Option<CopyInviteEvent> {
        self.rx.try_recv().ok()
    }

    /// 阻塞收一条事件（**测试专用**的显式收敛点；不用于 UI 线程）。
    pub fn recv(&self, timeout: Duration) -> Option<CopyInviteEvent> {
        self.rx.recv_timeout(timeout).ok()
    }
}

/// worker 主体：本地前置检查 → 读身份 → 构造**唯一** context → 生成链接（不改任何本地状态）。
fn run_copy_invite(
    request: &CopyInviteRequest,
) -> Result<InviteLink, (anyhow::Error, InviteCopyFailure)> {
    // 0) 前置检查（**不联网**）：没有 durable 基线事实时，链接即使生成也会让 B 在
    //    bootstrap 阶段拿到 NO_VALID_BASELINE；必须在这里明确挡住并指向「发布基线」。
    match DevLocal::published_baseline(&request.vault) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err((
                anyhow::anyhow!("生成邀请被拒绝：创建端尚未发布初始基线（本地检查，未发网络请求）"),
                InviteCopyFailure::NoBaseline,
            ));
        }
        Err(error) => {
            let kind = InviteCopyFailure::classify(&error);
            return Err((anyhow::Error::new(error), kind));
        }
    }
    let identity: LocalIdentityV1 = DevIdentityStore
        .load(&request.vault_path)
        .map_err(|error| (error, InviteCopyFailure::Other))?;
    let context = RelayContext::from_mode(
        request.relay_mode,
        &request.relay_url,
        DEFAULT_TRANSPORT_TIMEOUTS,
    )
    .and_then(|context| {
        let context = context.with_api_version(request.api_version);
        if request.test_extra_roots.is_empty() {
            Ok(context)
        } else {
            // 只追加显式信任根（测试 harness）；生产恒为空。
            context.with_test_extra_roots(request.test_extra_roots.iter())
        }
    })
    .map_err(|error| {
        let kind = InviteCopyFailure::classify(&error);
        (anyhow::Error::new(error), kind)
    })?;
    create_invite_link(&context, &request.vault, &identity).map_err(|error| {
        let kind = InviteCopyFailure::classify(&error);
        (
            anyhow::Error::new(error).context(format!(
                "生成邀请链接失败（模式 {}，relay = {}）",
                request.relay_mode.as_str(),
                request.relay_url
            )),
            kind,
        )
    })
}
