//! 未 ACK journal 的后台 flush 线程（ADR 0003 §5 / ADR 0001 §3.4）。
//!
//! 契约：
//!
//! - `flush()` 只投递一条命令就返回票据，**调用线程绝不碰 SQLite**；
//! - 后台线程每次取 journal 的**连续前缀**，合并成一次 A 事务写 durable；
//! - 写成功后先裁剪前缀、推进 durable 水位，再把 [`SessionEvent::Durable`] 推进事件队列；
//! - 写失败**不裁剪、不 ACK**：同一前缀下次重试，因此「已 ACK 的部分」不会丢；
//! - **head 过期**（[`StorageError::HeadConflict`]）：同一命令内**至多**自动
//!   `load_head` + 重建 sink 一次，并用新 CAS 基准重试**同一前缀**一次。只有新 head 的
//!   ops 全部落在未 ACK 前缀最后一条的 `end_vv` 里（即外部推进写的是本会话 live 已知的
//!   op）才允许重试——否则写入的 `head_vv`/`frontier` 会缺掉外部 ops，下一次
//!   `load_body_doc` 的完整性校验直接 `Corrupt`。不满足或重建后仍失败 → **不可恢复**：
//!   `last_write_error` 明确提示「需要重开文档」，journal 原样保留，后续 flush 立即失败、
//!   不再碰数据库（有界重试，不无限写）。

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::journal::{DurableSink, JournalEntry, JournalOrigin, PendingJournal, VaultJournalSink};
use crate::store::{load_head, HeadState};
use crate::sync_bridge::SyncContext;
use crate::{StorageError, Vault};

use super::gate::AppliedHeads;
use super::protocol::{BatchId, DurableAck, FlushTicketId, LiveRev, SessionEvent};

/// 闸门判定用的水位快照（见 [`JournalHandle::gate_progress`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GateProgress {
    /// 未 ACK 条数。
    pub(crate) pending: usize,
    /// 已 durable 到的 live 修订号。
    pub(crate) durable_through: LiveRev,
    /// 已 durable 的 head seq（外部提交的 CAS 基准）。
    pub(crate) durable_head_seq: u64,
    /// 最近一次前缀写失败原因（成功写入后清空；不可恢复时保留「需要重开文档」提示）。
    pub(crate) last_write_error: Option<String>,
}

/// 取锁；锁被毒化也继续用内部值（清理路径不允许 panic）。
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 后台 flush 命令（票据号已由调用方持有，线程只需要目标水位）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct FlushCommand {
    /// 本次 flush 需要覆盖到的 live 修订号。
    pub(crate) target_live_rev: LiveRev,
}

/// 会话与 flush 线程共享的未 ACK 状态。
#[derive(Debug)]
pub(crate) struct PendingState {
    /// 未 ACK journal（容量准入 / 连续前缀 / durable 后裁剪）。
    pub(crate) journal: PendingJournal,
    /// 与 journal 前缀一一对应的 live 修订号。
    pub(crate) revs: VecDeque<LiveRev>,
    /// 与 journal 前缀一一对应的 update 字节数（64 KiB 阈值用）。
    pub(crate) sizes: VecDeque<usize>,
    /// 未 ACK update 的总字节数。
    pub(crate) pending_bytes: usize,
    /// 下一个批次号。
    pub(crate) next_batch: u64,
    /// 下一张票据号。
    pub(crate) next_ticket: u64,
    /// 已 durable 的 head 状态（A 事务的 CAS 基准）。
    pub(crate) durable: HeadState,
    /// 最近一次成功 durable 的批次号。
    pub(crate) durable_batch: Option<BatchId>,
    /// 已 durable 到的 live 修订号。
    pub(crate) durable_through: LiveRev,
    /// 最近一次前缀写失败的原因；下一次成功写入后清空。
    ///
    /// 外部提交闸门据此把「还在 Flushing」明确升级成 `Failed`，不静默等一个永远不来的 ACK。
    pub(crate) last_write_error: Option<String>,
    /// 不可恢复的 flush 失败（head 过期且无法安全自动重建）。
    ///
    /// 一旦置位就**不再自动重试**：后续 flush 命令立即失败、不再碰数据库；journal 原样
    /// 保留（未 ACK 的数据不丢也不会被静默丢弃），只有关闭并重开文档才能恢复。
    /// 外部提交闸门通过 [`GateProgress::last_write_error`] 把它升级成 `Failed`，UI 可据此
    /// 提示重开文档。
    pub(crate) unrecoverable: Option<String>,
}

