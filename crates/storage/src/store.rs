//! `loro_docs` / `loro_updates` 的读写与 A 事务。
//!
//! A 事务（ADR 0001 §3.4）在一次 SQLite 事务里完成三件事：
//!
//! 1. 逐个 append `loro_updates`（seq 连续，`update_id` 唯一）；
//! 2. 以 CAS 方式推进 `loro_docs.head_seq / head_frontier / head_vv`；
//! 3. 写 `projection_outbox.target_seq`（提醒 B 物化器）。
//!
//! CAS 用「该语句必须影响 1 行」的事务守卫实现：一旦 head 已被别人推进，
//! 整笔事务回滚，**不会出现「update 落库了但 head 没动」的半截状态**。

use loro::Frontiers;
use rusqlite::types::Value;

use crate::actor::{AffectedGuard, Statement};
use crate::body_doc::{BodyDoc, PendingUpdate, APP_DOC_SCHEMA, CODEC_VERSION, MIN_READER};
use crate::error::StorageError;
use crate::Vault;

/// 文档在 `loro_docs` 里的权威行摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadState {
    /// 文档 ID。
    pub doc_id: String,
    /// 已持久化的最后一个 batch seq（0 表示还没有任何 update）。
    pub head_seq: u64,
    /// 已持久化的 frontier（`Frontiers::encode`）。
    pub head_frontier: Vec<u8>,
    /// 已持久化的 VV（`VersionVector::encode`）。
    pub head_vv: Vec<u8>,
}

impl HeadState {
    /// 解码 frontier。
    pub fn frontiers(&self) -> Result<Frontiers, StorageError> {
        if self.head_frontier.is_empty() {
            return Ok(Frontiers::default());
        }
        Frontiers::decode(&self.head_frontier).map_err(StorageError::from)
    }

    /// 解码 VV。全新的文档 `head_vv` 是 NULL/空，等价于空版本向量。
    pub fn version_vector(&self) -> Result<loro::VersionVector, StorageError> {
        if self.head_vv.is_empty() {
            return Ok(loro::VersionVector::default());
        }
        loro::VersionVector::decode(&self.head_vv).map_err(StorageError::from)
    }
}

/// `loro_docs` 的一行（含快照）。
struct StoredDoc {
    snapshot: Option<Vec<u8>>,
    snapshot_seq: u64,
    head: HeadState,
}

/// 新建一个正文文档行（`kind = 'body'`）。
pub fn create_body_doc(vault: &Vault, doc_id: &str) -> Result<(), StorageError> {
    vault.exec(
        "INSERT INTO loro_docs(doc_id, kind, generation, app_doc_schema, codec_version, min_reader) \
         VALUES(?1, 'body', 1, ?2, ?3, ?4)",
        &[
            Value::Text(doc_id.to_owned()),
            Value::Integer(APP_DOC_SCHEMA),
            Value::Text(CODEC_VERSION.to_owned()),
            Value::Integer(MIN_READER),
        ],
    )?;
    Ok(())
}

/// 为一篇笔记准备好投影行，关联它的正文文档。
///
/// 注意：`notes` 是**投影**（ADR 0001 §3.1），最终由结构 CRDT 的投影器写入；
/// 本函数只是 S0 阶段的脚手架，让两阶段物化有落点，后续切片会由投影器接管。
pub fn create_note_slot(
    vault: &Vault,
    note_id: &str,
    body_doc_id: &str,
    title: &str,
    created_at: i64,
) -> Result<(), StorageError> {
    vault.exec(
        "INSERT INTO notes(note_id, body_doc_id, title, created_at, updated_at, deleted, is_favorite, is_pinned) \
         VALUES(?1, ?2, ?3, ?4, ?4, 0, 0, 0)",
        &[
            Value::Text(note_id.to_owned()),
            Value::Text(body_doc_id.to_owned()),
            Value::Text(title.to_owned()),
            Value::Integer(created_at),
        ],
    )?;
    Ok(())
}

