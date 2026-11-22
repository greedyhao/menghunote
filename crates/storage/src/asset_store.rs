//! S7-16：资产写入与引用 API。
//!
//! `assets` 是内容寻址表：`asset_hash`（SHA-256，32B）由调用方计算；本模块只校验
//! 参数一致性（`byte_len == data.len()`、非空明文），不重复做哈希。
//! `data IS NULL` 的资产是"只有元数据"的占位；首次写入明文时会补全。

use rusqlite::types::Value;

use crate::error::StorageError;
use crate::read_api::AssetMeta;
use crate::Vault;

/// `asset_refs` 的一行：某篇 note 在 `start_byte` 处引用了某资产。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetRef {
    pub note_id: String,
    pub asset_hash: [u8; 32],
    pub start_byte: u64,
    pub body_seq: u64,
}

/// 资产明文的 SHA-256（内容寻址的哈希来源）。
///
/// 只做纯计算：`sha2` 已经是本 crate 的依赖（基线快照/同步桥都在用），因此调用方
/// （如 app 的附件插入）**不需要**为了算一个哈希而新增依赖。
#[must_use]
pub fn asset_content_hash(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).into()
}

/// 插入一份资产明文；同 hash 完全一致时幂等。
///
/// - 空 `data` 拒绝（资产明文必须非空）；
/// - `byte_len` 必须等于 `data.len()`；
/// - 同 hash 已存在且 `data` 相同 → `Ok(())`；
/// - 同 hash 已存在但字节/长度不同 → [`StorageError::Corrupt`]；
/// - 已存在 `data IS NULL` 的元数据占位且长度一致 → 补全明文。
pub fn insert_asset(
    vault: &Vault,
    hash: &[u8; 32],
    byte_len: u64,
    media_type: Option<&str>,
    data: &[u8],
) -> Result<(), StorageError> {
    if data.is_empty() {
        return Err(StorageError::Corrupt {
            context: "资产明文不能为空".to_owned(),
        });
    }
    let expected_len = u64::try_from(data.len()).map_err(|_| StorageError::Corrupt {
        context: format!("资产明文长度 {} 超出 u64", data.len()),
    })?;
    if byte_len != expected_len {
        return Err(StorageError::Corrupt {
            context: format!("byte_len={byte_len} 与 data.len()={expected_len} 不一致"),
        });
    }
    let byte_len = i64::try_from(byte_len).map_err(|_| StorageError::Corrupt {
        context: format!("byte_len={byte_len} 超出 SQLite INTEGER"),
    })?;

    let existing = vault.query(
        "SELECT byte_len, media_type, data FROM assets WHERE asset_hash=?1",
        &[Value::Blob(hash.to_vec())],
    )?;
    if let Some(row) = existing.first() {
        let stored_len = optional_i64(row, 0, "assets.byte_len")?;
        let stored_data = optional_blob(row, 2)?;
        if let Some(stored) = stored_data {
            if stored_len != Some(byte_len) || stored != data {
                return Err(StorageError::Corrupt {
                    context: format!("资产 {hash:02x?} 已存在且内容不同"),
                });
            }
            return Ok(());
        }
        // 元数据占位（data IS NULL）：长度必须一致，再补明文。
        if stored_len.is_some_and(|stored| stored != byte_len) {
            return Err(StorageError::Corrupt {
                context: format!("资产 {hash:02x?} 已存在且 byte_len 不同"),
            });
        }
        let affected = vault.exec(
            "UPDATE assets SET byte_len=?2, media_type=?3, data=?4 WHERE asset_hash=?1",
            &[
                Value::Blob(hash.to_vec()),
                Value::Integer(byte_len),
                media_type.map_or(Value::Null, |media_type| Value::Text(media_type.to_owned())),
                Value::Blob(data.to_vec()),
            ],
        )?;
        if affected != 1 {
            return Err(StorageError::Corrupt {
                context: format!("补全资产 {hash:02x?} 明文失败"),
            });
        }
        return Ok(());
    }

    vault.exec(
        "INSERT INTO assets(asset_hash, byte_len, media_type, data, orphan_since) \
         VALUES(?1, ?2, ?3, ?4, NULL)",
        &[
            Value::Blob(hash.to_vec()),
            Value::Integer(byte_len),
            media_type.map_or(Value::Null, |media_type| Value::Text(media_type.to_owned())),
            Value::Blob(data.to_vec()),
        ],
    )?;
    Ok(())
}

/// 建立/更新 note → asset 的引用；同一 `(note, hash, start_byte)` 幂等。
pub fn link_asset_ref(
    vault: &Vault,
    note_id: &str,
    hash: &[u8; 32],
    start_byte: u64,
    body_seq: u64,
) -> Result<(), StorageError> {
    vault.exec(
        "INSERT INTO asset_refs(note_id, asset_hash, start_byte, body_seq) \
         VALUES(?1, ?2, ?3, ?4) \
         ON CONFLICT(note_id, asset_hash, start_byte) DO UPDATE SET body_seq=excluded.body_seq",
        &[
            Value::Text(note_id.to_owned()),
            Value::Blob(hash.to_vec()),
            Value::Integer(i64_of(start_byte, "start_byte")?),
            Value::Integer(i64_of(body_seq, "body_seq")?),
        ],
    )?;
    Ok(())
}

/// 删除一条引用；返回是否真的删到行。
pub fn unlink_asset_ref(
    vault: &Vault,
    note_id: &str,
    hash: &[u8; 32],
    start_byte: u64,
) -> Result<bool, StorageError> {
    let affected = vault.exec(
        "DELETE FROM asset_refs WHERE note_id=?1 AND asset_hash=?2 AND start_byte=?3",
        &[
            Value::Text(note_id.to_owned()),
            Value::Blob(hash.to_vec()),
            Value::Integer(i64_of(start_byte, "start_byte")?),
        ],
    )?;
    Ok(affected > 0)
}