/// journal 句柄：会话侧投递命令，后台线程负责写。
#[derive(Debug)]
pub(crate) struct JournalHandle {
    state: Arc<Mutex<PendingState>>,
    events: Arc<Mutex<VecDeque<SessionEvent>>>,
    sender: Option<Sender<FlushCommand>>,
    worker: Option<JoinHandle<()>>,
}

impl JournalHandle {
    /// 启动后台 flush 线程。
    ///
    /// `flush_delay` 是**测试/故障注入**用的可控延迟（模拟慢盘），生产传 `None`；
    /// `sync` 是 ADR 0009 切片 2b 的本地意图上下文（`None` = 不写 `sync_outbox`）。
    pub(crate) fn spawn(
        vault: Vault,
        doc_id: String,
        head: HeadState,
        capacity: usize,
        max_batch: usize,
        flush_delay: Option<Duration>,
        sync: Option<SyncContext>,
    ) -> Self {
        let state = Arc::new(Mutex::new(PendingState {
            journal: PendingJournal::new(capacity),
            revs: VecDeque::new(),
            sizes: VecDeque::new(),
            pending_bytes: 0,
            next_batch: 1,
            next_ticket: 1,
            durable: head,
            durable_batch: None,
            durable_through: LiveRev(0),
            last_write_error: None,
            unrecoverable: None,
        }));
        let events: Arc<Mutex<VecDeque<SessionEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (sender, receiver) = channel::<FlushCommand>();

        let worker_state = Arc::clone(&state);
        let worker_events = Arc::clone(&events);
        let config = FlusherConfig {
            doc_id,
            max_batch: max_batch.max(1),
            flush_delay,
            sync,
        };
        let worker = thread::Builder::new()
            .name("menghunote-flush".to_owned())
            .spawn(move || {
                flusher_main(vault, worker_state, worker_events, receiver, config);
            });

        // 线程起不来时退化为「没有后台 flush」：票据照发，但不会有 ACK（调用方可重启会话）。
        let worker = worker.ok();

        Self {
            state,
            events,
            sender: Some(sender),
            worker,
        }
    }

    /// 受理一次 flush：登记票据并投递命令，立即返回票据号。
    pub(crate) fn submit_ticket(&self, target: LiveRev) -> FlushTicketId {
        let ticket_id = {
            let mut guard = lock(&self.state);
            guard.next_ticket += 1;
            FlushTicketId(guard.next_ticket)
        };
        if let Some(sender) = self.sender.as_ref() {
            let _ = sender.send(FlushCommand {
                target_live_rev: target,
            });
        }
        ticket_id
    }

    /// 共享状态（会话核在持锁区间内完成「准入 + 改 Loro + 追加」）。
    pub(crate) fn state_ref(&self) -> &Mutex<PendingState> {
        &self.state
    }

    /// 事件队列（flush 线程写入，会话核读取）。
    pub(crate) fn events_ref(&self) -> &Mutex<VecDeque<SessionEvent>> {
        &self.events
    }

    /// 未 ACK 条数。
    pub(crate) fn pending_len(&self) -> usize {
        lock(&self.state).journal.pending_len()
    }

    /// 是否已满（背压判定）。
    pub(crate) fn is_full(&self) -> bool {
        lock(&self.state).journal.is_full()
    }

    /// 未 ACK update 总字节数（调度器判断 64 KiB 阈值）。
    pub(crate) fn pending_bytes(&self) -> usize {
        lock(&self.state).pending_bytes
    }

    /// 已 durable 的 head seq。
    pub(crate) fn durable_head_seq(&self) -> u64 {
        lock(&self.state).durable.head_seq
    }

    /// 已 durable 到哪个 live 修订号。
    pub(crate) fn durable_through(&self) -> LiveRev {
        lock(&self.state).durable_through
    }

    /// 外部提交闸门判定用的**一次性快照**：一次取锁同时读 pending / durable 水位 /
    /// 最近一次写失败原因，避免分多次取锁看到互相矛盾的组合。
    pub(crate) fn gate_progress(&self) -> GateProgress {
        let guard = lock(&self.state);
        GateProgress {
            pending: guard.journal.pending_len(),
            durable_through: guard.durable_through,
            durable_head_seq: guard.durable.head_seq,
            last_write_error: guard.last_write_error.clone(),
        }
    }

