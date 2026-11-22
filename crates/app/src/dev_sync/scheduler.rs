//! **纯**自动同步调度器（ADR 0012 S1；S3 接线后仍不含 GPUI/线程/时钟）。
//!
//! ## 本片边界（硬约束）
//!
//! - **不连接真实时钟**：所有输入都带 `now: Instant`，测试显式推进（不 sleep、不 flaky）；
//! - **不碰网络/库/身份**：它只回答一个问题——"现在该不该启动一次自动同步"；
//! - 定时唤醒/实际派发由调用方（[`super::autosync`] + `AppShell`）负责。
//!
//! ## 状态
//!
//! | 状态 | 含义 |
//! | --- | --- |
//! | [`SchedulerState::Idle`] | 没有待办、没有退避 |
//! | [`SchedulerState::Scheduled`] | 有一个自动 pending（编辑 debounce / 周期 / 恢复 / 启动） |
//! | [`SchedulerState::Running`] | 已启动一次同步（调用方必须随后 `on_finished`） |
//! | [`SchedulerState::Backoff`] | 上一次失败可重试，等到点再试 |
//! | [`SchedulerState::Paused`] | 人工暂停（自动与手动都不能绕过） |
//!
//! ## 决策规则（与 ADR 0012「调度」一致）
//!
//! 1. **只有一个自动 pending**：多个来源合并成一条，取更高优先级的原因与更早的到期时间；
//! 2. **尾缘 2s debounce**：只有**本地**且已 `DurableAck` 的编辑才触发；远端 adopt 的 ACK
//!    一律不触发（防御性过滤）。编辑**不重置退避**；
//! 3. **周期 45s**（可配 30–60s），失焦降频留给 S3（本片只提供配置位）；
//! 4. **自动启动间隔 ≥5s**；失败退避 5/10/20/40/80/160/300s（封顶）+ 可注入 jitter；
//! 5. **手动**优先级最高：可**绕过一次**网络退避，但不能绕过单操作（[`Self::Running`]）、
//!    Held/繁忙（[`Self::set_host_busy`]）与 [`SchedulerState::Paused`]；
//! 6. 只有**明确 retryable** 的失败才退避重试；其余进入 `Paused`（需要人工处理）。
//!
//! ## 调用方契约
//!
//! `Decision::Start` 返回时调度器**已经**把状态置为 `Running` 并（自动路径）记录启动时刻；
//! 调用方必须真的启动一次同步，并在结束时调用 [`Scheduler::on_finished`]。如果派发被宿主
//! 拒绝（例如 worker 已经忙），调用方应把它当作一次失败传给 `on_finished`（通常
//! `retryable = false`，避免自动重试掩盖真实问题）。

use std::fmt;
use std::time::{Duration, Instant};

/// 默认周期（ADR 0012：45s）。
pub const DEFAULT_PERIOD: Duration = Duration::from_secs(45);
/// 周期下界（ADR 0012：可配 30–60s）。
pub const PERIOD_MIN: Duration = Duration::from_secs(30);
/// 周期上界。
pub const PERIOD_MAX: Duration = Duration::from_secs(60);
/// 默认编辑 debounce（尾缘 2s）。
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(2);
/// 自动启动最小间隔（ADR 0012：至少 5s）。
pub const DEFAULT_MIN_START_INTERVAL: Duration = Duration::from_secs(5);
/// 退避基数（5/10/20/40…）。
pub const BACKOFF_BASE: Duration = Duration::from_secs(5);
/// 退避上限（300s）。
pub const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// 一次自动同步的触发来源（也是 pending 合并时的优先级依据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// 周期探测（最低优先级）。
    Periodic,
    /// 网络恢复/重新聚焦合并的探测。
    NetworkRecovered,
    /// 一次 Integrate 因批次预算/依赖留下 deferred：公平续批（ADR 0012 S3）。
    Continuation,
    /// 自动请求遇到 Held/单操作在途：稍后重试（不抢已开始的事务）。
    BusyRetry,
    /// 本地编辑 debounce 到期。
    LocalEdit,
    /// 启动（身份解锁、初始化完成、进入 Main）合并一次。
    Startup,
    /// 失败退避到期（自动重试）。
    BackoffRetry,
    /// 用户手动点「同步」（最高优先级）。
    Manual,
}

impl Trigger {
    /// 合并 pending 时的优先级（数值越大越优先）。
    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::Periodic => 1,
            // 退避重试比普通周期更有意义：两者同时到期时报告"退避重试"。
            Self::BackoffRetry => 2,
            // 网络恢复 / 稍后重试 / 续批都是"有明确外部理由"的探测，优先于纯周期。
            Self::NetworkRecovered | Self::BusyRetry | Self::Continuation => 3,
            Self::LocalEdit => 4,
            Self::Startup => 5,
            Self::Manual => 6,
        }
    }

    /// 中文标签（面板/日志用；本片不渲染）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Periodic => "周期",
            Self::BackoffRetry => "退避重试",
            Self::NetworkRecovered => "网络恢复",
            Self::BusyRetry => "稍后重试",
            Self::Continuation => "续批",
            Self::LocalEdit => "本地编辑",
            Self::Startup => "启动",
            Self::Manual => "手动",
        }
    }
}

