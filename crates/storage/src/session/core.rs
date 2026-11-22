//! 会话核：`EditorDocumentPort` 的串行 reducer（ADR 0003 §5）。
//!
//! 一个 [`DocumentSession`] = 一个正文文档的**唯一 live owner**：
//!
//! - 持有 live [`BodyDoc`]（Loro）、generation / session_epoch / live_rev；
//! - 持有唯一未 ACK journal 与事件队列；
//! - 所有编辑走 [`DocumentSession::apply`]，受理顺序**固定**（见方法文档）；
//! - `flush` 只投递命令即返回票据，SQLite 由后台线程写（见 [`super::flush`]）。
//!
//! 端口协议类型在 [`super::protocol`]，本模块**不新增协议字段**（缺字段应先报告 Lead）。
//! 远端导入路径在 [`DocumentSession::apply_remote`]，撤销/重做在
//! [`DocumentSession::undo`] / [`DocumentSession::redo`]，都用同一个 Loro `UndoManager`。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use loro::{UndoManager, VersionVector};
use menghunote_text::UnicodeScalar;
use rusqlite::types::Value;

use super::flush::{lock, JournalHandle};
use super::gate::ExternalCommitGate;
use super::protocol::{
    Applied, ChangeSource, CommandId, DeltaSplice, DocId, DurableAck, EditOutcome, EditRequest,
    FlushReason, FlushTicket, Generation, HistoryState, LiveDelta, LiveRev, MergeInfo,
    OpenSnapshot, PortError, RejectReason, Rejected, ScalarEdit, SessionEpoch, SessionEvent,
    StaleBase,
};
use crate::body_doc::BodyDoc;
use crate::error::StorageError;
use crate::store;
use crate::Vault;

/// 会话配置。
///
/// `flush_delay` 是**测试/故障注入**用的可控慢盘延迟，生产用 `None`；
/// `sync` 是 ADR 0009 切片 2b 的本地意图上下文，`None` = 不产生 `sync_outbox`
/// 行（默认构建与既有调用点行为完全不变）。
#[derive(Debug, Clone, Copy)]
pub struct SessionOptions {
    /// 本机 peer id（每设备唯一）。
    pub peer_id: u64,
    /// 会话纪元：旧会话的迟到请求会被拒。
    pub session_epoch: SessionEpoch,
    /// 未 ACK journal 容量（条）。满了就是背压（拒绝新写，不丢已 ACC 部分）。
    pub journal_capacity: usize,
    /// 一次 A 事务最多合并多少条（慢盘时 pending 会堆到这里为止）。
    pub flush_batch: usize,
    /// 注入的 flush 延迟（模拟慢盘）。
    pub flush_delay: Option<Duration>,
    /// 本地写入的同步上下文；`None` = 不产生出站意图（默认）。
    pub sync: Option<crate::sync_bridge::SyncContext>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            peer_id: 1,
            session_epoch: SessionEpoch(1),
            journal_capacity: 64,
            flush_batch: 8,
            flush_delay: None,
            sync: None,
        }
    }
}

/// 一个正文文档的会话（串行 reducer）。
///
/// 字段对 `super::gate` 开放（`pub(crate)`）：外部提交闸门与 adopt 必须直接操作**同一份**
/// live 文档、journal 与水位，复制状态只会制造撕裂。对外部 crate 仍然是完全私有的。
pub struct DocumentSession {
    pub(crate) vault: Vault,
    pub(crate) doc_id: DocId,
    /// 本机 peer id（重放 scratch 副本时用；不影响 import）。
    pub(crate) peer_id: u64,
    generation: Generation,
    pub(crate) session_epoch: SessionEpoch,
    pub(crate) doc: BodyDoc,
    pub(crate) live_rev: LiveRev,
    pub(crate) live_frontier: Vec<u8>,
    /// 上一次写入 journal 的 VV：增量导出的起点（不在 durable 前推进）。
    pub(crate) journal_base_vv: VersionVector,
    pub(crate) journal: JournalHandle,
    undo: UndoManager,
    /// command_id 幂等缓存（只缓存终态：Applied；背压/过期不缓存）。
    command_cache: HashMap<CommandId, EditOutcome>,
    /// composition 深度（`> 0` 表示组合中；嵌套 begin 计数）。
    composition_depth: u32,
    /// composition 期间延后的远端 update（按到达顺序；结束后串行合并，不丢）。
    deferred_remote: Vec<Vec<u8>>,
    /// 外部提交闸门（ADR 0009 切片 3a）；无新线程/新全局状态。
    pub(crate) gate: ExternalCommitGate,
}

