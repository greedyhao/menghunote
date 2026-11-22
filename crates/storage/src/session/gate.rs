//! ADR 0009 切片 3a：**外部提交闸门** + **已提交增量 adopt**（只做存储层）。
//!
//! # 闸门
//!
//! 同步 worker 在把外部写入（`engine.integrate`）落进库之前，必须先让本地会话进入
//! [`GateState::Held`]，否则本地 flusher 的 head CAS 基准会被外部提交悄悄推过头：
//!
//! 1. [`DocumentSession::begin_external_commit`] 登记一张票据（记录当时的 `live_rev`）；
//! 2. [`DocumentSession::poll_external_commit`] 把状态驱到 Held：
//!    - composition 未结束 → [`GateState::WaitingIME`]（**绝不**强行结束 IME，也不触发 flush）；
//!    - 否则触发并等待 flush 到**明确目标**：`durable_through >= 目标 live_rev`
//!      且 journal `pending == 0`（两个值在 flusher 的同一临界区里推进，因此它等价于
//!      "覆盖该目标的 `DurableAck` 已经落地"，但不会和事件队列的其它消费者抢 ACK）；
//!    - 目标达成后再核对「会话 durable 基准 == `loro_docs.head_seq`」：不相等说明库里有
//!      没 adopt 的外部行，闸门直接 [`GateState::Failed`]，绝不带着过期基准进 Held；
//! 3. Held 期间本地写入一律 fail-closed：`apply` / `undo` / `redo` 返回
//!    [`super::protocol::RejectReason::NotReady`]，`begin_composition` 不进入组合，
//!    `apply_remote` 报 [`super::protocol::PortError`]；
//! 4. 外部提交完成后 [`DocumentSession::end_external_commit`] 释放；失败/取消用
//!    [`DocumentSession::cancel_external_commit`]——**两者都先把已 durable 的变化交付进
//!    live**，所以「同步失败」不会留下"库里有、live 没有"的撕裂状态。
//!
//! 闸门没有新线程、没有全局状态：状态挂在 [`DocumentSession`] 上，与 flush 的协作只读
//! [`JournalHandle`] 的共享水位（见 [`super::flush`]）。
//!
//! # adopt
//!
//! [`DocumentSession::adopt_committed_delta`] 把**已经 durable** 的外部 update 合进 live：
//!
//! - **不进 journal、不产生 sync intent**（不走会 journal 的 `apply_remote` 路径）；
//! - 先在不碰 live 的 scratch 副本上预演，校验 target frontier/vv 与库中连续的 payload，
//!   失败时 live 与 durable 水位**都不变**（Loro 导入不可回滚，所以校验必须在导入之前）；
//! - 成功后推进 `live_rev` / `live_frontier` / `journal_base_vv`，并把 flusher 的 head
//!   CAS 基准重定到外部提交的 head（**仅在 `pending == 0` 时**），后续本地 flush 不再
//!   `HeadConflict`；
//! - 同 head（空 delta）与重复交付同一份 delta 都幂等：不重复推进 `live_rev`。

use loro::{Frontiers, VersionVector};
use menghunote_text::UnicodeScalar;
use thiserror::Error;

use super::core::{import_updates, DocumentSession};
use super::flush::{lock, JournalHandle};
use super::protocol::{
    BatchId, ChangeSource, DeltaSplice, DocId, DurableAck, LiveDelta, LiveRev, MergeInfo,
    SessionEpoch, SessionEvent,
};
use crate::body_doc::BodyDoc;
use crate::error::StorageError;
use crate::read_api::doc_updates_since;
use crate::store;
use crate::Vault;

/// 闸门代际：每张票据/租约唯一，用来拒绝过期票据与跨代租约。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GateGeneration(pub u64);

/// 一张**待进入 Held** 的外部提交票据（由 [`DocumentSession::begin_external_commit`] 发）。
///
/// 票据只记录"进入闸门"的意图；真正的 durable 目标由 poll 用它建立时的 `live_rev`
/// 与当时的 live 一起确定，绝不拿"队列为空"当 quiescence。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCommitTicket {
    doc_id: DocId,
    session_epoch: SessionEpoch,
    generation: GateGeneration,
    /// begin 时的 `live_rev`（Held 的目标 durable 水位下界）。
    target_live_rev: LiveRev,
    /// begin 时 flusher 已 durable 的 head seq（诊断/断言用）。
    baseline_head_seq: u64,
}

impl ExternalCommitTicket {
    /// 文档 ID。
    #[must_use]
    pub fn doc_id(&self) -> &str {
        &self.doc_id
    }

    /// 会话纪元。
    #[must_use]
    pub fn session_epoch(&self) -> SessionEpoch {
        self.session_epoch
    }

    /// 票据代际。
    #[must_use]
    pub fn generation(&self) -> GateGeneration {
        self.generation
    }

    /// begin 时的 live 修订号（Held 的目标 durable 水位下界）。
    #[must_use]
    pub fn target_live_rev(&self) -> LiveRev {
        self.target_live_rev
    }

    /// begin 时已 durable 的 head seq。
    #[must_use]
    pub fn baseline_head_seq(&self) -> u64 {
        self.baseline_head_seq
    }
}

/// 进入 [`GateState::Held`] 后拿到的租约：只有它能让外部提交写库、结束或 adopt。
///
/// 租约含 `doc_id` / `session_epoch` / `generation`：任何一项对不上都按
/// [`GateError::LeaseMismatch`] 拒绝，避免把上一轮闸门的结果套用到新提交上。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCommitLease {
    doc_id: DocId,
    session_epoch: SessionEpoch,
    generation: GateGeneration,
    /// Held 时已达成的 durable 目标（= 该此刻的 `live_rev`）。
    target_live_rev: LiveRev,
    /// Held 时 flusher 已 durable 的 head seq（外部提交的 CAS 基准）。
    durable_head_seq: u64,
}