/// 列出某篇 note 的全部资产引用，按 `start_byte` 升序。
pub fn list_asset_refs(vault: &Vault, note_id: &str) -> Result<Vec<AssetRef>, StorageError> {
    let rows = vault.query(
        "SELECT note_id, asset_hash, start_byte, body_seq FROM asset_refs \
         WHERE note_id=?1 ORDER BY start_byte ASC, asset_hash ASC",
        &[Value::Text(note_id.to_owned())],
    )?;
    rows.iter()
        .map(|row| {
            Ok(AssetRef {
                note_id: text_of(row, 0, "asset_refs.note_id")?,
                asset_hash: hash_of(row, 1)?,
                start_byte: u64_of(row, 2, "asset_refs.start_byte")?,
                body_seq: u64_of(row, 3, "asset_refs.body_seq")?,
            })
        })
        .collect()
}

/// 列出**被正文引用**的资产元数据（`asset_refs` 去重），不加载 `data`。
///
/// 同步的增量上传只应该覆盖"当前正文引用得到"的资产：`assets` 里可能还留着历史孤儿
/// （引用已删除但按"不删除本地资产"约束保留）。`data IS NULL` 的占位（只有元数据、
/// 没有明文的导入占位）也排除，避免上传空资产。
///
/// 返回顺序按 `asset_hash` 升序（稳定顺序，便于报告/测试）。
pub fn list_referenced_assets(vault: &Vault) -> Result<Vec<AssetMeta>, StorageError> {
    let rows = vault.query(
        "SELECT a.asset_hash, a.byte_len, a.media_type, a.data IS NOT NULL, a.orphan_since \
         FROM assets a \
         WHERE a.data IS NOT NULL \
           AND EXISTS(SELECT 1 FROM asset_refs r WHERE r.asset_hash=a.asset_hash) \
         ORDER BY a.asset_hash ASC",
        &[],
    )?;
    rows.iter()
        .map(|row| {
            Ok(AssetMeta {
                asset_hash: hash_of(row, 0)?,
                byte_len: optional_i64(row, 1, "assets.byte_len")?,
                media_type: optional_text(row, 2, "assets.media_type")?,
                has_data: true,
                orphan_since: optional_i64(row, 4, "assets.orphan_since")?,
            })
        })
        .collect()
}

/// 统计"有明文但没有任何正文引用"的资产数（同步报告里的孤儿策略口径）。
///
/// 只做计数，不加载元数据/字节：自动同步每轮都要报告该值，不能为孤儿做全量读。
pub fn count_unreferenced_assets(vault: &Vault) -> Result<u64, StorageError> {
    let rows = vault.query(
        "SELECT COUNT(*) FROM assets a \
         WHERE a.data IS NOT NULL \
           AND NOT EXISTS(SELECT 1 FROM asset_refs r WHERE r.asset_hash=a.asset_hash)",
        &[],
    )?;
    let value = rows
        .first()
        .and_then(|row| row.first())
        .and_then(|value| match value {
            Value::Integer(value) => u64::try_from(*value).ok(),
            _ => None,
        })
        .ok_or_else(|| StorageError::Corrupt {
            context: "count_unreferenced_assets 计数非法".to_owned(),
        })?;
    Ok(value)
}

/// 标记/清除资产的 GC 孤儿时间；返回是否找到该资产。
pub fn set_asset_orphan_since(
    vault: &Vault,
    hash: &[u8; 32],
    orphan_since: Option<i64>,
) -> Result<bool, StorageError> {
    let affected = vault.exec(
        "UPDATE assets SET orphan_since=?2 WHERE asset_hash=?1",
        &[
            Value::Blob(hash.to_vec()),
            orphan_since.map_or(Value::Null, Value::Integer),
        ],
    )?;
    Ok(affected > 0)
}

/// 读资产的只读元数据（供写入测试/调用方核对）。
pub fn asset_meta_by_hash(
    vault: &Vault,
    hash: &[u8; 32],
) -> Result<Option<AssetMeta>, StorageError> {
    crate::read_api::asset_meta(vault, hash)
}

fn text_of(row: &crate::Row, index: usize, field: &str) -> Result<String, StorageError> {
    match row.get(index) {
        Some(Value::Text(value)) => Ok(value.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 类型异常：{other:?}"),
        }),
    }
}

fn optional_i64(row: &crate::Row, index: usize, field: &str) -> Result<Option<i64>, StorageError> {
    match row.get(index) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Integer(value)) => Ok(Some(*value)),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 类型异常：{other:?}"),
        }),
    }
}

fn optional_text(
    row: &crate::Row,
    index: usize,
    field: &str,
) -> Result<Option<String>, StorageError> {
    match row.get(index) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Text(value)) => Ok(Some(value.clone())),
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
            context: format!("assets.data 类型异常：{other:?}"),
        }),
    }
}

fn hash_of(row: &crate::Row, index: usize) -> Result<[u8; 32], StorageError> {
    match row.get(index) {
        Some(Value::Blob(value)) => {
            value
                .as_slice()
                .try_into()
                .map_err(|_| StorageError::Corrupt {
                    context: format!("asset hash 长度不是 32：{}", value.len()),
                })
        }
        other => Err(StorageError::Corrupt {
            context: format!("asset hash 类型异常：{other:?}"),
        }),
    }
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

fn i64_of(value: u64, field: &str) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("{field}={value} 超出 SQLite INTEGER"),
    })
}
