//! 两阶段物化的 **B 事务**骨架：从固定 frontier 把正文写进 `notes.body_md`。
//!
//! 规则（ADR 0001 §3.4）：
//!
//! - B 只从**已持久化的固定 frontier**（本实现用 batch `seq` 表示）物化，
//!   不在事务里读「当前最新状态」，否则同一任务重跑会得到不同结果；
//! - 物化结果用 CAS 写入：`WHERE body_seq < :target_seq`。
//!   旧任务（target 更小）永远命中 0 行 → [`MaterializeOutcome::Stale`]，**不会覆盖新结果**；
//! - `projection_outbox` 只在成功物化到目标 seq 后清掉（且只清 `target_seq <= 已物化`）；
//! - 启动时 `body_frontier ≠ head_frontier`（或 body_seq 落后）必须重算，
//!   见 [`scan_stale_bodies`] / [`reconcile`]。

use rusqlite::types::Value;

use crate::actor::Statement;
use crate::error::StorageError;
use crate::store;
use crate::Vault;

/// 一篇需要重算正文投影的笔记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleBody {
    /// 笔记 ID。
    pub note_id: String,
    /// 正文文档 ID。
    pub doc_id: String,
    /// 应该追到的 head seq。
    pub head_seq: u64,
    /// 当前物化水位。
    pub body_seq: u64,
}

/// B 事务的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaterializeOutcome {
    /// 已写入新的 body_md。
    Applied {
        /// 笔记 ID。
        note_id: String,
        /// 物化到哪个 seq。
        body_seq: u64,
        /// 物化出的正文长度（Unicode scalar）。
        body_scalars: u64,
    },
    /// 目标 seq 不比已有结果新，本次不写（旧任务不能覆盖新结果）。
    Stale {
        /// 笔记 ID。
        note_id: String,
        /// 库里已有的物化水位。
        body_seq: u64,
    },
}

/// 一条待物化的 outbox 记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingProjection {
    /// 文档 ID。
    pub doc_id: String,
    /// 期望物化到的 seq。
    pub target_seq: u64,
    /// 关联的笔记（结构投影尚未建行时为 `None`）。
    pub note_id: Option<String>,
}

/// 从 `up_to_seq` 这个**固定 frontier** 物化一篇笔记的正文。
pub fn materialize_body(
    vault: &Vault,
    note_id: &str,
    doc_id: &str,
    up_to_seq: u64,
    peer_id: u64,
) -> Result<MaterializeOutcome, StorageError> {
    if up_to_seq == 0 {
        // 还没有任何 durable batch，正文就是空串；仍走一次写入把 body_frontier 对齐。
        let current = store::load_body_state(vault, note_id)?;
        return Ok(MaterializeOutcome::Stale {
            note_id: note_id.to_owned(),
            body_seq: current.body_seq,
        });
    }

    // 固定 frontier：取该 batch 落库时记录的 end_frontier，并从同样的重放边界取文本
    let frontier = store::end_frontier_at(vault, doc_id, up_to_seq)?;
    let (doc, _head) = store::load_body_doc(vault, doc_id, peer_id, Some(up_to_seq))?;
    let text = doc.text();
    let body_scalars = doc.scalar_len() as u64;

    let statements = [
        Statement::new(
            // CAS 规则：
            //  - body_seq 落后 → 正常推进；
            //  - body_seq 相同但 frontier 不一致 → 允许重算修复（启动校验用）；
            //  - body_seq 更大（旧任务迟到）→ 一行都不命中，绝不覆盖新结果。
            "UPDATE notes SET body_md = ?2, body_seq = ?3, body_frontier = ?4 \
             WHERE note_id = ?1 AND body_doc_id = ?5 \
               AND (body_seq < ?3 \
                    OR (body_seq = ?3 AND (body_frontier IS NULL OR body_frontier <> ?4)))",
            vec![
                Value::Text(note_id.to_owned()),
                Value::Text(text),
                Value::Integer(i64_of(up_to_seq)?),
                Value::Blob(frontier),
                Value::Text(doc_id.to_owned()),
            ],
        ),
        Statement::new(
            "DELETE FROM projection_outbox WHERE doc_id = ?1 AND target_seq <= ?2",
            vec![
                Value::Text(doc_id.to_owned()),
                Value::Integer(i64_of(up_to_seq)?),
            ],
        ),
    ];

    let affected = vault.exec_transaction(&statements, &[])?;
    if affected.first().copied().unwrap_or(0) == 1 {
        return Ok(MaterializeOutcome::Applied {
            note_id: note_id.to_owned(),
            body_seq: up_to_seq,
            body_scalars,
        });
    }

    // 没有更新说明 body_seq 已经 >= up_to_seq（或行不存在 → load_body_state 会报损坏）
    let current = store::load_body_state(vault, note_id)?;
    Ok(MaterializeOutcome::Stale {
        note_id: note_id.to_owned(),
        body_seq: current.body_seq,
    })
}