impl ExternalCommitLease {
    /// 文档 ID。
    #[must_use]
    pub fn doc_id(&self) -> &str {
        &self.doc_id
    }

    /// 会话纪元。
    #[must_use]
    pub fn session_epoch(&self) -> SessionEpoch {
        self.session_epoch
    }

    /// 租约代际。
    #[must_use]
    pub fn generation(&self) -> GateGeneration {
        self.generation
    }

    /// Held 时已达成的 durable 目标。
    #[must_use]
    pub fn target_live_rev(&self) -> LiveRev {
        self.target_live_rev
    }

    /// Held 时 flusher 已 durable 的 head seq。
    #[must_use]
    pub fn durable_head_seq(&self) -> u64 {
        self.durable_head_seq
    }
}

/// 闸门状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateState {
    /// IME composition 未结束：等它自然结束（闸门不强行结束、也不触发 flush）。
    WaitingIME,
    /// 正在触发/等待 flush 到明确目标。
    Flushing,
    /// 已持闸：外部提交可以写库；本地写入被拒绝。
    Held(ExternalCommitLease),
    /// 票据已失效（被释放、被新票据取代或从未登记）：调用方必须重新 begin。
    Stale,
    /// flush 明确失败 / 基准不一致：闸门**没有**静默释放，调用方应 retry 或 cancel。
    Failed(GateError),
}

/// 闸门错误（`begin` / `end` / `cancel` 的显式失败）。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GateError {
    /// 已有票据或租约在途（重复 begin）。
    #[error("外部提交闸门已被代际 {generation:?} 占用")]
    AlreadyHeld {
        /// 占用中的代际。
        generation: GateGeneration,
    },
    /// 票据已失效。
    #[error("外部提交票据已失效（代际 {generation:?}）")]
    StaleTicket {
        /// 调用方持有的代际。
        generation: GateGeneration,
    },
    /// 当前不是 Held（还没有租约）。
    #[error("外部提交闸门当前不是 Held 状态")]
    NotHeld,
    /// 租约与当前持有者不匹配（doc/epoch/generation 任一对不上）。
    #[error("外部提交租约不匹配（当前 {expected:?}，调用方 {actual:?}）")]
    LeaseMismatch {
        /// 当前持有者的代际。
        expected: GateGeneration,
        /// 调用方给的代际。
        actual: GateGeneration,
    },
    /// 租约属于其它文档。
    #[error("外部提交租约属于其它文档（当前 {expected}，调用方 {actual}）")]
    WrongDoc {
        /// 当前文档。
        expected: DocId,
        /// 调用方给的文档。
        actual: DocId,
    },
    /// flush 明确失败（例如 head CAS 冲突）。
    #[error("flush 到 {target:?} 失败：{message}")]
    FlushFailed {
        /// 目标 live 修订号。
        target: LiveRev,
        /// 失败的底层信息。
        message: String,
    },
    /// 库中存在尚未 adopt 的外部行：不能带着过期基准进入 Held。
    #[error("库中 head seq {database_head_seq} 超出会话 durable {durable_head_seq}，必须先 adopt")]
    Unadopted {
        /// 会话 durable head seq。
        durable_head_seq: u64,
        /// 库中实际 head seq。
        database_head_seq: u64,
    },
    /// `applied` 与库中 head seq 不一致。
    #[error("applied head seq 与库中不一致（库中 {expected}，调用方 {actual}）")]
    HeadsMismatch {
        /// 库中 head seq。
        expected: u64,
        /// 调用方给的 head seq。
        actual: u64,
    },
    /// frontier / vv 与预期不一致。
    #[error("head 的 {field} 与预期不一致")]
    TargetMismatch {
        /// 不一致的字段（`frontier` / `vv`）。
        field: &'static str,
    },
    /// 仍有未 ACK 的本地更新：此时不能重定 durable 基准（fail-closed）。
    #[error("仍有 {pending} 条未 ACK 更新，不能结束外部提交")]
    Pending {
        /// 未 ACK 条数。
        pending: usize,
    },
    /// 已有外部行无法交付进 live（import/校验失败）。
    #[error("交付已 durable 的外部变更失败：{0}")]
    Delivery(String),
    /// 存储层错误。
    #[error("存储层错误：{0}")]
    Storage(String),
}

/// 已 durable 的 heads（`end_external_commit` 用它校验外部提交的落点）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppliedHeads {
    /// `loro_docs.head_seq`。
    pub head_seq: u64,
    /// `loro_docs.head_frontier`（空 = 默认 frontier）。
    pub frontier: Vec<u8>,
    /// `loro_docs.head_vv`（空 = 空版本向量）。
    pub vv: Vec<u8>,
}

impl AppliedHeads {
    /// 从 [`crate::store::HeadState`] 取一份（NULL frontier/vv 会归一成默认编码）。
    #[must_use]
    pub fn from_head(head: &crate::store::HeadState) -> Self {
        Self {
            head_seq: head.head_seq,
            frontier: store::encoded_frontier(head),
            vv: head.head_vv.clone(),
        }
    }

    /// 读库中当前 head。
    pub fn load(vault: &Vault, doc_id: &str) -> Result<Self, StorageError> {
        Ok(Self::from_head(&store::load_head(vault, doc_id)?))
    }
}

