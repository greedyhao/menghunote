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
//! assert_eq!(vault.user_version()?, 4);
//! # Ok(())
//! # }
//! ```

mod actor;
pub mod asset_gc;
mod asset_store;
mod baseline_apply;
mod body_doc;
mod connection;
mod error;
mod events;
pub mod history;
mod index_manifest;
mod journal;
pub mod links;
mod materialize;
mod migrate;
mod projector;
mod read_api;
mod read_snapshot;
pub mod rename_links;
pub mod session;
mod store;
mod structure;
mod structure_store;
mod sync_bridge;
mod tree_view;

pub use actor::{AffectedGuard, Row, Statement, Vault};
pub use asset_gc::{scan_asset_gc, AssetGcCandidate, AssetGcReport, ASSET_GC_MAX_CANDIDATES};
pub use asset_store::{
    asset_content_hash, asset_meta_by_hash, count_unreferenced_assets, insert_asset,
    link_asset_ref, list_asset_refs, list_referenced_assets, replace_asset_refs,
    set_asset_orphan_since, unlink_asset_ref, AssetRef, AssetRefReconcile,
};
pub use baseline_apply::{
    abort_baseline_apply, apply_baseline_assets, apply_baseline_batch, baseline_apply_state,
    begin_baseline_apply, finish_baseline_apply, BaselineApplyFinish, BaselineApplyState,
    BaselineApplyStatus, BaselineAsset, BASELINE_MERGE_BATCH, META_SYNC_RELAY_CURSOR,
};
pub use body_doc::{
    BodyDoc, PendingUpdate, APP_DOC_SCHEMA, BODY_CONTAINER, CODEC_VERSION, MIN_READER,
};
pub use error::StorageError;
pub use events::{StorageEvent, StorageEventSubscriber, DEFAULT_EVENT_QUEUE};
pub use history::{
    capture_note_version, list_note_versions, load_note_version, load_note_version_for_note,
    prune_note_versions, CaptureNoteVersion, HistoryVersion, HistoryVersionSummary,
    HISTORY_MAX_VERSIONS_PER_NOTE, HISTORY_RETENTION_DAYS, HISTORY_SOURCE_LOCAL,
};
pub use index_manifest::{list_index_manifest, IndexManifestRow};
pub use journal::{
    DurableSink, FlushReport, JournalEntry, JournalOrigin, JournalWriter, PendingJournal,
    VaultJournalSink,
};
pub use links::{list_backlinks, Backlink};
pub use materialize::{
    materialize_body, materialize_body_profiled, pending_outbox, project_pending, reconcile,
    reconcile_batched, scan_stale_bodies, MaterializeOutcome, MaterializeProgress,
    MaterializeStageTimings, MaterializeSummary, PendingProjection, StaleBody,
};
pub use migrate::SCHEMA_VERSION;
pub use projector::{project, trashed_notes, visible_notes, NoteView, ProjectionReport};
pub use read_api::{
    asset_bytes, asset_meta, body_doc_meta, doc_exists, doc_snapshot, doc_updates_since, doc_vv,
    list_all_docs, list_assets, loro_update_base_vv, loro_update_payload, read_doc_bootstrap,
    read_doc_for_export, AssetMeta, BodyDocMeta, DocBootstrap, DocExport, DocKind, DocMeta,
    DocSnapshot, StoredLoroUpdate,
};
pub use read_snapshot::{existing_vault_has_content, ReadSnapshot};
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
pub use sync_bridge::{
    allocate_local_seq_for_import, append_updates_with_sync_intents, can_recycle_update,
    count_inbox_ready, enqueue_sync_intent, intent_message_id, intent_object_id, local_sync_intent,
    mark_acked, mark_inbox_quarantine, mark_integrated, mark_sent, mark_skipped_event_received,
    outbox_parts, outbox_repack_candidates, outbox_row, outbox_rows, pending_encryption,
    pending_send, repack_outbox, repack_required, set_confirmed_highwater, set_repack_required,
    set_sync_log_id, store_encrypted_parts, store_inbox_verified, sync_cursor_state,
    take_inbox_ready, take_inbox_ready_where, update_referenced_by_outbox, OutboxKey,
    OutboxRepackCandidate, RepackOutcome, RepackPart, RepackPartSummary, RepackSpec, SyncContext,
    SyncCursorState, SyncEncryptedPart, SyncInboxRow, SyncInboxState, SyncIntent, SyncOutboxPart,
    SyncOutboxRow, SyncOutboxState, META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ,
    META_SYNC_LOG_ID, META_SYNC_RECEIVED_SEQ, META_SYNC_REPACK_REQUIRED,
    META_SYNC_SKIPPED_RECEIVED_PREFIX, SYNC_INTENT_ID_DOMAIN,
};
pub use tree_view::{list_notes, list_tree, list_tree_sorted, NodeKind, TreeEntry, TreeSort};
