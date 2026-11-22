//! 存储侧调度（ADR 0003 切片 4a）：自动 flush + B 物化后台驱动。
//!
//! ## 自动 flush（ADR 0002 §9）
//!
//! - **固定窗口**：从「首次 dirty」起算 250 ms，**后续输入不重置**（不是尾缘 debounce），
//!   否则连续输入会无限延期；
//! - **64 KiB**：未 ACK 的 update 字节数达到阈值立即 flush；
//! - `Ctrl+S` / 切换 / 关闭仍可显式 [`SessionDriver::flush`]；[`SessionDriver::close`] 有界排空。
//!
//! 时钟通过 [`Clock`] 注入：生产用 [`SystemClock`]（墙钟），测试用 [`ManualClock`]
//! 直接 [`ManualClock::set`] 推进——不靠真睡 250 ms 碰运气。
//!
//! ## B 物化
//!
//! 只有收到 [`SessionEvent::Durable`] 之后才把 B 任务入队；后台线程按 ACK 里的
//! `head_seq` 从**固定 frontier**调用 [`materialize_body`]（内部 `end_frontier_at` + CAS），
//! 因此：
//!
//! - 不读取、不修改正在编辑的 live doc（worker 自己从库里重放）；
//! - 旧任务迟到时 CAS 命中 0 行 → `Stale`，不会覆盖新结果；
//! - 失败会记录在 [`DriverEvent::MaterializeFailed`]，可用 [`SessionDriver::retry_failed`] 重试。
//!
//! 调用方线程只做内存操作：`apply` / `flush` / `pump` 都不等待 SQLite；只有 `close`
//! 与显式 `materialize_now` 会有界等待。
//!
//! ## 切片 3b：外部提交闸门 / adopt 的驱动转发
//!
//! [`SessionDriver`] 不重新实现闸门语义，只把 [`DocumentSession`] 的 3a API 原样转发，
//! 并把 adopt 的结果变成**驱动事件**：
//!
//! - `adopt` 成功后把 `AdoptOutcome.delta` 作为 [`DriverEvent::Remote`]、把 `ack` 作为
//!   [`DriverEvent::Durable`] 推入事件队列（顺序固定：先 Remote 再 Durable），
//!   这样 UI 的 pump 就能在不重开文档的情况下更新镜像与 durable 水位；
//! - **绝不**调用会 journal 的 `apply_remote`：那份 update 已经 durable，
//!   再走一次会让 flusher 重复写库并产生多余的出站意图；
//! - Held 期间 `apply` 的拒绝（[`super::protocol::RejectReason::NotReady`]）原样透传，
//!   驱动不额外吞错、也不替调用方兜底。

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusqlite::types::Value;

use super::core::{DocumentSession, SessionOptions};
use super::gate::{
    AdoptError, AdoptOutcome, AppliedHeads, ExternalCommitLease, ExternalCommitTicket, GateError,
    GateState, PreparedCommittedDelta,
};
use super::protocol::{
    DurableAck, EditOutcome, EditRequest, FlushReason, FlushTicket, HistoryState, LiveDelta,
    LiveRev, PortError, SessionEvent,
};
use crate::error::StorageError;
use crate::materialize::{materialize_body, MaterializeOutcome};
use crate::Vault;

/// 取锁；毒化也继续用内部值。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 可注入时钟：返回「驱动启动以来」的时长。
pub trait Clock: Send + 'static {
    /// 当前虚拟/墙钟时刻。
    fn now(&self) -> Duration;
}

/// 墙钟（生产用）。
#[derive(Debug)]
pub struct SystemClock {
    started: Instant,
}

impl SystemClock {
    /// 从现在开始计时。
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.started.elapsed()
    }
}

/// 手动时钟（测试用）：显式 `set`/`advance`，不依赖真实时间。
#[derive(Debug, Clone)]
pub struct ManualClock {
    now: Arc<Mutex<Duration>>,
}

impl ManualClock {
    /// 从 0 开始。
    pub fn new() -> Self {
        Self {
            now: Arc::new(Mutex::new(Duration::ZERO)),
        }
    }

    /// 设置到某个时刻。
    pub fn set(&self, at: Duration) {
        *lock(&self.now) = at;
    }

