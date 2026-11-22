//! **自动同步接线**（ADR 0012 S3）：把 S1 的纯调度器、S2a/S2b 的两阶段流程与状态 UX
//! 组合成宿主可直接使用的形态。
//!
//! ## 分工（硬边界）
//!
//! | 谁 | 负责 |
//! | --- | --- |
//! | [`Scheduler`]（S1） | **只**回答"现在该不该启动一次自动同步"（合并 pending/退避/最小间隔） |
//! | 本模块 | 时刻表（启动合并、失焦降频、跨操作续批）、失败分类、常驻状态投影 |
//! | `AppShell` | 真正派发/推进两阶段状态机、持闸、IME 等待与 defer、窗口生命周期 |
//!
//! 本模块**不碰** GPUI、不碰网络、不碰库：`Backlog`（待发送/待应用计数）由宿主用
//! [`durable_backlog`] 从 SQLite 读出后喂进来，时钟经 [`OperationClock`] 注入
//! （测试用手动时钟，生产用 [`super::service::SystemOperationClock`]）。
//!
//! ## 与 ADR 0012「调度」的对应
//!
//! - 启动（进入 Main / 重启后检测到 durable 待办）合并一次：[`AutoSync::prime_startup`]；
//! - 本地编辑 `DurableAck` 尾缘 2s：[`AutoSync::on_local_durable`]（远端 adopt 不触发）；
//! - 周期 45s、失焦 60s：[`AutoSync::on_activation`]；
//! - 自动启动间隔 ≥5s、失败退避 5/10/20/40…300s + 可注入 jitter：由 [`Scheduler`] 保证；
//! - 自动请求遇到 Held/单操作：宿主先 `set_host_busy(true)` 跳过，或派发被拒时
//!   [`AutoSync::on_busy`]（合并成一条"稍后重试"）；
//! - 跨操作公平续批：[`AutoSync::finish_round`] 用**真实剩余**（durable inbox）与
//!   "本轮是否有进展"决定继续还是暂停这一轮（[`StatusState::NoProgress`]）。
//!
//! ## 状态 UX（不打扰）
//!
//! [`SyncStatus`] 是**纯投影**：`line()` 给常驻状态行（`sync-status`），`error_line()`
//! 只在"需要处理 / 交付失败仍冻结"时给出（`sync-status-error`）。每轮成功不弹 toast、
//! 不抢焦点；"已接收"与"已应用"分别用 `inbox`（待应用）与 `deferred` 如实显示。

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::scheduler::{
    Decision, Finished, LocalEditAck, Scheduler, SchedulerConfig, SkipReason, Trigger,
    DEFAULT_PERIOD,
};
use super::service::OperationClock;

/// 失焦后的周期（ADR 0012：可降到 60s）。
pub const UNFOCUSED_PERIOD: Duration = Duration::from_secs(60);
/// NeedGate 之后等"输入法自然结束/空闲"的上限。
///
/// 拿不到空闲就 **defer**：不强制结束组合、释放单操作槽，稍后再试（持闸只覆盖
/// integrate+adopt，网络阶段本来就不冻结输入）。
pub const GATE_IDLE_DEFER: Duration = Duration::from_secs(2);
/// 自动请求遇到 Held/单操作在途时的重试延迟（不抢占已开始的事务）。
pub const BUSY_RETRY_DELAY: Duration = Duration::from_secs(1);
/// 跨操作续批的合并延迟（真正的最小启动间隔由 [`Scheduler`] 的 5s 保证）。
pub const CONTINUATION_DELAY: Duration = Duration::from_secs(1);
/// 待发送/待应用计数的刷新节流（避免每 40ms 查一次 SQLite）。
pub const STATUS_REFRESH: Duration = Duration::from_secs(1);

/// 一轮同步是谁发起的（手动/自动共用同一条单操作槽与两阶段状态机）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpOrigin {
    /// 用户点「同步」/「发布基线」（或测试显式调用）。
    Manual,
    /// 调度器自动发起。
    Auto,
}

impl OpOrigin {
    /// 是否自动发起。
    #[must_use]
    pub const fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// durable 待办计数（宿主从 SQLite 读出后喂进来）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Backlog {
    /// `sync_outbox` 里 `pending` + `encrypted` 的行数（待发送）。
    pub outbox: u64,
    /// `sync_inbox` 里已验证、落在 durable 连续前缀内、尚未集成的行数（已接收·待应用）。
    pub inbox: u64,
}

impl Backlog {
    /// 是否还有任何 durable 待办（启动/聚焦探测的判据）。
    #[must_use]
    pub const fn has_work(self) -> bool {
        self.outbox > 0 || self.inbox > 0
    }
}

/// 从库读出 durable 待办计数（只读；失败按 0 处理，不阻塞 UI）。
///
/// 为什么不用 `pending_encryption/pending_send`：它们带 `limit`，拿不到总数；
/// 状态行要的是**真实**的"待发送 N"。
#[must_use]
pub fn durable_backlog(vault: &menghunote_storage::Vault) -> Backlog {
    let outbox = vault
        .query(
            "SELECT COUNT(*) FROM sync_outbox WHERE state IN ('pending','encrypted')",
            &[],
        )
        .ok()
        .and_then(|rows| {
            rows.first().and_then(|row| match row.first() {
                Some(menghunote_storage::Value::Integer(value)) => {
                    Some(u64::try_from(*value).unwrap_or(0))
                }
                _ => None,
            })
        })
        .unwrap_or(0);
    let inbox = menghunote_storage::count_inbox_ready(vault).unwrap_or(0);
    Backlog { outbox, inbox }
}

/// 失败分类：只有**明确可重试**的才退避重试，其余进入"需要处理"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureClass {
    /// 瞬时/并发错误（传输、5xx、BUSY、head CAS…）：退避后自动重试。
    Retryable,
    /// 鉴权/TLS/签名/epoch/游标异常：不自动重试，等人工处理。
    NeedsAttention,
}

