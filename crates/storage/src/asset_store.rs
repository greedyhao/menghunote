//! S7-16：资产写入与引用 API。
//!
//! `assets` 是内容寻址表：`asset_hash`（SHA-256，32B）由调用方计算；本模块只校验
//! 参数一致性（`byte_len == data.len()`、非空明文），不重复做哈希。
//! `data IS NULL` 的资产是"只有元数据"的占位；首次写入明文时会补全。

use rusqlite::types::Value;

use crate::error::StorageError;
use crate::read_api::AssetMeta;
use crate::{Statement, Vault};

/// `asset_refs` 的一行：某篇 note 在 `start_byte` 处引用了某资产。
///
/// `start_byte` 是**引用定位点**：对账路径（[`replace_asset_refs`]）写的是正文里
/// `asset://` 前缀的字节偏移；早期的路径插入写的是链接起点。两种都只是"位置标签"，
/// 任何时候都可以按正文整篇重建，不要把它当长期身份。
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

/// 一次引用对账的结果（[`replace_asset_refs`] 的返回值）。
///
/// 只做观测/日志用，不参与任何裁决：引用是**账本**，正文才是真源（ADR 0019 §3）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssetRefReconcile {
    /// 事务提交后该 note 的引用条数（`refs` 去重后的条数）。
    pub refs: usize,
    /// 为"正文引用了、但本地没有 `assets` 行"的 hash 新建的占位元数据条数
    /// （`byte_len/media_type/data` 全 `NULL`，等同步/导入补齐字节）。
    pub placeholders: usize,
    /// 输入里被合并掉的重复 `(hash, start_byte)` 条数。
    pub duplicates: usize,
}

