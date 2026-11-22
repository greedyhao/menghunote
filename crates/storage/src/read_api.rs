//! S7-7：出站加密/bootstrap 所需的只读查询 API。
//!
//! sync 不再直连 `loro_updates` / `loro_docs` 表；本模块把它们收口成 storage 的
//! 公共读接口。`read_doc_for_export` 还会做 head CAS 复核，避免导出期间被新编辑
//! 悄悄改到一半。

use rusqlite::types::Value;

use crate::error::StorageError;
use crate::store;
use crate::Vault;

/// `loro_docs.kind`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Structure,
    Body,
}

impl DocKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Structure => "structure",
            Self::Body => "body",
        }
    }
}

impl std::fmt::Display for DocKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl PartialEq<&str> for DocKind {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<DocKind> for &str {
    fn eq(&self, other: &DocKind) -> bool {
        *self == other.as_str()
    }
}

/// `loro_docs` 的正文文档元数据（出站 UpdateV1 / checkpoint 需要）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyDocMeta {
    pub doc_id: String,
    pub kind: DocKind,
    pub generation: u64,
    pub codec_version: String,
    pub app_doc_schema: u32,
    pub min_reader: u32,
    pub head_seq: u64,
    pub head_frontier: Option<Vec<u8>>,
    pub head_vv: Option<Vec<u8>>,
}

/// 全量文档枚举的别名；语义同 [`BodyDocMeta`]（含 structure/tombstone）。
pub type DocMeta = BodyDocMeta;

/// 一条 `loro_updates` 记录（出站打包需要的明文列）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLoroUpdate {
    pub doc_id: String,
    pub seq: u64,
    pub update_id: String,
    pub batch_id: String,
    pub payload: Vec<u8>,
    pub end_frontier: Vec<u8>,
    pub end_vv: Vec<u8>,
}

/// 一次 head-CAS 导出快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocExport {
    pub meta: BodyDocMeta,
    pub updates: Vec<StoredLoroUpdate>,
}

/// 文档的持久化快照元数据（bootstrap 用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocSnapshot {
    pub doc_id: String,
    pub snapshot: Option<Vec<u8>>,
    pub snapshot_seq: u64,
    pub snapshot_frontier: Option<Vec<u8>>,
    pub head_seq: u64,
    pub head_frontier: Option<Vec<u8>>,
    pub head_vv: Option<Vec<u8>>,
}

/// 从快照恢复所需的最小读取结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocBootstrap {
    pub meta: BodyDocMeta,
    pub snapshot: Option<Vec<u8>>,
    pub snapshot_seq: u64,
    pub snapshot_frontier: Option<Vec<u8>>,
    /// `seq > snapshot_seq` 且 `seq <= head_seq` 的 update，升序。
    pub updates_since_snapshot: Vec<StoredLoroUpdate>,
}

/// 读正文文档元数据；不存在返回 `None`。
pub fn body_doc_meta(vault: &Vault, doc_id: &str) -> Result<Option<BodyDocMeta>, StorageError> {
    let rows = vault.query(
        "SELECT kind, generation, codec_version, app_doc_schema, min_reader, head_seq, head_frontier, head_vv \
         FROM loro_docs WHERE doc_id=?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some(BodyDocMeta {
        doc_id: doc_id.to_owned(),
        kind: doc_kind_of(row, 0)?,
        generation: u64_of(row, 1, "loro_docs.generation")?,
        codec_version: text_of(row, 2, "loro_docs.codec_version")?,
        app_doc_schema: u32_of(row, 3, "loro_docs.app_doc_schema")?,
        min_reader: u32_of(row, 4, "loro_docs.min_reader")?,
        head_seq: u64_of(row, 5, "loro_docs.head_seq")?,
        head_frontier: optional_blob(row, 6)?,
        head_vv: optional_blob(row, 7)?,
    }))
}

/// 遍历 `loro_docs` 全表（structure + 所有 body，含无 note 行的墓碑）。
pub fn list_all_docs(vault: &Vault) -> Result<Vec<DocMeta>, StorageError> {
    let rows = vault.query(
        "SELECT doc_id, kind, generation, codec_version, app_doc_schema, min_reader, \
         head_seq, head_frontier, head_vv FROM loro_docs ORDER BY doc_id ASC",
        &[],
    )?;
    rows.iter()
        .map(|row| {
            Ok(BodyDocMeta {
                doc_id: text_of(row, 0, "loro_docs.doc_id")?,
                kind: doc_kind_of(row, 1)?,
                generation: u64_of(row, 2, "loro_docs.generation")?,
                codec_version: text_of(row, 3, "loro_docs.codec_version")?,
                app_doc_schema: u32_of(row, 4, "loro_docs.app_doc_schema")?,
                min_reader: u32_of(row, 5, "loro_docs.min_reader")?,
                head_seq: u64_of(row, 6, "loro_docs.head_seq")?,
                head_frontier: optional_blob(row, 7)?,
                head_vv: optional_blob(row, 8)?,
            })
        })
        .collect()
}

