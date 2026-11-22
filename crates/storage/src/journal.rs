//! 未 ACK 的写入 journal + 背压（ADR 0001 §3.4「A 已提交而 B 未完成」）。
//!
//! 模型：
//!
//! - **单份可追加 journal**：已产生但**尚未 durable** 的 update 按到达顺序排成一条 FIFO；
//! - **writer 取连续前缀**：flush 永远从头取 `min(pending, max_batch)` 条，
//!   不会跳过更早的条目（skip 就会造成 seq 空洞）；
//! - **durable 后裁剪**：只有 sink 成功返回后，才把这一段从 journal 里去掉并推进水位；
//! - **慢 I/O 合并 + 背压**：一批 flush 把当前所有 pending 前缀**一次性**写进同一个
//!   SQLite 事务；pending 达到 `capacity` 时 `submit` 直接返回
//!   [`StorageError::WriteBackpressure`]，**不 append**，调用方必须先 flush（拒绝无界增长）。
//!
//! 崩溃语义：sink 成功 = 已 durable；未 flush 的条目只在内存里，重启后自然丢掉。
//! 因此「已 ACK 的不丢、未 ACK 的只丢连续前缀之后」由本模块与 [`crate::store`] 的 A 事务共同保证。
//!
//! # head 过期：可重试 vs 必须重建 session（P1-3 契约）
//!
//! journal 条目里的 `PendingUpdate` 是**追加那一刻**按当时 head 导出的字节
//! （`end_frontier` / `end_vv` 是那次导出的快照），**不可重算**。如果同一文档的 head
//! 被别的 writer 推进（同步引擎 integrate、`SessionDriver` 的 adopt、另一个 Vault
//! 进程……），前缀的 CAS 基准（`loro_docs.head_seq`）就永远对不上：`append_updates`
//! 每次都返回 [`StorageError::HeadConflict`]，但**同一份字节重试多少次都不会成功**。
//! 这正是旧实现「flush 一直 HeadConflict、水位永远不前进」的卡死来源。
//!
//! 本模块用一条明确的二分法代替无限重试：
//!
//! - [`JournalWriter::flush`] 捕获 sink 的 [`StorageError::HeadConflict`]，**不重试**
//!   （0 次），立即转成终态错误 [`StorageError::JournalHeadStale`]
//!   （`doc_id` / `expected` / `actual` / `pending` 齐全，
//!   [`StorageError::requires_session_rebuild`] 为 `true`）。其它错误（磁盘失败等）
//!   原样透传 —— 那才是可重试的，journal 原样保留。
//! - 进入 stale 之后，`flush` / `flush_all` **不再触碰数据库**，幂等地返回同一个
//!   stale 错误；`submit` 也返回 `JournalHeadStale`（writer 已冻结，新条目必须交给
//!   重建后的 writer）。因此调用方无论怎么轮询，都不会挂死、也不会刷屏重试。
//! - **谁负责重建**：持有 live `BodyDoc` 的会话层（`session::JournalHandle` /
//!   `SessionDriver`，实现在 `crate::session::flush`）。重建顺序：
//!   ① 先停外部写并按 ADR 0012 对账（`HeadConflict` 先停外部写、验证授权、再对账，
//!   然后才允许有界重放）；② 把 durable journal 重放到**新 head**（`load_body_doc` /
//!   `load_head`），让内存文档与库一致；③ 用新 head 的 VV 重新导出本地意图
//!   （`BodyDoc::pending_update`）——只有这一步得到的字节才能写；④ 用新 `head_seq`
//!   造一个 [`VaultJournalSink`]，调用 [`JournalWriter::rebuild_sink`] 装回去。
//! - **未 durable 的旧条目怎么办**：`rebuild_sink` 把它们**原样交还**调用方（返回值）
//!   并从 writer 的 journal 里清空。它们**没有被写入、也没有被静默丢弃** —— 调用方
//!   必须用 ③ 重新导出的增量覆盖这段内容；若调用方决定不要，也必须显式消费返回值
//!   并在上层报告，不能假装成功。重建后 `durable_seq` 等于新 sink 的 CAS 基准。
//! - **有界重试规则**：CAS 冲突在 journal 层的自动重试是 **0** 次（基准是常量，
//!   重试不可能成功）；需要重放时由 session 层按 ADR 0012 做**有界**重放（一次），
//!   再失败就上报告需要人工处理。本模块不提供、也不会退化成无限重试。