/// 把 worker 的失败文案分类（`SyncError` 的 `Display` 前缀；不含 secret）。
///
/// 为什么按文案而不是类型：worker 只把 `String` 交给事件通道（S1/S2 的既有边界），
/// 这里按 `SyncError` 的稳定错误前缀保守判定——**未知一律不重试**（fail-safe），
/// 避免自动路径反复撞同一面墙。
#[must_use]
pub fn classify_failure(message: &str) -> FailureClass {
    // 1) 明确瞬时的传输/并发错误（`SyncError::is_retryable()` 对应的稳定前缀）。
    for marker in [
        "relay 传输失败",
        "head CAS 冲突",
        "系统随机源失败",
        "本地存储失败：snapshot busy",
    ] {
        if message.contains(marker) {
            return FailureClass::Retryable;
        }
    }
    // 2) HTTP 5xx / 408 / 429：relay 侧瞬时不可用。
    if let Some(rest) = message.split("relay HTTP 状态 ").nth(1) {
        let code: u16 = rest
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        if (500..600).contains(&code) || code == 408 || code == 429 {
            return FailureClass::Retryable;
        }
    }
    // 3) relay 业务错误码里只有 4 个是可重试的（见 `ErrorCode::retryable`）。
    if let Some(rest) = message.split("relay 拒绝：").nth(1) {
        let code: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if matches!(
            code.as_str(),
            "BUSY" | "OBJECT_BUSY" | "TOO_MANY_REQUESTS" | "INTERNAL"
        ) {
            return FailureClass::Retryable;
        }
    }
    FailureClass::NeedsAttention
}

/// 脱敏/截断失败原因（状态行只显示一行）。
fn sanitized_reason(message: &str) -> String {
    let one_line = message
        .split(['\n', '\r'])
        .next()
        .unwrap_or(message)
        .trim()
        .to_owned();
    if one_line.chars().count() > 120 {
        let mut truncated: String = one_line.chars().take(120).collect();
        truncated.push('…');
        truncated
    } else {
        one_line
    }
}

/// 常驻状态的主状态（互斥；附加计数另存于 [`SyncStatus`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusState {
    /// 还没有过任何一轮（新实例/刚进 Main）。
    Idle,
    /// 一轮正在跑（`auto = true` 显示"自动同步中"）。
    Syncing {
        /// 是否自动发起。
        auto: bool,
    },
    /// 最近一轮成功（`at` 是墙钟时刻；状态行显示 `HH:MM`）。
    Synced {
        /// 成功时刻。
        at: SystemTime,
    },
    /// 可重试失败后的退避窗口。
    Retry {
        /// 退避到期时间。
        until: Instant,
        /// 连续第几次失败（1 起）。
        attempt: u32,
    },
    /// 需要人工处理（不会自动重试）。
    NeedsAttention {
        /// 脱敏原因。
        reason: String,
    },
    /// 续批暂停：durable 待应用还在，但本轮零进展（等新依赖/新数据）。
    NoProgress {
        /// 还剩多少待应用。
        remaining: u64,
    },
}

/// 常驻状态投影（`sync-status` / `sync-status-error` 的唯一来源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncStatus {
    /// 主状态。
    pub state: StatusState,
    /// durable 待发送（outbox `pending` + `encrypted`）。
    pub outbox: u64,
    /// durable 已接收·待应用（inbox verified 且落在连续前缀内）。
    pub inbox: u64,
    /// 最近一轮 worker 报告的 deferred（真实剩余，不跨 deferred 提前 ACK）。
    pub deferred: u64,
    /// 交付失败但仍持有闸门时的说明（fail-closed；只有它才显示"冻结"）。
    pub frozen: Option<String>,
}

impl Default for SyncStatus {
    fn default() -> Self {
        Self {
            state: StatusState::Idle,
            outbox: 0,
            inbox: 0,
            deferred: 0,
            frozen: None,
        }
    }
}

impl SyncStatus {
    /// 常驻状态行文案（一行；不打扰、不弹 toast）。
    ///
    /// `now` 只用于把退避窗口换算成"还有多少秒"。
    #[must_use]
    pub fn line(&self, now: Instant) -> String {
        if let Some(reason) = &self.frozen {
            return format!("需要处理：收尾未完成（编辑器保持冻结）· {reason}");
        }
        let mut text = match &self.state {
            StatusState::Idle => "尚未同步".to_owned(),
            StatusState::Syncing { auto: true } => "自动同步中".to_owned(),
            StatusState::Syncing { auto: false } => "同步中".to_owned(),
            StatusState::Synced { at } => format!("已同步 {}", hhmm_utc(*at)),
            StatusState::Retry { until, attempt } => {
                let remaining = until.saturating_duration_since(now);
                format!(
                    "重试于 {:.0}s 后（第 {attempt} 次失败）",
                    remaining.as_secs_f64().ceil().max(0.0)
                )
            }
            StatusState::NeedsAttention { reason } => format!("需要处理：{reason}"),
            StatusState::NoProgress { remaining } => {
                format!("已接收·待应用 {remaining}（续批暂停：无进展）")
            }
        };
        // 计数只在主状态没表达它时才追加（"收到"≠"已应用"，两个数分开）。
        let already_shows_pending = matches!(
            self.state,
            StatusState::NoProgress { .. } | StatusState::NeedsAttention { .. }
        );
        if !already_shows_pending {
            if self.inbox > 0 {
                text.push_str(&format!(" · 已接收·待应用 {}", self.inbox));
            }
            if self.outbox > 0 {
                text.push_str(&format!(" · 待发送 {}", self.outbox));
            }
        }
        text
    }