/// 资产元数据；不包含 `assets.data`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetMeta {
    pub asset_hash: [u8; 32],
    pub byte_len: Option<i64>,
    pub media_type: Option<String>,
    pub has_data: bool,
    pub orphan_since: Option<i64>,
}

/// 列出全部资产元数据（不加载 data），按 hash 升序。
pub fn list_assets(vault: &Vault) -> Result<Vec<AssetMeta>, StorageError> {
    let rows = vault.query(
        "SELECT asset_hash, byte_len, media_type, data IS NOT NULL, orphan_since \
         FROM assets ORDER BY asset_hash ASC",
        &[],
    )?;
    rows.iter()
        .map(|row| {
            Ok(AssetMeta {
                asset_hash: asset_hash_of(row, 0)?,
                byte_len: optional_i64(row, 1)?,
                media_type: optional_text(row, 2)?,
                has_data: bool_of(row, 3, "assets.data IS NOT NULL")?,
                orphan_since: optional_i64(row, 4)?,
            })
        })
        .collect()
}

/// 单个资产元数据；不存在返回 `None`。
pub fn asset_meta(
    vault: &Vault,
    hash: impl AsRef<[u8]>,
) -> Result<Option<AssetMeta>, StorageError> {
    let hash = hash_blob(hash.as_ref())?;
    let rows = vault.query(
        "SELECT asset_hash, byte_len, media_type, data IS NOT NULL, orphan_since \
         FROM assets WHERE asset_hash=?1",
        &[Value::Blob(hash)],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some(AssetMeta {
        asset_hash: asset_hash_of(row, 0)?,
        byte_len: optional_i64(row, 1)?,
        media_type: optional_text(row, 2)?,
        has_data: bool_of(row, 3, "assets.data IS NOT NULL")?,
        orphan_since: optional_i64(row, 4)?,
    }))
}