/// 启动扫描：找出 `body_frontier ≠ head_frontier` 或 `body_seq` 落后的笔记。
pub fn scan_stale_bodies(vault: &Vault) -> Result<Vec<StaleBody>, StorageError> {
    let rows = vault.query(
        "SELECT n.note_id, n.body_doc_id, d.head_seq, n.body_seq, n.body_frontier, d.head_frontier \
         FROM notes n JOIN loro_docs d ON d.doc_id = n.body_doc_id \
         WHERE n.body_frontier IS NULL \
            OR n.body_frontier <> d.head_frontier \
            OR n.body_seq < d.head_seq \
         ORDER BY n.note_id",
        &[],
    )?;

    let mut stale = Vec::with_capacity(rows.len());
    for row in &rows {
        stale.push(StaleBody {
            note_id: text_of(row, 0, "notes.note_id")?,
            doc_id: text_of(row, 1, "notes.body_doc_id")?,
            head_seq: integer_u64(row, 2, "loro_docs.head_seq")?,
            body_seq: integer_u64(row, 3, "notes.body_seq")?,
        });
    }
    Ok(stale)
}

/// 启动重算：把所有 stale 的正文按当前 head 重算一遍。
pub fn reconcile(vault: &Vault, peer_id: u64) -> Result<Vec<MaterializeOutcome>, StorageError> {
    let stale = scan_stale_bodies(vault)?;
    let mut outcomes = Vec::with_capacity(stale.len());
    for item in stale {
        outcomes.push(materialize_body(
            vault,
            &item.note_id,
            &item.doc_id,
            item.head_seq,
            peer_id,
        )?);
    }
    Ok(outcomes)
}

/// 读 `projection_outbox` 里所有待物化的文档。
pub fn pending_outbox(vault: &Vault) -> Result<Vec<PendingProjection>, StorageError> {
    let rows = vault.query(
        "SELECT o.doc_id, o.target_seq, n.note_id \
         FROM projection_outbox o LEFT JOIN notes n ON n.body_doc_id = o.doc_id \
         ORDER BY o.doc_id",
        &[],
    )?;

    let mut pending = Vec::with_capacity(rows.len());
    for row in &rows {
        pending.push(PendingProjection {
            doc_id: text_of(row, 0, "projection_outbox.doc_id")?,
            target_seq: integer_u64(row, 1, "projection_outbox.target_seq")?,
            note_id: match row.get(2) {
                Some(Value::Text(note_id)) => Some(note_id.clone()),
                Some(Value::Null) | None => None,
                other => {
                    return Err(StorageError::Corrupt {
                        context: format!("notes.note_id 读回异常：{other:?}"),
                    });
                }
            },
        });
    }
    Ok(pending)
}

/// B 物化器的一轮：处理 outbox 里所有能处理的条目（没有 note 行的跳过）。
pub fn project_pending(
    vault: &Vault,
    peer_id: u64,
) -> Result<Vec<MaterializeOutcome>, StorageError> {
    let mut outcomes = Vec::new();
    for pending in pending_outbox(vault)? {
        let Some(note_id) = pending.note_id else {
            continue;
        };
        outcomes.push(materialize_body(
            vault,
            &note_id,
            &pending.doc_id,
            pending.target_seq,
            peer_id,
        )?);
    }
    Ok(outcomes)
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER 范围"),
    })
}

fn integer_u64(row: &[Value], index: usize, field: &str) -> Result<u64, StorageError> {
    match row.get(index) {
        Some(Value::Integer(value)) if *value >= 0 => Ok(*value as u64),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}

fn text_of(row: &[Value], index: usize, field: &str) -> Result<String, StorageError> {
    match row.get(index) {
        Some(Value::Text(text)) => Ok(text.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}
