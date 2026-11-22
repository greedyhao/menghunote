//! 文档会话层（ADR 0003 §5）：`EditorDocumentPort` 的串行 reducer 与协议类型。
//!
//! 归属：**`storage`**。它持有 live `LoroDoc`、唯一未 ACK journal、A/B 两阶段状态；
//! `editor` 通过本模块的**协议类型**与具体会话交互，但不得拿到可绕过会话的 live 句柄。
//!
//! 协议（[`protocol`]）由 Lead 冻结，**只读不改**；实现见 [`core`]（串行 reducer）
//! 与 [`flush`]（后台 A 事务写线程）。ADR 0009 切片 3a 的外部提交闸门与 adopt 在
//! [`gate`]（只做存储层，app/Driver 接线留 3b）。

mod core;
mod flush;
mod gate;
pub mod protocol;
mod schedule;

pub use core::{DocumentSession, SessionOptions};
pub use gate::{
    prepare_committed_delta, AdoptError, AdoptOutcome, AppliedHeads, ExternalCommitLease,
    ExternalCommitTicket, GateError, GateGeneration, GateState, PreparedCommittedDelta,
};
pub use schedule::{
    Clock, CloseReport, DriverEvent, DriverOptions, FlushPolicy, ManualClock, SessionDriver,
    SystemClock,
};

pub use protocol::{
    Applied, BatchId, ChangeSource, CommandId, DeltaSplice, DocId, DurableAck, EditOutcome,
    EditRequest, FlushReason, FlushTicket, FlushTicketId, Generation, HistoryState, LiveDelta,
    LiveRev, MergeInfo, OpenSnapshot, PortError, RejectReason, Rejected, ScalarEdit, SessionEpoch,
    SessionEvent, StaleBase,
};