    /// 需要用户处理的那一行（`None` = 不显示，避免每轮成功/普通退避刷屏）。
    #[must_use]
    pub fn error_line(&self) -> Option<String> {
        if let Some(reason) = &self.frozen {
            return Some(format!(
                "需要处理：{reason}；闸门仍持有（编辑器保持冻结），点「同步」重试收尾"
            ));
        }
        match &self.state {
            StatusState::NeedsAttention { reason } => {
                Some(format!("需要处理：{reason}；可点「同步」重试"))
            }
            _ => None,
        }
    }

    /// 是否显示"同步中"（面板按钮置灰/提示用）。
    #[must_use]
    pub fn is_syncing(&self) -> bool {
        matches!(self.state, StatusState::Syncing { .. })
    }
}

/// `HH:MM`（**UTC**；dev-only 状态行，不为此引入时区依赖）。
#[must_use]
pub fn hhmm_utc(at: SystemTime) -> String {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let minutes = (seconds / 60) % (24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// 自动同步的配置（周期/debounce/失焦/退避 jitter/时钟/ticker）。
#[derive(Debug, Clone, Copy)]
pub struct AutoSyncConfig {
    /// 聚焦时的周期（构造时夹到 30–60s）。
    pub period: Duration,
    /// 失焦后的周期（默认 60s）。
    pub unfocused_period: Duration,
    /// 本地编辑尾缘 debounce（默认 2s）。
    pub debounce: Duration,
    /// 两次自动启动的最小间隔（默认 5s）。
    pub min_start_interval: Duration,
    /// 可重试失败的退避 jitter（`attempt` = 第几次失败）。
    pub backoff_jitter: fn(u32) -> Duration,
    /// NeedGate 后等空闲的上限（超时即 defer）。
    pub gate_idle_defer: Duration,
    /// 是否运行墙钟 ticker（生产 `true`；测试 `false` + 显式 tick）。
    pub ticker: bool,
}

impl Default for AutoSyncConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoSyncConfig {
    /// 生产默认（45s / 60s / 2s / 5s / 2s / 无 jitter / ticker on）。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            period: DEFAULT_PERIOD,
            unfocused_period: UNFOCUSED_PERIOD,
            debounce: super::scheduler::DEFAULT_DEBOUNCE,
            min_start_interval: super::scheduler::DEFAULT_MIN_START_INTERVAL,
            backoff_jitter: |_| Duration::ZERO,
            gate_idle_defer: GATE_IDLE_DEFER,
            ticker: true,
        }
    }

    /// 测试用：同一套默认，但**关闭墙钟 ticker**（显式 `dev_sync_auto_tick`）。
    #[must_use]
    pub const fn no_ticker(mut self) -> Self {
        self.ticker = false;
        self
    }

    /// 换周期（[`SchedulerConfig::with_period`] 的夹取语义）。
    #[must_use]
    pub fn with_period(mut self, period: Duration) -> Self {
        self.period = period.clamp(super::scheduler::PERIOD_MIN, super::scheduler::PERIOD_MAX);
        self
    }

    /// 换退避 jitter。
    #[must_use]
    pub fn with_backoff_jitter(mut self, jitter: fn(u32) -> Duration) -> Self {
        self.backoff_jitter = jitter;
        self
    }

    /// 换"等 IME 空闲"的上限。
    #[must_use]
    pub fn with_gate_idle_defer(mut self, defer: Duration) -> Self {
        self.gate_idle_defer = defer;
        self
    }

    /// 换成纯调度器配置。
    #[must_use]
    fn scheduler_config(self) -> SchedulerConfig {
        SchedulerConfig::new()
            .with_period(self.period)
            .with_debounce(self.debounce)
            .with_backoff_jitter(self.backoff_jitter)
    }
}

/// 一轮的起始记账（续批要判断"有没有进展"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RoundTicket {
    /// 本轮触发来源。
    trigger: Trigger,
    /// 起始时的 durable 待办（`inbox_before` 用来算"应用了没有"）。
    before: Backlog,
    /// worker 在 `NeedGate` 上报的 durable 已接收数。
    stored: usize,
    /// worker 在 `NeedGate`/`Integrated` 上报的 deferred（真实剩余）。
    deferred: usize,
    /// worker 在 `Integrated` 上报的本轮已应用数。
    integrated: usize,
}

/// 一轮结束的事实（宿主从报告/事件里折算后交给 [`AutoSync::finish_round`]）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoundOutcome {
    /// 本轮是否成功（拿到 worker 的 Ok 且没有 pending_error）。
    pub success: bool,
    /// 失败原因（脱敏；成功时 `None`）。
    pub error: Option<String>,
    /// 本轮是因为"拿不到 IME 空闲"主动 defer 的（**不是失败**，不计退避）。
    pub deferred_by_gate: bool,
    /// 交付失败且闸门仍持有（fail-closed；状态行必须显示冻结说明）。
    pub frozen: Option<String>,
}

/// 自动同步接线本体（纯逻辑 + 注入时钟；GPUI/IO 全在宿主）。
#[derive(Clone)]
pub struct AutoSync {
    config: AutoSyncConfig,
    clock: Arc<dyn OperationClock>,
    scheduler: Scheduler,
    status: SyncStatus,
    /// 在途一轮的来源（`None` = 没有自动/手动轮在跑）。
    origin: Option<OpOrigin>,
    /// 在途一轮的起始记账。
    round: Option<RoundTicket>,
    /// 启动合并还没兑现（构造时发现 durable 待办，第一次 tick 才真正 on_startup）。
    startup_pending: bool,
    /// 自动轮在跑时用户点了「同步」：本轮结束后立即补一次手动轮。
    manual_requested: bool,
    /// 已人工停止（关窗）：此后不再自动启动。
    stopped: bool,
    /// 窗口是否处于激活（焦点）状态：失焦降频到 [`AutoSyncConfig::unfocused_period`]。
    focus_active: bool,
    /// NeedGate 之后开始等"IME 自然结束/空闲"的时刻（`None` = 还没到 NeedGate）。
    gate_wait_since: Option<Instant>,
    /// 下一次刷新滚动计数的最早时刻（节流）。
    next_status_refresh: Option<Instant>,
    /// 有一个本地编辑还没等到它的 `DurableAck`。
    local_edit_pending: bool,
}