use std::collections::VecDeque;

use crate::body_doc::PendingUpdate;
use crate::error::StorageError;
use crate::store::{OriginMarks, UpdateRecord};
use crate::sync_bridge::{self, SyncContext, SyncIntent};
use crate::Vault;

/// 一条 update 的**来源**（ADR 0009：只有本地提交才产生出站意图）。
///
/// - [`JournalOrigin::Local`]：真实本地写入 —— 用户输入 / IME / undo / redo / 结构提交，
///   启用同步上下文时在同一个 A 事务里写一条 `sync_outbox` 意图；
/// - [`JournalOrigin::Remote`]：远端导入 / adopt，**必须不产生任何意图**，否则会把刚
///   收到的数据原样推回 relay（自激循环，且破坏 epoch/ACK 语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JournalOrigin {
    /// 本机产生的更新（本地输入、undo/redo）。
    Local,
    /// 从远端导入的更新（`DocumentSession::apply_remote`）。
    Remote,
}

impl JournalOrigin {
    /// 是否应该产生出站同步意图。
    #[must_use]
    pub const fn produces_sync_intent(self) -> bool {
        matches!(self, Self::Local)
    }
}

/// 一条待落库的 update。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// 所属文档。
    pub doc_id: String,
    /// 批次 ID（同一次 flush 共用一个 batch_id）。
    pub batch_id: String,
    /// 增量本身。
    pub update: PendingUpdate,
    /// 来源（决定 flush 时是否产生出站意图）。
    pub origin: JournalOrigin,
}

impl JournalEntry {
    /// 本地提交（会产生同步意图）。
    #[must_use]
    pub fn local(
        doc_id: impl Into<String>,
        batch_id: impl Into<String>,
        update: PendingUpdate,
    ) -> Self {
        Self {
            doc_id: doc_id.into(),
            batch_id: batch_id.into(),
            update,
            origin: JournalOrigin::Local,
        }
    }

    /// 远端导入（**不**产生同步意图）。
    #[must_use]
    pub fn remote(
        doc_id: impl Into<String>,
        batch_id: impl Into<String>,
        update: PendingUpdate,
    ) -> Self {
        Self {
            doc_id: doc_id.into(),
            batch_id: batch_id.into(),
            update,
            origin: JournalOrigin::Remote,
        }
    }
}

/// 只可追加的未 ACK journal。
#[derive(Debug)]
pub struct PendingJournal {
    capacity: usize,
    entries: VecDeque<JournalEntry>,
    next_id: u64,
}

impl PendingJournal {
    /// 创建容量为 `capacity` 条的 journal；容量必须 > 0，否则永远只能报背压。
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: VecDeque::new(),
            next_id: 0,
        }
    }

    /// 追加一条。已满时返回 [`StorageError::WriteBackpressure`] 且**不写入**。
    pub fn append(&mut self, entry: JournalEntry) -> Result<u64, StorageError> {
        if self.entries.len() >= self.capacity {
            return Err(StorageError::WriteBackpressure {
                capacity: self.capacity,
            });
        }
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push_back(entry);
        Ok(id)
    }

    /// 当前未 ACK 条数。
    pub fn pending_len(&self) -> usize {
        self.entries.len()
    }

    /// 容量上限。
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// journal 是否已满（调用方应先 flush）。
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    /// 取从头开始的连续前缀（最多 `max` 条），只读不改。
    ///
    /// 取的一定是 FIFO 的最前面若干条：跳过更早的条目会造成 seq 空洞，
    /// 所以 flush 只能「从头开始」而不能挑着写。
    pub fn peek_prefix(&mut self, max: usize) -> &[JournalEntry] {
        let take = max.min(self.entries.len());
        if take == 0 {
            return &[];
        }
        // VecDeque 可能是环形，先整理成连续内存再切片
        self.entries.make_contiguous();
        let (front, _) = self.entries.as_slices();
        &front[..take]
    }

    /// durable 之后裁剪前缀。
    pub fn ack_prefix(&mut self, count: usize) -> Result<(), StorageError> {
        if count > self.entries.len() {
            return Err(StorageError::Corrupt {
                context: format!("ack_prefix({count}) 超过未 ACK 条数 {}", self.entries.len()),
            });
        }
        for _ in 0..count {
            self.entries.pop_front();
        }
        Ok(())
    }

    /// 取走全部未 ACK 条目。
    ///
    /// **只**供 [`JournalWriter::rebuild_sink`] 在 head 过期后显式交还给调用方使用：
    /// 正常路径一律走 [`Self::ack_prefix`]，这里既不写库也不推进任何水位。
    fn drain_all(&mut self) -> Vec<JournalEntry> {
        self.entries.drain(..).collect()
    }
}