impl std::fmt::Debug for DocumentSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentSession")
            .field("doc_id", &self.doc_id)
            .field("generation", &self.generation)
            .field("session_epoch", &self.session_epoch)
            .field("live_rev", &self.live_rev)
            .field("pending", &self.pending_len())
            .finish()
    }
}

impl DocumentSession {
    /// 打开会话：从库里的 snapshot + updates 重放出 live 文档。
    pub fn open(vault: &Vault, doc_id: &str, options: SessionOptions) -> Result<Self, PortError> {
        // 同步上下文在打开时校验一次：epoch/peer 非法要尽早报错，
        // 而不是让之后每一次 flush 都因非法 intent 失败、journal 永远重试。
        if let Some(sync) = options.sync {
            sync.validate().map_err(port)?;
        }
        let generation = load_generation(vault, doc_id)?;
        let (doc, head) =
            store::load_body_doc(vault, doc_id, options.peer_id, None).map_err(port)?;
        let live_frontier = doc.frontiers().encode();
        // 打开时没有未 ACK 条目：增量基准就是 durable 的 VV
        let journal_base_vv = head.version_vector().map_err(port)?;
        let journal = JournalHandle::spawn(
            vault.clone(),
            doc_id.to_owned(),
            head,
            options.journal_capacity,
            options.flush_batch,
            options.flush_delay,
            options.sync,
        );
        let undo = doc.undo_manager();
        Ok(Self {
            vault: vault.clone(),
            doc_id: doc_id.to_owned(),
            peer_id: options.peer_id,
            generation,
            session_epoch: options.session_epoch,
            doc,
            live_rev: LiveRev(0),
            live_frontier,
            journal_base_vv,
            journal,
            undo,
            command_cache: HashMap::new(),
            composition_depth: 0,
            deferred_remote: Vec::new(),
            gate: ExternalCommitGate::default(),
        })
    }

    /// 文档 ID。
    pub fn doc_id(&self) -> &str {
        &self.doc_id
    }

    /// 文档代际。
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// 会话纪元。
    pub fn session_epoch(&self) -> SessionEpoch {
        self.session_epoch
    }

    /// 当前 live 修订号（`EditRequest.base_live_rev` 的正确基准）。
    pub fn live_rev(&self) -> LiveRev {
        self.live_rev
    }

    /// 当前 live frontier（不透明字节）。
    pub fn live_frontier(&self) -> &[u8] {
        &self.live_frontier
    }

    /// 当前 live 正文（LF 归一后的 Markdown）。
    pub fn markdown(&self) -> String {
        self.doc.text()
    }

    /// 打开快照（调用方据此初始化自己的 Rope 镜像）。
    pub fn snapshot(&self) -> OpenSnapshot {
        OpenSnapshot {
            doc_id: self.doc_id.clone(),
            generation: self.generation,
            session_epoch: self.session_epoch,
            live_rev: self.live_rev,
            live_frontier: self.live_frontier.clone(),
            live_vv: self.doc.vv().encode(),
            // 批次号不跨会话持久化：重启后只能确认 durable 水位（head_seq）
            durable_batch: None,
            durable_head_seq: self.journal.durable_head_seq(),
            markdown: self.doc.text(),
        }
    }

    /// 未 ACK 条数。
    pub fn pending_len(&self) -> usize {
        self.journal.pending_len()
    }

    /// 未 ACK update 的总字节数（调度器用：达到 64 KiB 立即 flush）。
    pub fn pending_bytes(&self) -> usize {
        self.journal.pending_bytes()
    }

    /// 已 durable 的 head seq。
    pub fn durable_head_seq(&self) -> u64 {
        self.journal.durable_head_seq()
    }