/// 用给定集合**原子替换**某篇 note 的全部资产引用（ADR 0019 §3「引用对账」）。
///
/// ## 语义
///
/// - 一个事务里：`DELETE` 该 note 的全部 `asset_refs` → 为缺失的 hash 建占位
///   `assets` 行 → 逐条 `INSERT` 新引用；任一步失败整笔回滚（旧引用原样保留）。
/// - **不删资产**：没有任何引用的 `assets` 行只是变成"无引用"，物理 GC 不在本 API
///   的范围（ADR 0019 §4 明确 v1 不做孤儿删除）。
/// - `data IS NULL` 的合法引用（缺字节/待同步）**保留**：只要 hash 对应行存在就照写；
///   完全不存在的 hash 会先建一条占位元数据（否则 `asset_refs` 的外键不成立）。
/// - 同一 `(hash, start_byte)` 重复出现会被合并（`duplicates` 计数）；`start_byte`
///   相同的不同 hash 各自保留（结构上允许，引用账本不做额外裁决）。
/// - 调用方负责给出**准确版本**的 `(hash, start_byte)` 列表：本函数不解析 Markdown、
///   不猜偏移，也不检查 `start_byte` 是否落在正文长度内。
///
/// ## 为什么不用 `start_byte` 做长期身份
///
/// 正文一改，偏移就变；引用只保证"按本次正文重建后是对的"。因此每次正文提交/导入/
/// 撤销重做后都应整篇重建并调用本函数，而不是增量改某一条的偏移。
pub fn replace_asset_refs(
    vault: &Vault,
    note_id: &str,
    body_seq: u64,
    refs: &[([u8; 32], u64)],
) -> Result<AssetRefReconcile, StorageError> {
    if note_id.trim().is_empty() {
        return Err(StorageError::Corrupt {
            context: "引用对账需要非空 note_id".to_owned(),
        });
    }
    let body_seq = i64_of(body_seq, "body_seq")?;

    // 1) 去重（保持 `start_byte` 升序，插入顺序稳定、测试可复现）。
    let mut unique: Vec<([u8; 32], u64)> = Vec::with_capacity(refs.len());
    let mut seen: std::collections::HashSet<([u8; 32], u64)> =
        std::collections::HashSet::with_capacity(refs.len());
    for (hash, start_byte) in refs {
        if seen.insert((*hash, *start_byte)) {
            unique.push((*hash, *start_byte));
        }
    }
    let duplicates = refs.len() - unique.len();
    unique.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));

    // 2) 组装语句：DELETE → 占位（每个 hash 一条）→ INSERT。整笔在一个事务里。
    let mut statements = Vec::with_capacity(1 + unique.len() * 2);
    statements.push(Statement::new(
        "DELETE FROM asset_refs WHERE note_id=?1",
        vec![Value::Text(note_id.to_owned())],
    ));
    let placeholder_from = 1;
    let mut unique_hashes: Vec<[u8; 32]> = Vec::new();
    for (hash, _) in &unique {
        if !unique_hashes.contains(hash) {
            unique_hashes.push(*hash);
            statements.push(Statement::new(
                "INSERT INTO assets(asset_hash, byte_len, media_type, data, orphan_since) \
                 VALUES(?1, NULL, NULL, NULL, NULL) \
                 ON CONFLICT(asset_hash) DO NOTHING",
                vec![Value::Blob(hash.to_vec())],
            ));
        }
    }
    let placeholder_to = statements.len();
    for (hash, start_byte) in &unique {
        statements.push(Statement::new(
            "INSERT INTO asset_refs(note_id, asset_hash, start_byte, body_seq) \
             VALUES(?1, ?2, ?3, ?4)",
            vec![
                Value::Text(note_id.to_owned()),
                Value::Blob(hash.to_vec()),
                Value::Integer(i64_of(*start_byte, "start_byte")?),
                Value::Integer(body_seq),
            ],
        ));
    }

    let affected = vault.exec_transaction(&statements, &[])?;
    // `ON CONFLICT DO NOTHING` 命中已有行时影响 0 行：只有真正新建的才是占位。
    let placeholders = affected
        .get(placeholder_from..placeholder_to)
        .unwrap_or_default()
        .iter()
        .filter(|rows| **rows == 1)
        .count();
    Ok(AssetRefReconcile {
        refs: unique.len(),
        placeholders,
        duplicates,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{asset_bytes, create_body_doc, create_note_slot, list_assets};
    use std::path::Path;

    /// 临时 vault：目录建在 `target/tmp` 下（AGENTS §6：不用 `%TEMP%`）。
    ///
    /// lib 单元测试拿不到 `CARGO_TARGET_TMPDIR`（只有集成测试有），所以从
    /// `CARGO_MANIFEST_DIR` 拼出同一个位置。
    fn fresh_vault(prefix: &str) -> (tempfile::TempDir, Vault) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("asset-store-{prefix}"));
        std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录");
        let dir = tempfile::Builder::new()
            .prefix("vault-")
            .tempdir_in(&root)
            .expect("创建临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
        (dir, vault)
    }

    /// 建 `loro_docs` + `notes` 行（`asset_refs.note_id` 有外键）。
    fn seed_note(vault: &Vault, note_id: &str, doc_id: &str) {
        create_body_doc(vault, doc_id).expect("插入 loro_docs 行");
        create_note_slot(vault, note_id, doc_id, "对账测试", 1).expect("插入 notes 行");
    }

    fn hash(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    #[test]
    fn replace_asset_refs_rewrites_the_whole_ledger_atomically() {
        let (_dir, vault) = fresh_vault("replace");
        seed_note(&vault, "note-1", "doc-1");
        insert_asset(&vault, &hash(0x11), 1, Some("image/png"), b"a").unwrap();
        insert_asset(&vault, &hash(0x22), 1, Some("image/png"), b"b").unwrap();

        // 首发：两条引用。
        let first = replace_asset_refs(&vault, "note-1", 7, &[(hash(0x11), 0), (hash(0x22), 40)])
            .expect("首次对账");
        assert_eq!(first.refs, 2);
        assert_eq!(first.placeholders, 0, "资产都已存在，不需要占位");
        assert_eq!(first.duplicates, 0);
        let refs = list_asset_refs(&vault, "note-1").unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].start_byte, 0);
        assert_eq!(refs[1].start_byte, 40);
        assert_eq!(refs[0].body_seq, 7);
        assert_eq!(refs[1].asset_hash, hash(0x22));

        // 正文变短：删掉后面那条 → 旧引用必须全部消失（不是"增量改偏移"）。
        let second = replace_asset_refs(&vault, "note-1", 9, &[(hash(0x11), 5)]).unwrap();
        assert_eq!(second.refs, 1);
        let refs = list_asset_refs(&vault, "note-1").unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].start_byte, 5);
        assert_eq!(refs[0].body_seq, 9);
        // 资产不删：没有引用的资产仍在库里（v1 不做物理 GC）。
        assert_eq!(list_assets(&vault).unwrap().len(), 2);

        // 清空引用集合：合法（正文里一个 asset 链接都没有）。
        let third = replace_asset_refs(&vault, "note-1", 10, &[]).unwrap();
        assert_eq!(third.refs, 0);
        assert!(list_asset_refs(&vault, "note-1").unwrap().is_empty());
    }

    #[test]
    fn replace_asset_refs_is_idempotent_and_dedupes() {
        let (_dir, vault) = fresh_vault("idempotent");
        seed_note(&vault, "note-1", "doc-1");
        insert_asset(&vault, &hash(0x33), 1, Some("text/plain"), b"x").unwrap();
        let refs = [(hash(0x33), 3), (hash(0x33), 3), (hash(0x33), 3)];

        let first = replace_asset_refs(&vault, "note-1", 1, &refs).unwrap();
        assert_eq!(first.refs, 1, "同 (hash, start) 只留一条");
        assert_eq!(first.duplicates, 2);
        let second = replace_asset_refs(&vault, "note-1", 2, &refs).unwrap();
        assert_eq!(second.refs, 1);
        assert_eq!(second.duplicates, 2);
        let listed = list_asset_refs(&vault, "note-1").unwrap();
        assert_eq!(listed.len(), 1, "重复对账不得堆行");
        assert_eq!(listed[0].start_byte, 3);
        assert_eq!(listed[0].body_seq, 2, "body_seq 按本次对账刷新");
    }

    #[test]
    fn replace_asset_refs_keeps_missing_bytes_and_creates_placeholders() {
        let (_dir, vault) = fresh_vault("placeholder");
        seed_note(&vault, "note-1", "doc-1");
        // `data IS NULL` 的合法引用（缺字节/待同步）：必须保留，不能当孤儿。
        let missing_bytes = hash(0x44);
        insert_asset(&vault, &missing_bytes, 3, Some("image/png"), b"abc").unwrap();
        vault
            .exec(
                "UPDATE assets SET data=NULL WHERE asset_hash=?1",
                &[Value::Blob(missing_bytes.to_vec())],
            )
            .unwrap();

        let unknown = hash(0x55);
        let report =
            replace_asset_refs(&vault, "note-1", 1, &[(missing_bytes, 0), (unknown, 64)]).unwrap();
        assert_eq!(report.refs, 2);
        assert_eq!(report.placeholders, 1, "完全未知的 hash 建一条占位元数据");
        let listed = list_asset_refs(&vault, "note-1").unwrap();
        assert_eq!(listed.len(), 2);
        // `data IS NULL` 的资产引用保留，占位行没有任何明文。
        assert_eq!(asset_bytes(&vault, missing_bytes).unwrap(), None);
        assert_eq!(asset_bytes(&vault, unknown).unwrap(), None);
        assert!(asset_meta_by_hash(&vault, &unknown).unwrap().is_some());
        // 占位不带 byte_len/media_type，不会被当成"可上传的引用资产"。
        assert!(list_referenced_assets(&vault).unwrap().is_empty());
        // 两个资产都没有明文：都不进"孤儿（有明文但无引用）"统计。
        assert_eq!(count_unreferenced_assets(&vault).unwrap(), 0);
    }

    #[test]
    fn replace_asset_refs_leaves_existing_refs_untouched_on_failure() {
        let (_dir, vault) = fresh_vault("rollback");
        seed_note(&vault, "note-1", "doc-1");
        insert_asset(&vault, &hash(0x66), 1, Some("text/plain"), b"y").unwrap();
        replace_asset_refs(&vault, "note-1", 1, &[(hash(0x66), 0)]).unwrap();

        // 未知 note：外键失败 → 整笔回滚（现有引用不变）。
        assert!(
            replace_asset_refs(&vault, "note-unknown", 1, &[(hash(0x66), 8)]).is_err(),
            "note_id 不存在必须失败"
        );
        assert!(
            replace_asset_refs(&vault, "  ", 1, &[]).is_err(),
            "空 note_id 必须失败"
        );
        let refs = list_asset_refs(&vault, "note-1").unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].start_byte, 0, "失败不得改动已有引用");

        // 目标 note 自己的引用在失败后也保持原样：`start_byte` 超出 SQLite INTEGER 时
        // 在**写之前**就失败（DELETE 尚未执行，也就没有需要回滚的东西）。
        assert!(replace_asset_refs(&vault, "note-1", 2, &[(hash(0x66), u64::MAX)]).is_err());
        let refs = list_asset_refs(&vault, "note-1").unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].start_byte, 0);
        assert_eq!(refs[0].body_seq, 1);
    }
}