/// 一份**已 durable**、待 adopt 的外部增量（`prepare_committed_delta` 产出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCommittedDelta {
    /// 文档 ID。
    pub doc_id: DocId,
    /// 起点：会话当前已 adopt 到的 durable head seq。
    pub base_seq: u64,
    /// 目标 head seq（= `base_seq + updates.len()`）。
    pub target_head_seq: u64,
    /// 目标 frontier（`Frontiers::encode`）。
    pub target_frontier: Vec<u8>,
    /// 目标 VV（`VersionVector::encode`）。
    pub target_vv: Vec<u8>,
    /// 连续覆盖 `base_seq + 1 ..= target_head_seq` 的 update payload；可为空（同 head）。
    pub updates: Vec<Vec<u8>>,
}

/// adopt 成功后交给 3b（SessionDriver/UI）的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptOutcome {
    /// 驱动 UI 镜像与选区变换的增量（`source = Remote`，`cause = None`）。
    pub delta: LiveDelta,
    /// 已 durable 的水位（外部提交的目标 head）。
    pub ack: DurableAck,
}

/// adopt 的错误：**任何**错误都保证 live 与 durable 水位未被改动。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AdoptError {
    /// 当前不是 Held。
    #[error("外部提交闸门当前不是 Held 状态，不能 adopt")]
    NotHeld,
    /// 租约不匹配。
    #[error("外部提交租约不匹配（当前 {expected:?}，调用方 {actual:?}）")]
    LeaseMismatch {
        /// 当前持有者的代际。
        expected: GateGeneration,
        /// 调用方给的代际。
        actual: GateGeneration,
    },
    /// delta 属于其它文档。
    #[error("delta 属于其它文档（当前 {expected}，delta {actual}）")]
    WrongDoc {
        /// 当前文档。
        expected: DocId,
        /// delta 的文档。
        actual: DocId,
    },
    /// 仍有未 ACK 的本地更新。
    #[error("仍有 {pending} 条未 ACK 更新，不能 adopt")]
    Pending {
        /// 未 ACK 条数。
        pending: usize,
    },
    /// updates 没有连续覆盖 `base_seq + 1 ..= target_head_seq`。
    #[error("updates 未连续覆盖 {base_seq}+1..={target_head_seq}（实际 {updates} 条）")]
    NonContiguous {
        /// 起点 seq。
        base_seq: u64,
        /// 目标 head seq。
        target_head_seq: u64,
        /// 实际 update 条数。
        updates: usize,
    },
    /// `base_seq` 与当前 durable head 不一致。
    #[error("delta 起点 {base_seq} 不是当前 durable head {durable_head_seq}")]
    StaleBase {
        /// delta 的起点。
        base_seq: u64,
        /// 当前 durable head seq。
        durable_head_seq: u64,
    },
    /// 目标 head seq 超过了库中已有的行。
    #[error("delta 目标 {target_head_seq} 超过库中 head {database_head_seq}")]
    TargetAhead {
        /// delta 的目标。
        target_head_seq: u64,
        /// 库中 head seq。
        database_head_seq: u64,
    },
    /// delta 的 payload 与库中同一 seq 的行不一致（不是"已 durable 的那份"）。
    #[error("delta 第 {seq} 条 payload 与库中不一致")]
    PayloadMismatch {
        /// 不一致的 seq。
        seq: u64,
    },
    /// 目标 frontier / vv 与预演结果不一致。
    #[error("目标 {field} 与导入结果不一致")]
    TargetMismatch {
        /// 不一致的字段（`frontier` / `vv`）。
        field: &'static str,
    },
    /// live 与库中 `base_seq` 的状态对不上（会话与库不同步，fail-closed）。
    #[error("live 与库中 base_seq 状态不一致：{context}")]
    Inconsistent {
        /// 具体上下文。
        context: String,
    },
    /// Loro 导入失败。
    #[error("导入已 durable 的 update 失败：{0}")]
    Import(String),
    /// 存储层错误。
    #[error("存储层错误：{0}")]
    Storage(String),
}

impl From<AdoptError> for GateError {
    fn from(error: AdoptError) -> Self {
        match error {
            AdoptError::NotHeld => Self::NotHeld,
            AdoptError::LeaseMismatch { expected, actual } => {
                Self::LeaseMismatch { expected, actual }
            }
            AdoptError::WrongDoc { expected, actual } => Self::WrongDoc { expected, actual },
            AdoptError::Pending { pending } => Self::Pending { pending },
            AdoptError::StaleBase {
                base_seq,
                durable_head_seq,
            } => Self::HeadsMismatch {
                expected: durable_head_seq,
                actual: base_seq,
            },
            other => Self::Delivery(other.to_string()),
        }
    }
}

/// 闸门内部状态（挂在 [`DocumentSession`] 上，无新线程/新全局状态）。
#[derive(Debug, Default)]
pub(crate) struct ExternalCommitGate {
    /// 已发出的最大代际（0 = 还没 begin 过）。
    next_generation: u64,
    /// 已登记但还没进入 Held 的票据。
    ticket: Option<TicketState>,
    /// 已持闸的租约。
    held: Option<ExternalCommitLease>,
}

#[derive(Debug)]
struct TicketState {
    ticket: ExternalCommitTicket,
    /// 已投递的 flush 目标（同一目标只投递一次，避免每帧 poll 重复塞命令）。
    submitted: Option<LiveRev>,
}

impl ExternalCommitGate {
    /// 是否处于 Held（本地写入必须被拒绝）。
    pub(crate) fn is_held(&self) -> bool {
        self.held.is_some()
    }

    /// 当前租约。
    pub(crate) fn held(&self) -> Option<&ExternalCommitLease> {
        self.held.as_ref()
    }

    /// 释放票据与租约（只有校验通过的 end/cancel 才允许调用）。
    fn release(&mut self) {
        self.ticket = None;
        self.held = None;
    }
}