    /// 已 durable 到哪个 live 修订号。
    pub fn durable_through(&self) -> LiveRev {
        self.journal.durable_through()
    }

    /// 取走事件队列（Durable / Remote）。
    pub fn events(&self) -> Vec<SessionEvent> {
        self.journal.drain_events()
    }

    /// 等待某个 live 修订号变成 durable（测试/关闭前等待用）。
    ///
    /// 只消费覆盖到 `target` 的 `Durable`，其它事件留在队列里。
    pub fn wait_for_durable(&self, target: LiveRev, timeout: Duration) -> Option<DurableAck> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut found = None;
            {
                let mut queue = lock(self.journal.events_ref());
                let mut remaining = std::collections::VecDeque::new();
                while let Some(event) = queue.pop_front() {
                    match event {
                        SessionEvent::Durable(ack)
                            if ack.through_live_rev >= target && found.is_none() =>
                        {
                            found = Some(ack);
                        }
                        other => remaining.push_back(other),
                    }
                }
                *queue = remaining;
            }
            if let Some(ack) = found {
                return Some(ack);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// 受理一次编辑请求（顺序固定，任何一步失败都**不改 Loro**）。
    ///
    /// 顺序：
    ///
    /// 1. 校验 `doc_id` / `generation` / `session_epoch`；
    /// 2. 按 `command_id` 幂等（终态直接返回首次结果）；
    /// 3. 外部提交闸门 Held → `Rejected{NotReady}`（ADR 0009：Held 期间禁写，fail-closed）；
    /// 4. composition 进行中 → `Rejected{CompositionActive}`；
    /// 5. 校验 `edits`：升序、互不重叠、`end <= scalar_len`；`base_live_rev` 不匹配 → `Stale`；
    /// 6. journal 容量准入（满了 → `Rejected{Backpressure}`，此时 Loro 未被触碰）；
    /// 7. 才允许改 Loro：按**逆序** `delete + insert`，导出增量、进 journal、推进 live_rev。
    pub fn apply(&mut self, request: EditRequest) -> EditOutcome {
        // 1. 身份与代际
        if request.doc_id != self.doc_id {
            return rejected(request.command_id, RejectReason::UnknownDoc);
        }
        if request.generation != self.generation {
            return rejected(request.command_id, RejectReason::WrongGeneration);
        }
        if request.session_epoch != self.session_epoch {
            return rejected(request.command_id, RejectReason::WrongEpoch);
        }

        // 2. 幂等：只命中终态（背压会随 flush 解除，不能缓存）
        if let Some(cached) = self.command_cache.get(&request.command_id) {
            return cached.clone();
        }

        // 2.5 外部提交闸门 Held：一律拒绝新的本地正文改动（fail-closed，不碰 Loro）。
        //     3b 负责让界面在 Held 期间禁写；存储层这里必须自己兜底。
        if self.gate.is_held() {
            return rejected(request.command_id, RejectReason::NotReady);
        }

        // 3. composition 闸门：组合期间拒绝其它本地正文改动（**不碰 Loro**）
        if self.is_composing() {
            return rejected(request.command_id, RejectReason::CompositionActive);
        }

        // 4. 区间校验 + 基准校验（都在改 Loro 之前）
        let scalar_len = self.doc.scalar_len();
        if validate_edits(&request.edits, scalar_len).is_err() {
            return rejected(request.command_id, RejectReason::InvalidRange);
        }
        if request.base_live_rev != self.live_rev {
            return EditOutcome::Stale(StaleBase {
                command_id: request.command_id,
                current_live_rev: self.live_rev,
                compensation: None,
            });
        }

        // 5+6. 容量准入与 Loro 修改在同一把锁内完成：
        //      持锁期间 flush 线程只会**释放**容量，因此准入通过后 append 不可能失败。
        let mut guard = lock(self.journal.state_ref());
        if guard.journal.is_full() {
            drop(guard);
            return rejected(request.command_id, RejectReason::Backpressure);
        }

        if request.edits.is_empty() {
            let outcome = EditOutcome::Applied(Applied {
                command_id: request.command_id.clone(),
                live_rev: self.live_rev,
                live_frontier: self.live_frontier.clone(),
                splice: DeltaSplice {
                    at: UnicodeScalar::new(0),
                    delete_scalars: 0,
                    insert: String::new(),
                },
                local_echo: true,
                source: ChangeSource::Local,
                merge: MergeInfo::default(),
            });
            self.command_cache
                .insert(request.command_id.clone(), outcome.clone());
            return outcome;
        }

        // 逆序应用：后面的编辑先做，前面编辑的坐标才不会失效（协议 §「按逆序应用」）
        self.doc.set_next_commit_origin(&request.origin);
        if self.undo.group_start().is_err() {
            drop(guard);
            return rejected(request.command_id, RejectReason::NotReady);
        }
        for edit in request.edits.iter().rev() {
            let start = edit.start.get();
            let len = edit.end.get() - start;
            if let Err(_error) = self.doc.replace_scalars(start, len, &edit.replacement) {
                // 校验已保证范围内合法；真出错时丢弃分组，不推进任何水位
                self.undo.group_end();
                drop(guard);
                return rejected(request.command_id, RejectReason::InvalidRange);
            }
        }
        self.undo.group_end();

        // 没有产生任何 op（例如空替换）→ 不回显、不进 journal、不推进 live_rev
        let vv_after = self.doc.vv();
        if vv_after == self.journal_base_vv {
            let first = request.edits.first().map_or(0, |edit| edit.start.get());
            let outcome = EditOutcome::Applied(Applied {
                command_id: request.command_id.clone(),
                live_rev: self.live_rev,
                live_frontier: self.live_frontier.clone(),
                splice: DeltaSplice {
                    at: UnicodeScalar::new(first),
                    delete_scalars: 0,
                    insert: String::new(),
                },
                local_echo: true,
                source: ChangeSource::Local,
                merge: MergeInfo::default(),
            });
            self.command_cache
                .insert(request.command_id.clone(), outcome.clone());
            return outcome;
        }

        let update = match self.doc.pending_update(&self.journal_base_vv) {
            Ok(update) => update,
            Err(_) => {
                drop(guard);
                return rejected(request.command_id, RejectReason::NotReady);
            }
        };

        let rev = LiveRev(self.live_rev.0 + 1);
        // 持锁且已准入：这里不可能失败（见上面的注释），失败只可能是内部不一致
        if JournalHandle::append(
            &mut guard,
            &self.doc_id,
            "session",
            crate::journal::JournalOrigin::Local,
            update,
            rev,
        )
        .is_err()
        {
            debug_assert!(false, "持锁且已准入时 journal 追加失败");
            drop(guard);
            return rejected(request.command_id, RejectReason::NotReady);
        }
        drop(guard);

        self.journal_base_vv = vv_after;
        self.live_rev = rev;
        self.live_frontier = self.doc.frontiers().encode();

        let first = request.edits.first();
        let splice = DeltaSplice {
            at: first.map_or(UnicodeScalar::new(0), |edit| edit.start),
            delete_scalars: first.map_or(0, |edit| (edit.end.get() - edit.start.get()) as u64),
            insert: first
                .map(|edit| edit.replacement.clone())
                .unwrap_or_default(),
        };
        let outcome = EditOutcome::Applied(Applied {
            command_id: request.command_id.clone(),
            live_rev: self.live_rev,
            live_frontier: self.live_frontier.clone(),
            splice,
            // 调用方已经把自己的镜像写好了：不得再按 splice 二次应用
            local_echo: true,
            source: ChangeSource::Local,
            merge: MergeInfo::default(),
        });
        self.command_cache
            .insert(request.command_id.clone(), outcome.clone());
        outcome
    }

    /// 开始一次 IME composition（可嵌套，深度计数）。
    ///
    /// preedit **不经过本层**：会话只记录「组合是否在进行」，不接收 preedit 文本，
    /// 也不因 preedit 更新而阻塞。返回当前嵌套深度。
    ///
    /// 外部提交闸门 Held 期间**不进入组合**（fail-closed）并返回当前深度；需要显式
    /// 错误请用 [`DocumentSession::try_begin_composition`]（3b 用）。
    pub fn begin_composition(&mut self) -> u32 {
        match self.try_begin_composition() {
            Ok(depth) => depth,
            Err(_) => self.composition_depth,
        }
    }

    /// 开始一次 IME composition 的**可报错**版本：Held 期间返回 [`PortError`]。
    ///
    /// Held 期间通道已冻结，这里再开组合只会让 preedit 与提交竞争，因此直接拒绝。
    pub fn try_begin_composition(&mut self) -> Result<u32, PortError> {
        if self.gate.is_held() {
            return Err(PortError::Storage(
                "外部提交闸门 Held：拒绝开始 IME 组合".to_owned(),
            ));
        }
        self.composition_depth += 1;
        Ok(self.composition_depth)
    }

    /// 结束一次 composition；只有**最外层**结束才合并延后的远端导入。
    ///
    /// 重复 `end`（没有对应 `begin`）是 no-op：平台可能重复回调 cancel，
    /// 这里不 panic、不改变状态。
    pub fn end_composition(&mut self) -> Vec<LiveDelta> {
        if self.composition_depth > 0 {
            self.composition_depth -= 1;
        }
        if self.is_composing() {
            return Vec::new();
        }
        self.merge_deferred_remote()
    }

    /// 按到达顺序串行合并延后的远端 update。
    ///
    /// - 背压（journal 满）时**保留剩余项**，等 `flush` 之后调用方再调一次；
    /// - 解码/写入失败时也保留队首，不丢远端数据。
    pub fn merge_deferred_remote(&mut self) -> Vec<LiveDelta> {
        let mut merged = Vec::new();
        loop {
            if self.deferred_remote.is_empty() || !self.can_accept_remote() {
                break;
            }
            let Some(update) = self.deferred_remote.first().cloned() else {
                break;
            };
            match self.merge_remote_inner(&update, false) {
                Ok(outcome) => {
                    self.deferred_remote.remove(0);
                    if let Some(delta) = outcome {
                        merged.push(delta);
                    }
                }
                // 保留在队首：不丢远端数据，调用方可稍后重试
                Err(_) => break,
            }
        }
        merged
    }

    /// 是否正处于 composition 中。
    pub fn is_composing(&self) -> bool {
        self.composition_depth > 0
    }

    /// 当前嵌套深度（0 = 不在组合中）。
    pub fn composition_depth(&self) -> u32 {
        self.composition_depth
    }

    /// 仍未合并的远端 update 条数。
    pub fn deferred_remote_len(&self) -> usize {
        self.deferred_remote.len()
    }

    fn can_accept_remote(&self) -> bool {
        // Held 期间不合并延后的远端：闸门要求 live 水位静止，等 end 之后再合并
        !self.journal.is_full() && !self.gate.is_held()
    }

    /// 远端导入：把 update 合进 live 文档，返回驱动镜像的 [`LiveDelta`]。
    ///
    /// 容量不足时拒绝（返回 `PortError::Storage`），保证「背压时 Loro 未被修改」对远端路径同样成立。
    /// Held 期间一律拒绝（fail-closed）：调用方应在 `end_external_commit` 之后重试。
    pub fn apply_remote(&mut self, update: &[u8]) -> Result<Option<LiveDelta>, PortError> {
        if self.gate.is_held() {
            return Err(PortError::Storage(
                "外部提交闸门 Held：暂不接受远端导入".to_owned(),
            ));
        }
        if self.is_composing() {
            // 组合期间延后：进队列（不丢、不报错），结束后按到达顺序串行合并
            self.deferred_remote.push(update.to_vec());
            return Ok(None);
        }
        self.merge_remote_now(update)
    }

    /// 立即合并一条远端 update（**不做** composition 判断；闸门在 [`Self::apply_remote`]）。
    fn merge_remote_now(&mut self, update: &[u8]) -> Result<Option<LiveDelta>, PortError> {
        self.merge_remote_inner(update, true)
    }

    /// `publish = false` 时只返回 delta、不重复推进事件队列（延后合并的唯一投递口是
    /// [`Self::end_composition`] 的返回值，避免调用方把同一条变更应用到镜像两次）。
    fn merge_remote_inner(
        &mut self,
        update: &[u8],
        publish: bool,
    ) -> Result<Option<LiveDelta>, PortError> {
        if self.journal.is_full() {
            return Err(PortError::Storage(
                "未 ACK journal 已满，暂不接受远端导入（背压）".to_owned(),
            ));
        }

        let mut guard = lock(self.journal.state_ref());
        let diff = match import_updates(&self.doc, std::iter::once(update)) {
            Ok(diff) => diff,
            Err(error) => {
                drop(guard);
                return Err(port(error));
            }
        };
        if !diff.changed() {
            drop(guard);
            return Ok(None);
        }

        let splice = diff.splice();
        let after_vv = diff.after_vv;
        let from_rev = self.live_rev;
        let to_rev = LiveRev(self.live_rev.0 + 1);

        // 远端 op 也进 journal（A 事务落库后才算 durable）；容量已准入。
        let pending = match self.doc.pending_update(&self.journal_base_vv) {
            Ok(update) => update,
            Err(error) => {
                drop(guard);
                return Err(port(error));
            }
        };
        if JournalHandle::append(
            &mut guard,
            &self.doc_id,
            "remote",
            // 远端导入**绝不**产生出站意图（ADR 0009）；否则会把刚收到的数据推回 relay。
            crate::journal::JournalOrigin::Remote,
            pending,
            to_rev,
        )
        .is_err()
        {
            drop(guard);
            return Err(PortError::Storage("journal 追加失败".to_owned()));
        }
        drop(guard);

        self.journal_base_vv = after_vv;
        self.live_rev = to_rev;
        self.live_frontier = self.doc.frontiers().encode();

        let delta = LiveDelta {
            doc_id: self.doc_id.clone(),
            from_rev,
            to_rev,
            live_frontier: self.live_frontier.clone(),
            splice,
            cause: None,
            source: ChangeSource::Remote,
            merge: MergeInfo {
                concurrent: true,
                remote_changes: 1,
            },
        };
        if publish {
            lock(self.journal.events_ref()).push_back(SessionEvent::Remote(delta.clone()));
        }
        Ok(Some(delta))
    }

    /// 撤销：用**唯一** `UndoManager` 生成本地新 op（不回退 VV，也不回退远端字符）。
    ///
    /// 返回：
    /// - `Ok(Some(Applied))`：已撤销并进 journal；
    /// - `Ok(None)`：没有可撤销项（no-op undo，应提示调用方消费该项）；
    /// - `Ok(Some(Rejected{Backpressure}))`：journal 满，**未 undo**（Loro 未被修改）；
    /// - `Ok(Some(Rejected{NotReady}))`：外部提交闸门 Held，**未 undo**（fail-closed）。
    pub fn undo(&mut self) -> Result<Option<EditOutcome>, PortError> {
        // Held 期间 undo 也会改 Loro/journal：fail-closed，等 end 之后重试
        if self.gate.is_held() {
            return Ok(Some(rejected(
                CommandId::from("undo"),
                RejectReason::NotReady,
            )));
        }
        // composition 期间 undo/redo 也属于「其它本地正文改动」：拒绝且不碰 Loro
        if self.is_composing() {
            return Ok(Some(rejected(
                CommandId::from("undo"),
                RejectReason::CompositionActive,
            )));
        }
        // 容量准入在改 Loro 之前；准入通过后 flusher 只会释放容量，append 必然成功
        if self.journal.is_full() {
            return Ok(Some(rejected(
                CommandId::from("undo"),
                RejectReason::Backpressure,
            )));
        }
        let before = self.doc.text();
        let before_vv = self.doc.vv();
        if self.undo.group_start().is_err() {
            return Err(PortError::Storage("UndoManager 分组失败".to_owned()));
        }
        let changed = match self.undo.undo() {
            Ok(changed) => changed,
            Err(error) => {
                self.undo.group_end();
                return Err(port_loro(error));
            }
        };
        self.undo.group_end();
        if !changed || self.doc.vv() == before_vv {
            return Ok(None);
        }
        Ok(Some(EditOutcome::Applied(
            self.finish_history_change(&before, ChangeSource::Undo)?,
        )))
    }

    /// 重做：与 [`DocumentSession::undo`] 对称。
    pub fn redo(&mut self) -> Result<Option<EditOutcome>, PortError> {
        if self.gate.is_held() {
            return Ok(Some(rejected(
                CommandId::from("redo"),
                RejectReason::NotReady,
            )));
        }
        if self.is_composing() {
            return Ok(Some(rejected(
                CommandId::from("redo"),
                RejectReason::CompositionActive,
            )));
        }
        if self.journal.is_full() {
            return Ok(Some(rejected(
                CommandId::from("redo"),
                RejectReason::Backpressure,
            )));
        }
        let before = self.doc.text();
        let before_vv = self.doc.vv();
        let changed = match self.undo.redo() {
            Ok(changed) => changed,
            Err(error) => return Err(port_loro(error)),
        };
        if !changed || self.doc.vv() == before_vv {
            return Ok(None);
        }
        Ok(Some(EditOutcome::Applied(
            self.finish_history_change(&before, ChangeSource::Redo)?,
        )))
    }

    /// 撤销/重做共同的收尾：导出增量、进 journal、推进 live_rev、产出 Applied。
    fn finish_history_change(
        &mut self,
        before: &str,
        source: ChangeSource,
    ) -> Result<Applied, PortError> {
        let after_vv = self.doc.vv();
        let update = self
            .doc
            .pending_update(&self.journal_base_vv)
            .map_err(port)?;
        let rev = LiveRev(self.live_rev.0 + 1);
        let label = match source {
            ChangeSource::Undo => "undo",
            ChangeSource::Redo => "redo",
            _ => "history",
        };
        {
            let mut guard = lock(self.journal.state_ref());
            // 外层已做容量准入，且持锁期间容量只增不减 → 这里不会失败
            // undo/redo 是本机产生的新 op（不是远端回放），因此同样产生出站意图。
            if JournalHandle::append(
                &mut guard,
                &self.doc_id,
                label,
                crate::journal::JournalOrigin::Local,
                update,
                rev,
            )
            .is_err()
            {
                debug_assert!(false, "持锁且已准入时 journal 追加失败");
                return Err(PortError::Storage("journal 追加失败".to_owned()));
            }
        }
        self.journal_base_vv = after_vv;
        self.live_rev = rev;
        self.live_frontier = self.doc.frontiers().encode();

        let splice = diff_splice(before, &self.doc.text());
        Ok(Applied {
            command_id: format!("{label}-{}", rev.0),
            live_rev: rev,
            live_frontier: self.live_frontier.clone(),
            splice,
            // 撤销不是用户"已经写好"的输入：调用方要按 splice 更新镜像
            local_echo: false,
            source,
            merge: MergeInfo::default(),
        })
    }

    /// 撤销/重做栈状态。
    pub fn history(&self) -> HistoryState {
        HistoryState {
            can_undo: self.undo.can_undo(),
            undo_count: self.undo.undo_count(),
            can_redo: self.undo.can_redo(),
            redo_count: self.undo.redo_count(),
        }
    }

    /// 受理一次 flush：同步返回票据，后台线程写 A 事务并推 `DurableAck`。
    pub fn flush(&mut self, _reason: FlushReason) -> FlushTicket {
        let ticket_id = self.journal.submit_ticket(self.live_rev);
        FlushTicket {
            ticket_id,
            target_live_rev: self.live_rev,
        }
    }

    /// 已 durable 的 head seq（方便测试/状态栏）。
    pub fn vault(&self) -> &Vault {
        &self.vault
    }
}

fn rejected(command_id: CommandId, reason: RejectReason) -> EditOutcome {
    EditOutcome::Rejected(Rejected { command_id, reason })
}

fn port(error: StorageError) -> PortError {
    PortError::Storage(error.to_string())
}

/// Loro 错误同样归到端口错误（协议不泄露 Loro 类型）。
fn port_loro(error: loro::LoroError) -> PortError {
    PortError::Storage(error.to_string())
}

/// 区间校验：升序、互不重叠、`end <= scalar_len`。
fn validate_edits(edits: &[ScalarEdit], scalar_len: usize) -> Result<(), ()> {
    let mut previous_end = 0usize;
    for (index, edit) in edits.iter().enumerate() {
        let start = edit.start.get();
        let end = edit.end.get();
        if start > end || end > scalar_len {
            return Err(());
        }
        if index > 0 && start < previous_end {
            return Err(());
        }
        previous_end = end;
    }
    Ok(())
}

/// 读文档代际（`loro_docs.generation`），并确认它是正文文档。
fn load_generation(vault: &Vault, doc_id: &str) -> Result<Generation, PortError> {
    let rows = vault
        .query(
            "SELECT generation, kind FROM loro_docs WHERE doc_id = ?1",
            &[Value::Text(doc_id.to_owned())],
        )
        .map_err(port)?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| PortError::NotOpen(doc_id.to_owned()))?;

