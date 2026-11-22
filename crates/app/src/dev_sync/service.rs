//! dev-sync 的串行 worker（ADR 0009 Phase 1 / 切片 1、4c-2）。
//!
//! ## 边界（硬约束）
//!
//! - **不在 GPUI 线程做 IO/crypto**：本文件起一个专用 `std::thread`，命令经 channel 进来，
//!   事件经 channel 出去；GPUI 侧只做非阻塞 `poll()`。
//! - **单操作在途**：由 [`SyncState`](super::state::SyncState) 保证；worker 串行消费命令。
//! - **GUI 不 join**：`Stop` 后 worker 自己退出，面板只查 [`SyncService::poll_close`]；
//!   回收交给 [`SyncService::try_reap_worker`]（非阻塞）或进程退出（线程已 detached）。
//! - **不谎报成功**：未实现的操作命令返回明确错误；`SyncOnce` 由真实 runner 执行（切片
//!   4c-2），失败一律 `Err(String)`，面板显示原文。
//!
//! ## 两种 runner
//!
//! | runner | 谁用 | 行为 |
//! | --- | --- | --- |
//! | [`UnimplementedRunner`] | [`SyncService::new`]（测试/未接线路径） | 所有操作命令返回"未实现" |
//! | [`DevLocalRunner`] | [`SyncService::new_dev_local`]（生产面板） | 真实跑 [`DevLocal::sync_once`] |
//!
//! ## 持闸纪律（切片 4c-2 / ADR 0012 S2a）
//!
//! worker 与 UI 共用同一个 SQLite，`engine.integrate` 会绕过本地会话直接写外部行。
//! 旧整轮 `SyncOnce` **必须**带 [`HeldGate`] 派发（[`SyncService::try_command_with_gate`]）；
//! 两阶段路径由 [`SyncService::try_command_two_phase`] 发起 **Prepare（不持闸）**，worker
//! 停在 [`Event::NeedGate`] 等 UI 的 [`SyncControl::GrantGate`]；授权里带文档、durable head、
//! 闸门代际与会话纪元，worker 在 `integrate` 前再复核（不满足就不 append）。UI 交付完成
//! 后发 [`SyncControl::Finish`]，worker 才 push tail + ack 并回 `Completed`。
//!
//! 授权/取消/停止走**独立控制通道** [`SyncControl`]：两阶段 worker 会停在阶段等待里，
//! 不能从普通 job 队列取授权；控制通道也保证 `GrantGate`/`Cancel` 不被在途普通任务阻塞。
//!
//! ## 无会话同步（没有活动文档）
//!
//! 空库 / 还没打开笔记时**没有** `DocumentSession` 可以 begin/adopt，闸门无从谈起；
//! 这条路径改用 [`Command::SyncOnceNoSession`]（[`SyncService::try_command_no_session`]），
//! 由 [`DevLocalRunner::sync_once_no_session`] 执行。它**不接受** [`HeldGate`]（带了就
//! 明确拒绝），调用方契约见该函数的文档。两条路径互斥，不能混用。
//!
//! ## 超时
//!
//! 两层，各管一段、互不替代：
//!
//! 1. **操作 deadline**（[`ServiceConfig::operation_deadline`]）：worker 线程里的同步调用
//!    无法被中断，因此超时**只在服务侧判定**——`poll()` 看到在途 op 超时后合成一条
//!    `Completed{Failed}`，并把该 op 记为"已放弃"，此后新命令会被 `Busy` 拒绝，直到
//!    真实终态事件回来（迟到事件不会重复通知，见 `state.rs` 的迟到纪律）。
//! 2. **传输超时**（[`ServiceConfig::transport`]）：连接/读写/整次请求的 socket 超时，
//!    由 `menghunote_sync::http_transport::StdTcpTransport` 提供；真实请求由
//!    [`DevLocalRunner`] 在 worker 线程里发起（`DevLocal::sync_once` 内部自己构造传输）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use menghunote_storage::{load_head, Vault, STRUCTURE_DOC_ID};
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::error::SyncError;
use menghunote_sync::http_transport::StdTcpTransport;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::DeviceId;

pub use menghunote_sync::http_transport::{
    TransportTimeouts, AUTOSYNC_TRANSPORT_TIMEOUTS, DEFAULT_TRANSPORT_TIMEOUTS,
};

use super::identity::DevIdentityStore;
use super::state::{
    short_device_id, Busy, BusyReason, CloseState, Command, Event, HeldGate, OpId, OpOutcome,
    OpResult, Phase, StateSnapshot, StopTicket, SyncPhase, SyncState, REVOKE_UNCHANGED_MARKER,
};

/// 默认操作 deadline：开发演示下 Argon2/首次 bootstrap 可能很慢，但也不能无限挂。
pub const DEFAULT_OPERATION_DEADLINE: Duration = Duration::from_secs(120);

/// worker 的配置（门禁产出、面板显示、测试注入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// relay 基地址（只用于显示与传输配置；**不含 secret**）。
    pub relay: String,
    /// 单条命令的 deadline（`None` = 不限制）。
    pub operation_deadline: Option<Duration>,
    /// 传输层超时（ADR 0012 S1：**真实**接到 `DevLocal` 的 transport 构造上）。
    ///
    /// 默认 [`DEFAULT_TRANSPORT_TIMEOUTS`]（连接/读写各 30s、无整次请求预算），与手动
    /// 同步的历史行为逐项一致；自动同步场景由 [`AUTOSYNC_TRANSPORT_TIMEOUTS`] 提供
    /// 5/10/30s（S3 才接线）。
    pub transport: TransportTimeouts,
}

impl ServiceConfig {
    /// 按 relay 地址给一套开发默认配置（手动同步：保持既有传输行为）。
    #[must_use]
    pub fn new(relay: impl Into<String>) -> Self {
        Self {
            relay: relay.into(),
            operation_deadline: Some(DEFAULT_OPERATION_DEADLINE),
            transport: DEFAULT_TRANSPORT_TIMEOUTS,
        }
    }

    /// 自动同步场景的建议配置（ADR 0012：连接/IO/请求 = 5/10/30s）。
    ///
    /// **S1 不启用自动同步**：本构造器只是把预算定义在一处，供 S3 接线与测试使用。
    #[must_use]
    pub fn for_autosync(relay: impl Into<String>) -> Self {
        Self {
            transport: AUTOSYNC_TRANSPORT_TIMEOUTS,
            ..Self::new(relay)
        }
    }

    /// 显式换一套传输预算（连接/IO/整次请求）。
    #[must_use]
    pub fn with_transport(mut self, transport: TransportTimeouts) -> Self {
        self.transport = transport;
        self
    }

    /// 按本配置构造传输对象（`http://` 只允许 loopback，由 transport 的策略兜底）。
    ///
    /// 与 worker 真实同步用的是**同一个** [`TransportTimeouts`]：这里只是把"配置能不能
    /// 构造出一个合法 transport"也暴露出来（门禁/诊断/测试）。
    pub fn build_transport(&self) -> Result<StdTcpTransport, SyncError> {
        StdTcpTransport::new_local_dev_with(&self.relay, Some(self.transport))
    }
}

/// 操作 deadline 的时钟（ADR 0012 测试注入点；生产用系统单调时钟）。
///
/// 为什么需要它：S2a 的超时用例要求"持闸期间才到期"，既不能真睡到 deadline，也不能
/// 用 `Duration::ZERO` 让 Prepare 阶段先超时。测试注入手动时钟，只在闸门 Held 后推进。
pub trait OperationClock: Send + Sync {
    /// 当前单调时刻（`Instant`；只用于同一时钟内的差值）。
    fn now(&self) -> Instant;
}

/// 生产时钟：`Instant::now()`（单调，不受墙钟调整影响）。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemOperationClock;

impl OperationClock for SystemOperationClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// 在途操作的控制通道（ADR 0012 S1）：取消标志由服务侧置位、worker 侧读取。
///
/// 为什么必须是共享原子：`Cancel` / deadline 发生在 UI 线程，真正的复核点在 worker
/// 线程即将 `integrate` 之前；runner 的签名同步执行，不能靠 channel 再读一次。
#[derive(Debug, Clone, Default)]
pub struct OpControl {
    /// `None` = 控制命令/无取消通道（永远不算被取消）。
    cancelled: Option<Arc<AtomicBool>>,
}

impl OpControl {
    /// 新建一条可取消的控制通道（非控制命令派发时创建）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            cancelled: Some(Arc::new(AtomicBool::new(false))),
        }
    }

    /// 请求取消（`Cancel` 命令或 operation deadline 到期时调用）。
    pub fn request_cancel(&self) {
        if let Some(flag) = &self.cancelled {
            flag.store(true, Ordering::SeqCst);
        }
    }

    /// 是否已请求取消（worker 在外部事务边界复核）。
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    /// 有没有真正的取消通道（控制命令没有）。
    #[must_use]
    pub fn is_controllable(&self) -> bool {
        self.cancelled.is_some()
    }
}

/// 两阶段同步的**独立控制通道**（ADR 0012 S2a）。
///
/// 为什么不走普通命令队列：两阶段 worker 会在 `NeedGate` / `Integrated` 处**停下等回执**，
/// 此时它已经不在 `jobs.recv()` 上；`GrantGate` / `Finish` / 取消 / 停止必须能直接穿透
/// 这个等待。普通命令仍走 job 队列（单操作串行），不会被这条通道插队。
#[derive(Debug)]
pub enum SyncControl {
    /// UI 已短持闸，授权 worker 执行 `integrate`。
    ///
    /// 三重绑定：`op_id`（本次操作）、`authorization_epoch`（worker 在 NeedGate 给出的
    /// 代际）、`gate`（文档 + durable head + 闸门代际 + 会话纪元）。worker 会再复核
    /// 这些值，任一对不上都不 append。
    GrantGate {
        /// 本次两阶段操作。
        op_id: OpId,
        /// worker 在 `NeedGate` 里给出的授权代际（旧授权 = 拒绝）。
        authorization_epoch: u64,
        /// 持闸证明。
        gate: HeldGate,
    },
    /// UI 已 `adopt` 并 `end`：授权 worker 做收尾（push tail + ack）。
    Finish {
        /// 本次两阶段操作。
        op_id: OpId,
    },
    /// 取消本次操作（用户取消 / operation deadline）：worker 在下一个外部事务边界前停手。
    Cancel {
        /// 本次两阶段操作。
        op_id: OpId,
    },
    /// 停止 worker（`Stop`）：解开任何阶段的等待，随后仍由 job 队列里的 `Stop` 收尾。
    Stop,
}