/// 票据与给定代际/文档/会话是否同一张。
fn same_ticket(
    ticket: &ExternalCommitTicket,
    doc_id: &str,
    session_epoch: SessionEpoch,
    generation: GateGeneration,
) -> bool {
    ticket.doc_id == doc_id
        && ticket.session_epoch == session_epoch
        && ticket.generation == generation
}

fn normalize_frontier(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.is_empty() {
        return Ok(Frontiers::default().encode());
    }
    Frontiers::decode(bytes)
        .map(|frontier| frontier.encode())
        .map_err(|error| error.to_string())
}

fn normalize_vv(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.is_empty() {
        return Ok(VersionVector::default().encode());
    }
    VersionVector::decode(bytes)
        .map(|vv| vv.encode())
        .map_err(|error| error.to_string())
}

/// 从库里读出 `base_seq+1 ..= target_head_seq` 的连续 payload。
fn durable_updates_between(
    vault: &Vault,
    doc_id: &str,
    base_seq: u64,
    target_head_seq: u64,
) -> Result<Vec<Vec<u8>>, AdoptError> {
    if target_head_seq < base_seq {
        return Err(AdoptError::NonContiguous {
            base_seq,
            target_head_seq,
            updates: 0,
        });
    }
    if target_head_seq == base_seq {
        return Ok(Vec::new());
    }
    let rows = doc_updates_since(vault, doc_id, base_seq)
        .map_err(|error| AdoptError::Storage(error.to_string()))?;
    let mut updates = Vec::with_capacity(rows.len());
    let mut expected = base_seq + 1;
    for row in rows {
        if row.seq > target_head_seq {
            break;
        }
        if row.seq != expected {
            return Err(AdoptError::NonContiguous {
                base_seq,
                target_head_seq,
                updates: updates.len(),
            });
        }
        updates.push(row.payload);
        expected += 1;
    }
    if expected != target_head_seq + 1 {
        return Err(AdoptError::NonContiguous {
            base_seq,
            target_head_seq,
            updates: updates.len(),
        });
    }
    Ok(updates)
}

/// 从库中读出 `base_seq` 之后的连续增量（供 worker/测试构造 delta）。
///
/// 校验：`base_seq <= head`、seq 连续、最后一行就是 head、head 的 frontier/vv 与最后一行
/// 一致；没有新行时返回**空 delta**（`target_head_seq == base_seq`）。读取期间 head 被推进
/// 会返回 [`StorageError::HeadConflict`]，绝不返回半截 delta。
pub fn prepare_committed_delta(
    vault: &Vault,
    doc_id: &str,
    base_seq: u64,
) -> Result<PreparedCommittedDelta, StorageError> {
    let head = store::load_head(vault, doc_id)?;
    if head.head_seq < base_seq {
        return Err(StorageError::HeadConflict {
            doc_id: doc_id.to_owned(),
            expected: base_seq,
            actual: head.head_seq,
        });
    }

    let rows = doc_updates_since(vault, doc_id, base_seq)?;
    let mut updates = Vec::with_capacity(rows.len());
    for (expected, row) in (base_seq + 1..).zip(rows.iter()) {
        if row.seq > head.head_seq {
            return Err(StorageError::HeadConflict {
                doc_id: doc_id.to_owned(),
                expected: head.head_seq,
                actual: row.seq,
            });
        }
        if row.seq != expected {
            return Err(StorageError::Corrupt {
                context: format!(
                    "{doc_id} 的 loro_updates 出现断档：期望 seq {expected}，实际 {}",
                    row.seq
                ),
            });
        }
        updates.push(row.payload.clone());
    }

    // head 可能在读取期间被推进：复核，绝不返回半截 delta
    let after = store::load_head(vault, doc_id)?;
    if after.head_seq != head.head_seq {
        return Err(StorageError::HeadConflict {
            doc_id: doc_id.to_owned(),
            expected: head.head_seq,
            actual: after.head_seq,
        });
    }

    let (target_frontier, target_vv) = match rows.last() {
        Some(last) => {
            if last.seq != head.head_seq {
                return Err(StorageError::Corrupt {
                    context: format!(
                        "{doc_id} 最后一行 seq {} 不是 head {}",
                        last.seq, head.head_seq
                    ),
                });
            }
            (last.end_frontier.clone(), last.end_vv.clone())
        }
        None => (store::encoded_frontier(&head), head.head_vv.clone()),
    };

    // head 与最后一行必须一致（frontier/vv 任一漂移都说明数据不完整）
    let head_frontier = store::encoded_frontier(&head);
    if normalize_frontier(&target_frontier).map_err(|context| StorageError::Corrupt { context })?
        != normalize_frontier(&head_frontier)
            .map_err(|context| StorageError::Corrupt { context })?
    {
        return Err(StorageError::Corrupt {
            context: format!("{doc_id} head frontier 与最后一行 end_frontier 不一致"),
        });
    }
    if normalize_vv(&target_vv).map_err(|context| StorageError::Corrupt { context })?
        != normalize_vv(&head.head_vv).map_err(|context| StorageError::Corrupt { context })?
    {
        return Err(StorageError::Corrupt {
            context: format!("{doc_id} head vv 与最后一行 end_vv 不一致"),
        });
    }

    Ok(PreparedCommittedDelta {
        doc_id: doc_id.to_owned(),
        base_seq,
        target_head_seq: head.head_seq,
        target_frontier,
        target_vv,
        updates,
    })
}

impl DocumentSession {
    /// 是否处于外部提交 Held（Held 期间本地写入一律被拒）。
    #[must_use]
    pub fn is_external_commit_held(&self) -> bool {
        self.gate.is_held()
    }

    /// 当前外部提交租约（`None` = 不在 Held）。
    #[must_use]
    pub fn external_commit_lease(&self) -> Option<&ExternalCommitLease> {
        self.gate.held()
    }