/// 读 `loro_docs` 的 head 三件套；行不存在时报 [`StorageError::Corrupt`]。
pub fn load_head(vault: &Vault, doc_id: &str) -> Result<HeadState, StorageError> {
    let rows = vault.query(
        "SELECT head_seq, head_frontier, head_vv FROM loro_docs WHERE doc_id = ?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("loro_docs 缺少文档 {doc_id}"),
        })?;

    Ok(HeadState {
        doc_id: doc_id.to_owned(),
        head_seq: u64_of(&row, 0, "loro_docs.head_seq")?,
        head_frontier: blob_of(&row, 1, "loro_docs.head_frontier")?,
        head_vv: blob_of(&row, 2, "loro_docs.head_vv")?,
    })
}

/// 读文档行 + 快照元信息。
fn load_stored(vault: &Vault, doc_id: &str) -> Result<StoredDoc, StorageError> {
    let rows = vault.query(
        "SELECT snapshot, snapshot_seq, head_seq, head_frontier, head_vv FROM loro_docs WHERE doc_id = ?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("loro_docs 缺少文档 {doc_id}"),
        })?;

    Ok(StoredDoc {
        snapshot: optional_blob(&row, 0)?,
        snapshot_seq: u64_of(&row, 1, "loro_docs.snapshot_seq")?,
        head: HeadState {
            doc_id: doc_id.to_owned(),
            head_seq: u64_of(&row, 2, "loro_docs.head_seq")?,
            head_frontier: blob_of(&row, 3, "loro_docs.head_frontier")?,
            head_vv: blob_of(&row, 4, "loro_docs.head_vv")?,
        },
    })
}

/// 库里的 frontier 可能是 NULL（从未提交），语义等价于空 frontier。
///
/// 注意：`Frontiers::default().encode()` **不是**空字节，所以不能直接比较原始 bytes。
pub(crate) fn encoded_frontier(head: &HeadState) -> Vec<u8> {
    if head.head_frontier.is_empty() {
        Frontiers::default().encode()
    } else {
        head.head_frontier.clone()
    }
}

/// 重放所需材料：快照 + 后续 update payload + 目标 head。
pub(crate) struct ReplayParts {
    pub(crate) snapshot: Option<Vec<u8>>,
    pub(crate) payloads: Vec<Vec<u8>>,
    pub(crate) head: HeadState,
}

/// 读取重放材料（快照 + 连续 update 前缀），并校验 seq 连续、不越过快照边界。
///
/// `up_to_seq = Some(n)` 只重放到第 n 个 batch（固定 frontier）；`None` 重放到 head。
pub(crate) fn load_replay_parts(
    vault: &Vault,
    doc_id: &str,
    up_to_seq: Option<u64>,
) -> Result<ReplayParts, StorageError> {
    let stored = load_stored(vault, doc_id)?;
    let limit = match up_to_seq {
        Some(seq) => {
            if seq > stored.head.head_seq {
                return Err(StorageError::Corrupt {
                    context: format!(
                        "请求重放到 seq {seq}，但 {doc_id} 的 head_seq 只有 {}",
                        stored.head.head_seq
                    ),
                });
            }
            seq
        }
        None => stored.head.head_seq,
    };
    if limit < stored.snapshot_seq {
        return Err(StorageError::Corrupt {
            context: format!(
                "请求重放到 seq {limit}，但快照已经覆盖到 seq {}；不能倒着物化",
                stored.snapshot_seq
            ),
        });
    }

    let rows = vault.query(
        "SELECT seq, payload FROM loro_updates WHERE doc_id = ?1 AND seq > ?2 AND seq <= ?3 ORDER BY seq",
        &[
            Value::Text(doc_id.to_owned()),
            Value::Integer(i64_of(stored.snapshot_seq)?),
            Value::Integer(i64_of(limit)?),
        ],
    )?;

    let mut payloads = Vec::with_capacity(rows.len());
    for (expected_seq, row) in (stored.snapshot_seq + 1..).zip(rows.iter()) {
        let seq = u64_of(row, 0, "loro_updates.seq")?;
        if seq != expected_seq {
            return Err(StorageError::Corrupt {
                context: format!(
                    "{doc_id} 的 loro_updates 出现断档：期望 seq {expected_seq}，实际 {seq}"
                ),
            });
        }
        payloads.push(blob_of(row, 1, "loro_updates.payload")?);
    }

    let snapshot = if stored.snapshot_seq == 0 {
        None
    } else {
        stored.snapshot.clone()
    };

    let mut head = stored.head;
    if let Some(limit) = up_to_seq {
        head = HeadState {
            doc_id: doc_id.to_owned(),
            head_seq: limit,
            head_frontier: end_frontier_at(vault, doc_id, limit)?,
            head_vv: end_vv_at(vault, doc_id, limit)?,
        };
    }

    Ok(ReplayParts {
        snapshot,
        payloads,
        head,
    })
}