/// 同步执行体（测试注入点）。
///
/// 生产实现是 [`DevLocalRunner`]（切片 4c-2：真实 `DevLocal::sync_once`）；测试用
/// [`UnimplementedRunner`] 或自定义 runner（见 `tests/dev_sync_service.rs` 的
/// `ControlledRunner`）。
///
/// 实现者通过 `events` 上报两类事件：
///
/// - `Progress`：`op_id` 由 worker 统一盖章，runner 用 [`OpId::UNASSIGNED`] 占位；
/// - 领域事件（`DevicesListed` / `DeviceRevoked`，ADR 0010 切片 2a-2）：不带 op 标签，
///   原样穿过 worker。
///
/// 终态由 worker 依据返回值生成（runner 不得自己伪造 `Completed`/`Stopped`，否则会被
/// 当作迟到事件丢弃）。
///
/// 返回 `Ok(OpOutcome)` 表示"目标 head 已确认 durable"（真实 runner 以 `SyncEngine` 的
/// ACK/integrate 为准）。
pub trait SyncRunner: Send {
    /// 执行一条操作命令。`Err(String)` 是**明确失败**（面板显示原文，不得含 secret）。
    fn run(&mut self, op: Command, events: &mut dyn FnMut(Event)) -> Result<OpOutcome, String>;

    /// 带持闸证明执行（切片 4c-2）。
    ///
    /// 默认实现忽略证明、转发给 [`SyncRunner::run`]：这样只关心状态机的假 runner 不必
    /// 关心闸门；**生产 runner 重写它**并在缺证明时明确拒绝（不能盲跑 `SyncOnce`）。
    fn run_gated(
        &mut self,
        op: Command,
        gate: Option<&HeldGate>,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        let _ = gate;
        self.run(op, events)
    }

    /// 带持闸证明 + **取消控制通道**执行（ADR 0012 S1）。
    ///
    /// 默认实现忽略控制通道并转发给 [`SyncRunner::run_gated`]：只关心状态机的假 runner
    /// 不必实现它。生产 runner 重写它，在**首次外部集成前**复核取消/授权（取消赢则
    /// 不 append）。
    fn run_gated_controlled(
        &mut self,
        op: Command,
        gate: Option<&HeldGate>,
        control: &OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        let _ = control;
        self.run_gated(op, gate, events)
    }

    /// 接管两阶段同步的**独立控制通道**（ADR 0012 S2a）。
    ///
    /// worker 启动时调用一次；默认实现把通道丢掉（只关心旧整轮路径的假 runner 不必实现）。
    /// 生产 runner 保存 receiver，在 Prepare/Integrate 阶段之间阻塞等授权/收尾。
    fn attach_control(&mut self, control: Receiver<SyncControl>) {
        let _ = control;
    }

    /// 执行一次**可暂停的两阶段**同步：Prepare → 等 `GrantGate` → Integrate → 等
    /// `Finish` → 收尾。
    ///
    /// 与 [`SyncRunner::run_gated_controlled`] 的差别：入口**不带** `HeldGate`（网络阶段
    /// 不持闸）；worker 内部通过 [`SyncRunner::attach_control`] 拿到的通道等待授权。
    /// 默认实现显式失败——不支持两阶段的 runner 不能被当成"跑完了"。
    fn run_two_phase(
        &mut self,
        op_id: OpId,
        control: &OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        let _ = (op_id, control, events);
        Err("两阶段同步未实现：runner 不支持可暂停的阶段握手".to_owned())
    }
}

/// 生产默认 runner：真实同步在切片 4，这里对一切操作命令明确返回"未实现"。
///
/// 为什么还要留一个会失败的默认值：ADR 0009「安全暂停优于看起来成功」——面板必须能
/// 显示"还没实现"，而不是转个圈假装同步成功。真实执行体见 [`DevLocalRunner`]。
#[derive(Debug, Default)]
pub struct UnimplementedRunner;

impl SyncRunner for UnimplementedRunner {
    fn run(&mut self, op: Command, events: &mut dyn FnMut(Event)) -> Result<OpOutcome, String> {
        if op.is_control() {
            // 控制命令没有自己的产物（Stop 的终态是 `Stopped`，Cancel 不改结果）。
            return Ok(OpOutcome::new("控制命令无同步动作"));
        }
        let label = op.kind().label();
        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Preparing,
            detail: format!("{label}：等待切片 4 接入真实同步"),
        });
        Err(format!(
            "切片 4 未实现：{label}（本切片只交付状态机、面板与传输超时）"
        ))
    }
}

/// 生产 runner（ADR 0009 切片 4c-2）：在 worker 线程里跑真实的
/// [`DevLocal::sync_once`]（`encrypt_pending → push → pull → integrate → ack`）。
///
/// ## 为什么在 worker 里再开一个 `Vault`
///
/// UI 的 `Vault` 属于 GPUI 线程；worker 必须有自己的连接（SQLite 连接不跨线程共享）。
/// 两个连接指向同一个库文件，`synchronous=FULL` + 5s `busy_timeout` 由存储层负责。
/// 身份同理：`DevIdentityStore::load` 会在**第一次** SyncOnce 时于 worker 线程内读取
/// （DPAPI / Linux 用户主键），之后缓存；绝不把身份交给 UI 或线程外。
///
/// ## 持闸纪律
///
/// 只执行**带 [`HeldGate`]** 的 `SyncOnce` / `PublishInitialPair`：缺证明 → 明确失败；
/// 库中 head 与 `durable_head_seq` 不一致 → 明确失败（说明 UI 并没有真的把闸门持住）。
/// 没有活动文档时走 [`SyncRunner::run_gated`] 的 `SyncOnceNoSession` 分支（不持闸，
/// 调用方契约见 [`DevLocalRunner::sync_once_no_session`]）——**发布基线没有无会话路径**：
/// 它要求有活动文档先持闸 flush（见 [`DevLocalRunner::publish_initial_pair`]）。
///
/// 另外两个命令不需要闸门（都不 integrate，也不改任何本地同步状态）：
///
/// - `ListDevices`：只读设备列表（ADR 0010 切片 2a-2）；
/// - `RevokeDevice`：管理端移除一台设备的访问（**不轮换密钥**），成功后带上刷新后的列表。
///
/// 发布基线（切片 4c-4）：worker 复核 head 后调
/// [`DevLocal::publish_initial_pair_with_identity`]——**不需要恢复码**（信任锚是受保护身份里
/// 的管理私钥；Join 设备没有 → 明确拒绝），初始 recovery 槽由 Create 上传
/// （`K_slot_mne` 只能由恢复码派生，事后无法补传）。Create/Join 仍返回"未实现"，走 CLI
/// （`--dev-sync-create` / `--dev-sync-join`，见 `dev_sync::setup`）。
pub struct DevLocalRunner {
    relay: String,
    vault_path: PathBuf,
    /// 传输预算（ADR 0012 S1：由 `ServiceConfig` 传入，不在 `DevLocal` 内部硬编）。
    transport: TransportTimeouts,
    /// 懒加载的身份（只在 worker 线程里读写）。
    identity: Option<LocalIdentityV1>,
    /// 两阶段同步的独立控制通道（worker 启动时注入；`None` = 只能跑旧整轮路径）。
    control: Option<Receiver<SyncControl>>,
}

impl DevLocalRunner {
    /// 构造（不触碰磁盘/网络：所有 IO 都发生在 worker 线程的 `run_gated` 里）。
    ///
    /// 传输预算用历史默认 [`DEFAULT_TRANSPORT_TIMEOUTS`]（手动同步现状）。
    #[must_use]
    pub fn new(relay: impl Into<String>, vault_path: PathBuf) -> Self {
        Self::new_with_transport(relay, vault_path, DEFAULT_TRANSPORT_TIMEOUTS)
    }

    /// 构造并显式指定传输预算（`ServiceConfig::transport` 透传到这里）。
    #[must_use]
    pub fn new_with_transport(
        relay: impl Into<String>,
        vault_path: PathBuf,
        transport: TransportTimeouts,
    ) -> Self {
        Self {
            relay: relay.into(),
            vault_path,
            transport,
            identity: None,
            control: None,
        }
    }

    /// 当前生效的传输预算（配置透传的单测/诊断用）。
    #[must_use]
    pub fn transport_timeouts(&self) -> TransportTimeouts {
        self.transport
    }

    /// 读回持久身份（首次从 `dev-identity.bin` 解封，之后复用 worker 内的缓存）。
    fn load_identity(&mut self) -> Result<&LocalIdentityV1, String> {
        if self.identity.is_none() {
            let identity = DevIdentityStore.load(&self.vault_path).map_err(|error| {
                format!(
                    "dev-sync 读取持久身份失败（{}）：{error:#}",
                    DevIdentityStore::path_for(&self.vault_path).display()
                )
            })?;
            self.identity = Some(identity);
        }
        self.identity.as_ref().ok_or_else(|| {
            // `identity.is_none()` 分支刚返回错误；走到这里说明逻辑出错（不 panic）。
            "dev-sync 身份缓存异常（内部错误）".to_owned()
        })
    }
}