    /// 登记一次外部提交（只建立票据，不阻塞、不 flush）。
    ///
    /// 重复 begin / 已有租约返回 [`GateError::AlreadyHeld`]；真正的等待全在
    /// [`Self::poll_external_commit`]。
    pub fn begin_external_commit(&mut self) -> Result<ExternalCommitTicket, GateError> {
        if let Some(lease) = self.gate.held.as_ref() {
            return Err(GateError::AlreadyHeld {
                generation: lease.generation,
            });
        }
        if let Some(state) = self.gate.ticket.as_ref() {
            return Err(GateError::AlreadyHeld {
                generation: state.ticket.generation,
            });
        }
        self.gate.next_generation += 1;
        let ticket = ExternalCommitTicket {
            doc_id: self.doc_id.clone(),
            session_epoch: self.session_epoch,
            generation: GateGeneration(self.gate.next_generation),
            target_live_rev: self.live_rev,
            baseline_head_seq: self.journal.durable_head_seq(),
        };
        self.gate.ticket = Some(TicketState {
            ticket: ticket.clone(),
            submitted: None,
        });
        Ok(ticket)
    }

    /// 非阻塞推进闸门：`WaitingIME` → `Flushing` → `Held(lease)`。
    ///
    /// 判据是 flusher 的 durable 水位与 `pending == 0`（同一临界区），不读事件队列，
    /// 因此调用方可以把 `events()` 用于别处；也不会用"队列为空"冒充 quiescence。
    /// flush 明确失败时返回 [`GateState::Failed`]，闸门**不会**静默释放。
    pub fn poll_external_commit(&mut self, ticket: &ExternalCommitTicket) -> GateState {
        // 1) 已 Held：只认同一代际/文档/会话
        if let Some(lease) = self.gate.held.clone() {
            if same_ticket(ticket, &lease.doc_id, lease.session_epoch, lease.generation) {
                return GateState::Held(lease);
            }
            return GateState::Stale;
        }

        // 2) 票据必须仍是当前登记的那张
        let Some(state) = self.gate.ticket.as_ref() else {
            return GateState::Stale;
        };
        if !same_ticket(
            ticket,
            &state.ticket.doc_id,
            state.ticket.session_epoch,
            state.ticket.generation,
        ) {
            return GateState::Stale;
        }
        let target = self.live_rev.max(state.ticket.target_live_rev);

        // 3) 组合中：等它自然结束（不强行结束 IME、也不触发 flush）
        if self.is_composing() {
            return GateState::WaitingIME;
        }

        // 4) 明确目标：durable 水位 + pending 归零（同一个快照，避免两次取锁看到不一致）
        let progress = self.journal.gate_progress();
        if progress.pending == 0 && progress.durable_through >= target {
            // 4a) 会话 durable 基准必须与库中 head 一致：有未 adopt 的外部行就 fail-closed
            let head = match store::load_head(&self.vault, &self.doc_id) {
                Ok(head) => head,
                Err(error) => return GateState::Failed(GateError::Storage(error.to_string())),
            };
            if head.head_seq != progress.durable_head_seq {
                return GateState::Failed(GateError::Unadopted {
                    durable_head_seq: progress.durable_head_seq,
                    database_head_seq: head.head_seq,
                });
            }
            // 4b) 晋升 Held（票据销毁，租约接管）
            let lease = ExternalCommitLease {
                doc_id: self.doc_id.clone(),
                session_epoch: self.session_epoch,
                generation: ticket.generation,
                target_live_rev: target,
                durable_head_seq: progress.durable_head_seq,
            };
            self.gate.ticket = None;
            self.gate.held = Some(lease.clone());
            return GateState::Held(lease);
        }

        if progress.pending > 0 {
            // 4c) 失败的 flush 不能静默吞掉：明确报错，等调用方 retry / cancel
            if let Some(message) = progress.last_write_error {
                return GateState::Failed(GateError::FlushFailed { target, message });
            }
            // 4d) 触发（或复用）一张覆盖明确目标的 flush 票据
            if let Some(state) = self.gate.ticket.as_mut() {
                if state.submitted != Some(target) {
                    self.journal.submit_ticket(target);
                    state.submitted = Some(target);
                }
            }
            return GateState::Flushing;
        }

        // 防御：pending == 0 但 durable 水位没到目标（每次 live 变更都会进 journal，
        // 正常路径不可达）；宁可明确失败，也不假装 Held。
        GateState::Failed(GateError::Storage(format!(
            "闸门等待目标 {target:?} 时出现水位不一致（durable_through={:?}, pending=0）",
            progress.durable_through
        )))
    }