    /// 前进一段时间。
    pub fn advance(&self, by: Duration) {
        let mut now = lock(&self.now);
        *now += by;
    }

    /// 读取当前时刻。
    pub fn get(&self) -> Duration {
        *lock(&self.now)
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        self.get()
    }
}

/// 自动 flush 策略。
#[derive(Debug, Clone, Copy)]
pub struct FlushPolicy {
    /// 首次 dirty 起算的固定窗口（默认 250 ms）。
    pub interval: Duration,
    /// 未 ACK update 字节阈值（默认 64 KiB）。
    pub size_bytes: usize,
}

impl Default for FlushPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(250),
            size_bytes: 64 * 1024,
        }
    }
}

/// 驱动配置。
#[derive(Debug, Clone, Copy)]
pub struct DriverOptions {
    /// 会话配置。
    pub session: SessionOptions,
    /// flush 策略。
    pub policy: FlushPolicy,
    /// 是否启动墙钟 ticker（生产 `true`；测试用假时钟时可关）。
    pub auto_tick: bool,
    /// ticker 间隔（默认 = `policy.interval`）。
    pub tick_interval: Option<Duration>,
}

impl Default for DriverOptions {
    fn default() -> Self {
        Self {
            session: SessionOptions::default(),
            policy: FlushPolicy::default(),
            auto_tick: true,
            tick_interval: None,
        }
    }
}

/// 驱动向调用方推送的事件（不含请求的同步回复）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverEvent {
    /// 已提交一张 flush 票据（后台还没写完）。
    FlushScheduled {
        /// 票据。
        ticket: FlushTicket,
        /// 触发原因。
        reason: FlushReason,
    },
    /// durable 水位推进（同时会触发 B 物化入队）。
    Durable(DurableAck),
    /// 远端变更。
    Remote(LiveDelta),
    /// B 物化完成。
    Materialized(MaterializeOutcome),
    /// B 物化失败（已记录，可 `retry_failed` 重试）。
    MaterializeFailed {
        /// 笔记 ID。
        note_id: String,
        /// 失败信息。
        message: String,
    },
}

/// `close` 的排空报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseReport {
    /// 覆盖到最后一个 live_rev 的 ACK（超时为 `None`）。
    pub durable: Option<DurableAck>,
    /// 本次关闭过程中完成的 B 物化数。
    pub materialized: usize,
    /// 仍未 ACK 的编辑条数（正常情况下为 0）。
    pub pending_edits: usize,
    /// 仍未完成的 B 任务数（正常情况下为 0）。
    pub pending_materialize: usize,
    /// 是否超时（`true` 表示没排空，调用方应保留现场）。
    pub timed_out: bool,
}

/// 一条 B 物化任务（只带固定 seq，不带 live 句柄）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct MaterializeJob {
    note_id: String,
    doc_id: String,
    target_seq: u64,
}

/// B 物化后台线程。
struct Materializer {
    sender: Option<Sender<MaterializeJob>>,
    results: Arc<Mutex<VecDeque<MaterializeOutcome>>>,
    errors: Arc<Mutex<VecDeque<(MaterializeJob, String)>>>,
    pending: Arc<Mutex<usize>>,
    worker: Option<JoinHandle<()>>,
}