/// durable 落点（生产实现见 [`VaultJournalSink`]，测试可注入慢盘/失败实现）。
pub trait DurableSink {
    /// 把一段**连续前缀**写 durable；成功返回这段里最后一条的 `seq`。
    ///
    /// 实现必须是原子的：要么整段 durable，要么什么都没写（否则会丢已 ACK 数据）。
    ///
    /// CAS 基准冲突必须返回 [`StorageError::HeadConflict`]（不能自己改基准硬写）：
    /// [`JournalWriter`] 据此把 writer 判成 head 过期，而不是无限重试同一份字节。
    fn write_prefix(&mut self, entries: &[JournalEntry]) -> Result<u64, StorageError>;

    /// 当前 sink 使用的 CAS 基准（= 它认为已 durable 的 `head_seq`）。
    ///
    /// writer 用它给 `durable_seq` 定初值，并在 [`JournalWriter::rebuild_sink`] 之后重新对齐
    /// 水位。实现必须返回**真正用于 CAS 的**那个值，否则重建后的水位会骗人。
    fn head_seq(&self) -> u64;
}

/// 一次 flush 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushReport {
    /// 本次写 durable 的条数。
    pub entries: usize,
    /// 写完后的 durable seq（sink 返回）。
    pub durable_seq: u64,
}

/// head 过期的终态记录（见模块头契约）。
///
/// 只要它非空，writer 就冻结：`submit` / `flush` 一律返回同一个
/// [`StorageError::JournalHeadStale`]，直到调用方 [`JournalWriter::rebuild_sink`]。
#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleHead {
    doc_id: String,
    expected: u64,
    actual: u64,
}

impl StaleHead {
    fn to_error(&self, pending: usize) -> StorageError {
        StorageError::JournalHeadStale {
            doc_id: self.doc_id.clone(),
            expected: self.expected,
            actual: self.actual,
            pending,
        }
    }
}

/// journal writer：持有 journal 与 sink，串行 flush。
#[derive(Debug)]
pub struct JournalWriter<S: DurableSink> {
    journal: PendingJournal,
    sink: S,
    max_batch: usize,
    durable_seq: u64,
    flushed_entries: u64,
    /// 非空 = CAS 基准已过期，writer 已冻结（见模块头契约）。
    stale: Option<StaleHead>,
}

impl<S: DurableSink> JournalWriter<S> {
    /// `max_batch` 是一次 flush 最多合并多少条（慢 I/O 时 pending 会堆到这里为止）。
    ///
    /// `durable_seq` 初值取 sink 的 CAS 基准（[`DurableSink::head_seq`]），
    /// 因此空 journal 上 flush 报告的是真实水位，而不是 0。
    pub fn new(sink: S, capacity: usize, max_batch: usize) -> Self {
        let durable_seq = sink.head_seq();
        Self {
            journal: PendingJournal::new(capacity),
            sink,
            max_batch: max_batch.max(1),
            durable_seq,
            flushed_entries: 0,
            stale: None,
        }
    }

    /// 追加一条待落库 update；满了就是背压，调用方必须 flush 后重试。
    ///
    /// head 过期后拒绝（返回 [`StorageError::JournalHeadStale`]）：writer 已冻结，
    /// 新条目必须交给重建后的 writer，否则只会堆在一条永远写不动的前缀后面。
    pub fn submit(&mut self, entry: JournalEntry) -> Result<u64, StorageError> {
        if let Some(stale) = self.stale.as_ref() {
            return Err(stale.to_error(self.journal.pending_len()));
        }
        self.journal.append(entry)
    }