    /// 结束外部提交并释放闸门（正常路径在 adopt 之后调用）。
    ///
    /// 校验：
    /// - 当前必须 Held 且 `lease` 逐字段匹配（doc/epoch/generation）；
    /// - `applied` 必须与库中**当前** head 完全一致（含 frontier/vv）；
    /// - 若 Held 期间又有已 durable 的行（失败路径），先交付进 live 再释放——取消/失败
    ///   同样不丢已 durable 的数据；交付的增量会作为 [`SessionEvent::Remote`] 入事件队列。
    ///
    /// 任何校验失败都**不释放**闸门（fail-closed，"失败不得静默释放"）；调用方可修好
    /// 现场后再调一次。
    pub fn end_external_commit(
        &mut self,
        lease: ExternalCommitLease,
        applied: AppliedHeads,
    ) -> Result<(), GateError> {
        let held = self.gate.held.clone().ok_or(GateError::NotHeld)?;
        if held.doc_id != lease.doc_id
            || held.session_epoch != lease.session_epoch
            || held.generation != lease.generation
        {
            return Err(GateError::LeaseMismatch {
                expected: held.generation,
                actual: lease.generation,
            });
        }
        if held.doc_id != self.doc_id || held.session_epoch != self.session_epoch {
            return Err(GateError::WrongDoc {
                expected: self.doc_id.clone(),
                actual: held.doc_id,
            });
        }

        // applied 必须就是库中当前 head：不猜、不替调用方决定
        let head = store::load_head(&self.vault, &self.doc_id)
            .map_err(|error| GateError::Storage(error.to_string()))?;
        let current = AppliedHeads::from_head(&head);
        if current.head_seq != applied.head_seq {
            return Err(GateError::HeadsMismatch {
                expected: current.head_seq,
                actual: applied.head_seq,
            });
        }
        if normalize_frontier(&current.frontier).map_err(GateError::Storage)?
            != normalize_frontier(&applied.frontier).map_err(GateError::Storage)?
        {
            return Err(GateError::TargetMismatch { field: "frontier" });
        }
        if normalize_vv(&current.vv).map_err(GateError::Storage)?
            != normalize_vv(&applied.vv).map_err(GateError::Storage)?
        {
            return Err(GateError::TargetMismatch { field: "vv" });
        }
        if applied.head_seq < held.durable_head_seq {
            return Err(GateError::HeadsMismatch {
                expected: held.durable_head_seq,
                actual: applied.head_seq,
            });
        }

        // 失败/取消路径：把 Held 期间变成 durable、但 live 还没导入的行补齐
        let base = self.journal.durable_head_seq();
        if applied.head_seq > base {
            let updates =
                durable_updates_between(&self.vault, &self.doc_id, base, applied.head_seq)?;
            let delivered = self.apply_durable_segment(
                base,
                applied.head_seq,
                &applied.frontier,
                &applied.vv,
                &updates,
            )?;
            if let Some(delta) = delivered {
                lock(self.journal.events_ref()).push_back(SessionEvent::Remote(delta));
            }
        }

        // 复核 live 已与 applied 一致，否则不得释放
        if !self.live_matches(&applied.frontier, &applied.vv)? {
            return Err(GateError::TargetMismatch {
                field: "live frontier/vv",
            });
        }
        self.gate.release();
        Ok(())
    }

    /// 放弃一次外部提交（失败/取消路径），**先把已 durable 的变化交付进 live**。
    ///
    /// 返回交付产生的 [`LiveDelta`]（没有可交付行时为 `None`），同一份 delta 也会进
    /// [`SessionEvent::Remote`] 事件队列。仍有未 ACK 的本地更新时返回
    /// [`GateError::Pending`] 并保持闸门不变：此时 journal 条目的 `end_vv` 早于外部 head，
    /// 重定基准会让水位与条目自相矛盾，必须先 retry flush 或重启会话。
    pub fn cancel_external_commit(
        &mut self,
        ticket: &ExternalCommitTicket,
    ) -> Result<Option<LiveDelta>, GateError> {
        let active = match (&self.gate.held, &self.gate.ticket) {
            (Some(lease), _) => {
                same_ticket(ticket, &lease.doc_id, lease.session_epoch, lease.generation)
            }
            (None, Some(state)) => same_ticket(
                ticket,
                &state.ticket.doc_id,
                state.ticket.session_epoch,
                state.ticket.generation,
            ),
            (None, None) => false,
        };
        if !active {
            return Err(GateError::StaleTicket {
                generation: ticket.generation,
            });
        }
        if ticket.doc_id != self.doc_id || ticket.session_epoch != self.session_epoch {
            return Err(GateError::WrongDoc {
                expected: self.doc_id.clone(),
                actual: ticket.doc_id.clone(),
            });
        }

        let pending = self.pending_len();
        if pending != 0 {
            return Err(GateError::Pending { pending });
        }
        let head = store::load_head(&self.vault, &self.doc_id)
            .map_err(|error| GateError::Storage(error.to_string()))?;
        let applied = AppliedHeads::from_head(&head);
        let base = self.journal.durable_head_seq();
        let mut delivered = None;
        if applied.head_seq > base {
            let updates =
                durable_updates_between(&self.vault, &self.doc_id, base, applied.head_seq)?;
            delivered = self.apply_durable_segment(
                base,
                applied.head_seq,
                &applied.frontier,
                &applied.vv,
                &updates,
            )?;
            if let Some(delta) = &delivered {
                lock(self.journal.events_ref()).push_back(SessionEvent::Remote(delta.clone()));
            }
        }
        self.gate.release();
        Ok(delivered)
    }

    /// 重投一次 flush（`GateState::Failed(FlushFailed)` 之后的显式重试）。
    ///
    /// 只对当前票据有效；已 Held 时是 no-op。注意 head CAS 冲突是确定性失败：同一基准
    /// 重试还会失败，必须 cancel/重启会话（这里不掩盖失败）。
    pub fn retry_external_commit(
        &mut self,
        ticket: &ExternalCommitTicket,
    ) -> Result<(), GateError> {
        if let Some(lease) = self.gate.held.as_ref() {
            if same_ticket(ticket, &lease.doc_id, lease.session_epoch, lease.generation) {
                return Ok(());
            }
            return Err(GateError::StaleTicket {
                generation: ticket.generation,
            });
        }
        let Some(state) = self.gate.ticket.as_ref() else {
            return Err(GateError::StaleTicket {
                generation: ticket.generation,
            });
        };
        if !same_ticket(
            ticket,
            &state.ticket.doc_id,
            state.ticket.session_epoch,
            state.ticket.generation,
        ) {
            return Err(GateError::StaleTicket {
                generation: ticket.generation,
            });
        }
        let target = self.live_rev.max(ticket.target_live_rev);
        self.journal.submit_ticket(target);
        if let Some(state) = self.gate.ticket.as_mut() {
            state.submitted = Some(target);
        }
        Ok(())
    }