impl SyncRunner for DevLocalRunner {
    /// 不带持闸证明的入口：设备列表/移除不需要闸门（只读 + 管理端请求，不 integrate），
    /// 其余操作命令一律被拒绝（`SyncOnce` 不能盲跑）。
    fn run(&mut self, op: Command, events: &mut dyn FnMut(Event)) -> Result<OpOutcome, String> {
        self.run_gated(op, None, events)
    }

    fn run_gated(
        &mut self,
        op: Command,
        gate: Option<&HeldGate>,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        // 没有控制通道（CLI/测试直接调用）：语义与 ADR 0012 S1 之前完全一致。
        let control = OpControl::default();
        self.run_gated_controlled(op, gate, &control, events)
    }

    fn run_gated_controlled(
        &mut self,
        op: Command,
        gate: Option<&HeldGate>,
        control: &OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        match op {
            Command::SyncOnce => self.sync_once(gate, control, events),
            // 两阶段同步必须走 `run_two_phase`（独立控制通道）；旧整轮入口一律拒绝，
            // 防止有人把"网络阶段不持闸"的操作塞进会整轮持闸的旧路径。
            Command::SyncTwoPhase => Err(
                "两阶段同步必须走 run_two_phase/控制通道：旧整轮持闸入口不接受该命令".to_owned(),
            ),
            // 发布初始基线（切片 4c-4）：与 SyncOnce 一样**必须**带持闸证明。
            Command::PublishInitialPair => self.publish_initial_pair(gate, control, events),
            // 无会话同步（空库/没有打开的笔记）：没有活动文档就没有闸门目标。
            // 与 `SyncOnce` 严格互斥——带持闸证明的调用一律拒绝，防止两条路径混用。
            Command::SyncOnceNoSession => {
                if gate.is_some() {
                    return Err(
                        "无会话同步不接受持闸证明（HeldGate）：没有活动文档就没有闸门目标；\
                         有活动文档请走普通「同步」（SyncOnce 必须先持闸）"
                            .to_owned(),
                    );
                }
                self.sync_once_no_session(control, events)
            }
            Command::ListDevices => self.list_devices(events),
            Command::RevokeDevice { target } => self.revoke_device(target, events),
            control if control.is_control() => Ok(OpOutcome::new("控制命令无同步动作")),
            other => {
                let label = other.kind().label();
                events(Event::Progress {
                    op_id: OpId::UNASSIGNED,
                    phase: Phase::Preparing,
                    detail: format!("{label}：等待 CLI/后续切片"),
                });
                Err(format!(
                    "{label}：dev-sync worker 目前只执行 SyncOnce / 无会话同步 / 发布基线 / \
                     刷新设备 / 移除设备；Create/Join 仍走 CLI（--dev-sync-create / --dev-sync-join）"
                ))
            }
        }
    }

    fn attach_control(&mut self, control: Receiver<SyncControl>) {
        self.control = Some(control);
    }

    fn run_two_phase(
        &mut self,
        op_id: OpId,
        control: &OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        self.sync_two_phase(op_id, control, events)
    }
}

impl DevLocalRunner {
    /// worker 自己的 `Vault` 连接（SQLite 连接不跨线程共享，UI 的句柄不能被 worker 用）。
    fn open_vault(&self) -> Result<Vault, String> {
        Vault::open(&self.vault_path)
            .map_err(|error| format!("打开本地库失败（{}）：{error}", self.vault_path.display()))
    }