/// 读资产原始字节；资产不存在或 `data IS NULL` 都返回 `None`。
pub fn asset_bytes(vault: &Vault, hash: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>, StorageError> {
    let hash = hash_blob(hash.as_ref())?;
    let rows = vault.query(
        "SELECT data FROM assets WHERE asset_hash=?1",
        &[Value::Blob(hash)],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    match row.first() {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Blob(data)) => Ok(Some(data.clone())),
        other => Err(StorageError::Corrupt {
            context: format!("assets.data 类型异常：{other:?}"),
        }),
    }
}

/// 文档是否存在（入站 update 前检查依赖是否就绪）。
pub fn doc_exists(vault: &Vault, doc_id: &str) -> Result<bool, StorageError> {
    let rows = vault.query(
        "SELECT 1 FROM loro_docs WHERE doc_id=?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    Ok(!rows.is_empty())
}

/// 读指定 `(doc_id, seq)` 的 update；不存在视为损坏/缺失。
pub fn loro_update_payload(
    vault: &Vault,
    doc_id: &str,
    seq: u64,
) -> Result<StoredLoroUpdate, StorageError> {
    let rows = vault.query(
        "SELECT update_id, batch_id, payload, end_frontier, end_vv \
         FROM loro_updates WHERE doc_id=?1 AND seq=?2",
        &[Value::Text(doc_id.to_owned()), Value::Integer(i64_of(seq)?)],
    )?;
    let row = rows.first().ok_or_else(|| StorageError::Corrupt {
        context: format!("loro_updates 缺少 {doc_id}/{seq}"),
    })?;
    Ok(StoredLoroUpdate {
        doc_id: doc_id.to_owned(),
        seq,
        update_id: text_of(row, 0, "loro_updates.update_id")?,
        batch_id: text_of(row, 1, "loro_updates.batch_id")?,
        payload: blob_of(row, 2, "loro_updates.payload")?,
        end_frontier: blob_of(row, 3, "loro_updates.end_frontier")?,
        end_vv: blob_of(row, 4, "loro_updates.end_vv")?,
    })
}

/// `seq` 之前的 VV（`seq <= 1` 时为空字节串）；供出站 `base_vv` 使用。
pub fn loro_update_base_vv(vault: &Vault, doc_id: &str, seq: u64) -> Result<Vec<u8>, StorageError> {
    if seq <= 1 {
        return Ok(Vec::new());
    }
    Ok(loro_update_payload(vault, doc_id, seq - 1)?.end_vv)
}

/// `seq > after_seq` 的 update，按 seq 升序；`after_seq=0` 返回全部。
///
/// 只返回 `seq <= head_seq` 的行（正常库不存在更长的行）。
pub fn doc_updates_since(
    vault: &Vault,
    doc_id: &str,
    after_seq: u64,
) -> Result<Vec<StoredLoroUpdate>, StorageError> {
    let meta = body_doc_meta(vault, doc_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("loro_docs 缺少 {doc_id}"),
    })?;
    let rows = vault.query(
        "SELECT seq, update_id, batch_id, payload, end_frontier, end_vv \
         FROM loro_updates WHERE doc_id=?1 AND seq>?2 AND seq<=?3 ORDER BY seq ASC",
        &[
            Value::Text(doc_id.to_owned()),
            Value::Integer(i64_of(after_seq)?),
            Value::Integer(i64_of(meta.head_seq)?),
        ],
    )?;
    rows.iter()
        .map(|row| {
            Ok(StoredLoroUpdate {
                doc_id: doc_id.to_owned(),
                seq: u64_of(row, 0, "loro_updates.seq")?,
                update_id: text_of(row, 1, "loro_updates.update_id")?,
                batch_id: text_of(row, 2, "loro_updates.batch_id")?,
                payload: blob_of(row, 3, "loro_updates.payload")?,
                end_frontier: blob_of(row, 4, "loro_updates.end_frontier")?,
                end_vv: blob_of(row, 5, "loro_updates.end_vv")?,
            })
        })
        .collect()
}

/// 读当前 head VV；文档不存在返回 `None`。
pub fn doc_vv(vault: &Vault, doc_id: &str) -> Result<Option<Vec<u8>>, StorageError> {
    Ok(body_doc_meta(vault, doc_id)?.and_then(|meta| meta.head_vv))
}

/// 读快照装载入口：`loro_docs.snapshot/snapshot_seq/snapshot_frontier`。
pub fn doc_snapshot(vault: &Vault, doc_id: &str) -> Result<Option<DocSnapshot>, StorageError> {
    let rows = vault.query(
        "SELECT snapshot, snapshot_seq, snapshot_frontier, head_seq, head_frontier, head_vv \
         FROM loro_docs WHERE doc_id=?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some(DocSnapshot {
        doc_id: doc_id.to_owned(),
        snapshot: optional_blob(row, 0)?,
        snapshot_seq: u64_of(row, 1, "loro_docs.snapshot_seq")?,
        snapshot_frontier: optional_blob(row, 2)?,
        head_seq: u64_of(row, 3, "loro_docs.head_seq")?,
        head_frontier: optional_blob(row, 4)?,
        head_vv: optional_blob(row, 5)?,
    }))
}

/// 从已有快照 + 后续 update 组装 bootstrap 读取包（无快照时 snapshot 为 None）。
pub fn read_doc_bootstrap(
    vault: &Vault,
    doc_id: &str,
) -> Result<Option<DocBootstrap>, StorageError> {
    let Some(snapshot) = doc_snapshot(vault, doc_id)? else {
        return Ok(None);
    };
    let meta = body_doc_meta(vault, doc_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("loro_docs 缺少 {doc_id}"),
    })?;
    let updates_since_snapshot = doc_updates_since(vault, doc_id, snapshot.snapshot_seq)?;
    let after = body_doc_meta(vault, doc_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("loro_docs 缺少 {doc_id}"),
    })?;
    if after.head_seq != snapshot.head_seq {
        return Err(StorageError::HeadConflict {
            doc_id: doc_id.to_owned(),
            expected: snapshot.head_seq,
            actual: after.head_seq,
        });
    }
    Ok(Some(DocBootstrap {
        meta,
        snapshot: snapshot.snapshot,
        snapshot_seq: snapshot.snapshot_seq,
        snapshot_frontier: snapshot.snapshot_frontier,
        updates_since_snapshot,
    }))
}

