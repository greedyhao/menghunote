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

use loro::{Frontiers, VersionVector};
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

/// 读 `loro_docs.head_seq` 仍等于 `expected_head_seq` 时的 `head_vv`（远端已知 VV 的起点）。
///
/// 若 head 已被并发推进（CAS 必然失败、整笔事务回滚），返回空 VV 即可：它不会落库。
fn load_head_vv_at(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
) -> Result<VersionVector, StorageError> {
    let rows = vault.query(
        "SELECT head_seq, head_vv FROM loro_docs WHERE doc_id = ?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("loro_docs 缺少文档 {doc_id}"),
        })?;
    if u64_of(&row, 0, "loro_docs.head_seq")? != expected_head_seq {
        return Ok(VersionVector::default());
    }
    match row.get(1) {
        Some(Value::Null) | None => Ok(VersionVector::default()),
        Some(Value::Blob(bytes)) => VersionVector::decode(bytes).map_err(StorageError::from),
        Some(other) => Err(StorageError::Corrupt {
            context: format!("loro_docs.head_vv 读回异常：{other:?}"),
        }),
    }
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
    append_updates_inner(
        vault,
        doc_id,
        expected_head_seq,
        records,
        &[],
        &[],
        OriginMarks::AllLocal,
    )
}

/// A 事务：追加**一条**组合 update，并把同一批 remote inbox 行标为 `integrated`。
///
/// Phase 1A-2 的聚合 integrate 入口：同一文档的多条 inbox 行先在内存里逐条
/// `BodyDoc::import`，再把 `before..end` 的增量合成**一条** `UpdateRecord` 落盘；
/// 「追加 update + 推进 head + projection_outbox + 标记 inbox 行」全部在**同一个**
/// A 事务里（守卫要求每条 `UPDATE sync_inbox` 恰好影响 1 行，任何一行不存在/已不是
/// `verified` 都会整笔回滚，不会出现「update 写了但 inbox 没标」的半份状态）。
///
/// 与 [`append_updates`] 的差别只有同事务的批量标记：
/// - `relay_seqs` 为空时行为与 [`append_updates`] 逐字节一致（仍然只有一条记录）；
/// - 本函数**不推进** `sync.integrated_seq`；调用方在整批正文都完成后统一调用
///   [`crate::advance_integrated_prefix`]，避免每个 doc 组各写一次前缀；
/// - 校验失败映射：head CAS 失败仍是 [`StorageError::HeadConflict`]；inbox 标记失败
///   （行不存在 / 已 integrated / 已 quarantine）映射为 [`StorageError::SyncConflict`]，
///   两种情况下 update 都没有落库；
/// - 同事务把 `end_vv - 旧 head_vv`（= 这条远端消息实际带来的 ops）并入本机的
///   「远端已知 VV」（`sync.remote_known_vv.<doc_id>`，non-destructive meta），
///   供 `baseline_apply` 的 tombstone 判定区分已同步文档与 local-only 文档。
pub fn append_updates_with_integrated_marks(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    record: &UpdateRecord,
    log_id: &[u8; 16],
    relay_seqs: &[u64],
) -> Result<HeadState, StorageError> {
    let marks: Vec<(&[u8; 16], u64)> = relay_seqs
        .iter()
        .map(|relay_seq| (log_id, *relay_seq))
        .collect();
    append_updates_inner(
        vault,
        doc_id,
        expected_head_seq,
        std::slice::from_ref(record),
        &[],
        &marks,
        // engine integrate 的这条组合 update 全部来自远端消息：其 `end_vv - 旧 head_vv`
        // 就是远端实际带来的 ops，记入「远端已知 VV」（tombstone 判定用）。
        OriginMarks::PerRecord(&[true]),
    )
}