    /// 设备列表（ADR 0010 切片 2a-2）：**只读**快照，不发任何管理端/写请求。
    ///
    /// 顺带把"哪台是本机""本机有没有管理密钥"两个布尔/公开事实报给面板——UI 线程
    /// 不读身份文件（ADR 0009），所以这两件事只能由 worker 上报。
    fn list_devices(&mut self, events: &mut dyn FnMut(Event)) -> Result<OpOutcome, String> {
        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Preparing,
            detail: "读取持久身份并打开本地库".to_owned(),
        });
        let vault = self.open_vault()?;
        let relay = self.relay.clone();
        let identity = self.load_identity()?;

        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Transport,
            detail: format!("读取设备列表（{relay}）"),
        });
        let members = DevLocal::members(&relay, &vault, identity)
            .map_err(|error| format!("读取设备列表失败：{error}"))?;
        let count = members.len();
        let self_device = identity.device_id;
        let can_manage = identity.manager_seed.is_some();
        events(Event::DevicesListed {
            members,
            self_device,
            can_manage,
        });
        Ok(OpOutcome::new(format!("设备列表已刷新（{count} 台）")))
    }

    /// 移除一台设备对 relay 的访问（ADR 0010：**不轮换密钥**、不删本地身份/outbox）。
    ///
    /// 三条守卫都在这里再兜一遍（UI 会禁用按钮，但命令可能来自任何地方）：
    /// 本机禁止移除；缺管理密钥明确失败；其余交给 `DevLocal::revoke_device`。
    /// 成功后立刻带上 relay 返回的**刷新后**成员列表，面板不需要再发一条列表命令。
    fn revoke_device(
        &mut self,
        target: DeviceId,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Preparing,
            detail: "读取持久身份并打开本地库".to_owned(),
        });
        let vault = self.open_vault()?;
        let relay = self.relay.clone();
        let identity = self.load_identity()?;
        if target == identity.device_id {
            return Err("移除设备被拒绝：不能移除本机设备（会立即自断同步与管理访问）".to_owned());
        }
        if identity.manager_seed.is_none() {
            return Err(
                "移除设备被拒绝：本机没有管理密钥（Join 设备）；请用 Create 设备或最新恢复码"
                    .to_owned(),
            );
        }
        let can_manage = identity.manager_seed.is_some();
        let self_device = identity.device_id;
        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Transport,
            detail: format!("发送管理端移除请求（目标 {}）", short_device_id(&target)),
        });
        let revoked = DevLocal::revoke_device(&relay, &vault, identity, target)
            .map_err(|error| format!("移除设备失败：{error}"))?;
        let summary = if revoked.outcome.changed {
            format!(
                "已移除 {}（仅断访问；auth_rev {}）",
                short_device_id(&revoked.outcome.target),
                revoked.outcome.auth_rev
            )
        } else {
            format!(
                "{} 此前已是 revoked（{}）",
                short_device_id(&revoked.outcome.target),
                REVOKE_UNCHANGED_MARKER
            )
        };
        events(Event::DeviceRevoked {
            target: revoked.outcome.target,
            result: OpResult::Ok {
                summary: summary.clone(),
            },
        });
        events(Event::DevicesListed {
            members: revoked.members,
            self_device,
            can_manage,
        });
        Ok(OpOutcome::new(summary))
    }

    /// 一轮真实同步（持闸 -> 复核 durable 基准 -> `DevLocal::sync_once_with`）。
    ///
    /// ## 取消/授权边界（ADR 0012 S1）
    ///
    /// worker 在**首次外部事务边界（integrate 前）**再次复核：
    ///
    /// 1. 取消通道已置位（用户 `Cancel` 或 operation deadline）→ 返回
    ///    `CancelledBeforeIntegrate`，**不 append**；
    /// 2. 库里持闸文档的 head 仍等于派发时的 `durable_head_seq` → 否则授权已失效，
    ///    同样不 append（fail-closed）。
    ///
    /// 一旦进入 `integrate`，本轮事务必须跑完并交付已 durable 的变化——取消不能吞掉
    /// 已经提交的本地行（S2 才做可暂停握手）。
    fn sync_once(
        &mut self,
        gate: Option<&HeldGate>,
        control: &OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        let Some(gate) = gate else {
            return Err(
                "同步一次必须带持闸证明（HeldGate）：UI 未持闸时 worker 拒绝 integrate".to_owned(),
            );
        };
        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Preparing,
            detail: format!(
                "持闸已确认（{}，基准 head {}），打开本地库",
                gate.doc_id, gate.durable_head_seq
            ),
        });

        // 1) worker 自己的连接（不在 UI 线程，也不复用 UI 的 Vault）。
        let vault = self.open_vault()?;

        // 2) 持闸复核：库中 head 必须就是持闸时的 durable 基准。
        //    不相等说明 UI 侧闸门并没有真的把本地写入冻住（或有别的 writer），
        //    此时 integrate 会制造"库里有、live 没有"的撕裂，必须拒绝。
        let head = load_head(&vault, &gate.doc_id)
            .map_err(|error| format!("读取持闸文档 head 失败（{}）：{error}", gate.doc_id))?;
        if head.head_seq != gate.durable_head_seq {
            return Err(format!(
                "持闸基准与库中 head 不一致（库中 {}，持闸时 {}）：拒绝在未持闸状态下 integrate",
                head.head_seq, gate.durable_head_seq
            ));
        }

        // 3) 身份（worker 线程内解封/缓存；失败不触发任何网络请求）。
        let relay = self.relay.clone();
        let transport = self.transport;
        let identity = self.load_identity()?;

        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Transport,
            detail: format!("连接 relay 并完整同步一轮（{relay}）"),
        });
        println!(
            "[dev-sync] sync_once 开始：relay={relay} doc={} base_head={} \
             transport={:?}/{:?}/{:?}",
            gate.doc_id,
            gate.durable_head_seq,
            transport.connect,
            transport.io,
            transport.request_deadline
        );
        // integrate 前的最后一道门：取消赢 或 授权基准被移动 → 一律不 append。
        let mut before_integrate = || {
            if control.is_cancelled() {
                return Err(SyncError::CancelledBeforeIntegrate);
            }
            let head = load_head(&vault, &gate.doc_id).map_err(SyncError::Storage)?;
            if head.head_seq != gate.durable_head_seq {
                return Err(SyncError::CancelledBeforeIntegrate);
            }
            Ok(())
        };
        let options = menghunote_sync::dev_local::SyncOnceOptions {
            transport: Some(transport),
            before_integrate: Some(&mut before_integrate),
        };
        let report =
            DevLocal::sync_once_with(&relay, &vault, identity, options).map_err(|error| {
                eprintln!("[dev-sync] sync_once 失败：{error}");
                format!("同步失败：{error}")
            })?;
        println!(
            "[dev-sync] sync_once 完成：encrypted={} pushed={} pulled={} integrated={} deferred={}",
            report.encrypted,
            report.push.acked,
            report.pull.stored,
            report.integrate.integrated,
            report.integrate.deferred
        );

        let summary = format!(
            "推送 {} / 拉取 {} / 整合 {} / 延后 {}",
            report.push.acked,
            report.pull.stored,
            report.integrate.integrated,
            report.integrate.deferred
        );
        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Durability,
            detail: format!("同步完成（{summary}），等待 UI 交付已落盘增量"),
        });
        Ok(OpOutcome::new(summary))
    }

    /// 无会话同步（**没有活动文档**：空库 / 还没打开笔记）：不持闸，直接跑一轮。
    ///
    /// ## 调用方契约（worker 无法从库里反查 UI 状态，必须由宿主保证）
    ///
    /// 1. **只在没有活动文档时派发**：`AppShell.active.is_none()`。没有编辑器会话 =
    ///    没有 `DocumentSession` 可以 begin/adopt，所以本命令既不收持闸证明、也不做
    ///    integrate 前的 head 复核；worker 打开的是**自己的**连接，本来就没有、也不
    ///    接受任何 UI 会话可传入；
    /// 2. **同步期间冻结结构写入**：宿主在同步在途时拒绝新建/重命名/删除/移动
    ///    （`AppShell::write_structure`），否则本地结构提交会与 integrate 争同一份
    ///    `head_seq`；
    /// 3. **收尾由宿主做**：结构 head 变了要重投影 + 刷新侧栏，树里出现笔记就自动打开
    ///    第一篇（worker 不碰 UI，也不做假刷新）。
    ///
    /// worker 侧能自己确认的只有：库能打开、结构文档存在（integrate 的结构更新与宿主
    /// 收尾的重投影都要落在它上面）、命令**没有**带持闸证明（带了就是两条路径混用，
    /// [`Self::run_gated`] 已经提前拒绝）。
    ///
    /// 失败语义：明确失败、**不自动重试**（与 `sync_once` 一致；错误文案不含 secret）。
    fn sync_once_no_session(
        &mut self,
        control: &OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Preparing,
            detail: "无会话同步（没有活动文档，无需持闸）：打开本地库".to_owned(),
        });
        // 1) worker 自己的连接：没有 UI 会话可传入，也不接受任何 DocumentSession。
        let vault = self.open_vault()?;
        // 2) 结构文档必须已经在库里：本轮 integrate 的结构更新与宿主收尾的重投影都要
        //    落在它上面；缺失说明库不完整 → 明确失败（不凭空补行，避免掩盖损坏）。
        load_head(&vault, STRUCTURE_DOC_ID)
            .map_err(|error| format!("无会话同步被拒绝：本地库没有可用的结构文档（{error}）"))?;

        // 3) 身份（worker 线程内解封/缓存；失败不触发任何网络请求）。
        let relay = self.relay.clone();
        let transport = self.transport;
        let identity = self.load_identity()?;

        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Transport,
            detail: format!("连接 relay 并完整同步一轮（无会话，{relay}）"),
        });
        println!("[dev-sync] 无会话 sync_once 开始：relay={relay}");
        // 没有闸门就没有 head 复核；但**取消**同样必须在首次外部集成前生效。
        let mut before_integrate = || {
            if control.is_cancelled() {
                Err(SyncError::CancelledBeforeIntegrate)
            } else {
                Ok(())
            }
        };
        let options = menghunote_sync::dev_local::SyncOnceOptions {
            transport: Some(transport),
            before_integrate: Some(&mut before_integrate),
        };
        let report =
            DevLocal::sync_once_with(&relay, &vault, identity, options).map_err(|error| {
                eprintln!("[dev-sync] 无会话 sync_once 失败：{error}");
                format!("同步失败：{error}")
            })?;
        println!(
            "[dev-sync] 无会话 sync_once 完成：encrypted={} pushed={} pulled={} integrated={} deferred={}",
            report.encrypted,
            report.push.acked,
            report.pull.stored,
            report.integrate.integrated,
            report.integrate.deferred
        );

        let summary = format!(
            "推送 {} / 拉取 {} / 整合 {} / 延后 {}",
            report.push.acked,
            report.pull.stored,
            report.integrate.integrated,
            report.integrate.deferred
        );
        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Durability,
            detail: format!("无会话同步完成（{summary}），由宿主重投影结构树"),
        });
        Ok(OpOutcome::new(summary))
    }

    /// 发布初始基线（ADR 0009 切片 4c-4）：与同步**同一条持闸纪律**。
    ///
    /// 顺序（与 [`Self::sync_once`] 逐条对应）：
    /// 1. 缺 `HeldGate` → 明确失败（发布内部会跑一轮同步，integrate 绕过本地会话）；
    /// 2. worker 自己的 `Vault` 连接 + 复核「库中 head == 持闸时的 durable head」；
    /// 3. 解封持久身份 → **先拒 Join 设备**（没有管理私钥）；
    /// 4. `DevLocal::publish_initial_pair_with_identity`：不接收恢复码，初始槽由 Create
    ///    上传、这里 fail-closed 校验（旧实例明确失败并指向 CLI）；
    /// 5. `OpOutcome` 只带公开事实：checkpoint 短 id / 覆盖 seq（notice 用）。
    ///
    /// 失败语义：明确失败、**不自动重试**（与 `sync_once` 一致）；恢复码/管理私钥绝不进
    /// 日志或摘要。
    fn publish_initial_pair(
        &mut self,
        gate: Option<&HeldGate>,
        control: &OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        let Some(gate) = gate else {
            return Err(
                "发布初始基线必须带持闸证明（HeldGate）：UI 未持闸时 worker 拒绝发布".to_owned(),
            );
        };
        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Preparing,
            detail: format!(
                "持闸已确认（{}，基准 head {}），打开本地库",
                gate.doc_id, gate.durable_head_seq
            ),
        });

        // 1) worker 自己的连接（SQLite 连接不跨线程共享）。
        let vault = self.open_vault()?;

        // 2) 持闸复核：发布读取的是 durable 快照，基准必须就是持闸时的 head。
        let head = load_head(&vault, &gate.doc_id)
            .map_err(|error| format!("读取持闸文档 head 失败（{}）：{error}", gate.doc_id))?;
        if head.head_seq != gate.durable_head_seq {
            return Err(format!(
                "持闸基准与库中 head 不一致（库中 {}，持闸时 {}）：拒绝在未持闸状态下发布基线",
                head.head_seq, gate.durable_head_seq
            ));
        }

        // 3) 身份（worker 线程内解封/缓存；失败不触发任何网络请求）。
        let relay = self.relay.clone();
        let transport = self.transport;
        let identity = self.load_identity()?;
        // UI 会禁用按钮；命令可能来自任何地方，这里再兜一次（权威判定在身份里）。
        if identity.manager_seed.is_none() {
            return Err(
                "发布初始基线被拒绝：本机没有管理密钥（Join 设备）；请用 Create 设备发布"
                    .to_owned(),
            );
        }

        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Transport,
            detail: format!("连接 relay 并发布初始基线（{relay}）"),
        });
        println!(
            "[dev-sync] publish_initial_pair 开始：relay={relay} doc={} base_head={}",
            gate.doc_id, gate.durable_head_seq
        );
        // 发布内部也会跑一轮同步并 integrate：与同步共用同一条集成前复核。
        let mut before_integrate = || {
            if control.is_cancelled() {
                return Err(SyncError::CancelledBeforeIntegrate);
            }
            let head = load_head(&vault, &gate.doc_id).map_err(SyncError::Storage)?;
            if head.head_seq != gate.durable_head_seq {
                return Err(SyncError::CancelledBeforeIntegrate);
            }
            Ok(())
        };
        let options = menghunote_sync::dev_local::SyncOnceOptions {
            transport: Some(transport),
            before_integrate: Some(&mut before_integrate),
        };
        let report =
            DevLocal::publish_initial_pair_with_identity_with(&relay, &vault, identity, options)
                .map_err(|error| {
                    eprintln!("[dev-sync] publish_initial_pair 失败：{error}");
                    format!("发布初始基线失败：{error}")
                })?;
        println!(
            "[dev-sync] publish_initial_pair 完成：checkpoint={} epoch={} covered_seq={} docs={}",
            short_checkpoint_id(&report.checkpoint_id),
            report.epoch,
            report.covered_seq,
            report.docs.len()
        );

        // 只带公开事实（checkpoint 短 id / 覆盖 seq）；恢复码、root、管理私钥一律不出现。
        let summary = format!(
            "初始基线已发布（checkpoint {}，覆盖 seq {}）；B 现在可以 Join",
            short_checkpoint_id(&report.checkpoint_id),
            report.covered_seq
        );
        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Durability,
            detail: format!("{summary}，等待 UI 交付已落盘增量并释放闸门"),
        });
        Ok(OpOutcome::new(summary))
    }

    /// ADR 0012 S2a：可暂停的两阶段同步（由 worker 的 `run_two_phase` 调用）。
    ///
    /// ## 顺序（硬约束）
    ///
    /// 1. **Prepare**：新 `Vault` + 新 engine，只跑 `encrypt_pending → push → pull`；
    ///    发 [`Event::NeedGate`] 后**阻塞等授权**（独立控制通道，不受 job 队列阻塞）。
    /// 2. **Integrate**：收到 [`SyncControl::GrantGate`] 后复核 op/授权代际/持闸基准；
    ///    取消已登记或 head 被移动 → **不 append**；通过后新 `Vault` + 新 engine 只跑
    ///    `integrate`；发 [`Event::Integrated`] 后阻塞等 UI 的交付完成回执。
    /// 3. **Finish**：收到 [`SyncControl::Finish`] 后新 `Vault` + 新 engine，push tail +
    ///    ACK；返回成功（`changed_heads` 来自 Integrate 阶段）。
    ///
    /// 每个阶段都重新构造 client/engine：真正的事实只在 DB（outbox/inbox/cursor/pins），
    /// 进程内 engine 从不跨阶段存活。
    fn sync_two_phase(
        &mut self,
        op_id: OpId,
        control: &OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        let relay = self.relay.clone();
        let transport = self.transport;

        // ---------- 1) Prepare：网络阶段，不持闸 ----------
        events(Event::Progress {
            op_id,
            phase: Phase::Preparing,
            detail: "两阶段同步：网络阶段（不持闸，可继续编辑/切换）".to_owned(),
        });
        let (prepare, ready) = {
            let vault = self.open_vault()?;
            let identity = self.load_identity()?;
            let prepare = DevLocal::sync_prepare(&relay, &vault, identity, Some(transport))
                .map_err(|error| format!("同步失败（Prepare）：{error}"))?;
            // "已接收·待应用"：verified 且落在 durable 连续前缀内、尚未集成的行
            // （ADR 0012：收到 ≠ 已应用，面板必须能显示这个数）。
            let ready = menghunote_storage::count_inbox_ready(&vault).unwrap_or(0);
            (prepare, usize::try_from(ready).unwrap_or(usize::MAX))
        };
        let authorization_epoch = u64::from_be_bytes(
            menghunote_sync::random::array::<8>(&mut menghunote_sync::random::OsRandom)
                .map_err(|error| format!("生成授权代际失败：{error}"))?,
        );
        let prepare_summary = format!(
            "推送 {} / 拉取 {}（已落盘待应用 {ready}，等待持闸授权）",
            prepare.push.acked, prepare.pull.stored
        );
        events(Event::Progress {
            op_id,
            phase: Phase::Pulling,
            detail: prepare_summary.clone(),
        });
        events(Event::NeedGate {
            op_id,
            authorization_epoch,
            stored: prepare.pull.stored,
            deferred: ready,
            summary: prepare_summary,
        });

        // 等授权：取消/停止由独立控制通道穿透（job 队列在这里读不到，也不该读）。
        let message = self.wait_for_grant(op_id, control)?;
        let gate = match message {
            SyncControl::GrantGate {
                authorization_epoch: granted_epoch,
                gate,
                ..
            } if granted_epoch == authorization_epoch => gate,
            SyncControl::GrantGate { .. } => {
                return Err(
                    "集成被拒绝：GrantGate 的授权代际与本次 Prepare 不一致（旧授权）".to_owned(),
                );
            }
            other => {
                return Err(format!(
                    "集成被拒绝：等待授权的通道收到了非授权消息 {other:?}"
                ));
            }
        };

        // ---------- 2) Integrate：复核取消 + 持闸基准后追加 ----------
        // 进入外部事务边界前最后一次复核取消：取消赢 → 不 append（inbox/outbox 保留）。
        if control.is_cancelled() {
            return Err(
                "同步已取消：在外部集成前停止（未追加任何本地行；已拉取的 durable inbox 保留）"
                    .to_owned(),
            );
        }
        events(Event::Progress {
            op_id,
            phase: Phase::Integrating,
            detail: format!(
                "已授权：持闸文档 {} 基准 head {}（代际 {}），开始整合",
                gate.doc_id, gate.durable_head_seq, gate.generation
            ),
        });
        let integrated = {
            let vault = self.open_vault()?;
            let identity = self.load_identity()?;
            DevLocal::sync_integrate(
                &relay,
                &vault,
                identity,
                Some(transport),
                Some((gate.doc_id.as_str(), gate.durable_head_seq)),
                &gate.doc_id,
            )
            .map_err(|error| match error {
                SyncError::CancelledBeforeIntegrate => format!(
                    "集成被拒绝：持闸基准与库中 head 不一致（授权已失效/会话纪元过期），未追加任何本地行（库中 head {}，持闸时 {}）",
                    load_head(&vault, &gate.doc_id)
                        .map(|head| head.head_seq)
                        .unwrap_or(u64::MAX),
                    gate.durable_head_seq
                ),
                other => format!("同步失败（Integrate）：{other}"),
            })?
        };
        let projection_note = if integrated.integrate.projected {
            "，结构已投影"
        } else {
            ""
        };
        let materialize_note = if integrated.materialize_failed > 0 {
            format!("，正文物化失败 {}", integrated.materialize_failed)
        } else {
            String::new()
        };
        let integrate_summary = format!(
            "整合 {} / 延后 {} / 批次 {}{projection_note}{materialize_note}",
            integrated.integrate.integrated,
            integrated.integrate.deferred,
            integrated.integrate.batches
        );
        events(Event::Progress {
            op_id,
            phase: Phase::Integrating,
            detail: integrate_summary.clone(),
        });
        events(Event::Integrated {
            op_id,
            changed_heads: integrated.changed_heads.clone(),
            integrated: integrated.integrate.integrated,
            deferred: integrated.integrate.deferred,
            summary: integrate_summary,
        });

        // 等 UI `adopt` + `end` 之后的交付完成回执：只有 UI 能交付 live。
        // 取消/停止在这里**不会**中断已完成的事务：返回明确错误（不 ack），UI 仍会交付
        // 已 durable 的变化（AppShell 的终态路径先 adopt/end 再报告）。
        self.wait_for_finish(op_id, control)?;

        // ---------- 3) Finish：push tail + ACK ----------
        events(Event::Progress {
            op_id,
            phase: Phase::Durability,
            detail: "交付已确认：推送 tail 并上报 ACK".to_owned(),
        });
        let finish = {
            let vault = self.open_vault()?;
            let identity = self.load_identity()?;
            DevLocal::sync_finish(&relay, &vault, identity, Some(transport))
                .map_err(|error| format!("同步失败（Finish）：{error}"))?
        };
        let summary = format!(
            "推送 {} / 拉取 {} / 整合 {} / 延后 {} / tail {} / ACK received={} integrated={}{projection_note}",
            prepare.push.acked,
            prepare.pull.stored,
            integrated.integrate.integrated,
            integrated.integrate.deferred,
            finish.push.acked,
            finish.ack.received_seq,
            finish.ack.integrated_seq
        );
        Ok(OpOutcome::new(summary).with_heads(integrated.changed_heads))
    }

    /// 等 `GrantGate`（两阶段 Prepare → Integrate 的握手）。
    ///
    /// 语义：
    /// - 取消标志已置位 → 立即返回错误（**不** consume 后续授权；worker 随后结束本次 op）；
    /// - `Cancel` / `Stop` 消息 → 明确错误（停止优先于授权）；
    /// - 其它 op 的迟到消息（上一轮残留）→ **丢弃并继续等**（不能毒化新一轮）；
    /// - 通道断开（worker 正在退出）→ 明确错误。
    fn wait_for_grant(&mut self, op_id: OpId, control: &OpControl) -> Result<SyncControl, String> {
        loop {
            if control.is_cancelled() {
                return Err(
                    "同步已取消：在外部集成前停止（未追加任何本地行；已拉取的 durable inbox 保留）"
                        .to_owned(),
                );
            }
            let Some(receiver) = self.control.as_ref() else {
                return Err("两阶段同步不可用：worker 没有接管控制通道".to_owned());
            };
            let message = receiver
                .recv()
                .map_err(|_| "控制通道已关闭（worker 正在停止）".to_owned())?;
            match message {
                SyncControl::GrantGate { op_id: id, .. } if id == op_id => return Ok(message),
                SyncControl::Finish { op_id: id } if id == op_id => {
                    return Err("控制消息顺序错误：尚未授权就收到 Finish".to_owned());
                }
                SyncControl::Cancel { op_id: id } if id == op_id => {
                    return Err(
                        "同步已取消：在外部集成前停止（未追加任何本地行；已拉取的 durable inbox 保留）"
                            .to_owned(),
                    );
                }
                SyncControl::Stop => {
                    return Err("worker 已停止：等待持闸授权时收到 Stop".to_owned());
                }
                // 上一轮残留/别家 op 的迟到消息：丢弃，不能毒化当前 op。
                other => {
                    eprintln!("[dev-sync] 忽略非本次 op 的控制消息（当前 {op_id}）：{other:?}");
                }
            }
        }
    }

    /// 等 UI 的交付完成回执（两阶段 Integrate → Finish 的握手）。
    ///
    /// 取消/停止在这里不会回滚已提交的事务：返回明确错误且**不 ack**（ACK 只是游标上报，
    /// 下一轮会补），已 durable 的变化仍由 UI 的终态路径 adopt/end 交付。
    fn wait_for_finish(&mut self, op_id: OpId, control: &OpControl) -> Result<(), String> {
        loop {
            if control.is_cancelled() {
                return Err(
                    "集成已提交，但交付前收到取消：本次未 ACK（durable 变化保留，由 UI 交付）"
                        .to_owned(),
                );
            }
            let Some(receiver) = self.control.as_ref() else {
                return Err("两阶段同步不可用：worker 没有接管控制通道".to_owned());
            };
            let message = receiver
                .recv()
                .map_err(|_| "控制通道已关闭（worker 正在停止）".to_owned())?;
            match message {
                SyncControl::Finish { op_id: id } if id == op_id => return Ok(()),
                SyncControl::Cancel { op_id: id } if id == op_id => {
                    return Err(
                        "集成已提交，但交付前收到取消：本次未 ACK（durable 变化保留，由 UI 交付）"
                            .to_owned(),
                    );
                }
                SyncControl::Stop => {
                    return Err("worker 已停止：等待交付完成回执时收到 Stop".to_owned());
                }
                SyncControl::GrantGate { op_id: id, .. } if id == op_id => {
                    return Err("控制消息顺序错误：集成完成后又收到 GrantGate".to_owned());
                }
                other => {
                    eprintln!("[dev-sync] 忽略非本次 op 的控制消息（当前 {op_id}）：{other:?}");
                }
            }
        }
    }
}

