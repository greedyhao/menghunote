//! 笔记本机编辑历史（ADR 0024 H1）。
//!
//! ## 范围
//!
//! 只做**本机**历史：数据源是本地 SQLite（`versions` 表 + 捕获时的正文快照），
//! 不查询 relay、不改同步 wire/schema。跨设备一致时间线后置。
//!
//! - 捕获粒度是「编辑会话」，不是每次按键：停止输入约 2 秒 / 切换笔记 / 关窗前
//!   由 app 调 [`capture_note_version`]；同一篇正文没变（`body_hash` 相同）不重复写。
//! - 版本只追加，不改写 CRDT 历史；恢复默认是 app 层「恢复为新笔记」。
//! - 保留策略在**同一次写事务**里执行：每篇最近 [`HISTORY_MAX_VERSIONS_PER_NOTE`]
//!   个版本或 [`HISTORY_RETENTION_DAYS`] 天，先到者。
//!
//! ## 存储口径
//!
//! 复用 v1 的 `versions` 表，v4 迁移补三列：
//!
//! | 列 | 含义 |
//! | --- | --- |
//! | `body_zstd` | **zstd 压缩的 UTF-8 Markdown 正文**（源文，不是 display 文本） |
//! | `asset_hashes` | 若干 32B SHA-256 **直接拼接**（空 BLOB = 没有资产引用），与 `asset_gc` 的解析口径一致 |
//! | `frontier` | `notes.body_frontier`，缺失时回退 `loro_docs.head_frontier`，都没有则空 BLOB |
//! | `source` | `local`（本机编辑，当前唯一写入方）/ `sync` / `checkpoint` |
//! | `device_id` | 捕获设备；本机历史可空（app 当前写 `None`） |
//! | `body_hash` | 正文 SHA-256，用于去重 |
//!
//! `asset_hashes` 来自 `asset_refs`（按 hash 升序去重拼接）：历史版本恢复后仍可能
//! 引用这些资产，因此 `asset_gc` 会把它们当作保护根（见 `asset_gc.rs`）。
//!
//! ## 失败语义
//!
//! 历史写失败**不阻塞正文**：调用方（app）只提示 + 打日志，正文保存/同步路径不变。
//! 库内不自洽（缺 notes 行、`asset_refs.asset_hash` 不是 32B、压缩流损坏）返回
//! [`StorageError::Corrupt`] / [`StorageError::HistoryCodec`]，不猜测、不静默写坏数据。

use rusqlite::types::Value;

use crate::actor::Statement;
use crate::error::StorageError;
use crate::Vault;

/// 本机编辑来源（当前唯一写入方）。
pub const HISTORY_SOURCE_LOCAL: &str = "local";

/// 每篇笔记默认保留的版本数上限。
pub const HISTORY_MAX_VERSIONS_PER_NOTE: usize = 200;

/// 每篇笔记默认保留的天数上限。
pub const HISTORY_RETENTION_DAYS: i64 = 90;

/// 一天对应的毫秒数（`captured_at` 与 `notes.*_at` 一样用毫秒）。
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// 正文压缩级别：3 是 zstd 的常用默认档，快照体积与 CPU 兼顾。
const ZSTD_LEVEL: i32 = 3;

/// 时间线列表项（**不**加载正文，避免一次解压 200 篇）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryVersionSummary {
    /// `versions.version_id`（时间线行标识；恢复时用它取正文）。
    pub version_id: i64,
    /// 笔记 ID。
    pub note_id: String,
    /// 捕获时的标题（不是当前标题）。
    pub title: String,
    /// 捕获时间（毫秒时间戳）。
    pub captured_at: i64,
    /// 来源（`local` / `sync` / `checkpoint`）。
    pub source: String,
    /// 捕获设备；本机历史通常为 `None`。
    pub device_id: Option<String>,
}

/// 一个历史版本的完整内容（只读预览/恢复用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryVersion {
    /// `versions.version_id`。
    pub version_id: i64,
    /// 笔记 ID。
    pub note_id: String,
    /// 捕获时的标题。
    pub title: String,
    /// 捕获时间（毫秒时间戳）。
    pub captured_at: i64,
    /// 来源。
    pub source: String,
    /// 捕获设备。
    pub device_id: Option<String>,
    /// 解压后的 UTF-8 Markdown 源文。
    pub body: String,
}