/// 一次 A 事务里每条 update 的**来源标记**。
///
/// 这是 P2-8 修掉的类型陷阱：旧接口用 `&[bool]` 表示来源，**空切片 = 全部本地**；
/// 而同一次调用的 `slots` 也是切片，**空切片 = 没有任何出站意图**。两个空切片含义
/// 相反、类型相同，调用点很容易把「没有意图」写成「全部本地」或反之。这里把来源标记
/// 收成枚举，让「空」不再是两种含义：
///
/// - [`OriginMarks::AllLocal`]：本批记录全部是本地写入，**不**扩大远端已知 VV；
/// - [`OriginMarks::PerRecord`]：与 `records` **一一对应**（长度不匹配直接报
///   [`StorageError::Corrupt`]），`true` = 该条来自远端消息，其
///   `end_vv - 前一条 end_vv` 并入「远端已知 VV」。
///
/// 本地写入即使已经 ACK 也不会扩大远端已知 VV（tombstone 判定只允许更保守）。
#[derive(Debug, Clone, Copy)]
pub enum OriginMarks<'a> {
    /// 全部记录都是本地写入。
    AllLocal,
    /// 逐条标记（必须与 records 等长）。
    PerRecord(&'a [bool]),
}

impl<'a> OriginMarks<'a> {
    /// 本批是否有远端条目。
    fn any_remote(self) -> bool {
        match self {
            Self::AllLocal => false,
            Self::PerRecord(marks) => marks.iter().any(|flag| *flag),
        }
    }

    /// 第 `index` 条是否来自远端（`AllLocal` 恒为 false）。
    fn is_remote(self, index: usize) -> bool {
        match self {
            Self::AllLocal => false,
            Self::PerRecord(marks) => marks.get(index).copied().unwrap_or(false),
        }
    }

    /// 校验来源标记与 records 的对应关系。
    ///
    /// `AllLocal` 不需要长度；`PerRecord` 必须与 records 等长——**空 records 配非空
    /// marks、或 records 配空 marks 都会在这里被拒绝**，不会退化成「全部本地」。
    fn validate(self, records_len: usize) -> Result<(), StorageError> {
        if let Self::PerRecord(marks) = self {
            if marks.len() != records_len {
                return Err(StorageError::Corrupt {
                    context: format!(
                        "append_updates: records={records_len} \
                         OriginMarks::PerRecord={} 必须一对一",
                        marks.len()
                    ),
                });
            }
        }
        Ok(())
    }
}

/// A 事务内同时写出站同步意图（按位置一一对应；`None` = 该条不产生意图）。
///
/// 与 [`append_updates_with_sync_intents`] 相比，这里允许"某些 update 没有意图"：
/// 一次 flush 的前缀可能混本地编辑与 remote 导入，而 remote 导入**绝不能**产生
/// 出站意图（ADR 0009）。仍然只有**一条** A 事务，失败即整笔回滚。
///
/// 参数语义（P2-8 钉死）：
/// - `slots` 空 = 本批**没有任何**出站意图（不是"每条都要一条意图"）；`None` 位 = 该条
///   不产生意图；
/// - `origin` 见 [`OriginMarks`]：只有远端条目会扩大远端已知 VV，本地条目永远不扩大。
pub(crate) fn append_updates_with_intent_slots(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[UpdateRecord],
    slots: &[Option<crate::sync_bridge::SyncIntent>],
    origin: OriginMarks<'_>,
) -> Result<HeadState, StorageError> {
    append_updates_inner(
        vault,
        doc_id,
        expected_head_seq,
        records,
        slots,
        &[],
        origin,
    )
}