/// 从库里重放出一个 [`BodyDoc`]。
///
/// `up_to_seq = Some(n)` 时只重放到第 n 个 batch，用于**固定 frontier** 的 B 物化；
/// `None` 表示重放到当前 head，并在结尾校验 frontier 与库中记录一致（不一致说明数据损坏）。
pub fn load_body_doc(
    vault: &Vault,
    doc_id: &str,
    peer_id: u64,
    up_to_seq: Option<u64>,
) -> Result<(BodyDoc, HeadState), StorageError> {
    let parts = load_replay_parts(vault, doc_id, up_to_seq)?;
    let doc = BodyDoc::restore(doc_id, peer_id, parts.snapshot.as_deref(), &parts.payloads)?;

    if up_to_seq.is_none() {
        // 启动校验：重放结果必须与库中 head_frontier 一致，否则视为损坏（不回写、不猜）
        let replayed = doc.frontiers().encode();
        let expected = encoded_frontier(&parts.head);
        if replayed != expected {
            return Err(StorageError::Corrupt {
                context: format!(
                    "重放 {doc_id} 得到的 frontier 与 loro_docs.head_frontier 不一致（len {} vs {}）",
                    replayed.len(),
                    expected.len()
                ),
            });
        }
    }

    Ok((doc, parts.head))
}

/// 读某个 batch 落库时的 `end_vv`。
pub fn end_vv_at(vault: &Vault, doc_id: &str, seq: u64) -> Result<Vec<u8>, StorageError> {
    let rows = vault.query(
        "SELECT end_vv FROM loro_updates WHERE doc_id = ?1 AND seq = ?2",
        &[Value::Text(doc_id.to_owned()), Value::Integer(i64_of(seq)?)],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("{doc_id} 缺少 seq {seq} 的 loro_updates 行"),
        })?;
    blob_of(&row, 0, "loro_updates.end_vv")
}

/// 读某个 batch 落库时的 `end_frontier`（B 物化的固定 frontier）。
pub fn end_frontier_at(vault: &Vault, doc_id: &str, seq: u64) -> Result<Vec<u8>, StorageError> {
    let rows = vault.query(
        "SELECT end_frontier FROM loro_updates WHERE doc_id = ?1 AND seq = ?2",
        &[Value::Text(doc_id.to_owned()), Value::Integer(i64_of(seq)?)],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("{doc_id} 缺少 seq {seq} 的 loro_updates 行"),
        })?;
    blob_of(&row, 0, "loro_updates.end_frontier")
}

/// A 事务里要写入的一条 update（带它自己的 `batch_id`）。
///
/// 一次 flush 的连续前缀可能来自不同逻辑批次（例如慢盘合并了两次提交），
/// 所以 `batch_id` 是**逐行**的，不能整批共用一个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRecord {
    /// 该 update 所属的逻辑批次。
    pub batch_id: String,
    /// 增量内容。
    pub update: PendingUpdate,
}

impl UpdateRecord {
    /// 构造一条记录。
    pub fn new(batch_id: impl Into<String>, update: PendingUpdate) -> Self {
        Self {
            batch_id: batch_id.into(),
            update,
        }
    }
}