    let generation = match row.first() {
        Some(Value::Integer(value)) if *value >= 0 => Generation(*value as u64),
        _ => return Err(PortError::NotOpen(doc_id.to_owned())),
    };
    match row.get(1) {
        Some(Value::Text(kind)) if kind == "body" => Ok(generation),
        _ => Err(PortError::NotOpen(doc_id.to_owned())),
    }
}

/// 一批 update 导入后的文本/版本变化（**不推进任何水位、不碰 journal**）。
pub(crate) struct ImportDiff {
    /// 导入前的正文。
    pub(crate) before_text: String,
    /// 导入后的正文（没有新 op 时与 `before_text` 相同）。
    pub(crate) after_text: String,
    /// 导入后的版本向量。
    pub(crate) after_vv: VersionVector,
    /// 是否真的产生了新的 op（以 VV 为准：文本相同也可能有纯格式 op）。
    changed: bool,
}

impl ImportDiff {
    /// 是否产生了新的 op。
    pub(crate) fn changed(&self) -> bool {
        self.changed
    }

    /// 本次变化的最小 splice（驱动调用方镜像与选区变换）。
    pub(crate) fn splice(&self) -> DeltaSplice {
        diff_splice(&self.before_text, &self.after_text)
    }
}

/// 把一批 update 合进 live 文档，返回前后文本与 VV。
///
/// **只做 Loro 导入**：journal、水位、事件全部由调用方决定——
/// `apply_remote` 走 journal（会产生 A 事务条目），`adopt_committed_delta` 走已 durable
/// 路径（不进 journal、不产生 sync intent）。两条路径共用同一份导入/选区变换逻辑。
pub(crate) fn import_updates<'a, I>(doc: &BodyDoc, updates: I) -> Result<ImportDiff, StorageError>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    let before_text = doc.text();
    let before_vv = doc.vv();
    for update in updates {
        doc.import(update)?;
    }
    let after_vv = doc.vv();
    let changed = after_vv != before_vv;
    let after_text = if changed {
        doc.text()
    } else {
        before_text.clone()
    };
    Ok(ImportDiff {
        before_text,
        after_text,
        after_vv,
        changed,
    })
}

/// 以 scalar 为单位求「最小差异」splice（公共前缀 + 公共后缀）。
pub(crate) fn diff_splice(before: &str, after: &str) -> DeltaSplice {
    let before_chars: Vec<char> = before.chars().collect();
    let after_chars: Vec<char> = after.chars().collect();

    let mut prefix = 0usize;
    while prefix < before_chars.len()
        && prefix < after_chars.len()
        && before_chars[prefix] == after_chars[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < before_chars.len() - prefix
        && suffix < after_chars.len() - prefix
        && before_chars[before_chars.len() - 1 - suffix]
            == after_chars[after_chars.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let delete_scalars = before_chars.len() - prefix - suffix;
    let insert: String = after_chars[prefix..after_chars.len() - suffix]
        .iter()
        .collect();
    DeltaSplice {
        at: UnicodeScalar::new(prefix),
        delete_scalars: delete_scalars as u64,
        insert,
    }
}