    /// 把当前前缀合并成一次 sink 写；成功后裁剪 journal 并推进水位。
    ///
    /// 失败时 journal **原样保留**（sink 承诺原子），下次重试不会丢已成功部分。
    ///
    /// 唯一的例外是 CAS 冲突：sink 返回 [`StorageError::HeadConflict`] 时前缀字节已不可
    /// 重放，本方法**不重试**（0 次），标记 stale 并返回
    /// [`StorageError::JournalHeadStale`] —— 调用方必须按模块头的契约重建 session，
    /// 而不是再 flush 一次。
    pub fn flush(&mut self) -> Result<FlushReport, StorageError> {
        // 终态优先：stale 之后连数据库都不碰（有界重试 = 0 次）。
        if let Some(stale) = self.stale.as_ref() {
            return Err(stale.to_error(self.journal.pending_len()));
        }

        // 先拷出前缀，避免同时借用 journal 与 sink
        let prefix: Vec<JournalEntry> = self.journal.peek_prefix(self.max_batch).to_vec();
        if prefix.is_empty() {
            return Ok(FlushReport {
                entries: 0,
                durable_seq: self.durable_seq,
            });
        }

        let entries = prefix.len();
        let durable_seq = match self.sink.write_prefix(&prefix) {
            Ok(seq) => seq,
            // CAS 冲突是**终态**：前缀的 end_vv/end_frontier 是按旧 head 导出的快照，
            // 重新导出只能由持有 live 文档的会话层做；同一份字节重试不可能成功。
            Err(StorageError::HeadConflict {
                doc_id,
                expected,
                actual,
            }) => {
                let stale = StaleHead {
                    doc_id,
                    expected,
                    actual,
                };
                let error = stale.to_error(self.journal.pending_len());
                self.stale = Some(stale);
                return Err(error);
            }
            // 其它错误（磁盘失败等）原样透传：journal 原样保留，同一前缀可以重试。
            Err(error) => return Err(error),
        };
        self.journal.ack_prefix(entries)?;
        self.durable_seq = durable_seq;
        self.flushed_entries += entries as u64;

        Ok(FlushReport {
            entries,
            durable_seq,
        })
    }

    /// flush 直到 journal 空或没有进展（一次性排空，供关闭/手动保存用）。
    ///
    /// head 过期时第一拍就返回 [`StorageError::JournalHeadStale`]，不会空转。
    pub fn flush_all(&mut self) -> Result<usize, StorageError> {
        let mut total = 0;
        while self.journal.pending_len() > 0 {
            let report = self.flush()?;
            if report.entries == 0 {
                break;
            }
            total += report.entries;
        }
        Ok(total)
    }

    /// writer 是否已进入「head 过期、必须重建」的终态。
    pub fn is_stale(&self) -> bool {
        self.stale.is_some()
    }

    /// 重建 sink：head 过期后**唯一**合法的恢复入口（见模块头契约）。
    ///
    /// 调用方必须先用新 head 重放 live 文档并重新导出本地意图；本方法只负责把 writer
    /// 从 stale 拉回可用状态：
    ///
    /// - 把旧 journal 里未写入的条目**原样交还**（返回值），并从 journal 里清空 ——
    ///   它们不写库、也**不被静默丢弃**；调用方要么用重新导出的增量覆盖其内容，
    ///   要么显式报告这次丢弃；
    /// - `durable_seq` 对齐新 sink 的 CAS 基准（[`DurableSink::head_seq`]）；
    /// - 未处于 stale 时拒绝：还有能写的前缀就换 sink，等于把本可落库的条目丢掉。
    pub fn rebuild_sink(&mut self, sink: S) -> Result<Vec<JournalEntry>, StorageError> {
        if self.stale.is_none() {
            return Err(StorageError::Corrupt {
                context: "journal 未处于 head 过期状态，拒绝重建 sink（会丢弃仍可写入的条目）"
                    .to_owned(),
            });
        }
        let handed_back = self.journal.drain_all();
        self.sink = sink;
        self.stale = None;
        self.durable_seq = self.sink.head_seq();
        Ok(handed_back)
    }

    /// 未 ACK 条数。
    pub fn pending_len(&self) -> usize {
        self.journal.pending_len()
    }

    /// journal 容量。
    pub fn capacity(&self) -> usize {
        self.journal.capacity()
    }

    /// 已 durable 的最后 seq。
    pub fn durable_seq(&self) -> u64 {
        self.durable_seq
    }

    /// 累计 flush 成功的条数。
    pub fn flushed_entries(&self) -> u64 {
        self.flushed_entries
    }