/// 把一份快照落库，记录它覆盖到的 seq 与 frontier。
///
/// 本切片只提供落库与「snapshot + 后续 update」重放路径；快照的生成节奏与旧 update 的
/// 真正 GC（ADR 0001 §3.5）留给后续切片。快照不能倒退覆盖更早的 seq。
pub fn store_snapshot(
    vault: &Vault,
    doc_id: &str,
    seq: u64,
    frontier: &[u8],
    snapshot: &[u8],
) -> Result<(), StorageError> {
    if seq == 0 || seq > load_head(vault, doc_id)?.head_seq {
        return Err(StorageError::Corrupt {
            context: format!("快照 seq {seq} 不在 (0, head] 区间内"),
        });
    }
    let affected = vault.exec(
        "UPDATE loro_docs SET snapshot = ?2, snapshot_seq = ?3, snapshot_frontier = ?4 \
         WHERE doc_id = ?1 AND snapshot_seq <= ?3",
        &[
            Value::Text(doc_id.to_owned()),
            Value::Blob(snapshot.to_vec()),
            Value::Integer(i64_of(seq)?),
            Value::Blob(frontier.to_vec()),
        ],
    )?;
    if affected != 1 {
        return Err(StorageError::Corrupt {
            context: format!("{doc_id} 的快照 seq 倒退，拒绝覆盖"),
        });
    }
    Ok(())
}

/// A 事务：追加一批 update + 推进 head + 写 projection_outbox。
///
/// `expected_head_seq` 是调用方看到的 head（CAS 基准）；不匹配时整笔回滚并返回
/// [`StorageError::HeadConflict`]，绝不覆盖别人的新结果。
pub fn append_updates(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[UpdateRecord],
) -> Result<HeadState, StorageError> {
    append_updates_inner(vault, doc_id, expected_head_seq, records, &[])
}

/// A 事务内同时写出站同步意图（一 update 一 intent）。
pub(crate) fn append_updates_with_sync_intents(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[UpdateRecord],
    intents: &[crate::sync_bridge::SyncIntent],
) -> Result<HeadState, StorageError> {
    append_updates_inner(vault, doc_id, expected_head_seq, records, intents)
}

fn append_updates_inner(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[UpdateRecord],
    intents: &[crate::sync_bridge::SyncIntent],
) -> Result<HeadState, StorageError> {
    if records.is_empty() {
        if !intents.is_empty() {
            return Err(StorageError::Corrupt {
                context: "空 update 批次不能带 sync intent".to_owned(),
            });
        }
        return load_head(vault, doc_id);
    }
    if !intents.is_empty() && intents.len() != records.len() {
        return Err(StorageError::Corrupt {
            context: format!(
                "append_updates: records={} intents={} 必须一对一",
                records.len(),
                intents.len()
            ),
        });
    }

    let last = &records
        .last()
        .ok_or_else(|| StorageError::Corrupt {
            context: "append_updates 收到空批次".to_owned(),
        })?
        .update;
    let head_seq = expected_head_seq + records.len() as u64;

    let mut statements = Vec::with_capacity(records.len() + 2 + intents.len());

    // 第 0 条就是 CAS：head_seq 必须还是调用方看到的那个值。
    // 放在最前面，过期基准会在碰到任何唯一约束之前就被守卫拦下，报 HeadConflict 而不是 UNIQUE。
    statements.push(Statement::new(
        "UPDATE loro_docs SET head_seq = ?2, head_frontier = ?3, head_vv = ?4 \
         WHERE doc_id = ?1 AND head_seq = ?5",
        vec![
            Value::Text(doc_id.to_owned()),
            Value::Integer(i64_of(head_seq)?),
            Value::Blob(last.end_frontier.clone()),
            Value::Blob(last.end_vv.clone()),
            Value::Integer(i64_of(expected_head_seq)?),
        ],
    ));

    for (index, record) in records.iter().enumerate() {
        let seq = expected_head_seq + 1 + index as u64;
        statements.push(Statement::new(
            "INSERT INTO loro_updates(doc_id, seq, update_id, batch_id, codec_version, payload, end_frontier, end_vv) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            vec![
                Value::Text(doc_id.to_owned()),
                Value::Integer(i64_of(seq)?),
                Value::Text(format!("{doc_id}#{seq}")),
                Value::Text(record.batch_id.clone()),
                Value::Text(CODEC_VERSION.to_owned()),
                Value::Blob(record.update.payload.clone()),
                Value::Blob(record.update.end_frontier.clone()),
                Value::Blob(record.update.end_vv.clone()),
            ],
        ));
    }

    statements.push(Statement::new(
        "INSERT INTO projection_outbox(doc_id, target_seq) VALUES(?1, ?2) \
         ON CONFLICT(doc_id) DO UPDATE SET target_seq = MAX(target_seq, excluded.target_seq)",
        vec![
            Value::Text(doc_id.to_owned()),
            Value::Integer(i64_of(head_seq)?),
        ],
    ));

    for (index, intent) in intents.iter().enumerate() {
        if intent.message_id.is_empty() || intent.batch_id.is_empty() || intent.key_epoch == 0 {
            return Err(StorageError::Corrupt {
                context: "sync intent message/batch/epoch 非法".to_owned(),
            });
        }
        let seq = expected_head_seq + 1 + index as u64;
        statements.push(Statement::new(
            "INSERT INTO sync_outbox(message_id,batch_id,doc_id,seq,key_epoch,object_id,state,ciphertext) \
             VALUES(?1,?2,?3,?4,?5,?6,'pending',NULL)",
            vec![
                Value::Text(intent.message_id.clone()),
                Value::Text(intent.batch_id.clone()),
                Value::Text(doc_id.to_owned()),
                Value::Integer(i64_of(seq)?),
                Value::Integer(i64_of(intent.key_epoch)?),
                Value::Blob(intent.object_id.to_vec()),
            ],
        ));
    }

    match vault.exec_transaction(&statements, &[AffectedGuard::exactly(0, 1)]) {
        Ok(_) => Ok(HeadState {
            doc_id: doc_id.to_owned(),
            head_seq,
            head_frontier: last.end_frontier.clone(),
            head_vv: last.end_vv.clone(),
        }),
        Err(StorageError::GuardFailed { .. }) => {
            let actual = load_head(vault, doc_id)
                .map(|head| head.head_seq)
                .unwrap_or(0);
            Err(StorageError::HeadConflict {
                doc_id: doc_id.to_owned(),
                expected: expected_head_seq,
                actual,
            })
        }
        Err(error) => Err(error),
    }
}