    /// adopt 一份**已 durable** 的外部增量（必须 Held 且 lease 匹配）。
    ///
    /// 不写 journal、不产生 sync intent；成功后把 live 与 flusher 的 durable 基准一起推进，
    /// 失败时 live / durable 水位不变（预演校验在导入之前完成）。
    pub fn adopt_committed_delta(
        &mut self,
        lease: &ExternalCommitLease,
        delta: PreparedCommittedDelta,
    ) -> Result<AdoptOutcome, AdoptError> {
        let held = self.gate.held.clone().ok_or(AdoptError::NotHeld)?;
        if held.doc_id != lease.doc_id
            || held.session_epoch != lease.session_epoch
            || held.generation != lease.generation
        {
            return Err(AdoptError::LeaseMismatch {
                expected: held.generation,
                actual: lease.generation,
            });
        }
        if delta.doc_id != self.doc_id {
            return Err(AdoptError::WrongDoc {
                expected: self.doc_id.clone(),
                actual: delta.doc_id,
            });
        }
        let progress = self.journal.gate_progress();
        if progress.pending != 0 {
            return Err(AdoptError::Pending {
                pending: progress.pending,
            });
        }

        // 连续性：target 必须恰好被 updates 覆盖
        if delta.target_head_seq < delta.base_seq
            || delta.target_head_seq - delta.base_seq != delta.updates.len() as u64
        {
            return Err(AdoptError::NonContiguous {
                base_seq: delta.base_seq,
                target_head_seq: delta.target_head_seq,
                updates: delta.updates.len(),
            });
        }

        // 幂等：重复交付同一份 delta（其 target 已是当前 durable head 且 live 已就位）
        if delta.base_seq != progress.durable_head_seq {
            if delta.target_head_seq == progress.durable_head_seq
                && self.live_matches(&delta.target_frontier, &delta.target_vv)?
            {
                return Ok(self.idempotent_outcome(&delta));
            }
            return Err(AdoptError::StaleBase {
                base_seq: delta.base_seq,
                durable_head_seq: progress.durable_head_seq,
            });
        }

        // target 不能超过库中已有行（否则 rebase 会在 seq 上留洞）
        let head = store::load_head(&self.vault, &self.doc_id)
            .map_err(|error| AdoptError::Storage(error.to_string()))?;
        if head.head_seq < delta.target_head_seq {
            return Err(AdoptError::TargetAhead {
                target_head_seq: delta.target_head_seq,
                database_head_seq: head.head_seq,
            });
        }

        // 同 head：空 delta 幂等，不推进 rev；target 必须与 live 当前 heads 一致
        if delta.updates.is_empty() {
            if normalize_frontier(&self.doc.frontiers().encode()).map_err(AdoptError::Storage)?
                != normalize_frontier(&delta.target_frontier).map_err(AdoptError::Storage)?
            {
                return Err(AdoptError::TargetMismatch { field: "frontier" });
            }
            if normalize_vv(&self.doc.vv().encode()).map_err(AdoptError::Storage)?
                != normalize_vv(&delta.target_vv).map_err(AdoptError::Storage)?
            {
                return Err(AdoptError::TargetMismatch { field: "vv" });
            }
            return Ok(self.idempotent_outcome(&delta));
        }

        // 只导入"确实已 durable"的 payload：与库中行逐字节比对
        let durable = durable_updates_between(
            &self.vault,
            &self.doc_id,
            delta.base_seq,
            delta.target_head_seq,
        )?;
        if durable.len() != delta.updates.len() {
            return Err(AdoptError::NonContiguous {
                base_seq: delta.base_seq,
                target_head_seq: delta.target_head_seq,
                updates: durable.len(),
            });
        }
        for (index, (expected, actual)) in durable.iter().zip(delta.updates.iter()).enumerate() {
            if expected != actual {
                return Err(AdoptError::PayloadMismatch {
                    seq: delta.base_seq + 1 + index as u64,
                });
            }
        }

        let delivered = self.apply_durable_segment(
            delta.base_seq,
            delta.target_head_seq,
            &delta.target_frontier,
            &delta.target_vv,
            &delta.updates,
        )?;
        match delivered {
            Some(delta_live) => {
                let ack = DurableAck {
                    batch_id: BatchId(0),
                    through_live_rev: delta_live.to_rev,
                    head_seq: delta.target_head_seq,
                    head_frontier: delta.target_frontier.clone(),
                };
                Ok(AdoptOutcome {
                    delta: delta_live,
                    ack,
                })
            }
            // 全部 op 已存在（没有产生新 op）：按幂等处理，水位由 apply 内部 rebase 推进
            None => Ok(self.idempotent_outcome(&delta)),
        }
    }

    /// 幂等结果：不推进 `live_rev`、不产生 splice。
    fn idempotent_outcome(&self, delta: &PreparedCommittedDelta) -> AdoptOutcome {
        AdoptOutcome {
            delta: LiveDelta {
                doc_id: self.doc_id.clone(),
                from_rev: self.live_rev,
                to_rev: self.live_rev,
                live_frontier: self.live_frontier.clone(),
                splice: DeltaSplice {
                    at: UnicodeScalar::new(0),
                    delete_scalars: 0,
                    insert: String::new(),
                },
                cause: None,
                source: ChangeSource::Remote,
                merge: MergeInfo {
                    concurrent: false,
                    remote_changes: 0,
                },
            },
            ack: DurableAck {
                batch_id: BatchId(0),
                through_live_rev: self.live_rev,
                head_seq: delta.target_head_seq,
                head_frontier: delta.target_frontier.clone(),
            },
        }
    }