    /// 外部提交 adopt / end 之后重定 durable 基准（调用方必须已持锁）。
    ///
    /// `loro_docs.head_*` 已由外部提交（`engine.integrate`）推进；这里把 flusher 的 CAS
    /// 基准与 durable 水位对齐，避免下一次本地 flush 报 `HeadConflict`。
    ///
    /// **只允许在 journal `pending == 0` 时调用**：journal 条目的 `end_vv` / `end_frontier`
    /// 是追加那一刻的快照，有在途条目时重定基准会让水位于条目自相矛盾（前瞻 head 回退）。
    pub(crate) fn rebase_locked(
        guard: &mut PendingState,
        heads: &AppliedHeads,
        through: LiveRev,
    ) -> Result<(), StorageError> {
        if guard.journal.pending_len() != 0 {
            return Err(StorageError::Corrupt {
                context: "有未 ACK 条目时不能重定 durable 基准".to_owned(),
            });
        }
        guard.durable.head_seq = heads.head_seq;
        guard.durable.head_frontier = heads.frontier.clone();
        guard.durable.head_vv = heads.vv.clone();
        guard.durable_through = through;
        Ok(())
    }

    /// 取走全部待推送事件。
    pub(crate) fn drain_events(&self) -> Vec<SessionEvent> {
        let mut queue = lock(&self.events);
        queue.drain(..).collect()
    }

    /// 在 journal 里登记一条未 ACK 条目（调用方必须已通过容量准入）。
    ///
    /// `origin` 决定 flush 时是否产生出站同步意图：只有本地更新才产生。
    pub(crate) fn append(
        guard: &mut PendingState,
        doc_id: &str,
        batch_label: &str,
        origin: JournalOrigin,
        update: crate::body_doc::PendingUpdate,
        rev: LiveRev,
    ) -> Result<BatchId, crate::StorageError> {
        let batch = BatchId(guard.next_batch);
        guard.next_batch += 1;
        let size = update.payload.len();
        let entry = JournalEntry {
            doc_id: doc_id.to_owned(),
            batch_id: format!("{batch_label}-{}", batch.0),
            update,
            origin,
        };
        guard.journal.append(entry)?;
        guard.revs.push_back(rev);
        guard.sizes.push_back(size);
        guard.pending_bytes += size;
        Ok(batch)
    }
}