/// 笔记当前的物化水位（供 B 事务与测试读取）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyState {
    /// 笔记 ID。
    pub note_id: String,
    /// 已物化的 body_md。
    pub body_md: Option<String>,
    /// 已物化的 batch seq。
    pub body_seq: u64,
    /// 已物化的 frontier。
    pub body_frontier: Option<Vec<u8>>,
}

/// 读一篇笔记的物化状态。
pub fn load_body_state(vault: &Vault, note_id: &str) -> Result<BodyState, StorageError> {
    let rows = vault.query(
        "SELECT body_md, body_seq, body_frontier FROM notes WHERE note_id = ?1",
        &[Value::Text(note_id.to_owned())],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("notes 缺少 {note_id}"),
        })?;

    let body_md = match row.first() {
        Some(Value::Text(text)) => Some(text.clone()),
        Some(Value::Null) | None => None,
        Some(other) => {
            return Err(StorageError::Corrupt {
                context: format!("notes.body_md 类型异常：{other:?}"),
            });
        }
    };

    Ok(BodyState {
        note_id: note_id.to_owned(),
        body_md,
        body_seq: u64_of(&row, 1, "notes.body_seq")?,
        body_frontier: optional_blob(&row, 2)?,
    })
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER 范围"),
    })
}

fn u64_of(row: &[Value], index: usize, field: &str) -> Result<u64, StorageError> {
    match row.get(index) {
        Some(Value::Integer(value)) if *value >= 0 => Ok(*value as u64),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}

fn blob_of(row: &[Value], index: usize, field: &str) -> Result<Vec<u8>, StorageError> {
    match row.get(index) {
        Some(Value::Blob(bytes)) => Ok(bytes.clone()),
        Some(Value::Null) | None => Ok(Vec::new()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}

fn optional_blob(row: &[Value], index: usize) -> Result<Option<Vec<u8>>, StorageError> {
    match row.get(index) {
        Some(Value::Blob(bytes)) => Ok(Some(bytes.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(StorageError::Corrupt {
            context: format!("期望 BLOB/NULL，实际 {other:?}"),
        }),
    }
}