    /// live 的 frontier/vv 是否已经等于给定目标。
    fn live_matches(&self, frontier: &[u8], vv: &[u8]) -> Result<bool, AdoptError> {
        let live_frontier =
            normalize_frontier(&self.doc.frontiers().encode()).map_err(AdoptError::Storage)?;
        let expected_frontier = normalize_frontier(frontier).map_err(AdoptError::Storage)?;
        let live_vv = normalize_vv(&self.doc.vv().encode()).map_err(AdoptError::Storage)?;
        let expected_vv = normalize_vv(vv).map_err(AdoptError::Storage)?;
        Ok(live_frontier == expected_frontier && live_vv == expected_vv)
    }

    /// 交付一段已 durable 的外部增量：预演校验 → 导入 live → 重定 flusher 基准。
    ///
    /// 返回 `Some(delta)` 表示 live 真的产生了新 op；`None` 表示没有新 op（幂等）。
    fn apply_durable_segment(
        &mut self,
        base_seq: u64,
        target_head_seq: u64,
        target_frontier: &[u8],
        target_vv: &[u8],
        updates: &[Vec<u8>],
    ) -> Result<Option<LiveDelta>, AdoptError> {
        if updates.is_empty() {
            return Ok(None);
        }

        // 相位 1：scratch 预演（不碰 live、不改任何水位）
        self.validate_durable_segment(base_seq, updates, target_frontier, target_vv)?;

        // 相位 2：导入 + 重定基准（同一临界区；Held 期间 journal 不会再增长）
        let mut guard = lock(self.journal.state_ref());
        if guard.journal.pending_len() != 0 {
            let pending = guard.journal.pending_len();
            drop(guard);
            return Err(AdoptError::Pending { pending });
        }
        if guard.durable.head_seq != base_seq {
            let durable_head_seq = guard.durable.head_seq;
            drop(guard);
            return Err(AdoptError::StaleBase {
                base_seq,
                durable_head_seq,
            });
        }
        let diff = match import_updates(&self.doc, updates.iter().map(|update| update.as_slice())) {
            Ok(diff) => diff,
            Err(error) => {
                drop(guard);
                return Err(AdoptError::Import(error.to_string()));
            }
        };
        let from_rev = self.live_rev;
        let changed = diff.changed();
        let to_rev = if changed {
            LiveRev(from_rev.0 + 1)
        } else {
            from_rev
        };
        let frontier = self.doc.frontiers().encode();
        let vv = self.doc.vv();
        let heads = AppliedHeads {
            head_seq: target_head_seq,
            frontier: target_frontier.to_vec(),
            vv: target_vv.to_vec(),
        };
        let rebased = JournalHandle::rebase_locked(&mut guard, &heads, to_rev);
        drop(guard);
        if let Err(error) = rebased {
            // 上一步已在同一把锁内确认 pending == 0，正常路径不可达
            return Err(AdoptError::Storage(error.to_string()));
        }

        self.live_rev = to_rev;
        self.live_frontier = frontier.clone();
        self.journal_base_vv = vv;
        if !changed {
            return Ok(None);
        }
        Ok(Some(LiveDelta {
            doc_id: self.doc_id.clone(),
            from_rev,
            to_rev,
            live_frontier: frontier,
            splice: diff.splice(),
            cause: None,
            source: ChangeSource::Remote,
            merge: MergeInfo {
                concurrent: false,
                remote_changes: updates.len() as u32,
            },
        }))
    }

    /// 在不碰 live 的 scratch 副本上预演一次导入并校验目标 frontier/vv。
    ///
    /// 同时确认 live 与库中 `base_seq` 的状态一致——这是"导入必然成功、且不会只导入一半"
    /// 的前提：scratch 与 live 起点相同、payload 相同，因此 live 的导入结果与 scratch 一致。
    fn validate_durable_segment(
        &self,
        base_seq: u64,
        updates: &[Vec<u8>],
        target_frontier: &[u8],
        target_vv: &[u8],
    ) -> Result<(), AdoptError> {
        // base_seq == 0 表示库里还没有任何行：此时不能走 `load_body_doc(Some(0))`
        // （它会去读 seq 0 的 end_frontier），直接用一个空文档做 scratch。
        let scratch = if base_seq == 0 {
            BodyDoc::restore(self.doc_id.clone(), self.peer_id, None, &[])
                .map_err(|error| AdoptError::Storage(error.to_string()))?
        } else {
            store::load_body_doc(&self.vault, &self.doc_id, self.peer_id, Some(base_seq))
                .map_err(|error| AdoptError::Storage(error.to_string()))?
                .0
        };
        if scratch.vv() != self.doc.vv() {
            return Err(AdoptError::Inconsistent {
                context: "live VV 与库中 base_seq 不一致".to_owned(),
            });
        }
        if normalize_frontier(&scratch.frontiers().encode()).map_err(AdoptError::Storage)?
            != normalize_frontier(&self.live_frontier).map_err(AdoptError::Storage)?
        {
            return Err(AdoptError::Inconsistent {
                context: "live frontier 与库中 base_seq 不一致".to_owned(),
            });
        }

        import_updates(&scratch, updates.iter().map(|update| update.as_slice()))
            .map_err(|error| AdoptError::Import(error.to_string()))?;
        if normalize_frontier(&scratch.frontiers().encode()).map_err(AdoptError::Storage)?
            != normalize_frontier(target_frontier).map_err(AdoptError::Storage)?
        {
            return Err(AdoptError::TargetMismatch { field: "frontier" });
        }
        if normalize_vv(&scratch.vv().encode()).map_err(AdoptError::Storage)?
            != normalize_vv(target_vv).map_err(AdoptError::Storage)?
        {
            return Err(AdoptError::TargetMismatch { field: "vv" });
        }
        Ok(())
    }
}
