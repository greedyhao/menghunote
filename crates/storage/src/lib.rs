//! menghunote 的本地存储层。
//!
//! 本 crate 只负责「SQLite 单库真源」这一层（见 `docs/adr/0001-storage-format.md`）：
//!
//! - 打开 / 创建 vault 数据库，并按 ADR 0001 §3.9 配置与**读回校验** PRAGMA；
//! - 基于 `PRAGMA user_version` 的逐级迁移，v1 为 ADR §7 附录 DDL 全量建表；
//! - 单写者 actor：一个专用线程独占 [`rusqlite::Connection`]，外部按命令交互；
//! - 正文 CRDT（Loro 1.17.1）：每个 body doc 一个 [`BodyDoc`]，
//!   单位换算的唯一实现在 [`menghunote_text`]（本 crate 实测结论见 body_doc 注释与 tests/loro_units.rs）；
//! - A 事务（[`store::append_updates`]）：append `loro_updates` + 推进 head + 写 outbox；
//! - 未 ACK journal + 背压（[`journal`]）；
//! - B 事务骨架（[`materialize`]）：从固定 frontier 物化 `notes.body_md`，旧任务不覆盖新结果。
//!
//! 本切片**不包含**：编辑器 UI/GPUI、Tantivy、relay、加密、同步协议。
//!
//! 用法：
//!
//! ```no_run
//! use menghunote_storage::{BodyDoc, Value, Vault};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let vault = Vault::open("D:/notes/vault.db")?;
//! vault.exec("INSERT INTO meta(key, value) VALUES(?1, ?2)", &[
//!     Value::Text("schema_owner".to_owned()),
//!     Value::Blob(b"menghunote".to_vec()),
//! ])?;
//!
//! let doc = BodyDoc::create("body-1", 1)?;
//! doc.insert_scalars(0, "A中😀e\u{301}")?;
//! assert_eq!(doc.scalar_len(), 5);
//! assert_eq!(vault.user_version()?, 1);
//! # Ok(())
//! # }
//! ```

mod actor;
mod body_doc;
mod connection;
mod error;
mod journal;
mod materialize;
mod migrate;
mod projector;
pub mod session;
mod store;
mod structure;
mod structure_store;
mod tree_view;

pub use actor::{AffectedGuard, Row, Statement, Vault};
pub use body_doc::{
    BodyDoc, PendingUpdate, APP_DOC_SCHEMA, BODY_CONTAINER, CODEC_VERSION, MIN_READER,
};
pub use error::StorageError;
pub use journal::{
    DurableSink, FlushReport, JournalEntry, JournalWriter, PendingJournal, VaultJournalSink,
};
pub use materialize::{
    materialize_body, pending_outbox, project_pending, reconcile, scan_stale_bodies,
    MaterializeOutcome, PendingProjection, StaleBody,
};
pub use migrate::SCHEMA_VERSION;
pub use projector::{project, trashed_notes, visible_notes, NoteView, ProjectionReport};
pub use rusqlite::types::Value;
pub use store::{
    append_updates, create_body_doc, create_note_slot, end_frontier_at, end_vv_at, load_body_doc,
    load_body_state, load_head, store_snapshot, BodyState, HeadState, UpdateRecord,
};
pub use structure::{
    normalize_tag, NodeSnapshot, NoteSeed, StructureDoc, StructureSnapshot, TagSnapshot,
    STRUCTURE_DOC_ID,
};
pub use structure_store::{ensure_structure_row, load_structure, StructureWriter};
pub use tree_view::{list_notes, list_tree, NodeKind, TreeEntry};