impl AutoSync {
    /// 以显式配置 + 注入时钟构造（`now` 只用于周期起点/退避基准）。
    #[must_use]
    pub fn new(config: AutoSyncConfig, clock: Arc<dyn OperationClock>, now: Instant) -> Self {
        Self {
            config,
            clock,
            scheduler: Scheduler::with_config(config.scheduler_config(), now),
            status: SyncStatus::default(),
            origin: None,
            round: None,
            startup_pending: false,
            manual_requested: false,
            stopped: false,
            // 首帧还没收到窗口激活回调：按"激活"处理（周期 45s，不是 60s）。
            focus_active: true,
            gate_wait_since: None,
            next_status_refresh: None,
            local_edit_pending: false,
        }
    }

    /// 生产默认（系统单调时钟；墙钟 ticker 由宿主决定是否跑）。
    #[must_use]
    pub fn with_system_clock(now: Instant) -> Self {
        Self::new(
            AutoSyncConfig::new(),
            Arc::new(super::service::SystemOperationClock),
            now,
        )
    }

    /// 配置。
    #[must_use]
    pub const fn config(&self) -> &AutoSyncConfig {
        &self.config
    }

    /// 注入时钟的当前时刻（宿主的 tick 用它，测试用同一份手动时钟）。
    #[must_use]
    pub fn now(&self) -> Instant {
        self.clock.now()
    }

    /// 是否运行墙钟 ticker。
    #[must_use]
    pub const fn ticker_enabled(&self) -> bool {
        self.config.ticker && !self.stopped
    }

    /// 是否已停止（关窗）。
    #[must_use]
    pub const fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// 调度器只读投影（测试断言用）。
    #[must_use]
    pub const fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// 常驻状态投影。
    #[must_use]
    pub const fn status(&self) -> &SyncStatus {
        &self.status
    }

    /// 在途一轮的来源（`None` = 空闲）。
    #[must_use]
    pub const fn origin(&self) -> Option<OpOrigin> {
        self.origin
    }

    /// 手动轮是否已排队等自动轮收尾（"可合并"语义）。
    #[must_use]
    pub const fn manual_queued(&self) -> bool {
        self.manual_requested
    }

    /// 关窗/切库：停止调度（不再自动启动；已在途的操作由 S1/S2 的安全收尾语义处理）。
    pub fn stop(&mut self) {
        self.stopped = true;
        self.startup_pending = false;
    }

    /// 进入 Main / 重启后的启动合并：只有存在 durable 待办时才排一次。
    ///
    /// 返回 `true` 表示"有待办、已经排队"（宿主可据此在状态行显示"待同步"）。
    ///
    /// 为什么不在构造里立刻 `on_startup`：构造期派发会重入宿主状态机；这里只登记，
    /// 第一次 [`Self::tick`] 才真正走调度决策（生产 ticker 40ms 后、测试显式 tick）。
    pub fn prime_startup(&mut self, backlog: Backlog, now: Instant) -> bool {
        self.observe_backlog(backlog, now);
        if self.stopped || !backlog.has_work() {
            return false;
        }
        self.startup_pending = true;
        true
    }

    /// 启动合并是否还等着第一拍兑现（测试断言用）。
    #[must_use]
    pub const fn startup_queued(&self) -> bool {
        self.startup_pending
    }

    /// 刷新 durable 待办计数（宿主节流后调用；状态行/续批判据都读它）。
    pub fn observe_backlog(&mut self, backlog: Backlog, now: Instant) {
        self.status.outbox = backlog.outbox;
        self.status.inbox = backlog.inbox;
        self.next_status_refresh = Some(now + STATUS_REFRESH);
    }

    /// 计数刷新是否到期（宿主用它决定要不要再查一次 SQLite）。
    #[must_use]
    pub fn status_refresh_due(&self, now: Instant) -> bool {
        self.next_status_refresh.is_none_or(|at| now >= at)
    }

    /// 窗口激活变化：失焦降到 60s；重新聚焦时若有 durable 待办，合并一次探测。
    pub fn on_activation(&mut self, active: bool, now: Instant) {
        self.focus_active = active;
        let period = if active {
            self.config.period
        } else {
            self.config.unfocused_period
        };
        self.scheduler.set_period(period, now);
        if active && !self.stopped && self.status.inbox > 0 && self.origin.is_none() {
            let _ = self.scheduler.on_network_recovered(now);
        }
    }

    /// 宿主把"外部提交闸门/单操作位被占用"转成调度器的 busy 标记。
    pub fn set_host_busy(&mut self, busy: bool) {
        self.scheduler.set_host_busy(busy);
    }

    /// 本地编辑已 `DurableAck`：尾缘 2s debounce（远端 adopt 不触发）。
    pub fn on_local_durable(&mut self, now: Instant) -> Decision {
        self.local_edit_pending = false;
        if self.stopped {
            return Decision::Wait(SkipReason::Paused);
        }
        self.scheduler
            .on_local_edit(LocalEditAck::local_durable(), now)
    }

    /// 有一个本地编辑但还没 durable：等它的 ACK（同一次编辑只记一次）。
    pub fn note_local_edit_pending(&mut self) {
        self.local_edit_pending = true;
    }

    /// 是否有本地编辑还在等 `DurableAck`。
    #[must_use]
    pub const fn local_edit_pending(&self) -> bool {
        self.local_edit_pending
    }