impl Drop for JournalHandle {
    fn drop(&mut self) {
        // 先关命令通道：flusher 的 recv 返回 Err 后退出循环；再 join，绝不 panic。
        drop(self.sender.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// flush 线程的一次性配置（与可变状态分开，避免 `flusher_main` 参数过多）。
#[derive(Debug, Clone)]
struct FlusherConfig {
    /// 归属文档（一次 flush 里不允许混文档）。
    doc_id: String,
    /// 一次 A 事务最多合并多少条。
    max_batch: usize,
    /// 注入的慢盘延迟（生产 `None`）。
    flush_delay: Option<Duration>,
    /// 本地意图上下文（`None` = 不写 `sync_outbox`）。
    sync: Option<SyncContext>,
}

/// flush 线程主体：一条命令 = 至少一次前缀写，直到覆盖 `target_live_rev` 或 journal 空。
fn flusher_main(
    vault: Vault,
    state: Arc<Mutex<PendingState>>,
    events: Arc<Mutex<VecDeque<SessionEvent>>>,
    receiver: Receiver<FlushCommand>,
    config: FlusherConfig,
) {
    let FlusherConfig {
        doc_id,
        max_batch,
        flush_delay,
        sync,
    } = &config;
    let doc_id = doc_id.clone();
    let max_batch = *max_batch;
    let flush_delay = *flush_delay;
    let sync = *sync;
    while let Ok(command) = receiver.recv() {
        if let Some(delay) = flush_delay {
            thread::sleep(delay);
        }

        let mut emitted_max: Option<LiveRev> = None;
        // P1-3：head 过期时同一命令**最多**自动重建 1 次 sink（`load_head` → 新基准 →
        // 重试同一前缀一次）；用尽后进入不可恢复状态，不再自动重试。
        let mut auto_rebuilt = false;
        loop {
            // 1) 取连续前缀（持锁只做内存操作，绝不在锁内写盘）
            let (entries, through, count, head_seq) = {
                let mut guard = lock(&state);
                // 不可恢复：后续 flush 立即失败、绝不再碰数据库（journal 原样保留）。
                if guard.unrecoverable.is_some() {
                    break;
                }
                let pending = guard.journal.pending_len();
                if pending == 0 {
                    break;
                }
                let take = max_batch.min(pending);
                let entries: Vec<JournalEntry> = guard.journal.peek_prefix(take).to_vec();
                let through = match guard.revs.get(take - 1) {
                    Some(rev) => *rev,
                    // 理论上不可能：journal 与 revs 一一对应；防御性退出
                    None => break,
                };
                let head_seq = guard.durable.head_seq;
                (entries, through, take, head_seq)
            };

            // 2) A 事务（在锁外，慢盘不会阻塞会话线程）
            let mut sink = VaultJournalSink::new(vault.clone(), doc_id.clone(), head_seq);
            if let Some(sync) = sync {
                sink = sink.with_sync_context(sync);
            }
            let written = match sink.write_prefix(&entries) {
                Ok(seq) => seq,
                // 普通写失败（磁盘/SQLite 等）：不裁剪、不 ACK，同一前缀下次重试；
                // 同时记录失败原因：外部提交闸门据此把 "Flushing" 明确升级为 Failed。
                Err(error) if !matches!(&error, StorageError::HeadConflict { .. }) => {
                    lock(&state).last_write_error = Some(error.to_string());
                    break;
                }
                // head 已被外部推进：同一份字节按旧 CAS 基准重试永远不会成功。
                // 先 `load_head` 拿真实基准、有界重建 sink，再重试**同一前缀**一次。
                Err(error) => {
                    if auto_rebuilt {
                        let message = reopen_message(
                            &doc_id,
                            lock(&state).journal.pending_len(),
                            format!(
                                "自动重建后再次 head 冲突（{error}），本命令的 1 次自动重建已用尽"
                            ),
                        );
                        let mut guard = lock(&state);
                        guard.last_write_error = Some(message.clone());
                        guard.unrecoverable = Some(message);
                        break;
                    }
                    auto_rebuilt = true;
                    let pending = lock(&state).journal.pending_len();
                    match retry_prefix_after_rebuild(
                        &vault, &doc_id, &entries, pending, head_seq, sync,
                    ) {
                        RebuildOutcome::Written(seq) => seq,
                        // 普通写失败：保持既有「同一前缀下次重试」契约，不升级成不可恢复。
                        RebuildOutcome::Retryable(message) => {
                            lock(&state).last_write_error = Some(message);
                            break;
                        }
                        RebuildOutcome::Unrecoverable(message) => {
                            let mut guard = lock(&state);
                            guard.last_write_error = Some(message.clone());
                            guard.unrecoverable = Some(message);
                            break;
                        }
                    }
                }
            };

            // 3) durable 之后才裁剪并推进水位
            let (batch, head_frontier) = {
                let mut guard = lock(&state);
                guard.last_write_error = None;
                let _ = guard.journal.ack_prefix(count);
                for _ in 0..count {
                    guard.revs.pop_front();
                    if let Some(size) = guard.sizes.pop_front() {
                        guard.pending_bytes = guard.pending_bytes.saturating_sub(size);
                    }
                }
                let batch = BatchId(guard.next_batch);
                guard.next_batch += 1;
                guard.durable.head_seq = written;
                let mut frontier = guard.durable.head_frontier.clone();
                let mut vv = guard.durable.head_vv.clone();
                if let Some(last) = entries.last() {
                    frontier = last.update.end_frontier.clone();
                    vv = last.update.end_vv.clone();
                }
                guard.durable.head_frontier = frontier.clone();
                guard.durable.head_vv = vv;
                guard.durable_through = through;
                guard.durable_batch = Some(batch);
                (batch, frontier)
            };

            let ack = DurableAck {
                batch_id: batch,
                through_live_rev: through,
                head_seq: written,
                head_frontier,
            };
            lock(&events).push_back(SessionEvent::Durable(ack));
            emitted_max = Some(through);

            if through >= command.target_live_rev {
                break;
            }
        }

        // 每个票据都必须有结局：没有待写条目（或已覆盖目标）时补一张确认票据，
        // 否则「手动保存」在无脏数据时永远等不到 ACK。
        let confirmation = {
            let guard = lock(&state);
            if guard.journal.pending_len() == 0
                && emitted_max.is_none_or(|through| through < command.target_live_rev)
            {
                Some((
                    guard.durable_batch.unwrap_or(BatchId(0)),
                    guard.durable_through,
                    guard.durable.head_seq,
                    guard.durable.head_frontier.clone(),
                ))
            } else {
                None
            }
        };
        if let Some((batch_id, through_live_rev, head_seq, head_frontier)) = confirmation {
            lock(&events).push_back(SessionEvent::Durable(DurableAck {
                batch_id,
                through_live_rev,
                head_seq,
                head_frontier,
            }));
        }
    }
}

/// head 冲突后自动重建 sink 的结果（P1-3 生产接线）。
#[derive(Debug)]
enum RebuildOutcome {
    /// 重建 sink 后用同一前缀重试成功；值为 durable 后的 `head_seq`。
    Written(u64),
    /// 不可恢复：必须关闭并重开文档（不再自动重试，也不再写库）。
    Unrecoverable(String),
    /// 重建后重试遇到普通写失败（磁盘/SQLite 等）：journal 保留，下一次 flush 再试。
    Retryable(String),
}

/// `HeadConflict` 后的有界自动重建：`load_head` → 新 CAS 基准 sink → 重试同一前缀一次。
///
/// **为什么不是无条件重试**：journal 条目的 `PendingUpdate` 是**追加那一刻**按旧 head
/// 导出的字节，写入 durable 的 `loro_docs.head_vv` / `head_frontier` 取自前缀最后一条的
/// `end_*` 快照。只有新 head 的 ops 全部包含在该快照的 VV 里（即外部推进写的是本会话
/// live 已知的 op）时，原样重试才与本会话 live 一致；否则 head 快照会缺掉外部 ops，
/// 下一次 `load_body_doc` 的完整性校验直接 `Corrupt`。后者 fail-closed：保留 journal、
/// 提示重开文档，绝不硬写。
///
/// `durable_base` 是本次写入用的 CAS 基准；新 head 必须严格大于它，回退的基准一律拒绝。
fn retry_prefix_after_rebuild(
    vault: &Vault,
    doc_id: &str,
    entries: &[JournalEntry],
    pending: usize,
    durable_base: u64,
    sync: Option<SyncContext>,
) -> RebuildOutcome {
    let head = match load_head(vault, doc_id) {
        Ok(head) => head,
        Err(error) => {
            return RebuildOutcome::Unrecoverable(reopen_message(
                doc_id,
                pending,
                format!("读不到当前 head（{error}），无法重建 sink"),
            ));
        }
    };
    if head.head_seq <= durable_base {
        return RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            format!(
                "库中 head 没有前进（CAS 基准 {durable_base}，读到 {}）；拒绝用回退的基准重写",
                head.head_seq
            ),
        ));
    }
    // 空前缀在 flusher 里不可达；防御性 fail-closed，不 panic。
    let Some(last) = entries.last() else {
        return RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            "未 ACK 前缀为空，无法校验 base".to_owned(),
        ));
    };
    let prefix_vv = match loro::VersionVector::decode(&last.update.end_vv) {
        Ok(vv) => vv,
        Err(error) => {
            return RebuildOutcome::Unrecoverable(reopen_message(
                doc_id,
                pending,
                format!("未 ACK 前缀的 end_vv 解码失败（{error}）"),
            ));
        }
    };
    let head_vv = match head.version_vector() {
        Ok(vv) => vv,
        Err(error) => {
            return RebuildOutcome::Unrecoverable(reopen_message(
                doc_id,
                pending,
                format!("新 head 的 VV 解码失败（{error}）"),
            ));
        }
    };
    if head_vv.is_empty() {
        return RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            format!(
                "新 head（head_seq {}）的 VV 为空，无法证明未 ACK 前缀与新 base 兼容",
                head.head_seq
            ),
        ));
    }
    if !prefix_vv.includes_vv(&head_vv) {
        return RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            format!(
                "库中 head 已推进到 {} 且包含本会话 live 文档未知的并发 ops；\
                 原样重试会写坏 head_vv/frontier，已拒绝自动重建",
                head.head_seq
            ),
        ));
    }

    // 同一个后台 flusher 线程内重建 sink（复用既有 Vault 写连接，不新建 journal/连接）。
    let mut sink = VaultJournalSink::new(vault.clone(), doc_id.to_owned(), head.head_seq);
    if let Some(sync) = sync {
        sink = sink.with_sync_context(sync);
    }
    match sink.write_prefix(entries) {
        Ok(seq) => RebuildOutcome::Written(seq),
        Err(StorageError::HeadConflict {
            expected, actual, ..
        }) => RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            format!("自动重建后再发生 head 冲突（基准 {expected}，实际 {actual}）"),
        )),
        Err(error) => RebuildOutcome::Retryable(format!(
            "{doc_id} 自动重建后重试前缀失败（journal 保留，等待下次 flush）：{error}"
        )),
    }
}

/// 统一的「需要重开文档」失败文案：闸门/UI 只看这一个字符串就必须知道该做什么。
fn reopen_message(doc_id: &str, pending: usize, reason: String) -> String {
    format!(
        "需要重开文档：{doc_id} 的未 ACK journal 无法安全自动重建（{reason}）；\
         {pending} 条未 ACK 更新保留在当前会话 journal 中（未写入、未静默丢弃）。\
         请关闭并重开该文档，从 durable 前缀重新加载后再继续编辑；\
         重开前未 durable 的本地改动不会自动落库"
    )
}
