//! storage 只读 API 的薄适配层（S7-8）。
//!
//! 真源是 `crates/storage/src/read_api.rs`（`body_doc_meta` / `loro_update_payload` /
//! `loro_update_base_vv` / `doc_updates_since` / `doc_snapshot` / `read_doc_bootstrap` /
//! `read_doc_for_export` / `doc_exists`）。本模块**不写任何 SQL、不直接访问表**，只做：
//!
//! 1. 错误分类：`StorageError::HeadConflict`（head CAS 失败）→ [`SyncError::HeadConflict`]，
//!    它是**可重试**的并发冲突，不是数据丢失；
//! 2. 统一 `storage_view::*` 调用点，方便后续 bootstrap/checkpoint 复用同一套读路径。
//!
//! 出站加密用 `doc_meta` + `stored_update` + `base_vv_at`；入站依赖检查用
//! `doc_exists`；bootstrap/导出用 `updates_since` / `bootstrap` / `export_at_head`。

use crate::error::SyncError;
use menghunote_storage::{
    body_doc_meta, doc_exists as storage_doc_exists, doc_updates_since, loro_update_base_vv,
    loro_update_payload, read_doc_bootstrap, read_doc_for_export, BodyDocMeta, DocBootstrap,
    DocExport, StorageError, StoredLoroUpdate, Vault,
};

pub use menghunote_storage::DocSnapshot;

/// 把 storage 错误映射成 sync 错误；head CAS 冲突保持"可重试"语义。
#[must_use]
pub fn map_storage_error(error: StorageError) -> SyncError {
    match error {
        StorageError::HeadConflict {
            doc_id,
            expected,
            actual,
        } => SyncError::HeadConflict {
            doc_id,
            expected,
            actual,
        },
        other => SyncError::Storage(other),
    }
}

/// 读正文文档元数据；不存在返回 `None`。
pub fn doc_meta(vault: &Vault, doc_id: &str) -> Result<Option<BodyDocMeta>, SyncError> {
    body_doc_meta(vault, doc_id).map_err(map_storage_error)
}

/// 文档是否存在（入站 update 的依赖是否就绪）。
pub fn doc_exists(vault: &Vault, doc_id: &str) -> Result<bool, SyncError> {
    storage_doc_exists(vault, doc_id).map_err(map_storage_error)
}

/// 读出站 `(doc_id, seq)` 的 update 明文列。
pub fn stored_update(vault: &Vault, doc_id: &str, seq: u64) -> Result<StoredLoroUpdate, SyncError> {
    loro_update_payload(vault, doc_id, seq).map_err(map_storage_error)
}

/// `seq` 之前的 VV（出站 `base_vv`；`seq <= 1` 为空）。
pub fn base_vv_at(vault: &Vault, doc_id: &str, seq: u64) -> Result<Vec<u8>, SyncError> {
    loro_update_base_vv(vault, doc_id, seq).map_err(map_storage_error)
}

/// `seq > after_seq` 的 update（升序），出站范围/校验用。
pub fn updates_since(
    vault: &Vault,
    doc_id: &str,
    after_seq: u64,
) -> Result<Vec<StoredLoroUpdate>, SyncError> {
    doc_updates_since(vault, doc_id, after_seq).map_err(map_storage_error)
}

/// 从快照 + 后续 update 组装 bootstrap 读取包（无快照时 `snapshot` 为 `None`）。
pub fn bootstrap(vault: &Vault, doc_id: &str) -> Result<Option<DocBootstrap>, SyncError> {
    read_doc_bootstrap(vault, doc_id).map_err(map_storage_error)
}

/// 按期望 head 做一致性导出；head 不匹配返回可重试的 [`SyncError::HeadConflict`]，
/// **绝不返回半截快照**。
pub fn export_at_head(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
) -> Result<DocExport, SyncError> {
    read_doc_for_export(vault, doc_id, expected_head_seq).map_err(map_storage_error)
}