    /// 启动合并（测试/宿主显式调用）：等价于第一拍兑现 [`Self::prime_startup`]。
    ///
    /// 生产不需要单独调它（[`Self::tick`] 会兑现），测试在"断言 Startup 决策"时用它。
    pub fn on_startup(&mut self, now: Instant) -> Decision {
        self.startup_pending = false;
        self.scheduler.on_startup(now)
    }

    /// 时间推进一拍（生产 ticker / 测试显式调用）：先兑现启动合并，再走调度决策。
    pub fn tick(&mut self, now: Instant) -> Decision {
        if self.stopped {
            return Decision::Wait(SkipReason::Paused);
        }
        if self.startup_pending {
            self.startup_pending = false;
            return self.scheduler.on_startup(now);
        }
        self.scheduler.on_tick(now)
    }

    /// 自动派发被宿主拒绝（Busy/Held/worker 停止）：合并成一条"稍后重试"。
    pub fn on_busy(&mut self, now: Instant) -> Decision {
        self.scheduler.on_busy_retry(now, BUSY_RETRY_DELAY)
    }

    /// 调度器已经决策 `Start`，但宿主在派发前发现单操作位被占（设备命令/关闭中）：
    /// 先把这次启动**退回**（不能在没派发的情况下假装 `Running`），再合并"稍后重试"。
    pub fn on_busy_after_start(&mut self, now: Instant) -> Decision {
        let _ = self.scheduler.on_finished_idle(Finished::Success, now);
        self.on_busy(now)
    }

    /// 用户手动点「同步」：最高优先级（可绕过一次网络退避，不能绕过单操作/Held）。
    ///
    /// - 自动轮在跑：只登记"合并"，返回 `Wait(Running)`（不派发第二条）；
    /// - "需要处理"的失败被用户显式重试：先清掉暂停再按手动语义启动。
    pub fn request_manual(&mut self, now: Instant) -> Decision {
        if self.origin == Some(OpOrigin::Auto) {
            self.manual_requested = true;
            return Decision::Wait(SkipReason::Running);
        }
        if let Some(origin) = self.origin {
            // 已经有一轮手动/发布在跑：单操作，不排队第二条。
            return Decision::Wait(match origin {
                OpOrigin::Manual | OpOrigin::Auto => SkipReason::Running,
            });
        }
        let decision = if self.scheduler.is_paused() {
            self.scheduler.retry_after_attention(now)
        } else {
            self.scheduler.on_manual(now)
        };
        if decision.is_start() {
            self.origin = Some(OpOrigin::Manual);
            self.status.frozen = None;
            self.status.state = StatusState::Syncing { auto: false };
        }
        decision
    }

    /// 派发被拒/没有真的跑起来：撤回本轮登记（不计失败、不退避）。
    pub fn abort_round(&mut self, now: Instant) {
        if self.origin.take().is_some() {
            self.round = None;
            self.gate_wait_since = None;
            self.status.state = StatusState::Idle;
            let _ = self.scheduler.on_finished_idle(Finished::Success, now);
        }
    }

    /// 自动轮在跑时用户点了「同步」：登记合并，不派发。返回 `true` = 已排队。
    pub fn queue_manual(&mut self) -> bool {
        if self.origin.is_some() {
            self.manual_requested = true;
            return true;
        }
        false
    }

    /// 取走"合并的手动请求"（自动轮收尾后宿主用它补一次手动轮）。
    pub fn take_manual_request(&mut self) -> bool {
        std::mem::take(&mut self.manual_requested)
    }

    /// 自动轮即将派发：登记起始记账与"同步中"状态。
    pub fn begin_auto_round(&mut self, trigger: Trigger, backlog: Backlog) {
        self.origin = Some(OpOrigin::Auto);
        self.round = Some(RoundTicket {
            trigger,
            before: backlog,
            stored: 0,
            deferred: 0,
            integrated: 0,
        });
        self.status.outbox = backlog.outbox;
        self.status.inbox = backlog.inbox;
        self.status.frozen = None;
        self.status.state = StatusState::Syncing { auto: true };
        self.gate_wait_since = None;
    }

    /// 手动轮即将派发：登记来源（手动轮不进自动退避）。
    pub fn begin_manual_round(&mut self, backlog: Backlog) {
        self.origin = Some(OpOrigin::Manual);
        self.round = Some(RoundTicket {
            trigger: Trigger::Manual,
            before: backlog,
            stored: 0,
            deferred: 0,
            integrated: 0,
        });
        self.status.outbox = backlog.outbox;
        self.status.inbox = backlog.inbox;
        self.status.frozen = None;
        self.status.state = StatusState::Syncing { auto: false };
        self.gate_wait_since = None;
    }

    /// worker 在 `NeedGate` 上报的计数（`stored` = 已接收待应用，`deferred` = 仍待应用）。
    pub fn note_need_gate(&mut self, stored: usize, deferred: usize) {
        if let Some(round) = self.round.as_mut() {
            round.stored = stored;
            round.deferred = deferred;
        }
        self.status.inbox = self.status.inbox.max(deferred as u64);
        self.status.deferred = deferred as u64;
    }

    /// worker 在 `Integrated` 上报的计数（本轮已应用 / 仍 deferred）。
    pub fn note_integrated(&mut self, integrated: usize, deferred: usize) {
        if let Some(round) = self.round.as_mut() {
            round.integrated = integrated;
            round.deferred = deferred;
        }
        self.status.deferred = deferred as u64;
    }

    /// 进入 NeedGate 的等待：宿主开始观察"输入法是否自然结束"。
    pub fn note_gate_wait(&mut self, now: Instant) {
        if self.gate_wait_since.is_none() {
            self.gate_wait_since = Some(now);
        }
    }