impl Materializer {
    fn spawn(vault: Vault, peer_id: u64, delay: Option<Duration>) -> Self {
        let (sender, receiver) = channel::<MaterializeJob>();
        let results: Arc<Mutex<VecDeque<MaterializeOutcome>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let errors: Arc<Mutex<VecDeque<(MaterializeJob, String)>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let pending = Arc::new(Mutex::new(0usize));

        let worker_results = Arc::clone(&results);
        let worker_errors = Arc::clone(&errors);
        let worker_pending = Arc::clone(&pending);
        let worker = thread::Builder::new()
            .name("menghunote-materialize".to_owned())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    if let Some(delay) = delay {
                        thread::sleep(delay);
                    }
                    // 运行期再解析 note_id：笔记行可能还没投影出来（失败要能记录并重试）
                    let outcome = run_materialize(&vault, &job, peer_id);
                    match outcome {
                        Ok(outcome) => lock(&worker_results).push_back(outcome),
                        Err(error) => {
                            let message = error.to_string();
                            lock(&worker_errors).push_back((job, message));
                        }
                    }
                    let mut pending = lock(&worker_pending);
                    *pending = pending.saturating_sub(1);
                }
            })
            .ok();

        Self {
            sender: Some(sender),
            results,
            errors,
            pending,
            worker,
        }
    }

    fn enqueue(&self, job: MaterializeJob) {
        if let Some(sender) = self.sender.as_ref() {
            *lock(&self.pending) += 1;
            if sender.send(job).is_err() {
                let mut pending = lock(&self.pending);
                *pending = pending.saturating_sub(1);
            }
        }
    }

    fn pending(&self) -> usize {
        *lock(&self.pending)
    }

    fn take_results(&self) -> Vec<MaterializeOutcome> {
        lock(&self.results).drain(..).collect()
    }

    fn take_errors(&self) -> Vec<(MaterializeJob, String)> {
        lock(&self.errors).drain(..).collect()
    }
}

impl Drop for Materializer {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 墙钟 ticker：按固定间隔投递 `()`，驱动 `pump` 检查固定窗口。
struct Ticker {
    stop: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Ticker {
    fn spawn(interval: Duration) -> (Self, Receiver<()>) {
        let interval = interval.max(Duration::from_millis(1));
        let (tick_sender, tick_receiver) = channel::<()>();
        let (stop_sender, stop_receiver) = channel::<()>();

        let worker = thread::Builder::new()
            .name("menghunote-tick".to_owned())
            .spawn(move || loop {
                match stop_receiver.recv_timeout(interval) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if tick_sender.send(()).is_err() {
                            break;
                        }
                    }
                }
            })
            .ok();

        (
            Self {
                stop: Some(stop_sender),
                worker,
            },
            tick_receiver,
        )
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 会话驱动：会话核 + 自动 flush 调度 + B 物化后台线程。
pub struct SessionDriver {
    session: DocumentSession,
    vault: Vault,
    peer_id: u64,
    policy: FlushPolicy,
    clock: Box<dyn Clock>,
    /// 首次 dirty 的虚拟时刻；`None` = 没有等待窗口的脏数据。
    dirty_since: Option<Duration>,
    /// 已提交 flush 覆盖到的 live_rev（避免空 flush 造成重复票据）。
    flushed_live_rev: LiveRev,
    materializer: Materializer,
    /// B 失败任务（可 `retry_failed`）。
    failed_jobs: Vec<MaterializeJob>,
    /// 只负责保活：Drop 时停线程；逻辑上不读取。
    _ticker: Option<Ticker>,
    tick_receiver: Option<Receiver<()>>,
    events: VecDeque<DriverEvent>,
}

impl std::fmt::Debug for SessionDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionDriver")
            .field("doc_id", &self.session.doc_id())
            .field("live_rev", &self.session.live_rev())
            .field("dirty_since", &self.dirty_since)
            .field("pending_bytes", &self.session.pending_bytes())
            .finish()
    }
}

impl SessionDriver {
    /// 打开驱动（墙钟 + ticker）。
    pub fn open(vault: &Vault, doc_id: &str, options: DriverOptions) -> Result<Self, PortError> {
        Self::open_with_clock(vault, doc_id, options, Box::new(SystemClock::new()))
    }

    /// 打开驱动并注入时钟（测试用假时钟）。
    pub fn open_with_clock(
        vault: &Vault,
        doc_id: &str,
        options: DriverOptions,
        clock: Box<dyn Clock>,
    ) -> Result<Self, PortError> {
        Self::open_with_clock_and_materialize_delay(vault, doc_id, options, clock, None)
    }