/// [`capture_note_version`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureNoteVersion {
    /// 本次是否真的**写入了新行**（`false` = 正文与最新版本相同，已去重）。
    pub captured: bool,
    /// 新写入的版本 ID；去重时回退为最新版本的 ID（没有版本则 `None`）。
    pub version_id: Option<i64>,
}

/// 捕获一个历史版本（只在正文与最新版本不同时写入）。
///
/// - `captured_at` 是毫秒时间戳，同时用作保留策略的「现在」；
/// - 写入、90 天清理、200 条上限清理在**同一个事务**里完成；
/// - 同一 `note_id` 的最新一行 `body_hash` 与本次正文相同 → 不写，返回
///   `captured = false`；
/// - `note_id` 不存在 → [`StorageError::Corrupt`]（不静默丢弃，调用方只提示）。
pub fn capture_note_version(
    vault: &Vault,
    note_id: &str,
    title: &str,
    body: &str,
    captured_at: i64,
) -> Result<CaptureNoteVersion, StorageError> {
    let body_hash = crate::asset_content_hash(body.as_bytes());

    // 1) 读 notes/loro_docs 的 generation 与 frontier（frontier 优先正文物化的那个）。
    let rows = vault.query(
        "SELECT n.body_doc_id, d.generation, n.body_frontier, d.head_frontier \
         FROM notes n JOIN loro_docs d ON d.doc_id = n.body_doc_id \
         WHERE n.note_id = ?1",
        &[Value::Text(note_id.to_owned())],
    )?;
    let row = rows.first().ok_or_else(|| StorageError::Corrupt {
        context: format!("历史捕获：notes 里找不到笔记 {note_id}"),
    })?;
    let generation = integer_of(row.get(1), "loro_docs.generation")?;
    let frontier = match row.get(2) {
        Some(Value::Blob(frontier)) => frontier.clone(),
        Some(Value::Null) | None => match row.get(3) {
            Some(Value::Blob(frontier)) => frontier.clone(),
            Some(Value::Null) | None => Vec::new(),
            Some(other) => {
                return Err(StorageError::Corrupt {
                    context: format!("历史捕获：loro_docs.head_frontier 类型异常（{other:?}）"),
                })
            }
        },
        Some(other) => {
            return Err(StorageError::Corrupt {
                context: format!("历史捕获：notes.body_frontier 类型异常（{other:?}）"),
            })
        }
    };

    // 2) 去重：只比较最新一行；旧库行 body_hash 为 NULL 时不参与去重。
    let latest = vault.query(
        "SELECT version_id, body_hash FROM versions WHERE note_id = ?1 \
         ORDER BY captured_at DESC, version_id DESC LIMIT 1",
        &[Value::Text(note_id.to_owned())],
    )?;
    if let Some(row) = latest.first() {
        if matches!(row.get(1), Some(Value::Blob(existing)) if existing.as_slice() == body_hash) {
            return Ok(CaptureNoteVersion {
                captured: false,
                version_id: match row.first() {
                    Some(Value::Integer(id)) => Some(*id),
                    _ => None,
                },
            });
        }
    }

    // 3) 资产 hash：`asset_refs` 去重 + 按 hash 升序，按 32B 直接拼接（asset_gc 口径）。
    let asset_hashes = note_asset_hashes(vault, note_id)?;

    // 4) 压缩正文（表列名就是 `body_zstd`，存 UTF-8 Markdown 源文）。
    let body_zstd = zstd::stream::encode_all(body.as_bytes(), ZSTD_LEVEL).map_err(|source| {
        StorageError::HistoryCodec {
            context: "zstd 压缩正文",
            source,
        }
    })?;

    let cutoff = captured_at.saturating_sub(HISTORY_RETENTION_DAYS.saturating_mul(DAY_MS));
    let keep = i64::try_from(HISTORY_MAX_VERSIONS_PER_NOTE).unwrap_or(i64::MAX);
    let statements = [
        Statement::new(
            "INSERT INTO versions(\
               note_id, captured_at, generation, frontier, title, body_zstd, \
               asset_hashes, source, device_id, body_hash\
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9)",
            vec![
                Value::Text(note_id.to_owned()),
                Value::Integer(captured_at),
                Value::Integer(generation),
                Value::Blob(frontier),
                Value::Text(title.to_owned()),
                Value::Blob(body_zstd),
                Value::Blob(asset_hashes),
                Value::Text(HISTORY_SOURCE_LOCAL.to_owned()),
                Value::Blob(body_hash.to_vec()),
            ],
        ),
        // 保留策略：90 天前的行 + 超出最近 200 个的行，与插入同一个事务。
        Statement::new(
            "DELETE FROM versions WHERE note_id = ?1 AND (\
               captured_at < ?2 OR version_id NOT IN (\
                 SELECT version_id FROM versions WHERE note_id = ?1 \
                 ORDER BY captured_at DESC, version_id DESC LIMIT ?3\
               )\
             )",
            vec![
                Value::Text(note_id.to_owned()),
                Value::Integer(cutoff),
                Value::Integer(keep),
            ],
        ),
    ];
    vault.exec_transaction(&statements, &[])?;

    // 5) 回读版本 ID（事务没有 RETURNING；单写者下按 body_hash 取最新一行即可）。
    let rows = vault.query(
        "SELECT version_id FROM versions WHERE note_id = ?1 AND body_hash = ?2 \
         ORDER BY version_id DESC LIMIT 1",
        &[
            Value::Text(note_id.to_owned()),
            Value::Blob(body_hash.to_vec()),
        ],
    )?;
    let version_id = rows.first().and_then(|row| match row.first() {
        Some(Value::Integer(id)) => Some(*id),
        _ => None,
    });
    Ok(CaptureNoteVersion {
        captured: true,
        version_id,
    })
}