    /// NeedGate 之后等空闲的剩余允许时间（`None` = 还没开始等）。
    #[must_use]
    pub fn gate_wait_remaining(&self, now: Instant) -> Option<Duration> {
        let since = self.gate_wait_since?;
        Some(
            self.config
                .gate_idle_defer
                .saturating_sub(now.saturating_duration_since(since)),
        )
    }

    /// 等不到 IME 空闲 → defer 记账（**不强制结束组合**、不 append）。
    ///
    /// 这里只更新状态；single-op 槽与调度器要等 worker 的真实终态回来后由
    /// [`Self::finish_round`] 统一收尾（defer 不算失败、不退避）。
    pub fn note_gate_deferred(&mut self, remaining: u64) {
        self.status.deferred = remaining;
        if remaining > 0 {
            self.status.state = StatusState::NoProgress { remaining };
        }
    }

    /// 一轮收尾：更新状态 + 自动退避/续批（**不**在本拍重入派发）。
    pub fn finish_round(
        &mut self,
        outcome: RoundOutcome,
        backlog: Backlog,
        now: Instant,
    ) -> Decision {
        let origin = self.origin.take();
        let round = self.round.take();
        self.gate_wait_since = None;
        // 1) durable 事实优先：状态行显示的是库里的真实计数，不是本轮报告。
        self.status.outbox = backlog.outbox;
        self.status.inbox = backlog.inbox;
        self.status.deferred = round.as_ref().map_or(0, |round| round.deferred as u64);
        self.status.frozen = outcome.frozen.clone();

        if let Some(frozen) = outcome.frozen {
            // 交付失败仍持有闸门：保持冻结说明（fail-closed），不再自动重试。
            self.status.state = StatusState::NeedsAttention {
                reason: sanitized_reason(&frozen),
            };
            self.scheduler
                .on_finished(Finished::failure_permanent(sanitized_reason(&frozen)), now);
            return Decision::Idle;
        }

        let is_auto = origin == Some(OpOrigin::Auto);
        if outcome.success {
            self.status.state = StatusState::Synced {
                at: SystemTime::now(),
            };
            let settled = self.scheduler.on_finished_idle(Finished::Success, now);
            // 手动轮成功也是"用户确认过"：清掉自动退避（不计失败）。
            if !is_auto {
                return settled;
            }
            // 2) 跨操作公平续批：还有 durable 待应用才考虑继续；有进展才继续。
            let remaining = backlog.inbox;
            if remaining > 0 {
                let progress = round
                    .as_ref()
                    .is_some_and(|round| round_progress(round, backlog));
                if progress {
                    self.status.state = StatusState::Synced {
                        at: SystemTime::now(),
                    };
                    return self.scheduler.on_continuation(now, CONTINUATION_DELAY);
                }
                // 连续无进展：暂停这一轮，如实显示剩余（等新依赖/新数据）。
                self.status.state = StatusState::NoProgress { remaining };
                return Decision::Idle;
            }
            return settled;
        }

        // 3) defer（拿不到 IME 空闲）：不算失败、不退避；清掉本轮占用后稍后续批。
        if outcome.deferred_by_gate {
            let remaining = backlog.inbox;
            self.status.state = if remaining > 0 {
                StatusState::NoProgress { remaining }
            } else {
                StatusState::Synced {
                    at: SystemTime::now(),
                }
            };
            let settled = self.scheduler.on_finished_idle(Finished::Success, now);
            if remaining > 0 {
                return self.scheduler.on_continuation(now, CONTINUATION_DELAY);
            }
            return settled;
        }
        let message = outcome.error.unwrap_or_else(|| "同步失败".to_owned());
        let reason = sanitized_reason(&message);
        match classify_failure(&message) {
            FailureClass::Retryable => {
                let decision = self
                    .scheduler
                    .on_finished_idle(Finished::failure_retryable(reason), now);
                self.status.state = StatusState::Retry {
                    until: self.scheduler.backoff_until().unwrap_or(now),
                    attempt: self.scheduler.attempt(),
                };
                decision
            }
            FailureClass::NeedsAttention => {
                self.scheduler
                    .on_finished(Finished::failure_permanent(reason.clone()), now);
                self.status.state = StatusState::NeedsAttention { reason };
                Decision::Wait(SkipReason::Paused)
            }
        }
    }

    /// 一次"没有新派发"的收尾（例如交付失败后的重试收尾）成功：把状态行从
    /// 冻结/需要处理恢复成已同步。
    pub fn note_recovery(&mut self, backlog: Backlog, now: Instant) {
        self.status.frozen = None;
        self.status.outbox = backlog.outbox;
        self.status.inbox = backlog.inbox;
        self.status.state = StatusState::Synced {
            at: SystemTime::now(),
        };
        // 用户显式重试成功 = 之前的"需要处理"已解除。
        if self.scheduler.is_paused() {
            self.scheduler.resume(now);
        }
    }
}