    /// 打开驱动并注入时钟 + **B 物化延迟**（测试/故障注入，与 `SessionOptions::flush_delay` 同性质）。
    ///
    /// 延迟是构造参数而不是 `DriverOptions` 字段：后者是公开结构体，
    /// 加字段会让外部（editor 的 e2e 夹具）的结构体字面量编译失败。
    pub fn open_with_clock_and_materialize_delay(
        vault: &Vault,
        doc_id: &str,
        options: DriverOptions,
        clock: Box<dyn Clock>,
        materialize_delay: Option<Duration>,
    ) -> Result<Self, PortError> {
        let session = DocumentSession::open(vault, doc_id, options.session)?;
        let materializer =
            Materializer::spawn(vault.clone(), options.session.peer_id, materialize_delay);
        let (ticker, tick_receiver) = if options.auto_tick {
            let interval = options.tick_interval.unwrap_or(options.policy.interval);
            let (ticker, receiver) = Ticker::spawn(interval);
            (Some(ticker), Some(receiver))
        } else {
            (None, None)
        };
        Ok(Self {
            session,
            vault: vault.clone(),
            peer_id: options.session.peer_id,
            policy: options.policy,
            clock,
            dirty_since: None,
            flushed_live_rev: LiveRev(0),
            materializer,
            failed_jobs: Vec::new(),
            _ticker: ticker,
            tick_receiver,
            events: VecDeque::new(),
        })
    }

    /// 会话核（只读能力：快照/历史/水位）。
    pub fn session(&self) -> &DocumentSession {
        &self.session
    }

    /// 当前 live 正文。
    pub fn markdown(&self) -> String {
        self.session.markdown()
    }

    /// 当前 live 修订号。
    pub fn live_rev(&self) -> LiveRev {
        self.session.live_rev()
    }

    /// 未 ACK update 字节数。
    pub fn pending_bytes(&self) -> usize {
        self.session.pending_bytes()
    }

    /// 未 ACK 编辑条数。
    pub fn pending_edits(&self) -> usize {
        self.session.pending_len()
    }

    /// 下一次固定窗口到期时刻（`None` 表示当前没有等待中的脏数据）。
    pub fn next_deadline(&self) -> Option<Duration> {
        self.dirty_since.map(|since| since + self.policy.interval)
    }

    /// 开始组合（透传会话闸门）。
    pub fn begin_composition(&mut self) -> u32 {
        self.session.begin_composition()
    }

    /// 结束组合：合并延后的远端导入；这些 op 已进 journal，需要计入 dirty。
    pub fn end_composition(&mut self) -> Vec<LiveDelta> {
        let deltas = self.session.end_composition();
        if !deltas.is_empty() {
            self.mark_dirty();
        }
        deltas
    }

    /// 背压导致延后队列没排空时，flush 之后再调一次。
    pub fn merge_deferred_remote(&mut self) -> Vec<LiveDelta> {
        let deltas = self.session.merge_deferred_remote();
        if !deltas.is_empty() {
            self.mark_dirty();
        }
        deltas
    }

    /// 是否正处于 composition。
    pub fn is_composing(&self) -> bool {
        self.session.is_composing()
    }

    /// 仍未合并的远端 update 条数。
    pub fn deferred_remote_len(&self) -> usize {
        self.session.deferred_remote_len()
    }

    /// 受理编辑；首次 dirty 记录窗口起点，达到 64 KiB 立即 flush。
    pub fn apply(&mut self, request: EditRequest) -> EditOutcome {
        let outcome = self.session.apply(request);
        if matches!(outcome, EditOutcome::Applied(_)) {
            self.mark_dirty();
        }
        outcome
    }

    /// 远端导入：同样计入 dirty（远端 op 也要落 durable）。
    ///
    /// **已 durable** 的外部增量不要走这里（见 [`Self::adopt_committed_delta`]）：
    /// 这条路径会 journal 并产生出站意图。
    pub fn apply_remote(&mut self, update: &[u8]) -> Result<Option<LiveDelta>, PortError> {
        let delta = self.session.apply_remote(update)?;
        if delta.is_some() {
            self.mark_dirty();
        }
        Ok(delta)
    }

    // ---- 外部提交闸门 / adopt（ADR 0009 切片 3b：只转发 3a 的存储层语义）----

    /// Held 期间本地写入是否被冻结（= 外部提交闸门持有中）。
    #[must_use]
    pub fn is_external_commit_held(&self) -> bool {
        self.session.is_external_commit_held()
    }