/// checkpoint id 的短展示形式（前 8 位小写 hex；checkpoint id 是公开事实）。
fn short_checkpoint_id(checkpoint_id: &[u8; 16]) -> String {
    menghunote_sync::ids::hex_of(checkpoint_id)
        .chars()
        .take(8)
        .collect()
}

/// 一条待执行的命令（op_id 由状态机分配，worker 只负责执行与回执）。
#[derive(Debug, Clone)]
struct Job {
    op_id: OpId,
    command: Command,
    /// 持闸证明（只有 `try_command_with_gate` 会带；控制命令永远是 `None`）。
    gate: Option<HeldGate>,
    /// 取消控制通道（非控制命令才有一条；控制命令为默认值）。
    control: OpControl,
}

/// 服务内部状态（一把锁保护，避免"状态与事件流不同步"）。
struct Inner {
    state: SyncState,
    events: Receiver<Event>,
    /// 已放弃但 worker 可能还在跑的 op（超时后置位；真实终态回来后清掉）。
    abandoned: Option<OpId>,
    /// 在途 op 的 deadline 起点。
    deadline: Option<(OpId, Instant)>,
    /// 在途 op 的取消通道（`Cancel`/deadline 置位；worker 在 integrate 前复核）。
    control: Option<(OpId, OpControl)>,
}