/// 列出某篇笔记的历史版本（按时间倒序；不加载/解压正文）。
///
/// `limit` 会被夹到 `1..=`[`HISTORY_MAX_VERSIONS_PER_NOTE`]；时间线首版固定读 200。
pub fn list_note_versions(
    vault: &Vault,
    note_id: &str,
    limit: usize,
) -> Result<Vec<HistoryVersionSummary>, StorageError> {
    let limit = limit.clamp(1, HISTORY_MAX_VERSIONS_PER_NOTE);
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = vault.query(
        "SELECT version_id, note_id, title, captured_at, source, device_id \
         FROM versions WHERE note_id = ?1 \
         ORDER BY captured_at DESC, version_id DESC LIMIT ?2",
        &[Value::Text(note_id.to_owned()), Value::Integer(limit)],
    )?;
    let mut versions = Vec::with_capacity(rows.len());
    for row in &rows {
        versions.push(HistoryVersionSummary {
            version_id: integer_of(row.first(), "versions.version_id")?,
            note_id: text_of(row.get(1), "versions.note_id")?,
            title: text_of(row.get(2), "versions.title")?,
            captured_at: integer_of(row.get(3), "versions.captured_at")?,
            source: text_of(row.get(4), "versions.source")?,
            device_id: match row.get(5) {
                Some(Value::Text(device)) => Some(device.clone()),
                Some(Value::Null) | None => None,
                Some(other) => {
                    return Err(StorageError::Corrupt {
                        context: format!("versions.device_id 类型异常（{other:?}）"),
                    })
                }
            },
        });
    }
    Ok(versions)
}

/// 读取一个历史版本的完整正文（只读预览/恢复用）。
///
/// 不存在时返回 `Ok(None)`（行可能被保留策略清掉了，不是错误）。
pub fn load_note_version(
    vault: &Vault,
    version_id: i64,
) -> Result<Option<HistoryVersion>, StorageError> {
    load_version_where(vault, "version_id = ?1", vec![Value::Integer(version_id)])
}

/// 与 [`load_note_version`] 相同，但要求版本属于指定笔记。
///
/// 覆盖层只应预览/恢复「当前笔记」的版本；`note_id` 防止把旧面板的 version_id
/// 误用到另一篇笔记上。
pub fn load_note_version_for_note(
    vault: &Vault,
    note_id: &str,
    version_id: i64,
) -> Result<Option<HistoryVersion>, StorageError> {
    load_version_where(
        vault,
        "version_id = ?1 AND note_id = ?2",
        vec![Value::Integer(version_id), Value::Text(note_id.to_owned())],
    )
}