    /// 当前外部提交租约（`None` = 不在 Held）。
    #[must_use]
    pub fn external_commit_lease(&self) -> Option<&ExternalCommitLease> {
        self.session.external_commit_lease()
    }

    /// 登记一次外部提交（只建立票据；重复 begin 由会话拒绝）。
    pub fn begin_external_commit(&mut self) -> Result<ExternalCommitTicket, GateError> {
        self.session.begin_external_commit()
    }

    /// 非阻塞推进闸门：`WaitingIME` → `Flushing` → `Held(lease)`。
    ///
    /// 进入 Held 时顺手把调度器自己的水位对齐：闸门的成立条件就是「目标已 durable 且
    /// journal 空闲」，因此这里不再欠这次 flush（否则下一次 pump 会为已经不脏的数据
    /// 再投一张空票据）。
    pub fn poll_external_commit(&mut self, ticket: &ExternalCommitTicket) -> GateState {
        let state = self.session.poll_external_commit(ticket);
        if let GateState::Held(lease) = &state {
            self.dirty_since = None;
            self.flushed_live_rev = self.flushed_live_rev.max(lease.target_live_rev());
        }
        state
    }

    /// adopt 一份**已 durable** 的外部增量（必须 Held 且租约匹配）。
    ///
    /// 成功后把增量与 ACK 变成驱动事件（**先 Remote 再 Durable**），`pump` / `tick`
    /// 立刻能取到；失败时 live 与 durable 水位都不变（3a 保证）。
    pub fn adopt_committed_delta(
        &mut self,
        lease: &ExternalCommitLease,
        prepared: PreparedCommittedDelta,
    ) -> Result<AdoptOutcome, AdoptError> {
        let outcome = self.session.adopt_committed_delta(lease, prepared)?;
        // 不调用 `apply_remote`：这份 CRDT 已经 durable，再 journal 一次会重复写库 + 产生意图。
        self.events
            .push_back(DriverEvent::Remote(outcome.delta.clone()));
        self.push_durable(outcome.ack.clone());
        // adopt 要求 journal 空闲，因此"欠一次 flush"的水位到此为止。
        self.dirty_since = None;
        self.flushed_live_rev = self.flushed_live_rev.max(outcome.ack.through_live_rev);
        Ok(outcome)
    }

    /// 结束外部提交并释放闸门（`applied` 必须就是库中当前 head）。
    ///
    /// 失败/取消路径下 3a 会把 Held 期间已 durable 的变化交付进 live 并作为
    /// `SessionEvent::Remote` 入队，驱动会在下一次 `pump` 转发成 [`DriverEvent::Remote`]。
    pub fn end_external_commit(
        &mut self,
        lease: ExternalCommitLease,
        applied: AppliedHeads,
    ) -> Result<(), GateError> {
        self.session.end_external_commit(lease, applied)
    }

    /// 放弃一次外部提交（失败/取消），同样先交付已 durable 的变化。
    ///
    /// 返回的错误原样来自会话（例如仍 `pending` 时 [`GateError::Pending`]）；交付的增量
    /// 会作为会话事件由下一次 `pump` 转发，这里不重复推送，避免同一份 delta 应用两次。
    pub fn cancel_external_commit(
        &mut self,
        ticket: &ExternalCommitTicket,
    ) -> Result<(), GateError> {
        self.session.cancel_external_commit(ticket).map(|_| ())
    }

    /// **固定窗口**：只在首次 dirty 时记录起点，后续输入不重置。
    fn mark_dirty(&mut self) {
        if self.dirty_since.is_none() {
            self.dirty_since = Some(self.clock.now());
        }
        if self.session.pending_bytes() >= self.policy.size_bytes
            && self.session.live_rev() > self.flushed_live_rev
        {
            self.flush(FlushReason::Size);
        }
    }

    /// 撤销（转发会话核的唯一 `UndoManager`）。
    ///
    /// dirty 规则与 [`SessionDriver::apply`] 对齐：
    /// - `Ok(Some(Applied))` → `mark_dirty()`（撤销也是正文改动，必须落 durable）；
    /// - `Ok(None)`（没有可撤销项）/ `Ok(Some(Rejected))` / `Stale`（不会出现）→ **不** dirty；
    /// - `Err(PortError)` → 原样透传，不 dirty。
    pub fn undo(&mut self) -> Result<Option<EditOutcome>, PortError> {
        let outcome = self.session.undo()?;
        if matches!(&outcome, Some(EditOutcome::Applied(_))) {
            self.mark_dirty();
        }
        Ok(outcome)
    }