/// 编辑来源：只有**本地**编辑能触发自动同步（远端 adopt 的 ACK 不触发）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOrigin {
    /// 本机编辑器产生的编辑。
    Local,
    /// 远端 adopt / 集成后产生的 ACK（**不得**触发自动同步）。
    RemoteAdopted,
}

/// 一次编辑的 ACK 事实（只有 `origin = Local` 且 `durable = true` 才触发）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalEditAck {
    /// 编辑来源。
    pub origin: EditOrigin,
    /// 目标编辑是否已经 `DurableAck`（未落盘的编辑不能作为同步基准）。
    pub durable: bool,
}

impl LocalEditAck {
    /// 本机编辑已 durable（唯一会触发 debounce 的形态）。
    #[must_use]
    pub const fn local_durable() -> Self {
        Self {
            origin: EditOrigin::Local,
            durable: true,
        }
    }

    /// 本机编辑但还没 durable（尾缘 debounce 要等下一次 durable ACK）。
    #[must_use]
    pub const fn local_pending() -> Self {
        Self {
            origin: EditOrigin::Local,
            durable: false,
        }
    }

    /// 远端 adopt 的 ACK（**不触发**）。
    #[must_use]
    pub const fn remote_adopted() -> Self {
        Self {
            origin: EditOrigin::RemoteAdopted,
            durable: true,
        }
    }

    /// 是否满足"触发自动同步"的条件。
    #[must_use]
    pub const fn triggers_autosync(self) -> bool {
        matches!(self.origin, EditOrigin::Local) && self.durable
    }
}

/// 一次同步的结局（`on_finished` 的输入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finished {
    /// 成功：清零退避与失败计数。
    Success,
    /// 失败：只有 `retryable = true` 才会自动退避重试。
    Failure {
        /// 是否明确可重试（传输失败/并发冲突等）。
        retryable: bool,
        /// 脱敏原因（面板/日志用，不含 secret）。
        reason: String,
    },
}

impl Finished {
    /// 可重试失败。
    #[must_use]
    pub fn failure_retryable(reason: impl Into<String>) -> Self {
        Self::Failure {
            retryable: true,
            reason: reason.into(),
        }
    }

    /// 不可重试失败（鉴权/TLS/签名/epoch/游标异常等）。
    #[must_use]
    pub fn failure_permanent(reason: impl Into<String>) -> Self {
        Self::Failure {
            retryable: false,
            reason: reason.into(),
        }
    }
}

/// 不启动的原因（诊断/测试断言用；面板措辞留给 S3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 还没到 pending 的到期时间。
    NotDue,
    /// 上一次同步还在跑（单操作在途）。
    Running,
    /// 已人工暂停。
    Paused,
    /// 安全暂停（信任/TLS/pin 类）：不自动重试，手动也被拒绝（ADR 0013 §4）。
    SecurityPaused,
    /// 宿主的单操作位/外部提交闸门被占用（Held）。
    HostBusy,
    /// 网络退避还没到期。
    Backoff,
    /// 距离上一次自动启动不足 5s。
    MinStartInterval,
    /// 这次 ACK 不该触发（远端 adopt / 未 durable）。
    NotEligible,
    /// 什么都没有可做。
    NoRequest,
}

impl SkipReason {
    /// 中文短标签（面板/状态行/日志用；不含 secret）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NotDue => "还没到时间",
            Self::Running => "单操作在途",
            Self::Paused => "需要人工处理",
            Self::SecurityPaused => {
                "安全暂停：请到设置 → 安全指纹核对后处理（不会自动清 pin/信任）"
            }
            Self::HostBusy => "持闸/单操作位被占用",
            Self::Backoff => "退避中",
            Self::MinStartInterval => "距上次自动启动不足 5s",
            Self::NotEligible => "这次编辑不该触发",
            Self::NoRequest => "没有待办",
        }
    }
}

/// 调度器给出的决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 现在启动一次自动同步（调用方随后必须 `on_finished`）。
    Start(Trigger),
    /// 不启动，原因见 [`SkipReason`]。
    Wait(SkipReason),
    /// 空闲（没有 pending、没有退避、没有运行）。
    Idle,
}

impl Decision {
    /// 是否要求启动。
    #[must_use]
    pub const fn is_start(self) -> bool {
        matches!(self, Self::Start(_))
    }
}

/// 调度器对外的状态投影。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerState {
    /// 空闲。
    Idle,
    /// 有一个自动 pending，等 `due` 到期。
    Scheduled {
        /// 到期时间。
        due: Instant,
        /// 合并后的最高优先级原因。
        trigger: Trigger,
    },
    /// 已启动一次同步。
    Running,
    /// 可重试失败后的退避窗口。
    Backoff {
        /// 退避到期时间。
        until: Instant,
        /// 连续第几次失败（1 = 第一次）。
        attempt: u32,
    },
    /// 已人工暂停。
    Paused {
        /// 暂停原因（脱敏）。
        reason: String,
    },
}

impl SchedulerState {
    /// 中文短标签。
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Idle => "空闲".to_owned(),
            Self::Scheduled { trigger, .. } => format!("待同步（{}）", trigger.label()),
            Self::Running => "同步中".to_owned(),
            Self::Backoff { attempt, .. } => format!("退避中（第 {attempt} 次失败）"),
            Self::Paused { reason } => format!("已暂停：{reason}"),
        }
    }
}