/// 本轮"有没有进展"：应用了对象、或 durable 待应用真的变少了（不是"拉到了就算"）。
fn round_progress(round: &RoundTicket, backlog: Backlog) -> bool {
    round.integrated > 0 || backlog.inbox < round.before.inbox
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn failure_classification_is_conservative() {
        for retryable in [
            "同步失败（Prepare）：relay 传输失败：connection reset",
            "同步失败（Integrate）：head CAS 冲突：d 期望 1 实际 2",
            "同步失败（Prepare）：relay HTTP 状态 503 服务不可用",
            "同步失败（Prepare）：relay HTTP 状态 408",
            "同步失败（Prepare）：relay 拒绝：BUSY",
            "同步失败（Prepare）：relay 拒绝：INTERNAL",
        ] {
            assert_eq!(
                classify_failure(retryable),
                FailureClass::Retryable,
                "{retryable}"
            );
        }
        for permanent in [
            "同步失败（Prepare）：relay 拒绝：UNAUTHORIZED",
            "同步失败（Prepare）：relay 拒绝：EPOCH",
            "同步失败（Prepare）：TLS 校验失败：certificate expired",
            "同步失败（Prepare）：签名验证失败",
            "同步失败（Prepare）：同步暂停：游标过期",
            "同步失败（Prepare）：同步已取消：在外部集成前停止（未追加任何本地行）",
            "同步失败（Integrate）：结构投影失败：tree/notes",
            "未知错误",
        ] {
            assert_eq!(
                classify_failure(permanent),
                FailureClass::NeedsAttention,
                "{permanent}"
            );
        }
    }

    #[test]
    fn hhmm_is_utc_and_zero_padded() {
        let at = UNIX_EPOCH + Duration::from_secs(9 * 3600 + 5 * 60 + 59);
        assert_eq!(hhmm_utc(at), "09:05");
        assert_eq!(hhmm_utc(UNIX_EPOCH), "00:00");
    }

    #[test]
    fn status_line_separates_received_from_applied_and_never_shows_toasts() {
        let now = Instant::now();
        let mut status = SyncStatus::default();
        assert_eq!(status.line(now), "尚未同步");
        assert_eq!(status.error_line(), None, "空闲不显示打扰行");

        status.inbox = 3;
        assert_eq!(status.line(now), "尚未同步 · 已接收·待应用 3");
        assert_eq!(status.error_line(), None);

        status.outbox = 2;
        assert!(status.line(now).contains("待发送 2"));

        status.state = StatusState::Synced {
            at: UNIX_EPOCH + Duration::from_secs(8 * 3600 + 42 * 60),
        };
        assert_eq!(
            status.line(now),
            "已同步 08:42 · 已接收·待应用 3 · 待发送 2"
        );

        status.state = StatusState::Retry {
            until: now + Duration::from_secs(20),
            attempt: 2,
        };
        let line = status.line(now);
        assert!(line.starts_with("重试于 "), "{line}");
        assert!(line.contains("第 2 次失败"), "{line}");
        assert_eq!(status.error_line(), None, "普通退避不显示打扰行");

        status.state = StatusState::NeedsAttention {
            reason: "TLS 校验失败".to_owned(),
        };
        assert_eq!(status.line(now), "需要处理：TLS 校验失败");
        assert!(status
            .error_line()
            .expect("需要处理行")
            .contains("可点「同步」重试"));

        status.frozen = Some("交付已落盘变化失败".to_owned());
        assert!(status.line(now).contains("编辑器保持冻结"));
        assert!(status.error_line().expect("冻结行").contains("重试收尾"));
    }

    /// 测试时钟：`now = base + offset`（显式推进，不 sleep）。
    #[derive(Clone)]
    struct TestClock {
        base: Instant,
        offset: Arc<Mutex<Duration>>,
    }

    impl TestClock {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                offset: Arc::new(Mutex::new(Duration::ZERO)),
            }
        }

        fn advance(&self, by: Duration) {
            *self.offset.lock().expect("测试时钟锁") += by;
        }
    }

    impl OperationClock for TestClock {
        fn now(&self) -> Instant {
            self.base + *self.offset.lock().expect("测试时钟锁")
        }
    }

    fn auto_with(clock: &TestClock, config: AutoSyncConfig) -> AutoSync {
        AutoSync::new(config, Arc::new(clock.clone()), clock.now())
    }

    #[test]
    fn retryable_failures_back_off_with_injected_jitter_and_defer_does_not_count() {
        let clock = TestClock::new();
        let auto = auto_with(
            &clock,
            AutoSyncConfig::new()
                .no_ticker()
                .with_backoff_jitter(|attempt| Duration::from_millis(u64::from(attempt) * 1000)),
        );
        let mut auto = auto;
        let backlog = Backlog {
            outbox: 1,
            inbox: 0,
        };
        let trigger = Trigger::Startup;
        auto.begin_auto_round(trigger, backlog);
        let t0 = clock.now();
        let decision = auto.finish_round(
            RoundOutcome {
                success: false,
                error: Some("同步失败（Prepare）：relay 传输失败：reset".to_owned()),
                deferred_by_gate: false,
                frozen: None,
            },
            backlog,
            clock.now(),
        );
        assert!(matches!(decision, Decision::Wait(SkipReason::Backoff)));
        assert_eq!(auto.scheduler().attempt(), 1);
        assert_eq!(
            auto.scheduler().backoff_until(),
            Some(t0 + Duration::from_secs(5) + Duration::from_millis(1000)),
            "退避必须含注入的 jitter"
        );

        // 退避到期 → 第二轮（第 2 次失败：10s + 2s jitter）。
        clock.advance(Duration::from_secs(6));
        assert!(auto.tick(clock.now()).is_start());
        let t1 = clock.now();
        auto.begin_auto_round(Trigger::BackoffRetry, backlog);
        let second = auto.finish_round(
            RoundOutcome {
                success: false,
                error: Some("同步失败（Prepare）：relay HTTP 状态 503".to_owned()),
                deferred_by_gate: false,
                frozen: None,
            },
            backlog,
            t1,
        );
        assert!(!second.is_start(), "可重试失败不立刻启动：{second:?}");
        assert_eq!(auto.scheduler().attempt(), 2);
        assert_eq!(
            auto.scheduler().backoff_until(),
            Some(t1 + Duration::from_secs(10) + Duration::from_millis(2000))
        );

        // defer（拿不到 IME 空闲）不算失败、不退避。
        clock.advance(Duration::from_secs(20));
        assert!(auto.tick(clock.now()).is_start());
        auto.begin_auto_round(Trigger::BackoffRetry, backlog);
        auto.note_gate_wait(clock.now());
        auto.note_gate_deferred(1);
        let decision = auto.finish_round(
            RoundOutcome {
                success: false,
                error: None,
                deferred_by_gate: true,
                frozen: None,
            },
            Backlog {
                outbox: 1,
                inbox: 1,
            },
            clock.now(),
        );
        // defer 说明网络阶段其实是通的：退避计数清零、不进退避（不是"失败"）。
        assert_eq!(auto.scheduler().attempt(), 0, "defer 不累计失败");
        assert_eq!(auto.scheduler().backoff_until(), None, "defer 不退避");
        assert!(matches!(
            auto.status().state,
            StatusState::NoProgress { remaining: 1 }
        ));
        assert!(!decision.is_start(), "defer 只排续批，不立刻启动");
    }

    #[test]
    fn continuation_respects_the_five_second_minimum_and_stops_without_progress() {
        let clock = TestClock::new();
        let mut auto = auto_with(&clock, AutoSyncConfig::new().no_ticker());
        let backlog = Backlog {
            outbox: 0,
            inbox: 2,
        };
        let start = clock.now();
        // 必须由调度器真的启动一次（最小启动间隔才以这次为基准）。
        assert!(auto.on_startup(start).is_start());
        auto.begin_auto_round(Trigger::Startup, backlog);
        auto.note_integrated(1, 1);
        // 有进展（integrated>0）→ 续批 pending。
        let decision = auto.finish_round(
            RoundOutcome {
                success: true,
                error: None,
                deferred_by_gate: false,
                frozen: None,
            },
            Backlog {
                outbox: 0,
                inbox: 1,
            },
            start + Duration::from_secs(1),
        );
        assert!(!decision.is_start());
        assert!(
            auto.scheduler().next_deadline().is_some(),
            "续批必须留一个 pending"
        );
        // 最小启动间隔 5s：4s 后还不启动，5s 后才启动。
        assert!(!matches!(
            auto.tick(start + Duration::from_secs(4)),
            Decision::Start(_)
        ));
        let mut now = start + Duration::from_secs(5);
        let mut started = false;
        for _ in 0..6 {
            if auto.tick(now).is_start() {
                started = true;
                break;
            }
            now += Duration::from_secs(1);
        }
        assert!(started, "有进展时下一批必须启动");

        // 第二轮零进展 → 暂停这一轮，如实显示剩余，不再排续批。
        auto.begin_auto_round(
            Trigger::Continuation,
            Backlog {
                outbox: 0,
                inbox: 1,
            },
        );
        auto.note_integrated(0, 1);
        let decision = auto.finish_round(
            RoundOutcome {
                success: true,
                error: None,
                deferred_by_gate: false,
                frozen: None,
            },
            Backlog {
                outbox: 0,
                inbox: 1,
            },
            now + Duration::from_secs(1),
        );
        assert!(matches!(decision, Decision::Idle), "零进展不排续批");
        assert!(matches!(
            auto.status().state,
            StatusState::NoProgress { remaining: 1 }
        ));
        assert!(auto.status().line(now).contains("续批暂停：无进展"));
    }

    #[test]
    fn manual_merges_into_a_running_auto_round_and_is_never_preempted() {
        let clock = TestClock::new();
        let mut auto = auto_with(&clock, AutoSyncConfig::new().no_ticker());
        let backlog = Backlog {
            outbox: 1,
            inbox: 0,
        };
        auto.begin_auto_round(Trigger::Startup, backlog);
        // 自动轮在跑：手动点击只合并，不派发第二条。
        assert!(auto.queue_manual());
        assert!(auto.manual_queued());
        assert_eq!(
            auto.request_manual(clock.now()),
            Decision::Wait(SkipReason::Running)
        );
        assert!(auto.take_manual_request(), "合并的请求必须能被取走");
        assert!(!auto.manual_queued());
        // 收尾成功后回到"已同步"。
        let _ = auto.finish_round(
            RoundOutcome {
                success: true,
                error: None,
                deferred_by_gate: false,
                frozen: None,
            },
            Backlog {
                outbox: 0,
                inbox: 0,
            },
            clock.now(),
        );
        assert!(matches!(auto.status().state, StatusState::Synced { .. }));
        assert_eq!(auto.status().error_line(), None);
    }

    #[test]
    fn busy_after_start_does_not_leave_a_fake_running_state() {
        let clock = TestClock::new();
        let mut auto = auto_with(&clock, AutoSyncConfig::new().no_ticker());
        assert_eq!(auto.tick(clock.now()), Decision::Idle);
        // 调度器已经决策 Start（模拟宿主刚被设备命令占住）。
        auto.on_startup(clock.now());
        auto.on_busy_after_start(clock.now());
        assert!(!auto.scheduler().is_running(), "没有派发就不能留在 Running");
        assert!(
            auto.scheduler().next_deadline().is_some(),
            "必须留下一条稍后重试的 pending"
        );
    }

    #[test]
    fn status_refresh_is_throttled_to_one_second() {
        let clock = TestClock::new();
        let mut auto = auto_with(&clock, AutoSyncConfig::new().no_ticker());
        assert!(auto.status_refresh_due(clock.now()));
        auto.observe_backlog(
            Backlog {
                outbox: 2,
                inbox: 3,
            },
            clock.now(),
        );
        assert!(!auto.status_refresh_due(clock.now()));
        assert_eq!(auto.status().outbox, 2);
        assert_eq!(auto.status().inbox, 3);
        clock.advance(Duration::from_secs(1));
        assert!(auto.status_refresh_due(clock.now()));
    }

    #[test]
    fn status_line_shows_no_progress_with_real_remaining() {
        let now = Instant::now();
        let status = SyncStatus {
            state: StatusState::NoProgress { remaining: 7 },
            inbox: 7,
            deferred: 7,
            ..SyncStatus::default()
        };
        assert_eq!(status.line(now), "已接收·待应用 7（续批暂停：无进展）");
        assert_eq!(status.error_line(), None);
    }
}