/// dev-only 同步服务：串行 worker + 非阻塞事件泵 + 关闭票据。
pub struct SyncService {
    config: ServiceConfig,
    /// 操作 deadline 时钟（测试可注入手动时钟）。
    clock: Arc<dyn OperationClock>,
    inner: Mutex<Inner>,
    jobs: Mutex<Option<Sender<Job>>>,
    /// 两阶段同步的独立控制通道（`GrantGate` / `Finish` / 取消 / 停止）。
    control: Mutex<Option<Sender<SyncControl>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl SyncService {
    /// 生产构造（默认 [`UnimplementedRunner`]）。返回 `Arc` 方便面板与关闭路径共享。
    #[must_use]
    pub fn new(config: ServiceConfig) -> Arc<Self> {
        Self::with_runner(config, Box::new(UnimplementedRunner))
    }

    /// 生产构造（ADR 0009 切片 4c-2）：[`DevLocalRunner`] 真实执行 `SyncOnce`。
    ///
    /// `vault_path` 是本实例自己的库文件：worker 线程会为每次同步打开自己的 `Vault`
    /// 连接，并在第一次同步时从 `dev-identity.bin` 解封持久身份。构造本身**不碰**磁盘。
    #[must_use]
    pub fn new_dev_local(config: ServiceConfig, vault_path: PathBuf) -> Arc<Self> {
        // ADR 0012 S1：传输预算真的透传给 `DevLocal`（不再由 `DevLocal` 内部硬编默认）。
        let runner =
            DevLocalRunner::new_with_transport(config.relay.clone(), vault_path, config.transport);
        Self::with_runner(config, Box::new(runner))
    }

    /// 注入执行体（测试/切片 4 用；deadline 用系统单调时钟）。
    #[must_use]
    pub fn with_runner(config: ServiceConfig, runner: Box<dyn SyncRunner>) -> Arc<Self> {
        Self::with_runner_and_clock(config, runner, Arc::new(SystemOperationClock))
    }

    /// 注入执行体 + 操作 deadline 时钟（ADR 0012 S2a 的确定性超时用例）。
    #[must_use]
    pub fn with_runner_and_clock(
        config: ServiceConfig,
        runner: Box<dyn SyncRunner>,
        clock: Arc<dyn OperationClock>,
    ) -> Arc<Self> {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (event_tx, event_rx) = mpsc::channel::<Event>();
        let (control_tx, control_rx) = mpsc::channel::<SyncControl>();
        let handle = thread::Builder::new()
            .name("menghunote-dev-sync".to_owned())
            .spawn(move || worker_loop(&job_rx, &event_tx, runner, control_rx))
            .expect("启动 dev-sync worker 线程失败（系统资源不足？）");
        Arc::new(Self {
            config,
            clock,
            inner: Mutex::new(Inner {
                state: SyncState::new(),
                events: event_rx,
                abandoned: None,
                deadline: None,
                control: None,
            }),
            jobs: Mutex::new(Some(job_tx)),
            control: Mutex::new(Some(control_tx)),
            worker: Mutex::new(Some(handle)),
        })
    }

    /// 当前配置（面板显示 relay 用）。
    #[must_use]
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    /// 只读状态快照（面板渲染用；不触碰通道）。
    #[must_use]
    pub fn snapshot(&self) -> StateSnapshot {
        self.lock_inner().state.snapshot()
    }

    /// 尝试发一条命令（ADR 0009 `try_command`）。
    ///
    /// `Stop` / `Cancel` 是控制命令：`Stop` 在 busy 时也接受（否则关不掉），
    /// `Cancel` 在 busy 时返回在途 op。
    pub fn try_command(&self, command: Command) -> Result<OpId, Busy> {
        self.try_command_with(command, None)
    }

    /// 在**外部提交闸门 Held** 的前提下发一条命令（ADR 0009 切片 4c-2）。
    ///
    /// 只有走这条路径，[`DevLocalRunner`] 才会执行真实 `SyncOnce`：证明会被原样交给
    /// worker，worker 复核「库中 head == 持闸时的 durable head」后再 integrate。
    /// 控制命令（`Cancel` / `Stop`）忽略证明。
    pub fn try_command_with_gate(&self, command: Command, gate: HeldGate) -> Result<OpId, Busy> {
        self.try_command_with(command, Some(gate))
    }

    /// 在**没有活动文档**（没有 UI 会话）的前提下发一轮同步（无会话路径）。
    ///
    /// 与 [`Self::try_command_with_gate`] 互斥的两条路径：本方法**不带**持闸证明，
    /// 只派发 [`Command::SyncOnceNoSession`]。调用方契约（宿主）：
    ///
    /// 1. 只在 `AppShell.active.is_none()` 时调用（空库 / 还没打开笔记）；
    /// 2. 同步在途期间冻结结构写入（新建/重命名/删除/移动）；
    /// 3. 结束后按结构 head 重投影并自动打开第一篇（见 `AppShell::dev_sync_finish`）。
    ///
    /// 无活动文档时普通 [`Self::try_command`] 的 `SyncOnce` 仍会被 runner 拒绝（它必须
    /// 先持闸）——本方法只是把"没有会话"这件事在类型/入口上显式化，不放宽任何检查。
    pub fn try_command_no_session(&self) -> Result<OpId, Busy> {
        self.try_command_with(Command::SyncOnceNoSession, None)
    }

    /// ADR 0012 S2a：发起一次**两阶段**同步（网络阶段不持闸）。
    ///
    /// 调用方契约：收到 `Event::NeedGate` 后才在**当前**活动文档上持闸，并用
    /// [`Self::grant_gate`] 回传授权；收到 `Event::Integrated` 后 `adopt` + `end`，
    /// 再用 [`Self::finish_op`] 授权收尾。三阶段共用同一个 op slot。
    pub fn try_command_two_phase(&self) -> Result<OpId, Busy> {
        self.try_command_with(Command::SyncTwoPhase, None)
    }

    /// 两阶段同步的授权：把 UI 的持闸证明交给正在等授权（`NeedGate`）的 worker。
    ///
    /// 拒绝条件（都返回明确原因，不派发第二条命令）：
    /// - worker 已停止 / 没有在途 op / 在途的不是本次 op；
    /// - 当前阶段不是 `NeedGate`（重复授权/顺序错误）；
    /// - `authorization_epoch` 与 worker 在 `NeedGate` 给出的代际不一致（旧授权）；
    /// - operation deadline 已过（`Cancelling`：worker 仍可能在外写边界内，不授权新的集成）；
    /// - 控制通道已断开。
    pub fn grant_gate(
        &self,
        op_id: OpId,
        authorization_epoch: u64,
        gate: HeldGate,
    ) -> Result<(), String> {
        let mut inner = self.lock_inner();
        if inner.state.worker_stopped() {
            return Err("授权被拒绝：worker 已停止".to_owned());
        }
        if inner.state.in_flight_op() != Some(op_id) {
            return Err(format!(
                "授权被拒绝：在途 op 不是 {op_id}（当前 {:?}）",
                inner.state.in_flight_op()
            ));
        }
        if inner.state.snapshot().deadline_exceeded.is_some() {
            return Err(
                "授权被拒绝：操作已超时（已登记取消），worker 仍可能在外写边界内；                 不再发起新的集成，等真实收尾"
                    .to_owned(),
            );
        }
        if inner.state.two_phase_epoch() != Some(authorization_epoch) {
            return Err(format!(
                "授权被拒绝：授权代际与 NeedGate 不一致（期望 {:?}，收到 {authorization_epoch}）",
                inner.state.two_phase_epoch()
            ));
        }
        if gate.doc_id.is_empty() {
            return Err("授权被拒绝：持闸证明缺少文档".to_owned());
        }
        // 先推进状态机再发消息：如果发送失败，AppShell 会走 cancel；worker 等不到授权
        // 也绝不会 integrate（安全侧），不会出现"已 append 但 UI 以为授权失败"。
        if !inner.state.note_grant_sent(op_id) {
            return Err(format!(
                "授权被拒绝：{op_id} 当前不在 NeedGate 阶段（阶段 {:?}）",
                inner.state.two_phase().map(|status| status.phase)
            ));
        }
        self.send_control(SyncControl::GrantGate {
            op_id,
            authorization_epoch,
            gate,
        })
        .inspect_err(|_| {
            // 状态机已经进入 Integrate：退回 NeedGate，让后续重试/取消的语义清晰。
            inner.state.note_grant_failed(op_id);
        })
        .map_err(|error| format!("授权消息发送失败：{error}"))
    }

    /// 两阶段同步的收尾授权：UI 已 `adopt` 并 `end`，worker 可以 push tail + ack。
    ///
    /// 只接受 `Integrated` 阶段的同一个 op；超时（`Cancelling`）不阻止安全收尾。
    pub fn finish_op(&self, op_id: OpId) -> Result<(), String> {
        let mut inner = self.lock_inner();
        if inner.state.worker_stopped() {
            return Err("收尾被拒绝：worker 已停止".to_owned());
        }
        if inner.state.in_flight_op() != Some(op_id) {
            return Err(format!(
                "收尾被拒绝：在途 op 不是 {op_id}（当前 {:?}）",
                inner.state.in_flight_op()
            ));
        }
        if !inner.state.note_finish_sent(op_id) {
            return Err(format!(
                "收尾被拒绝：{op_id} 当前不在 Integrated 阶段（阶段 {:?}）",
                inner.state.two_phase().map(|status| status.phase)
            ));
        }
        self.send_control(SyncControl::Finish { op_id })
    }

    fn try_command_with(&self, command: Command, gate: Option<HeldGate>) -> Result<OpId, Busy> {
        let mut inner = self.lock_inner();
        // 放弃过的 op 还在 worker 手里跑：仍然算"在途"，不能再排一条（保持单操作串行）。
        // 语义比 `InFlight` 更明确：已经超时/取消，在等 worker 真实收尾。
        if !command.is_control() {
            if let Some(abandoned) = inner.abandoned {
                return Err(Busy::new(BusyReason::Cancelling, Some(abandoned)));
            }
        }
        let op_id = inner.state.try_begin(command)?;
        if command == Command::Cancel {
            // `Cancel` 不是一条 worker 任务：它给在途 op 置取消标志（旧路径在首次外部
            // 集成前复核）+ 发一条控制消息（两阶段 worker 可能正停在 NeedGate/Integrated
            // 的等待里，必须能把取消穿透过去；ADR 0012 S2a）。
            if let Some((_, control)) = &inner.control {
                control.request_cancel();
            }
            if let Err(error) = self.send_control(SyncControl::Cancel { op_id }) {
                // 控制通道已关 = worker 已退出：不假装取消成功（真实收尾会由终态体现）。
                eprintln!("[dev-sync] 取消消息发送失败（{op_id}）：{error}");
            }
            return Ok(op_id);
        }
        // 控制命令没有"外部提交"语义：一律不带证明（runner 也不会用）。
        let gate = if command.is_control() { None } else { gate };
        let control = if command.is_control() {
            OpControl::default()
        } else {
            OpControl::new()
        };
        if !self.dispatch(Job {
            op_id,
            command,
            gate,
            control: control.clone(),
        }) {
            let reason = "worker 通道已关闭（worker 已退出）";
            inner.state.abort_dispatch(op_id, reason);
            return Err(Busy::new(BusyReason::WorkerStopped, None));
        }
        if !command.is_control() {
            inner.deadline = Some((op_id, self.clock.now()));
            inner.control = Some((op_id, control));
        }
        Ok(op_id)
    }

    /// 非阻塞收事件（GUI 每帧调用；`Progress` 已合并，`Completed`/`Stopped` 不丢）。
    ///
    /// 顺带做两件后台维护：判定操作 deadline、回收已退出的 worker 线程。
    pub fn poll(&self) -> Vec<Event> {
        let mut inner = self.lock_inner();
        while let Ok(event) = inner.events.try_recv() {
            let op_id = event.op_id();
            if event.is_terminal() {
                // 真实终态回来（含超时后才跑完的那条）：解除"已放弃"，但**不**重复通知。
                if inner.abandoned == Some(op_id) {
                    inner.abandoned = None;
                }
                if inner.deadline.is_some_and(|(id, _)| id == op_id) {
                    inner.deadline = None;
                }
                if inner.control.as_ref().is_some_and(|(id, _)| *id == op_id) {
                    inner.control = None;
                }
                if matches!(event, Event::Stopped { .. }) {
                    // worker 已停止 = 不会再有外部写：超时等待/取消通道一并了结。
                    inner.abandoned = None;
                    inner.deadline = None;
                    inner.control = None;
                }
            }
            inner.state.apply(event);
        }
        // 操作 deadline：worker 里的同步调用无法中断，这里只做**Cancelling**——
        // 绝不合成可解闸的 `Completed`（ADR 0012 S1）：worker 可能仍在 integrate，
        // 真实回执到达前 UI 必须保持 Held、单操作位不得复用。
        if let (Some((op_id, started)), Some(limit)) =
            (inner.deadline, self.config.operation_deadline)
        {
            if self.clock.now().duration_since(started) >= limit {
                inner.deadline = None;
                inner.abandoned = Some(op_id);
                // 两阶段：**已经授权、进入 integrate/交付之后**的 timeout 只做 Cancelling，
                // 不打断事务、也不取消交付（ADR 0012 S2a：等真实回执后收尾）。worker 会
                // 按正常路径 integrate → 等 UI 交付 → ack，迟到回执由状态机标注。
                let post_grant = inner.state.two_phase().is_some_and(|status| {
                    matches!(
                        status.phase,
                        SyncPhase::Integrate | SyncPhase::Integrated | SyncPhase::Finish
                    )
                });
                if !post_grant {
                    // 同时置位取消通道：worker 若还没进 integrate，就在边界前停手。
                    if let Some((id, control)) = &inner.control {
                        if *id == op_id {
                            control.request_cancel();
                        }
                    }
                    // 两阶段 worker 可能正阻塞在 NeedGate 的等待里：原子标志唤不醒阻塞的
                    // `recv`，必须再发一条控制消息（独立通道，不受 job 队列影响）。
                    if let Err(error) = self.send_control(SyncControl::Cancel { op_id }) {
                        eprintln!("[dev-sync] 超时取消消息发送失败（{op_id}）：{error}");
                    }
                }
                inner.state.apply(Event::DeadlineExceeded {
                    op_id,
                    message: format!(
                        "操作超时：超过 {limit:?} 未完成；worker 仍可能在外写边界内，\
                         等待真实收尾（未生成可解闸终态）"
                    ),
                });
            }
        }
        let worker_stopped = inner.state.worker_stopped();
        let events = inner.state.pull_events();
        drop(inner);
        if worker_stopped {
            // 已回执 Stopped：线程随后退出，这里只做**非阻塞**回收（GUI 不 join）。
            self.try_reap_worker();
        }
        events
    }

    /// 请求停止 worker，返回关闭票据（ADR 0009 `request_stop`）。
    ///
    /// 重复调用返回**同一张**票据（幂等）；worker 已停时也能拿到票据（`poll_close` 会给
    /// `Ready` 或 `Blocked`，绝不会假装还在关）。
    pub fn request_stop(&self) -> StopTicket {
        // 两阶段 worker 可能正停在 NeedGate/Integrated 的等待里：先发 Stop 穿透等待，
        // 再走 job 队列的 `Stop`（关闭语义：回执 Stopped 后退出）。
        let _ = self.send_control(SyncControl::Stop);
        let mut inner = self.lock_inner();
        match inner.state.try_begin(Command::Stop) {
            Ok(op_id) => {
                if !self.dispatch(Job {
                    op_id,
                    command: Command::Stop,
                    gate: None,
                    control: OpControl::default(),
                }) {
                    inner
                        .state
                        .abort_dispatch(op_id, "worker 通道已关闭（worker 已退出）");
                }
                match inner.state.last_ticket() {
                    Some(ticket) => ticket,
                    None => inner.state.stop_ticket(),
                }
            }
            // 已在关闭/已停止：复用本次（或上一次）关闭的票据。
            Err(_) => inner.state.stop_ticket(),
        }
    }

    /// 查关闭门禁（ADR 0009 `poll_close`）。
    ///
    /// 只等**明确目标 durable 确认**（本切片以"在途 op 返回 Ok"模拟），不等网络同步。
    #[must_use]
    pub fn poll_close(&self, ticket: &StopTicket) -> CloseState {
        self.lock_inner().state.poll_close(ticket)
    }

    /// 非阻塞回收：worker 线程已退出就 join（面板 tick 用，绝不阻塞 UI）。
    pub fn try_reap_worker(&self) -> bool {
        let finished = {
            let worker = self.worker.lock().expect("dev-sync worker 句柄锁被毒化");
            worker
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished)
        };
        if !finished {
            return false;
        }
        self.join_worker()
    }