/// 按指定 head 做一次一致性导出；head 不匹配返回 [`StorageError::HeadConflict`]。
///
/// 读取期间若 head 被新编辑推进，也会在末尾复核时返回冲突；绝不返回半截快照。
pub fn read_doc_for_export(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
) -> Result<DocExport, StorageError> {
    let meta = body_doc_meta(vault, doc_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("loro_docs 缺少 {doc_id}"),
    })?;
    if meta.head_seq != expected_head_seq {
        return Err(StorageError::HeadConflict {
            doc_id: doc_id.to_owned(),
            expected: expected_head_seq,
            actual: meta.head_seq,
        });
    }

    let updates = doc_updates_since(vault, doc_id, 0)?;

    // 复核：若读到一半 head 变了，不能把它当成 expected_head_seq 的完整导出。
    let after = store::load_head(vault, doc_id)?;
    if after.head_seq != expected_head_seq {
        return Err(StorageError::HeadConflict {
            doc_id: doc_id.to_owned(),
            expected: expected_head_seq,
            actual: after.head_seq,
        });
    }
    let expected_last = updates.last().map(|update| update.seq).unwrap_or(0);
    if expected_last != expected_head_seq {
        return Err(StorageError::Corrupt {
            context: format!(
                "{doc_id} head={expected_head_seq} 但 update 最后 seq={expected_last}"
            ),
        });
    }
    Ok(DocExport { meta, updates })
}

fn text_of(row: &crate::Row, index: usize, field: &str) -> Result<String, StorageError> {
    match row.get(index) {
        Some(Value::Text(value)) => Ok(value.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 类型异常：{other:?}"),
        }),
    }
}

fn blob_of(row: &crate::Row, index: usize, field: &str) -> Result<Vec<u8>, StorageError> {
    match row.get(index) {
        Some(Value::Blob(value)) => Ok(value.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 类型异常：{other:?}"),
        }),
    }
}

fn optional_blob(row: &crate::Row, index: usize) -> Result<Option<Vec<u8>>, StorageError> {
    match row.get(index) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Blob(value)) => Ok(Some(value.clone())),
        other => Err(StorageError::Corrupt {
            context: format!("可选 BLOB 类型异常：{other:?}"),
        }),
    }
}

fn doc_kind_of(row: &crate::Row, index: usize) -> Result<DocKind, StorageError> {
    match text_of(row, index, "loro_docs.kind")?.as_str() {
        "structure" => Ok(DocKind::Structure),
        "body" => Ok(DocKind::Body),
        other => Err(StorageError::Corrupt {
            context: format!("loro_docs.kind 未知：{other}"),
        }),
    }
}

fn optional_text(row: &crate::Row, index: usize) -> Result<Option<String>, StorageError> {
    match row.get(index) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Text(value)) => Ok(Some(value.clone())),
        other => Err(StorageError::Corrupt {
            context: format!("可选 TEXT 类型异常：{other:?}"),
        }),
    }
}

fn optional_i64(row: &crate::Row, index: usize) -> Result<Option<i64>, StorageError> {
    match row.get(index) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Integer(value)) => Ok(Some(*value)),
        other => Err(StorageError::Corrupt {
            context: format!("可选 INTEGER 类型异常：{other:?}"),
        }),
    }
}

fn bool_of(row: &crate::Row, index: usize, field: &str) -> Result<bool, StorageError> {
    match row.get(index) {
        Some(Value::Integer(0)) => Ok(false),
        Some(Value::Integer(1)) => Ok(true),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 非 0/1：{other:?}"),
        }),
    }
}

fn asset_hash_of(row: &crate::Row, index: usize) -> Result<[u8; 32], StorageError> {
    let blob = blob_of(row, index, "assets.asset_hash")?;
    blob.as_slice()
        .try_into()
        .map_err(|_| StorageError::Corrupt {
            context: format!("assets.asset_hash 长度不是 32：{}", blob.len()),
        })
}

fn hash_blob(hash: &[u8]) -> Result<Vec<u8>, StorageError> {
    if hash.len() != 32 {
        return Err(StorageError::Corrupt {
            context: format!("asset hash 长度不是 32：{}", hash.len()),
        });
    }
    Ok(hash.to_vec())
}

fn u64_of(row: &crate::Row, index: usize, field: &str) -> Result<u64, StorageError> {
    match row.get(index) {
        Some(Value::Integer(value)) => u64::try_from(*value).map_err(|_| StorageError::Corrupt {
            context: format!("{field} 为负数"),
        }),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 类型异常：{other:?}"),
        }),
    }
}

fn u32_of(row: &crate::Row, index: usize, field: &str) -> Result<u32, StorageError> {
    u32::try_from(u64_of(row, index, field)?).map_err(|_| StorageError::Corrupt {
        context: format!("{field} 超出 u32"),
    })
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER"),
    })
}
