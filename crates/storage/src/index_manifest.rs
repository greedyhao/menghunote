//! 索引清单读取（S4-1a，ADR 0005 §1）。
//!
//! 检索重建索引时需要的**全部**数据都从这一条查询来：标题、正文（可空）、
//! 版本水位，以及"这篇是不是墓碑"。这里刻意**不过滤** `deleted=1`：
//!
//! - 墓碑必须能被读到，search 才能把已删笔记从索引里摘掉；
//! - 过滤/对账策略属于 search，storage 不替它做"只查未删"的假设。

use rusqlite::types::Value;

use crate::error::StorageError;
use crate::read_snapshot::{map_read_error, ReadSnapshot};

/// `notes` 表的一行（索引清单）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexManifestRow {
    /// 笔记 ID。
    pub note_id: String,
    /// 标题（结构投影维护的最新值）。
    pub title: String,
    /// 物化后的 Markdown 源文；从未物化过时为 `None`。
    pub body_md: Option<String>,
    /// 正文文档 ID（Loro doc）。
    pub body_doc_id: String,
    /// 已物化到的 update 序号（`0` = 还没物化）。
    pub body_seq: i64,
    /// 已物化到的 frontier（编码后的 Loro frontier；`None` = 尚未物化）。
    pub body_frontier: Option<Vec<u8>>,
    /// 结构投影水位（结构变更时推进）。
    pub meta_seq: u64,
    /// 是否墓碑。
    pub deleted: bool,
    /// 最后更新时间（毫秒时间戳，来自结构投影）。
    pub updated_at: i64,
}

/// 读取整库索引清单（**包含墓碑**）。
pub fn list_index_manifest(snapshot: &ReadSnapshot) -> Result<Vec<IndexManifestRow>, StorageError> {
    let sql = "SELECT note_id, title, body_md, body_doc_id, body_seq, body_frontier, \
               meta_seq, deleted, updated_at FROM notes ORDER BY note_id";
    let mut statement = snapshot.prepare(sql)?;
    let mut rows = statement
        .query(rusqlite::params![])
        .map_err(|error| map_read_error(error, "执行索引清单查询"))?;
    let mut out = Vec::new();
    loop {
        let row = rows
            .next()
            .map_err(|error| map_read_error(error, "读取索引清单行"))?;
        let Some(row) = row else {
            break;
        };
        let note_id = row
            .get::<usize, String>(0)
            .map_err(|error| map_read_error(error, "解码 note_id"))?;
        let title = row
            .get::<usize, String>(1)
            .map_err(|error| map_read_error(error, "解码 title"))?;
        let body_md = match row
            .get::<usize, Value>(2)
            .map_err(|error| map_read_error(error, "解码 body_md"))?
        {
            Value::Null => None,
            Value::Text(text) => Some(text),
            other => {
                return Err(StorageError::ManifestShape {
                    column: "body_md",
                    found: describe(&other),
                })
            }
        };
        let body_doc_id = row
            .get::<usize, String>(3)
            .map_err(|error| map_read_error(error, "解码 body_doc_id"))?;
        let body_seq = row
            .get::<usize, i64>(4)
            .map_err(|error| map_read_error(error, "解码 body_seq"))?;
        let body_frontier = match row
            .get::<usize, Value>(5)
            .map_err(|error| map_read_error(error, "解码 body_frontier"))?
        {
            Value::Null => None,
            Value::Blob(blob) => Some(blob),
            other => {
                return Err(StorageError::ManifestShape {
                    column: "body_frontier",
                    found: describe(&other),
                })
            }
        };
        let meta_seq = row
            .get::<usize, i64>(6)
            .map_err(|error| map_read_error(error, "解码 meta_seq"))?;
        let deleted = row
            .get::<usize, i64>(7)
            .map_err(|error| map_read_error(error, "解码 deleted"))?
            != 0;
        let updated_at = row
            .get::<usize, i64>(8)
            .map_err(|error| map_read_error(error, "解码 updated_at"))?;
        out.push(IndexManifestRow {
            note_id,
            title,
            body_md,
            body_doc_id,
            body_seq,
            body_frontier,
            meta_seq: u64::try_from(meta_seq).unwrap_or(0),
            deleted,
            updated_at,
        });
    }
    Ok(out)
}

/// 列类型不符合预期时的可读描述。
fn describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Integer(_) => "INTEGER",
        Value::Real(_) => "REAL",
        Value::Text(_) => "TEXT",
        Value::Blob(_) => "BLOB",
    }
}