impl fmt::Display for SchedulerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

/// 调度器配置（周期/debounce/最小间隔/退避 jitter）。
#[derive(Debug, Clone, Copy)]
pub struct SchedulerConfig {
    /// 周期探测间隔（构造时夹到 30–60s）。
    pub period: Duration,
    /// 尾缘 debounce。
    pub debounce: Duration,
    /// 两次**自动**启动之间的最小间隔。
    pub min_start_interval: Duration,
    /// 退避 jitter 注入（`attempt` = 第几次失败；返回额外延迟）。
    ///
    /// 用 `fn` 指针而不是 trait object：配置可 `Copy`、测试可确定性注入。
    pub backoff_jitter: fn(u32) -> Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// 手工比较：`backoff_jitter` 是 `fn` 指针，直接比地址没有意义（且会触发编译器警告）。
impl PartialEq for SchedulerConfig {
    fn eq(&self, other: &Self) -> bool {
        self.period == other.period
            && self.debounce == other.debounce
            && self.min_start_interval == other.min_start_interval
    }
}

impl Eq for SchedulerConfig {}

impl SchedulerConfig {
    /// 默认配置（45s / 2s / 5s / 无 jitter）。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            period: DEFAULT_PERIOD,
            debounce: DEFAULT_DEBOUNCE,
            min_start_interval: DEFAULT_MIN_START_INTERVAL,
            backoff_jitter: |_| Duration::ZERO,
        }
    }

    /// 换周期（夹到 [`PERIOD_MIN`]..=[`PERIOD_MAX`]）。
    #[must_use]
    pub fn with_period(mut self, period: Duration) -> Self {
        self.period = period.clamp(PERIOD_MIN, PERIOD_MAX);
        self
    }

    /// 换 debounce。
    #[must_use]
    pub fn with_debounce(mut self, debounce: Duration) -> Self {
        self.debounce = debounce;
        self
    }

    /// 换可重试失败的退避 jitter（测试注入确定性序列）。
    #[must_use]
    pub fn with_backoff_jitter(mut self, jitter: fn(u32) -> Duration) -> Self {
        self.backoff_jitter = jitter;
        self
    }

    /// 第 `attempt` 次失败（1 起）该退避多久：5/10/20/40/80/160/300s（封顶）。
    #[must_use]
    pub fn backoff_for(attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(16);
        let factor = 1_u32 << shift;
        BACKOFF_BASE.saturating_mul(factor).min(BACKOFF_MAX)
    }

    /// 第 `attempt` 次失败的实际退避（含注入的 jitter）。
    #[must_use]
    pub fn backoff_with_jitter(&self, attempt: u32) -> Duration {
        Self::backoff_for(attempt) + (self.backoff_jitter)(attempt)
    }
}

/// 一个自动 pending（同一时刻只有一个）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    due: Instant,
    trigger: Trigger,
}

/// 退避窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BackoffWindow {
    until: Instant,
}

/// 纯自动同步调度器。
#[derive(Debug, Clone)]
pub struct Scheduler {
    config: SchedulerConfig,
    running: bool,
    paused: Option<String>,
    pending: Option<Pending>,
    backoff: Option<BackoffWindow>,
    /// 连续可重试失败次数（成功清零；编辑不重置）。
    attempt: u32,
    next_periodic: Instant,
    last_auto_start: Option<Instant>,
    host_busy: bool,
    /// 本次失败序列里是否已经用掉"手动绕过一次网络退避"（成功恢复）。
    manual_bypass_used: bool,
}