    /// 阻塞回收：等 worker 线程退出并 join。
    ///
    /// **GUI 不调用**（ADR 0009「Stop 后后台回收，GUI 不 join」）；测试与 CLI 收尾用它
    /// 当"worker 已全部发完事件"的确定性收敛点（join 建立 happens-before）。
    pub fn reap_worker(&self) -> bool {
        self.join_worker()
    }

    fn join_worker(&self) -> bool {
        let handle = {
            let mut worker = self.worker.lock().expect("dev-sync worker 句柄锁被毒化");
            worker.take()
        };
        let Some(handle) = handle else {
            return false;
        };
        if handle.join().is_err() {
            eprintln!("[menghunote] dev-sync worker 线程 panic（事件可能不完整）");
        }
        true
    }

    fn dispatch(&self, job: Job) -> bool {
        let jobs = self.jobs.lock().expect("dev-sync 命令通道锁被毒化");
        match jobs.as_ref() {
            Some(sender) => sender.send(job).is_ok(),
            None => false,
        }
    }

    /// 发一条**控制消息**（不走 job 队列：两阶段 worker 可能正停在阶段等待里）。
    fn send_control(&self, message: SyncControl) -> Result<(), String> {
        let control = self.control.lock().expect("dev-sync 控制通道锁被毒化");
        match control.as_ref() {
            Some(sender) => sender
                .send(message)
                .map_err(|_| "控制通道已关闭（worker 已退出）".to_owned()),
            None => Err("控制通道已关闭（worker 已退出）".to_owned()),
        }
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("dev-sync 状态锁被毒化")
    }
}