/// 同事务附加的 inbox 标记：`(log_id, relay_seq)`，每个都要求恰好命中一行 `verified`。
type IntegratedMarks<'a> = &'a [(&'a [u8; 16], u64)];

fn append_updates_inner(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[UpdateRecord],
    slots: &[Option<crate::sync_bridge::SyncIntent>],
    integrated_marks: IntegratedMarks<'_>,
    origin: OriginMarks<'_>,
) -> Result<HeadState, StorageError> {
    if records.is_empty() {
        if !slots.is_empty() {
            return Err(StorageError::Corrupt {
                context: "空 update 批次不能带 sync intent".to_owned(),
            });
        }
        if !integrated_marks.is_empty() {
            return Err(StorageError::Corrupt {
                context: "空 update 批次不能带 inbox 标记".to_owned(),
            });
        }
        // `PerRecord` 配空 records 时，非空标记同样算长度不匹配（不会退化成全部本地）。
        origin.validate(0)?;
        return load_head(vault, doc_id);
    }
    if !slots.is_empty() && slots.len() != records.len() {
        return Err(StorageError::Corrupt {
            context: format!(
                "append_updates: records={} intents={} 必须一对一",
                records.len(),
                slots.len()
            ),
        });
    }
    origin.validate(records.len())?;

    // 远端已知 VV 的增量：只有 remote 条目会扩大它（`after_vv - running_vv`）。
    //
    // 这里只算「本批新增了哪些远端 ops」；旧值读取与 CAS 语句在构造语句阶段生成，
    // 重试时按最新旧值重算并集，但 `added` 本身不变（head 基准没变）。
    let remote_added = if origin.any_remote() {
        let mut running = load_head_vv_at(vault, doc_id, expected_head_seq)?;
        let mut known = VersionVector::new();
        for (index, record) in records.iter().enumerate() {
            let after = VersionVector::decode(&record.update.end_vv).map_err(StorageError::from)?;
            if origin.is_remote(index) {
                let added = crate::remote_known::added_since(&after, &running);
                known = crate::remote_known::union(&known, &added);
            }
            running = after;
        }
        if known.is_empty() {
            None
        } else {
            Some(known)
        }
    } else {
        None
    };

    let last = &records
        .last()
        .ok_or_else(|| StorageError::Corrupt {
            context: "append_updates 收到空批次".to_owned(),
        })?
        .update;
    let head_seq = expected_head_seq + records.len() as u64;

    let mut statements = Vec::with_capacity(records.len() + 2 + slots.len());

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

    for (index, slot) in slots.iter().enumerate() {
        // `None`：这条 update 不产生出站意图（remote 导入 / adopt 等），同事务里跳过。
        let Some(intent) = slot else {
            continue;
        };
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

    // 远端已知 VV 的 meta 写入（与 update/head 同一 A 事务；失败一起回滚）。
    //
    // **值 CAS**：语句里带读到的旧值，只有库中值仍是旧值才会更新（`WHERE meta.value=?old`，
    // 旧值不存在时用 `INSERT ... WHERE NOT EXISTS`）。这样两个写者（普通 pull 的 A 事务
    // 与 baseline apply 批次）即使都拿着同一份旧并集，后到的那个也不会把先到的扩大值
    // 覆盖回更小的并集；CAS 不命中 → 0 行 → 守卫失败 → 整笔回滚后重读重试。
    let remote_slot = match &remote_added {
        Some(added) => {
            let index = statements.len();
            statements.push(cas_statement_or_corrupt(
                crate::remote_known::merge_statement(vault, doc_id, added)?,
                doc_id,
            )?);
            Some(index)
        }
        None => None,
    };

    // 同事务标记 inbox 行：每条都要求恰好影响 1 行（不存在 / 已 integrated /
    // 已 quarantine 都会让整笔 A 事务回滚）。标记语句放在最后，避免它们的守卫先于
    // head CAS 暴露，保证并发冲突仍然报 HeadConflict。
    let marks_start = statements.len();
    for (mark_log_id, relay_seq) in integrated_marks {
        statements.push(Statement::new(
            "UPDATE sync_inbox SET state='integrated' \
             WHERE log_id=?1 AND relay_seq=?2 AND state='verified'",
            vec![
                Value::Blob(mark_log_id.to_vec()),
                Value::Integer(i64_of(*relay_seq)?),
            ],
        ));
    }
    let mut guards = Vec::with_capacity(2 + integrated_marks.len());
    guards.push(AffectedGuard::exactly(0, 1));
    if let Some(index) = remote_slot {
        guards.push(AffectedGuard::exactly(index, 1));
    }
    guards.extend(
        integrated_marks
            .iter()
            .enumerate()
            .map(|(index, _)| AffectedGuard::exactly(marks_start + index, 1)),
    );

    // 远端已知 VV 的 CAS 失败是**可重试**的：并发写者只是先扩大了一步，重读旧值后
    // 把两边并集再写一次即可。重试期间 head CAS 仍在 index 0：若真的有人推进了 head，
    // 依旧按 HeadConflict 返回，不会把「基准过期」误当成 meta 冲突。
    const REMOTE_KNOWN_CAS_ATTEMPTS: usize = 4;
    let mut attempt = 0usize;
    loop {
        attempt += 1;
        match vault.exec_transaction(&statements, &guards) {
            Ok(_) => {
                return Ok(HeadState {
                    doc_id: doc_id.to_owned(),
                    head_seq,
                    head_frontier: last.end_frontier.clone(),
                    head_vv: last.end_vv.clone(),
                });
            }
            // 第 0 条守卫 = head CAS。
            Err(StorageError::GuardFailed { statement: 0, .. }) => {
                let actual = load_head(vault, doc_id)
                    .map(|head| head.head_seq)
                    .unwrap_or(0);
                return Err(StorageError::HeadConflict {
                    doc_id: doc_id.to_owned(),
                    expected: expected_head_seq,
                    actual,
                });
            }
            // 远端已知 VV 的值 CAS 不命中：指定下标只有它这一条守卫。
            Err(StorageError::GuardFailed { statement, .. }) if remote_slot == Some(statement) => {
                let Some(added) = remote_added.as_ref() else {
                    return Err(StorageError::Corrupt {
                        context: format!("文档 {doc_id} 的远端已知 VV CAS 守卫缺少重算依据"),
                    });
                };
                if attempt >= REMOTE_KNOWN_CAS_ATTEMPTS {
                    return Err(StorageError::SyncConflict {
                        context: format!(
                            "文档 {doc_id} 的远端已知 VV 值 CAS 连续 {attempt} 次被并发写者改写；\
                             整笔 A 事务（update+head+meta）已回滚，可重试"
                        ),
                    });
                }
                let slot = remote_slot.expect("守卫命中说明 remote_slot 存在");
                statements[slot] = cas_statement_or_corrupt(
                    crate::remote_known::merge_statement(vault, doc_id, added)?,
                    doc_id,
                )?;
            }
            // 标记失败：update 与所有标记一起回滚；调用方必须保持 inbox verified 原样。
            Err(StorageError::GuardFailed { statement, .. }) if statement >= marks_start => {
                return Err(StorageError::SyncConflict {
                    context: format!(
                        "inbox 标记 integrated 失败（第 {} 个标记行不存在或当前不是 verified）；\
                         整笔 A 事务（update+标记）已回滚",
                        statement - marks_start
                    ),
                });
            }
            Err(error) => return Err(error),
        }
    }
}

/// [`crate::remote_known::merge_statement`] 在 `remote_added` 非空时必然返回语句；
/// 这里把它转成带文档上下文的损坏错误，避免业务路径 `unwrap`。
fn cas_statement_or_corrupt(
    statement: Option<Statement>,
    doc_id: &str,
) -> Result<Statement, StorageError> {
    statement.ok_or_else(|| StorageError::Corrupt {
        context: format!("文档 {doc_id} 的远端已知 VV 增量非空，但没有生成 CAS 语句"),
    })
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use loro::VersionVector;

    use super::*;
    use crate::body_doc::BodyDoc;
    use crate::remote_known;

    /// 临时 vault：目录建在 `target/tmp` 下（AGENTS §6：不用 `%TEMP%`）。
    ///
    /// lib 单元测试拿不到 `CARGO_TARGET_TMPDIR`（只有集成测试有），所以从
    /// `CARGO_MANIFEST_DIR` 拼出同一个位置。
    fn fresh_vault(prefix: &str) -> (tempfile::TempDir, Vault) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("store-origin-{prefix}"));
        std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录");
        let dir = tempfile::Builder::new()
            .prefix("vault-")
            .tempdir_in(&root)
            .expect("创建临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
        (dir, vault)
    }

    fn scalar_i64(vault: &Vault, sql: &str) -> i64 {
        let rows = vault.query(sql, &[]).expect("查询");
        match rows.first().and_then(|row| row.first()) {
            Some(Value::Integer(value)) => *value,
            other => panic!("期望整数结果，实际 {other:?}"),
        }
    }

    fn local_update(doc: &BodyDoc, base: &VersionVector) -> PendingUpdate {
        doc.pending_update(base).expect("导出增量")
    }

    /// `OriginMarks::AllLocal`（显式类型）不扩大远端已知 VV，也不产生 outbox。
    #[test]
    fn all_local_origin_does_not_extend_remote_known() {
        let (_dir, vault) = fresh_vault("all-local");
        create_body_doc(&vault, "doc-local").expect("loro_docs 行");
        let doc = BodyDoc::create("doc-local", 0xA1).expect("创建 BodyDoc");
        doc.insert_scalars(0, "local").expect("本地编辑");
        let update = local_update(&doc, &VersionVector::default());

        let head = append_updates_inner(
            &vault,
            "doc-local",
            0,
            &[UpdateRecord::new("b0", update)],
            &[],
            &[],
            OriginMarks::AllLocal,
        )
        .expect("A 事务");

        assert_eq!(head.head_seq, 1);
        assert!(
            remote_known::load(&vault, "doc-local")
                .expect("读远端已知")
                .is_none(),
            "全部本地的批次绝不能创建/扩大远端已知 VV"
        );
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM sync_outbox"),
            0,
            "slots 空 = 本批没有任何出站意图"
        );
    }

    /// `PerRecord` 混排：只有 remote 条目的 `after - running` 记入远端已知 VV。
    #[test]
    fn per_record_origin_only_extends_for_remote_records() {
        let (_dir, vault) = fresh_vault("mixed");
        create_body_doc(&vault, "doc-mixed").expect("loro_docs 行");

        let doc = BodyDoc::create("doc-mixed", 0xA1).expect("创建 BodyDoc");
        doc.insert_scalars(0, "local").expect("本地编辑");
        let local = local_update(&doc, &VersionVector::default());
        let remote_doc = BodyDoc::create("doc-mixed", 0xB2).expect("创建 BodyDoc");
        remote_doc.insert_scalars(0, "remote").expect("远端编辑");
        doc.import(&remote_doc.export_all_updates().expect("远端 update"))
            .expect("本地副本导入远端");
        let local_end = VersionVector::decode(&local.end_vv).expect("本地 VV");
        let remote = local_update(&doc, &local_end);
        let remote_end = VersionVector::decode(&remote.end_vv).expect("远端 VV");
        let expected = remote_known::added_since(&remote_end, &local_end);

        let head = append_updates_inner(
            &vault,
            "doc-mixed",
            0,
            &[
                UpdateRecord::new("b0", local),
                UpdateRecord::new("b1", remote),
            ],
            &[],
            &[],
            OriginMarks::PerRecord(&[false, true]),
        )
        .expect("A 事务");

        assert_eq!(head.head_seq, 2, "两条记录都必须落库");
        let known = remote_known::load(&vault, "doc-mixed")
            .expect("读远端已知")
            .expect("有 remote 条目就必须写 meta");
        assert_eq!(
            known, expected,
            "远端条目带来的 ops 必须记入，且只记入远端那一小段"
        );
        assert_eq!(
            known.get(&0xA1).copied().unwrap_or(0),
            0,
            "本地条目（哪怕同批先落库）绝不能记入远端已知 VV"
        );
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM sync_outbox"),
            0,
            "没有传 slots 就不得写出站意图"
        );
    }

    /// 空 `PerRecord` 不再表示「全部本地」：长度不匹配直接拒绝，且不写任何行。
    #[test]
    fn per_record_origin_length_mismatch_is_rejected() {
        let (_dir, vault) = fresh_vault("mismatch");
        create_body_doc(&vault, "doc-mismatch").expect("loro_docs 行");
        let doc = BodyDoc::create("doc-mismatch", 0xA1).expect("创建 BodyDoc");
        doc.insert_scalars(0, "x").expect("编辑");
        let update = local_update(&doc, &VersionVector::default());
        let record = UpdateRecord::new("b0", update);

        let error = append_updates_inner(
            &vault,
            "doc-mismatch",
            0,
            std::slice::from_ref(&record),
            &[],
            &[],
            OriginMarks::PerRecord(&[]),
        )
        .expect_err("空 PerRecord 配非空 records 必须报错");
        assert!(
            matches!(error, StorageError::Corrupt { .. }),
            "必须是显式损坏/参数错误，实际 {error:?}"
        );
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM loro_updates"),
            0,
            "参数错误不得留下半写"
        );

        let error = append_updates_inner(
            &vault,
            "doc-mismatch",
            0,
            &[record],
            &[],
            &[],
            OriginMarks::PerRecord(&[true, false]),
        )
        .expect_err("标记多于 records 也必须报错");
        assert!(matches!(error, StorageError::Corrupt { .. }));
    }

    /// 值 CAS 不命中（并发写者改了 meta）→ 有界重试后返回明确 SyncConflict，
    /// 整笔 A 事务回滚：head/update/outbox/meta 都不留半写，也绝不写入更小值。
    ///
    /// 注入方式：`loro_updates` 的 BEFORE INSERT 触发器在**同一事务里**把 meta 改成
    /// 别的值（模拟「读旧值之后、CAS 之前的并发提交」）；每次重试都会再次命中，
    /// 因此有界重试必然耗尽。
    #[test]
    fn stale_remote_known_cas_fails_closed_and_rolls_back() {
        let (_dir, vault) = fresh_vault("cas");
        create_body_doc(&vault, "doc-cas").expect("loro_docs 行");
        let remote_doc = BodyDoc::create("doc-cas", 0xB2).expect("创建 BodyDoc");
        remote_doc.insert_scalars(0, "remote").expect("远端编辑");
        let update = local_update(&remote_doc, &VersionVector::default());
        vault
            .exec(
                "CREATE TRIGGER remote_known_race BEFORE INSERT ON loro_updates BEGIN \
                   INSERT OR REPLACE INTO meta(key,value) \
                   VALUES('sync.remote_known_vv.doc-cas', X'DEADBEEF'); \
                 END;",
                &[],
            )
            .expect("安装竞态触发器");

        let error = append_updates_inner(
            &vault,
            "doc-cas",
            0,
            &[UpdateRecord::new("b0", update)],
            &[],
            &[],
            OriginMarks::PerRecord(&[true]),
        )
        .expect_err("CAS 必须失败");

        match error {
            StorageError::SyncConflict { context } => {
                assert!(
                    context.contains("远端已知 VV"),
                    "错误必须点明远端已知 VV CAS：{context}"
                );
                assert!(context.contains("可重试"), "错误必须说明可重试：{context}");
            }
            other => panic!("期望 SyncConflict，实际 {other:?}"),
        }
        assert_eq!(load_head(&vault, "doc-cas").expect("head").head_seq, 0);
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM loro_updates"),
            0,
            "整笔回滚：update 不得落库"
        );
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM projection_outbox"),
            0
        );
        assert_eq!(
            scalar_i64(
                &vault,
                "SELECT count(*) FROM meta WHERE key='sync.remote_known_vv.doc-cas'"
            ),
            0,
            "触发器写入随整笔回滚，失败后不残留"
        );
        assert!(
            remote_known::load(&vault, "doc-cas")
                .expect("读远端已知")
                .is_none(),
            "失败路径绝不能写入更小/错误的值"
        );
    }
}