    /// 重做；dirty 规则同 [`SessionDriver::undo`]。
    pub fn redo(&mut self) -> Result<Option<EditOutcome>, PortError> {
        let outcome = self.session.redo()?;
        if matches!(&outcome, Some(EditOutcome::Applied(_))) {
            self.mark_dirty();
        }
        Ok(outcome)
    }

    /// 撤销/重做栈状态（真实计数来自 Loro `UndoManager`）。
    pub fn history(&self) -> HistoryState {
        self.session.history()
    }

    /// 手动/按原因 flush（同步返回票据，不等待 SQLite）。
    pub fn flush(&mut self, reason: FlushReason) -> FlushTicket {
        let ticket = self.session.flush(reason);
        self.flushed_live_rev = ticket.target_live_rev;
        self.dirty_since = None;
        self.events
            .push_back(DriverEvent::FlushScheduled { ticket, reason });
        ticket
    }

    /// 推进到指定虚拟时刻（测试用于精确控制固定窗口）。
    pub fn tick(&mut self, now: Duration) -> Vec<DriverEvent> {
        self.advance_to(now);
        self.collect_session_events();
        self.collect_materialize_results();
        self.drain_events()
    }

    /// 非阻塞泵：读 ticker、检查固定窗口、收 session/B 事件。
    pub fn pump(&mut self) -> Vec<DriverEvent> {
        self.take_ticks();
        let now = self.clock.now();
        self.advance_to(now);
        self.collect_session_events();
        self.collect_materialize_results();
        self.drain_events()
    }

    fn take_ticks(&mut self) {
        let Some(receiver) = self.tick_receiver.as_ref() else {
            return;
        };
        // 只做「消化信号」：固定窗口是否到期由虚拟/墙钟时刻决定
        while receiver.try_recv().is_ok() {}
    }

    fn advance_to(&mut self, now: Duration) {
        if let Some(since) = self.dirty_since {
            if now >= since + self.policy.interval {
                self.flush(FlushReason::Interval);
            }
        }
    }

    fn collect_session_events(&mut self) {
        for event in self.session.events() {
            match event {
                SessionEvent::Durable(ack) => self.push_durable(ack),
                SessionEvent::Remote(delta) => self.events.push_back(DriverEvent::Remote(delta)),
            }
        }
    }

    /// 推入一条 `Durable` 事件，并按既有约定入队 B 物化。
    ///
    /// 两条来源共用：flush 线程的 ACK，以及 adopt 之后由外部提交推进的 head（3b）。
    fn push_durable(&mut self, ack: DurableAck) {
        // DurableAck 之后才入队 B（用 ACK 的固定 head_seq，不碰 live doc）。
        self.enqueue_materialize(&ack);
        self.events.push_back(DriverEvent::Durable(ack));
    }

    /// DurableAck 之后才入队 B（用 ACK 的固定 head_seq，不碰 live doc）。
    fn enqueue_materialize(&mut self, ack: &DurableAck) {
        if ack.head_seq == 0 {
            return;
        }
        let doc_id = self.session.doc_id().to_owned();
        // note_id 只是给事件用的提示；权威解析在 worker 运行期做
        let note_hint = lookup_note_id(&self.vault, &doc_id)
            .ok()
            .flatten()
            .unwrap_or_default();
        self.materializer.enqueue(MaterializeJob {
            note_id: note_hint,
            doc_id,
            target_seq: ack.head_seq,
        });
    }

    fn collect_materialize_results(&mut self) {
        for outcome in self.materializer.take_results() {
            self.events.push_back(DriverEvent::Materialized(outcome));
        }
        for (job, message) in self.materializer.take_errors() {
            self.events.push_back(DriverEvent::MaterializeFailed {
                note_id: job.note_id.clone(),
                message,
            });
            self.failed_jobs.push(job);
        }
    }