/// worker 主循环：串行执行命令，`Stop` 回执后退出（GUI 侧不 join）。
fn worker_loop(
    jobs: &Receiver<Job>,
    events: &Sender<Event>,
    mut runner: Box<dyn SyncRunner>,
    controls: Receiver<SyncControl>,
) {
    // 两阶段握手通道由 runner 接管（只在这一条 worker 线程里读）。
    runner.attach_control(controls);
    while let Ok(job) = jobs.recv() {
        if job.command == Command::Stop {
            // `Stop` 本身没有同步动作：回执后立刻退出，哪怕前面有排队的命令也不再执行
            // （关闭语义优先，未执行的命令永远不会"看起来成功"）。
            let _ = events.send(Event::Stopped { op_id: job.op_id });
            break;
        }
        let mut emit = |event: Event| {
            // 接收端已丢弃（面板/进程关闭）：继续跑完当前操作，但不再发送（不能 panic）。
            let _ = events.send(event.stamped(job.op_id));
        };
        // 两阶段同步走**自己的**入口（内部等控制通道；旧的整轮入口不接受它）。
        let outcome = if job.command == Command::SyncTwoPhase {
            runner.run_two_phase(job.op_id, &job.control, &mut emit)
        } else {
            runner.run_gated_controlled(job.command, job.gate.as_ref(), &job.control, &mut emit)
        };
        let (changed_heads, result) = match outcome {
            Ok(outcome) => (
                outcome.changed_heads,
                OpResult::Ok {
                    summary: outcome.summary,
                },
            ),
            Err(message) => (Vec::new(), OpResult::Failed { message }),
        };
        let _ = events.send(Event::Completed {
            op_id: job.op_id,
            changed_heads,
            result,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_runner_refuses_every_operation_but_not_control_commands() {
        let mut runner = UnimplementedRunner;
        let mut progress = Vec::new();
        let mut emit = |event: Event| progress.push(event);
        for command in [
            Command::Create,
            Command::Join,
            Command::SyncOnce,
            Command::SyncOnceNoSession,
            Command::PublishInitialPair,
            Command::ListDevices,
            Command::RevokeDevice { target: [0x11; 16] },
            Command::Rotate,
            Command::Resume,
        ] {
            let error = runner
                .run(command, &mut emit)
                .expect_err("默认 runner 绝不能对操作命令返回成功");
            assert!(
                error.contains("切片 4 未实现"),
                "错误必须说清未实现：{error}"
            );
            assert!(
                !error.contains("secret") && !error.contains("恢复码"),
                "错误文案不得含 secret：{error}"
            );
        }
        assert!(
            runner.run(Command::Cancel, &mut emit).is_ok(),
            "控制命令不该被当成未实现的同步操作"
        );
        assert!(runner.run(Command::Stop, &mut emit).is_ok());
        assert!(
            progress
                .iter()
                .all(|event| matches!(event, Event::Progress { .. })),
            "runner 只能上报 Progress（终态由 worker 生成）"
        );
    }

    #[test]
    fn config_builds_a_loopback_transport_with_the_configured_timeouts() {
        // 手动默认：与 DevLocal 的历史传输默认逐项一致（30/30s + 无整次请求预算）。
        let config = ServiceConfig::new("http://127.0.0.1:18080");
        assert_eq!(config.transport, DEFAULT_TRANSPORT_TIMEOUTS);
        let transport = config.build_transport().expect("loopback 明文必须可构造");
        assert_eq!(transport.timeouts(), DEFAULT_TRANSPORT_TIMEOUTS);
        assert_eq!(transport.timeouts().connect, Duration::from_secs(30));
        assert_eq!(transport.timeouts().io, Duration::from_secs(30));
        assert_eq!(transport.timeouts().request_deadline, None);

        // ADR 0012 自动同步建议预算：5/10/30s（S1 只定义、不启用）。
        let autosync = ServiceConfig::for_autosync("http://127.0.0.1:18080");
        assert_eq!(autosync.transport, AUTOSYNC_TRANSPORT_TIMEOUTS);
        assert_eq!(
            autosync.build_transport().expect("loopback").timeouts(),
            AUTOSYNC_TRANSPORT_TIMEOUTS
        );
        assert_eq!(
            autosync
                .clone()
                .with_transport(DEFAULT_TRANSPORT_TIMEOUTS)
                .transport,
            DEFAULT_TRANSPORT_TIMEOUTS,
            "显式换预算必须覆盖"
        );

        // 非 loopback 明文：由 transport 的策略拒绝（门禁还会更早拒绝一次）。
        let bad = ServiceConfig::new("http://relay.example:18080");
        assert!(bad.build_transport().is_err(), "非 loopback 明文必须拒绝");
    }

    #[test]
    fn userinfo_in_the_relay_url_is_rejected() {
        // 凭据不允许进 URL（ADR 0009：秘密不进普通配置/日志）；脱敏由 gate 负责，
        // 这里只保证"带 userinfo 的 relay 构造不出传输对象"。
        let config = ServiceConfig::new("http://user:pass@127.0.0.1:18080");
        match config.build_transport() {
            Ok(_) => panic!("userinfo 必须被拒绝"),
            Err(error) => assert!(!error.to_string().is_empty(), "拒绝理由不能是空字符串"),
        }
    }

    #[test]
    fn dev_local_runner_refuses_sync_once_without_a_held_gate() {
        // 用不存在的路径：本测试只验"闸门缺失/未实现"这些**在打开库之前**就失败的路径，
        // 不碰任何真实库/网络。
        let mut runner = DevLocalRunner::new(
            "http://127.0.0.1:18080",
            PathBuf::from("/nonexistent-mn-dev-sync-runner/vault.sqlite3"),
        );
        let mut progress = Vec::new();
        let mut emit = |event: Event| progress.push(event);

        for error in [
            runner
                .run(Command::SyncOnce, &mut emit)
                .expect_err("裸 SyncOnce 必须被拒绝"),
            runner
                .run_gated(Command::SyncOnce, None, &mut emit)
                .expect_err("缺持闸证明必须被拒绝"),
        ] {
            assert!(error.contains("持闸"), "错误必须说清缺持闸证明：{error}");
        }

        // 无会话同步是另一条路径：**不接受**持闸证明（带了 = 两条路径混用），
        // 不带证明时先打开库（本用例路径不存在 → 在任何网络动作之前明确失败）。
        let error = runner
            .run_gated(
                Command::SyncOnceNoSession,
                Some(&HeldGate::new("doc-1", 0, 1)),
                &mut emit,
            )
            .expect_err("无会话同步带持闸证明必须被拒绝");
        assert!(
            error.contains("不接受持闸证明"),
            "错误必须说清两条路径互斥：{error}"
        );
        let error = runner
            .run(Command::SyncOnceNoSession, &mut emit)
            .expect_err("库路径不可用时无会话同步必须明确失败");
        assert!(
            error.contains("打开本地库失败"),
            "无会话同步必须点明打不开库：{error}"
        );

        // 发布基线（切片 4c-4）与同步一样**必须带持闸证明**：裸命令一律拒绝。
        let error = runner
            .run_gated(Command::PublishInitialPair, None, &mut emit)
            .expect_err("裸 PublishInitialPair 必须被拒绝");
        assert!(
            error.contains("持闸"),
            "发布基线缺持闸证明必须明确失败：{error}"
        );

        // 其余操作命令仍然是"明确未实现"（Create/Join 走 CLI）。
        let error = runner
            .run_gated(Command::Create, None, &mut emit)
            .expect_err("Create 仍走 CLI");
        assert!(
            error.contains("CLI") && error.contains("创建"),
            "未实现文案必须指出 CLI 路径：{error}"
        );

        // 设备命令不需要持闸证明，但要先能打开库：这组用例的路径不存在 → 明确失败
        // （在**任何网络动作之前**，也不 panic）。
        for command in [
            Command::ListDevices,
            Command::RevokeDevice { target: [0x22; 16] },
        ] {
            let error = runner
                .run(command, &mut emit)
                .expect_err("库路径不可用时设备命令必须明确失败");
            assert!(
                error.contains("打开本地库失败"),
                "设备命令必须点明打不开库：{error}"
            );
        }

        // 控制命令没有同步动作：不算失败。
        assert!(runner.run_gated(Command::Cancel, None, &mut emit).is_ok());
        assert!(runner.run(Command::Stop, &mut emit).is_ok());
        assert!(
            progress
                .iter()
                .all(|event| matches!(event, Event::Progress { .. })),
            "runner 只能上报 Progress"
        );
    }

    #[test]
    fn dev_local_runner_fails_before_network_when_the_vault_cannot_open() {
        // 持闸证明存在，但库路径不可用：必须在**发网络请求之前**明确失败（返回 Err，
        // 不 panic、不静默成功）。
        let gate = HeldGate::new("doc-1", 0, 1);
        let mut runner = DevLocalRunner::new(
            "http://127.0.0.1:18080",
            PathBuf::from("/nonexistent-mn-dev-sync-runner/vault.sqlite3"),
        );
        let mut emit = |_event: Event| {};
        let error = runner
            .run_gated(Command::SyncOnce, Some(&gate), &mut emit)
            .expect_err("打不开库必须明确失败");
        assert!(
            error.contains("打开本地库失败"),
            "错误必须点明打开库失败：{error}"
        );
    }
}