impl Scheduler {
    /// 以"现在"为起点构造（周期从此刻开始计）。
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self::with_config(SchedulerConfig::new(), now)
    }

    /// 以显式配置构造。
    #[must_use]
    pub fn with_config(config: SchedulerConfig, now: Instant) -> Self {
        Self {
            config,
            running: false,
            paused: None,
            pending: None,
            backoff: None,
            attempt: 0,
            next_periodic: now + config.period,
            last_auto_start: None,
            host_busy: false,
            manual_bypass_used: false,
        }
    }

    /// 当前配置。
    #[must_use]
    pub const fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// 状态投影。
    #[must_use]
    pub fn state(&self) -> SchedulerState {
        if self.running {
            return SchedulerState::Running;
        }
        if let Some(reason) = &self.paused {
            return SchedulerState::Paused {
                reason: reason.clone(),
            };
        }
        if let Some(backoff) = self.backoff {
            return SchedulerState::Backoff {
                until: backoff.until,
                attempt: self.attempt,
            };
        }
        if let Some(pending) = self.pending {
            return SchedulerState::Scheduled {
                due: pending.due,
                trigger: pending.trigger,
            };
        }
        SchedulerState::Idle
    }

    /// 下一次需要外部时钟唤醒的时间（S3 用来安排 ticker；本片不接线）。
    ///
    /// `Running` / `Paused` 时返回 `None`：这两态不需要自动唤醒（`Paused` 要人工恢复）。
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.running || self.paused.is_some() {
            return None;
        }
        let mut next = self.next_periodic;
        if let Some(pending) = self.pending {
            next = next.min(pending.due);
        }
        if let Some(backoff) = self.backoff {
            next = next.min(backoff.until);
        }
        Some(next)
    }

    /// 宿主的单操作位 / 外部提交闸门被占用（Held）时置位：自动/手动都不得启动。
    pub fn set_host_busy(&mut self, busy: bool) {
        self.host_busy = busy;
    }

    /// 是否被宿主标记为繁忙。
    #[must_use]
    pub const fn host_busy(&self) -> bool {
        self.host_busy
    }

    /// 人工暂停（自动与手动都不能绕过；正在跑的同步会先跑完）。
    pub fn pause(&mut self, reason: impl Into<String>) {
        self.paused = Some(reason.into());
    }

    /// 恢复（返回恢复后的决策：有待办且到期就可能立刻启动）。
    pub fn resume(&mut self, now: Instant) -> Decision {
        self.paused = None;
        self.decide(now)
    }

    /// 是否已暂停。
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.is_some()
    }

    /// 是否已启动过一次自动同步（`Running`）。
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.running
    }

    /// 连续可重试失败次数（成功清零）。
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// 当前退避窗口（`None` = 不在退避）。
    #[must_use]
    pub fn backoff_until(&self) -> Option<Instant> {
        self.backoff.map(|window| window.until)
    }

    /// 切换周期（失焦 60s / 聚焦 45s）。夹到 30–60s。
    ///
    /// 语义：`next_periodic - 旧周期` 就是"上一次周期点"，用同一个基准重新对齐到新周期；
    /// 已经错过的周期不会被一次性补发（[`Self::on_tick`] 仍只合并一条 pending）。
    pub fn set_period(&mut self, period: Duration, now: Instant) {
        let period = period.clamp(PERIOD_MIN, PERIOD_MAX);
        if period == self.config.period {
            return;
        }
        let previous = self
            .next_periodic
            .checked_sub(self.config.period)
            .unwrap_or(now);
        self.config.period = period;
        self.next_periodic = previous + period;
        if self.next_periodic <= now {
            self.next_periodic = now + period;
        }
    }

    /// 跨操作公平续批：上一轮留下 deferred/待应用，稍后（`delay`）再跑一批。
    ///
    /// 与编辑/周期一样合并进**唯一**的 pending（不会因为连续续批而并发）。
    pub fn on_continuation(&mut self, now: Instant, delay: Duration) -> Decision {
        self.merge_request(Pending {
            due: now + delay,
            trigger: Trigger::Continuation,
        });
        self.decide(now)
    }

    /// 自动请求遇到 Held/单操作在途：稍后重试（不抢占已开始的事务）。
    pub fn on_busy_retry(&mut self, now: Instant, delay: Duration) -> Decision {
        self.merge_request(Pending {
            due: now + delay,
            trigger: Trigger::BusyRetry,
        });
        self.decide(now)
    }

    /// 用户手动点「重试」：显式确认过"需要处理"的失败后，按手动语义再启动一次。
    ///
    /// 与 [`Self::on_manual`] 的差别只有一个：先清掉 `Paused`（那是不可重试失败的落点）。
    /// 单操作/Held 仍然挡住；退避仍只能绕过一次。
    pub fn retry_after_attention(&mut self, now: Instant) -> Decision {
        self.paused = None;
        self.on_manual(now)
    }

    /// 启动合并一次（身份解锁/初始化完成/进入 Main）。
    pub fn on_startup(&mut self, now: Instant) -> Decision {
        self.merge_request(Pending {
            due: now,
            trigger: Trigger::Startup,
        });
        self.decide(now)
    }

    /// 本地编辑的 `DurableAck`：尾缘 debounce（只有本地 + durable 才算）。
    pub fn on_local_edit(&mut self, ack: LocalEditAck, now: Instant) -> Decision {
        if !ack.triggers_autosync() {
            return Decision::Wait(SkipReason::NotEligible);
        }
        self.merge_request(Pending {
            due: now + self.config.debounce,
            trigger: Trigger::LocalEdit,
        });
        self.decide(now)
    }

    /// 网络恢复/重新聚焦：合并一次探测（调用方只在"有待发送/待应用内容"时调用）。
    pub fn on_network_recovered(&mut self, now: Instant) -> Decision {
        self.merge_request(Pending {
            due: now,
            trigger: Trigger::NetworkRecovered,
        });
        self.decide(now)
    }

    /// 手动同步：最高优先级，可绕过**一次**网络退避，但不能绕过单操作/Held/Paused。
    pub fn on_manual(&mut self, now: Instant) -> Decision {
        if self.running {
            return Decision::Wait(SkipReason::Running);
        }
        if self.paused.is_some() {
            return Decision::Wait(SkipReason::Paused);
        }
        if self.host_busy {
            return Decision::Wait(SkipReason::HostBusy);
        }
        if let Some(backoff) = &self.backoff {
            if now < backoff.until {
                if self.manual_bypass_used {
                    return Decision::Wait(SkipReason::Backoff);
                }
                self.manual_bypass_used = true;
            }
        }
        self.pending = None;
        self.backoff = None;
        self.running = true;
        Decision::Start(Trigger::Manual)
    }

    /// 时间推进一拍：推进周期、到期退避、以及到期的 pending。
    pub fn on_tick(&mut self, now: Instant) -> Decision {
        // 周期：即使 running/paused 也推进（恢复时不该把错过的周期一次性补发）。
        if now >= self.next_periodic {
            let due = self.next_periodic;
            self.merge_request(Pending {
                due,
                trigger: Trigger::Periodic,
            });
            loop {
                self.next_periodic += self.config.period;
                if self.next_periodic > now {
                    break;
                }
            }
        }
        // 退避到期：把它变成一个可启动的 pending（仍然走 decide 的检查）。
        if self.backoff.is_some_and(|backoff| now >= backoff.until) {
            self.backoff = None;
            self.merge_request(Pending {
                due: now,
                trigger: Trigger::BackoffRetry,
            });
        }
        self.decide(now)
    }

    /// 一次同步结束（成功清零退避；可重试失败进入 5/10/20/40…300s 退避）。
    pub fn on_finished(&mut self, finished: Finished, now: Instant) -> Decision {
        self.running = false;
        match finished {
            Finished::Success => {
                self.attempt = 0;
                self.backoff = None;
                self.manual_bypass_used = false;
                // 运行期间到达的编辑/周期请求：成功收尾后立刻可以合并成下一轮。
                self.decide(now)
            }
            Finished::Failure { retryable, reason } => {
                if !retryable {
                    // 鉴权/TLS/签名/epoch/游标异常：进入需处理状态，不自动重试。
                    self.paused = Some(reason);
                    return Decision::Wait(SkipReason::Paused);
                }
                self.attempt = self.attempt.saturating_add(1);
                let delay = self.config.backoff_with_jitter(self.attempt);
                self.backoff = Some(BackoffWindow { until: now + delay });
                Decision::Wait(SkipReason::Backoff)
            }
        }
    }

    /// [`Self::on_finished`] 的 S3 变体：**不在本拍重入派发**。
    ///
    /// 与 `on_finished` 的唯一差别：如果运行期间合并进来的 pending 已经到期（成功收尾
    /// 会立刻 `decide` 成 `Start`），这里把它退回成一条 `due = now` 的 pending 并返回
    /// `Wait(NotDue)`——收尾路径（adopt/end、面板回调）不该在同一调用栈里再启动一轮。
    /// 下一次 [`Self::on_tick`] 会照常把它启动。
    pub fn on_finished_idle(&mut self, finished: Finished, now: Instant) -> Decision {
        let last_start = self.last_auto_start;
        let decision = self.on_finished(finished, now);
        if let Decision::Start(trigger) = decision {
            self.running = false;
            self.last_auto_start = last_start;
            self.merge_request(Pending { due: now, trigger });
            return Decision::Wait(SkipReason::NotDue);
        }
        decision
    }

    /// 合并进唯一的 pending：更高优先级的原因胜出，到期时间取更早者。
    fn merge_request(&mut self, request: Pending) {
        match &mut self.pending {
            Some(pending) => {
                // 尾缘 debounce：连续的本地编辑**重算**到期时间（最后一次编辑 + debounce）。
                if pending.trigger == Trigger::LocalEdit && request.trigger == Trigger::LocalEdit {
                    pending.due = request.due;
                    return;
                }
                if request.trigger.priority() > pending.trigger.priority() {
                    pending.trigger = request.trigger;
                }
                pending.due = pending.due.min(request.due);
            }
            None => self.pending = Some(request),
        }
    }

    /// 公共决策：运行中/暂停/繁忙/退避都先挡住，再看 pending 与最小启动间隔。
    fn decide(&mut self, now: Instant) -> Decision {
        if self.running {
            return Decision::Wait(SkipReason::Running);
        }
        if self.paused.is_some() {
            return Decision::Wait(SkipReason::Paused);
        }
        if self.host_busy {
            return Decision::Wait(SkipReason::HostBusy);
        }
        if let Some(backoff) = &self.backoff {
            if now < backoff.until {
                return Decision::Wait(SkipReason::Backoff);
            }
        }
        let Some(pending) = self.pending else {
            return Decision::Idle;
        };
        if pending.due > now {
            return Decision::Wait(SkipReason::NotDue);
        }
        if let Some(last) = self.last_auto_start {
            if now < last + self.config.min_start_interval {
                return Decision::Wait(SkipReason::MinStartInterval);
            }
        }
        self.pending = None;
        self.backoff = None;
        self.running = true;
        self.last_auto_start = Some(now);
        Decision::Start(pending.trigger)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试时钟：只是一个基准 `Instant`，所有输入显式传 `base + Duration`（不 sleep）。
    fn base() -> Instant {
        Instant::now()
    }

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn local_edit_is_trailing_edge_debounced_and_remote_acks_do_not_trigger() {
        let t = base();
        let mut scheduler = Scheduler::new(t);

        // 远端 adopt 的 ACK / 未 durable 的本地编辑：一律不触发。
        assert_eq!(
            scheduler.on_local_edit(LocalEditAck::remote_adopted(), t),
            Decision::Wait(SkipReason::NotEligible)
        );
        assert_eq!(
            scheduler.on_local_edit(LocalEditAck::local_pending(), t),
            Decision::Wait(SkipReason::NotEligible)
        );
        assert_eq!(scheduler.state(), SchedulerState::Idle);

        // 三次本地编辑：尾缘从最后一次算 2s。
        assert_eq!(
            scheduler.on_local_edit(LocalEditAck::local_durable(), t),
            Decision::Wait(SkipReason::NotDue)
        );
        assert_eq!(
            scheduler.on_local_edit(LocalEditAck::local_durable(), t + secs(1)),
            Decision::Wait(SkipReason::NotDue)
        );
        let last_edit = t + Duration::from_millis(1900);
        assert_eq!(
            scheduler.on_local_edit(LocalEditAck::local_durable(), last_edit),
            Decision::Wait(SkipReason::NotDue)
        );
        assert_eq!(
            scheduler.state(),
            SchedulerState::Scheduled {
                due: last_edit + secs(2),
                trigger: Trigger::LocalEdit,
            },
            "pending 必须只剩一条，到期时间按最后一次编辑重算"
        );

        // 1.9s 后还没到；到点才启动（且只启动一次）。
        assert_eq!(
            scheduler.on_tick(last_edit + Duration::from_millis(1990)),
            Decision::Wait(SkipReason::NotDue)
        );
        assert_eq!(
            scheduler.on_tick(last_edit + secs(2)),
            Decision::Start(Trigger::LocalEdit)
        );
        assert_eq!(scheduler.state(), SchedulerState::Running);
        assert_eq!(
            scheduler.on_tick(last_edit + secs(3)),
            Decision::Wait(SkipReason::Running),
            "运行中不能重复启动"
        );
    }

    #[test]
    fn periodic_runs_every_45_seconds_by_default_and_clamps_config() {
        let t = base();
        let mut scheduler = Scheduler::new(t);
        assert_eq!(scheduler.config().period, secs(45));

        assert_eq!(scheduler.on_tick(t + secs(44)), Decision::Idle);
        assert_eq!(
            scheduler.on_tick(t + secs(45)),
            Decision::Start(Trigger::Periodic)
        );
        assert_eq!(
            scheduler.on_finished(Finished::Success, t + secs(45)),
            Decision::Idle
        );
        assert_eq!(scheduler.on_tick(t + secs(89)), Decision::Idle);
        assert_eq!(
            scheduler.on_tick(t + secs(90)),
            Decision::Start(Trigger::Periodic)
        );

        // 周期可配 30–60s：越界被夹住。
        assert_eq!(
            SchedulerConfig::new().with_period(secs(10)).period,
            PERIOD_MIN
        );
        assert_eq!(
            SchedulerConfig::new().with_period(secs(120)).period,
            PERIOD_MAX
        );
        let mut custom = Scheduler::with_config(SchedulerConfig::new().with_period(secs(30)), t);
        assert_eq!(
            custom.on_tick(t + secs(30)),
            Decision::Start(Trigger::Periodic)
        );
    }

    #[test]
    fn retryable_failures_back_off_five_ten_twenty_up_to_three_hundred() {
        let t = base();
        let mut scheduler = Scheduler::new(t);
        assert_eq!(
            scheduler.on_startup(t),
            Decision::Start(Trigger::Startup),
            "启动合并一次"
        );

        let mut expected = [5_u64, 10, 20, 40, 80, 160, 300];
        let mut now = t;
        for (index, delay) in expected.iter_mut().enumerate() {
            now += secs(1);
            assert_eq!(
                scheduler.on_finished(Finished::failure_retryable("relay 不可达"), now),
                Decision::Wait(SkipReason::Backoff)
            );
            assert_eq!(
                scheduler.state(),
                SchedulerState::Backoff {
                    until: now + secs(*delay),
                    attempt: (index + 1) as u32,
                },
                "第 {} 次失败的退避必须按序列递增",
                index + 1
            );
            // 退避没到期不能启动。
            assert_eq!(
                scheduler.on_tick(now + secs(*delay) - Duration::from_millis(1)),
                Decision::Wait(SkipReason::Backoff)
            );
            assert_eq!(
                scheduler.on_tick(now + secs(*delay)),
                Decision::Start(Trigger::BackoffRetry)
            );
            now += secs(*delay);
        }
        // 成功清零：下一次失败从 5s 重新开始。
        assert_eq!(
            scheduler.on_finished(Finished::Success, now + secs(1)),
            Decision::Idle
        );
        assert_eq!(
            scheduler.on_finished(Finished::failure_retryable("again"), now + secs(2)),
            Decision::Wait(SkipReason::Backoff)
        );
        assert_eq!(
            scheduler.state(),
            SchedulerState::Backoff {
                until: now + secs(2) + secs(5),
                attempt: 1,
            }
        );
    }

    #[test]
    fn jitter_is_injectable_and_added_to_the_backoff() {
        let t = base();
        let config = SchedulerConfig::new()
            .with_backoff_jitter(|attempt| Duration::from_millis(u64::from(attempt) * 100));
        let mut scheduler = Scheduler::with_config(config, t);
        assert_eq!(scheduler.on_startup(t), Decision::Start(Trigger::Startup));
        assert_eq!(
            scheduler.on_finished(Finished::failure_retryable("x"), t + secs(1)),
            Decision::Wait(SkipReason::Backoff)
        );
        assert_eq!(
            scheduler.state(),
            SchedulerState::Backoff {
                until: t + secs(1) + secs(5) + Duration::from_millis(100),
                attempt: 1,
            }
        );
    }

    #[test]
    fn manual_bypasses_the_network_backoff_exactly_once() {
        let t = base();
        let mut scheduler = Scheduler::new(t);
        assert!(scheduler.on_startup(t).is_start());
        assert_eq!(
            scheduler.on_finished(Finished::failure_retryable("relay"), t + secs(1)),
            Decision::Wait(SkipReason::Backoff)
        );
        // 第一次手动：绕过 5s 退避。
        assert_eq!(
            scheduler.on_manual(t + secs(2)),
            Decision::Start(Trigger::Manual)
        );
        // 手动失败仍累计失败次数（下一次退避 10s），且本窗口的绕过已用掉。
        assert_eq!(
            scheduler.on_finished(Finished::failure_retryable("relay"), t + secs(3)),
            Decision::Wait(SkipReason::Backoff)
        );
        assert_eq!(
            scheduler.state(),
            SchedulerState::Backoff {
                until: t + secs(3) + secs(10),
                attempt: 2,
            }
        );
        assert_eq!(
            scheduler.on_manual(t + secs(4)),
            Decision::Wait(SkipReason::Backoff),
            "同一退避窗口只能绕过一次"
        );
        // 退避到期后自动重试。
        assert_eq!(
            scheduler.on_tick(t + secs(13)),
            Decision::Start(Trigger::BackoffRetry)
        );
        // 运行中手动也不抢跑（单操作）。
        assert_eq!(
            scheduler.on_manual(t + secs(14)),
            Decision::Wait(SkipReason::Running)
        );
    }

    #[test]
    fn paused_blocks_every_trigger_until_explicit_resume() {
        let t = base();
        let mut scheduler = Scheduler::new(t);
        scheduler.pause("用户暂停");
        assert_eq!(
            scheduler.state(),
            SchedulerState::Paused {
                reason: "用户暂停".to_owned()
            }
        );
        assert_eq!(
            scheduler.on_manual(t + secs(1)),
            Decision::Wait(SkipReason::Paused)
        );
        assert_eq!(
            scheduler.on_local_edit(LocalEditAck::local_durable(), t + secs(2)),
            Decision::Wait(SkipReason::Paused)
        );
        assert_eq!(
            scheduler.on_tick(t + secs(45)),
            Decision::Wait(SkipReason::Paused)
        );
        // 恢复：暂停期间合并的编辑 pending 还在，到期即可启动。
        assert_eq!(
            scheduler.resume(t + secs(46)),
            Decision::Start(Trigger::LocalEdit)
        );
    }

    #[test]
    fn automatic_starts_are_at_least_five_seconds_apart() {
        let t = base();
        let mut scheduler = Scheduler::new(t);
        assert!(scheduler.on_startup(t).is_start());
        assert_eq!(
            scheduler.on_finished(Finished::Success, t + secs(1)),
            Decision::Idle
        );
        // 编辑 debounce 2s：t+3 到期，但距上一次自动启动只有 3s。
        assert_eq!(
            scheduler.on_local_edit(LocalEditAck::local_durable(), t + secs(1)),
            Decision::Wait(SkipReason::NotDue)
        );
        assert_eq!(
            scheduler.on_tick(t + secs(3)),
            Decision::Wait(SkipReason::MinStartInterval)
        );
        assert_eq!(
            scheduler.on_tick(t + secs(5)),
            Decision::Start(Trigger::LocalEdit)
        );
    }

    #[test]
    fn host_busy_skips_and_pending_merges_into_one_request() {
        let t = base();
        let mut scheduler = Scheduler::new(t);
        scheduler.set_host_busy(true);
        // 周期到期 + 网络恢复：合并成一条 pending（优先级取高者）。
        assert_eq!(
            scheduler.on_tick(t + secs(45)),
            Decision::Wait(SkipReason::HostBusy)
        );
        assert_eq!(
            scheduler.on_network_recovered(t + secs(46)),
            Decision::Wait(SkipReason::HostBusy)
        );
        assert_eq!(
            scheduler.state(),
            SchedulerState::Scheduled {
                due: t + secs(45),
                trigger: Trigger::NetworkRecovered,
            },
            "只有一条 pending，原因取更高优先级"
        );
        scheduler.set_host_busy(false);
        assert_eq!(
            scheduler.on_tick(t + secs(47)),
            Decision::Start(Trigger::NetworkRecovered)
        );
    }

    #[test]
    fn permanent_failures_pause_instead_of_retrying() {
        let t = base();
        let mut scheduler = Scheduler::new(t);
        assert!(scheduler.on_startup(t).is_start());
        assert_eq!(
            scheduler.on_finished(Finished::failure_permanent("TLS 校验失败"), t + secs(1)),
            Decision::Wait(SkipReason::Paused)
        );
        assert!(scheduler.is_paused());
        assert_eq!(
            scheduler.on_tick(t + secs(45)),
            Decision::Wait(SkipReason::Paused),
            "不可重试错误不能自己恢复"
        );
    }

    #[test]
    fn finished_idle_does_not_restart_in_the_same_call() {
        let t = base();
        let mut scheduler = Scheduler::new(t);
        assert!(scheduler.on_startup(t).is_start());
        // 运行期间来了一个已到期的本地编辑请求（成功收尾后它会立刻 due）。
        assert_eq!(
            scheduler.on_local_edit(LocalEditAck::local_durable(), t),
            Decision::Wait(SkipReason::Running)
        );
        // 收尾**不**在本拍重入派发：最小启动间隔（5s）仍然生效，pending 留着。
        assert_eq!(
            scheduler.on_finished_idle(Finished::Success, t + secs(3)),
            Decision::Wait(SkipReason::MinStartInterval)
        );
        assert!(!scheduler.is_running(), "收尾后不能在 Running 里");
        assert_eq!(
            scheduler.on_tick(t + secs(5)),
            Decision::Start(Trigger::LocalEdit),
            "下一拍照常启动（待办没有丢）"
        );
    }

    #[test]
    fn continuation_and_busy_retry_merge_into_the_single_pending() {
        let t = base();
        let mut scheduler = Scheduler::new(t);
        // 两个"稍后重试"来源 + 一个续批：只有一条 pending，优先级相同取更早到期。
        assert_eq!(
            scheduler.on_busy_retry(t, secs(5)),
            Decision::Wait(SkipReason::NotDue)
        );
        assert_eq!(
            scheduler.on_continuation(t, secs(1)),
            Decision::Wait(SkipReason::NotDue)
        );
        assert_eq!(
            scheduler.state(),
            SchedulerState::Scheduled {
                due: t + secs(1),
                trigger: Trigger::BusyRetry,
            },
            "优先级相同时保留先到的原因，到期取更早者"
        );
        assert_eq!(
            scheduler.on_continuation(t, secs(9)),
            Decision::Wait(SkipReason::NotDue)
        );
        assert_eq!(
            scheduler.on_tick(t + secs(1)),
            Decision::Start(Trigger::BusyRetry)
        );
    }

    #[test]
    fn focus_changes_the_period_without_catch_up_storms() {
        let t = base();
        let mut scheduler = Scheduler::new(t);
        assert_eq!(scheduler.config().period, secs(45));
        // 失焦：45s → 60s，下一次周期点按"上一次周期点 + 新周期"平移。
        scheduler.set_period(Duration::from_secs(60), t + secs(10));
        assert_eq!(scheduler.config().period, secs(60));
        assert_eq!(
            scheduler.on_tick(t + secs(44)),
            Decision::Idle,
            "失焦后 44s 不该启动"
        );
        assert_eq!(
            scheduler.on_tick(t + secs(60)),
            Decision::Start(Trigger::Periodic),
            "失焦周期是 60s"
        );
        assert_eq!(
            scheduler.on_finished(Finished::Success, t + secs(60)),
            Decision::Idle
        );
        // 重新聚焦：回到 45s（仍夹在 30–60s）。
        scheduler.set_period(Duration::from_secs(10), t + secs(61));
        assert_eq!(scheduler.config().period, PERIOD_MIN);
        scheduler.set_period(Duration::from_secs(120), t + secs(62));
        assert_eq!(scheduler.config().period, PERIOD_MAX);
    }

    #[test]
    fn explicit_retry_clears_paused_but_keeps_single_operation_rules() {
        let t = base();
        let mut scheduler = Scheduler::new(t);
        assert!(scheduler.on_startup(t).is_start());
        assert_eq!(
            scheduler.on_finished(Finished::failure_permanent("TLS 校验失败"), t + secs(1)),
            Decision::Wait(SkipReason::Paused)
        );
        // 手动点「重试」= 显式确认：清掉暂停后按手动语义启动。
        assert_eq!(
            scheduler.retry_after_attention(t + secs(2)),
            Decision::Start(Trigger::Manual)
        );
        assert!(!scheduler.is_paused());
        // 运行中仍然挡住第二条（单操作）。
        assert_eq!(
            scheduler.retry_after_attention(t + secs(3)),
            Decision::Wait(SkipReason::Running)
        );
    }

    #[test]
    fn skip_reasons_have_stable_labels() {
        for reason in [
            SkipReason::NotDue,
            SkipReason::Running,
            SkipReason::Paused,
            SkipReason::HostBusy,
            SkipReason::Backoff,
            SkipReason::MinStartInterval,
            SkipReason::NotEligible,
            SkipReason::NoRequest,
        ] {
            assert!(!reason.label().is_empty(), "{reason:?} 必须有中文标签");
        }
        assert_eq!(Trigger::Continuation.label(), "续批");
        assert_eq!(Trigger::BusyRetry.label(), "稍后重试");
    }

    #[test]
    fn nothing_to_do_is_idle_and_next_deadline_reflects_the_earliest_wakeup() {
        let t = base();
        let mut scheduler = Scheduler::new(t);
        assert_eq!(scheduler.on_tick(t + secs(1)), Decision::Idle);
        assert_eq!(scheduler.next_deadline(), Some(t + secs(45)));
        assert_eq!(
            scheduler.on_local_edit(LocalEditAck::local_durable(), t + secs(2)),
            Decision::Wait(SkipReason::NotDue)
        );
        assert_eq!(scheduler.next_deadline(), Some(t + secs(4)));
        assert!(scheduler.on_tick(t + secs(4)).is_start());
        assert_eq!(
            scheduler.next_deadline(),
            None,
            "运行中不需要定时唤醒（收尾由调用方回调）"
        );
    }
}