    fn drain_events(&mut self) -> Vec<DriverEvent> {
        self.events.drain(..).collect()
    }

    /// 有界排空：flush(Close) → 等 durable → 等 B 完成。
    ///
    /// 调用方可以是 UI 线程（关闭窗口），但必须有界：`timeout` 到了就返回并置 `timed_out`。
    pub fn close(&mut self, timeout: Duration) -> CloseReport {
        let target = self.session.live_rev();
        let needs_flush = self.session.pending_len() > 0 || target > self.flushed_live_rev;
        if needs_flush {
            self.flush(FlushReason::Close);
        }

        let deadline = Instant::now() + timeout;
        let mut durable: Option<DurableAck> = None;
        let mut materialized = 0usize;

        loop {
            for event in self.pump() {
                match event {
                    DriverEvent::Durable(ack) if ack.through_live_rev >= target => {
                        durable = Some(ack);
                    }
                    DriverEvent::Materialized(_) => materialized += 1,
                    _ => {}
                }
            }

            let durable_ok = durable.is_some() || !needs_flush;
            let drained = durable_ok && self.materializer.pending() == 0;
            if drained || Instant::now() >= deadline {
                // 收尾：把最后一批 B 结果也计入
                for event in self.pump() {
                    if matches!(event, DriverEvent::Materialized(_)) {
                        materialized += 1;
                    }
                }
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }

        CloseReport {
            durable,
            materialized,
            pending_edits: self.session.pending_len(),
            pending_materialize: self.materializer.pending(),
            timed_out: self.session.pending_len() > 0 || self.materializer.pending() > 0,
        }
    }

    /// 显式同步物化（启动修复/测试用）：从固定 `target_seq` 走 CAS。
    pub fn materialize_now(
        &mut self,
        note_id: &str,
        doc_id: &str,
        target_seq: u64,
    ) -> Result<MaterializeOutcome, StorageError> {
        materialize_body(&self.vault, note_id, doc_id, target_seq, self.peer_id)
    }

    /// 重新入队失败的 B 任务；返回重试条数。
    pub fn retry_failed(&mut self) -> usize {
        let jobs = std::mem::take(&mut self.failed_jobs);
        let count = jobs.len();
        for job in jobs {
            self.materializer.enqueue(job);
        }
        count
    }

    /// 已 durable 的 head seq。
    pub fn durable_head_seq(&self) -> u64 {
        self.session.durable_head_seq()
    }

    /// 等待 B 队列排空（测试/关闭辅助），返回是否排空。
    ///
    /// **契约**：返回 `true` 时，worker 已完成的 B 结果都已经被搬进事件队列并取走，
    /// 之后的 `pump()` 不会再冒出「上一次等待之前就该完成」的 `Materialized` 事件。
    /// （worker 是「先 push 结果、再减 pending」，所以 pending 归零后再泵一次才能收干净。）
    pub fn wait_materialize(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            // 搬走已完成的结果/错误（本方法是等待辅助，事件直接丢弃）
            for _ in self.pump() {}
            if self.materializer.pending() == 0 {
                // 收尾：把「pending 已减、结果刚入队」的那一批也搬走
                for _ in self.pump() {}
                return self.materializer.pending() == 0;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

/// worker 运行期：解析 note_id 后从固定 seq 物化。
fn run_materialize(
    vault: &Vault,
    job: &MaterializeJob,
    peer_id: u64,
) -> Result<MaterializeOutcome, StorageError> {
    let note_id = lookup_note_id(vault, &job.doc_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("找不到 body_doc_id={} 的笔记行，暂时无法物化", job.doc_id),
    })?;
    materialize_body(vault, &note_id, &job.doc_id, job.target_seq, peer_id)
}

/// `body_doc_id` → `note_id`（B 物化需要笔记行）。
fn lookup_note_id(vault: &Vault, doc_id: &str) -> Result<Option<String>, StorageError> {
    let rows = vault.query(
        "SELECT note_id FROM notes WHERE body_doc_id = ?1 ORDER BY note_id LIMIT 1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Text(note_id)) => Ok(Some(note_id.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(StorageError::Corrupt {
            context: format!("notes.note_id 读回异常：{other:?}"),
        }),
    }
}