fn load_version_where(
    vault: &Vault,
    predicate: &str,
    params: Vec<Value>,
) -> Result<Option<HistoryVersion>, StorageError> {
    let sql = format!(
        "SELECT version_id, note_id, title, captured_at, source, device_id, body_zstd \
         FROM versions WHERE {predicate} LIMIT 1"
    );
    let rows = vault.query(&sql, &params)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let body_zstd = match row.get(6) {
        Some(Value::Blob(bytes)) => bytes,
        Some(other) => {
            return Err(StorageError::Corrupt {
                context: format!("versions.body_zstd 类型异常（{other:?}）"),
            })
        }
        None => {
            return Err(StorageError::Corrupt {
                context: "versions.body_zstd 缺失".to_owned(),
            })
        }
    };
    let body = zstd::stream::decode_all(body_zstd.as_slice()).map_err(|source| {
        StorageError::HistoryCodec {
            context: "zstd 解压正文",
            source,
        }
    })?;
    let body = String::from_utf8(body).map_err(|error| StorageError::Corrupt {
        context: format!("历史版本正文不是合法 UTF-8：{error}"),
    })?;
    Ok(Some(HistoryVersion {
        version_id: integer_of(row.first(), "versions.version_id")?,
        note_id: text_of(row.get(1), "versions.note_id")?,
        title: text_of(row.get(2), "versions.title")?,
        captured_at: integer_of(row.get(3), "versions.captured_at")?,
        source: text_of(row.get(4), "versions.source")?,
        device_id: match row.get(5) {
            Some(Value::Text(device)) => Some(device.clone()),
            Some(Value::Null) | None => None,
            Some(other) => {
                return Err(StorageError::Corrupt {
                    context: format!("versions.device_id 类型异常（{other:?}）"),
                })
            }
        },
        body,
    }))
}

/// 只跑保留策略（不做捕获）；返回删除的行数。
///
/// 正常捕获路径会在同一事务里自动执行它；本函数给测试/将来的显式清理入口用，
/// v1 UI 不暴露自动裁剪按钮（ADR 0024 §2.4）。
pub fn prune_note_versions(vault: &Vault, note_id: &str, now: i64) -> Result<usize, StorageError> {
    let cutoff = now.saturating_sub(HISTORY_RETENTION_DAYS.saturating_mul(DAY_MS));
    let keep = i64::try_from(HISTORY_MAX_VERSIONS_PER_NOTE).unwrap_or(i64::MAX);
    vault.exec(
        "DELETE FROM versions WHERE note_id = ?1 AND (\
           captured_at < ?2 OR version_id NOT IN (\
             SELECT version_id FROM versions WHERE note_id = ?1 \
             ORDER BY captured_at DESC, version_id DESC LIMIT ?3\
           )\
         )",
        &[
            Value::Text(note_id.to_owned()),
            Value::Integer(cutoff),
            Value::Integer(keep),
        ],
    )
}

/// 某篇笔记 `asset_refs` 的去重 hash，按字节升序，32B 直接拼接。
fn note_asset_hashes(vault: &Vault, note_id: &str) -> Result<Vec<u8>, StorageError> {
    let rows = vault.query(
        "SELECT DISTINCT asset_hash FROM asset_refs WHERE note_id = ?1 ORDER BY asset_hash",
        &[Value::Text(note_id.to_owned())],
    )?;
    let mut out = Vec::with_capacity(rows.len() * 32);
    for row in &rows {
        match row.first() {
            Some(Value::Blob(hash)) if hash.len() == 32 => out.extend_from_slice(hash),
            other => {
                return Err(StorageError::Corrupt {
                    context: format!(
                        "历史捕获：asset_refs.asset_hash 不是 32B（note={note_id}，{other:?}）"
                    ),
                })
            }
        }
    }
    Ok(out)
}

fn integer_of(value: Option<&Value>, column: &'static str) -> Result<i64, StorageError> {
    match value {
        Some(Value::Integer(value)) => Ok(*value),
        other => Err(StorageError::Corrupt {
            context: format!("{column} 类型异常（{other:?}）"),
        }),
    }
}

fn text_of(value: Option<&Value>, column: &'static str) -> Result<String, StorageError> {
    match value {
        Some(Value::Text(value)) => Ok(value.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{column} 类型异常（{other:?}）"),
        }),
    }
}