    /// 取回 sink（关闭/测试用）。
    pub fn into_sink(self) -> S {
        self.sink
    }
}

/// 生产用 sink：把前缀写进 [`crate::store::append_updates`] 的 A 事务。
///
/// 带 [`SyncContext`] 时（`with_sync_context`），**本地**条目在同一 A 事务里各写一条
/// `sync_outbox` 意图（与 records 一一对应）；未启用时行为与切片 2a 之前完全一致。
#[derive(Debug, Clone)]
pub struct VaultJournalSink {
    vault: Vault,
    doc_id: String,
    head_seq: u64,
    sync: Option<SyncContext>,
}

impl VaultJournalSink {
    /// 以库中当前 head_seq 作为 CAS 基准（默认**不**产生同步意图）。
    pub fn new(vault: Vault, doc_id: impl Into<String>, head_seq: u64) -> Self {
        Self {
            vault,
            doc_id: doc_id.into(),
            head_seq,
            sync: None,
        }
    }

    /// 启用本地意图：每次 flush 为本地条目生成确定性 `SyncIntent`（同事务落库）。
    #[must_use]
    pub fn with_sync_context(mut self, sync: SyncContext) -> Self {
        self.sync = Some(sync);
        self
    }

    /// 当前已 durable 的 seq。
    pub fn head_seq(&self) -> u64 {
        self.head_seq
    }
}

impl DurableSink for VaultJournalSink {
    fn write_prefix(&mut self, entries: &[JournalEntry]) -> Result<u64, StorageError> {
        if entries.is_empty() {
            return Err(StorageError::Corrupt {
                context: "sink 收到空前缀".to_owned(),
            });
        }
        for entry in entries {
            if entry.doc_id != self.doc_id {
                return Err(StorageError::Corrupt {
                    context: format!(
                        "一次 flush 里混了多个文档：期望 {}，实际 {}",
                        self.doc_id, entry.doc_id
                    ),
                });
            }
        }
        // batch_id 逐条保留：合并 flush 可能跨多个逻辑批次
        let records: Vec<UpdateRecord> = entries
            .iter()
            .map(|entry| UpdateRecord::new(entry.batch_id.clone(), entry.update.clone()))
            .collect();
        // 远端导入条目：其 `end_vv - 上一条 end_vv` 是远端真正带来的 ops，
        // 同事务记入「远端已知 VV」（baseline tombstone 判定用）。
        let remote_marks: Vec<bool> = entries
            .iter()
            .map(|entry| entry.origin == JournalOrigin::Remote)
            .collect();
        let head = match self.sync {
            // 默认路径（未启用同步）：行为与切片 2a 之前一致（只是多记远端已知 VV）。
            None => sync_bridge::append_updates_with_intent_slots(
                &self.vault,
                &self.doc_id,
                self.head_seq,
                &records,
                &[],
                OriginMarks::PerRecord(&remote_marks),
            )?,
            Some(ctx) => {
                // 一 update 一意图（remote 条目为空位）；seq 必须与 A 事务写进
                // `loro_updates.seq` 的值完全一致，否则意图会指向别人的 update。
                let mut slots: Vec<Option<SyncIntent>> = Vec::with_capacity(entries.len());
                for (index, entry) in entries.iter().enumerate() {
                    if !entry.origin.produces_sync_intent() {
                        slots.push(None);
                        continue;
                    }
                    let seq = self
                        .head_seq
                        .checked_add(1)
                        .and_then(|value| value.checked_add(index as u64))
                        .ok_or_else(|| StorageError::Corrupt {
                            context: format!("{} 本地 seq 溢出", self.doc_id),
                        })?;
                    slots.push(Some(sync_bridge::local_sync_intent(
                        &self.doc_id,
                        seq,
                        &entry.batch_id,
                        ctx,
                    )?));
                }
                sync_bridge::append_updates_with_intent_slots(
                    &self.vault,
                    &self.doc_id,
                    self.head_seq,
                    &records,
                    &slots,
                    OriginMarks::PerRecord(&remote_marks),
                )?
            }
        };
        self.head_seq = head.head_seq;
        Ok(head.head_seq)
    }

    /// CAS 基准 = 本 sink 已 durable 的 head_seq（与固有方法 `head_seq` 同一个值）。
    fn head_seq(&self) -> u64 {
        self.head_seq
    }
}
