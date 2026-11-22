//! S7 同步桥：出站意图 → 加密 parts → 推送/ACK；入站 verified inbox → 本地 seq → integrate。
//!
//! 本模块只碰 SQLite；不做 Argon、AEAD、HTTP、Loro 解码。ADR 0006 §5 的
//! 关键约束：
//!
//! - A 事务只写「出站意图」，worker 在事务外加密后 CAS 落 parts；
//! - 同一 message/epoch 的既有 parts 必须与再次提交的字节完全一致，否则冲突；
//! - ACK 用 `(message_id, epoch, object_digest)` CAS，旧 ACK 不能删除新行；
//! - 入站对象先 durable 写 inbox（依赖未齐也保存），再由 sync 层决定何时可 integrate；
//! - `allocate_local_seq_for_import` 只返回 `head_seq+1`，**绝不把 sender_seq 写进 loro_updates.seq`。

use std::collections::BTreeMap;

use rusqlite::types::Value;

use crate::actor::{AffectedGuard, Statement};
use crate::error::StorageError;
use crate::store;
use crate::Vault;

/// `meta` 键：已 durable 连续接收前缀（BLOB BE64）。
pub const META_SYNC_RECEIVED_SEQ: &str = "sync.received_seq";
/// `meta` 键：已 integrate 连续前缀（BLOB BE64）。
pub const META_SYNC_INTEGRATED_SEQ: &str = "sync.integrated_seq";
/// `meta` 键：当前 relay log incarnation（BLOB 16B）。
pub const META_SYNC_LOG_ID: &str = "sync.log_id";
/// `meta` 键：已确认高水位（BLOB BE64）。
pub const META_SYNC_CONFIRMED_HIGHWATER: &str = "sync.confirmed_highwater";

/// 出站意图（A 事务内与 update 一起落库）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncIntent {
    /// 稳定 message id（UUID 文本；parts 表以 UTF-8 字节存同一值）。
    pub message_id: String,
    /// 逻辑 batch，通常与 `UpdateRecord.batch_id` 相同。
    pub batch_id: String,
    /// 加密 epoch。
    pub key_epoch: u64,
    /// 对象名（32B；asset 为 HMAC 对象名，其他为随机 32B）。
    pub object_id: [u8; 32],
}

/// outbox 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncOutboxState {
    Pending,
    Encrypted,
    Sent,
    Acked,
}

impl SyncOutboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Encrypted => "encrypted",
            Self::Sent => "sent",
            Self::Acked => "acked",
        }
    }

    fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "pending" => Ok(Self::Pending),
            "encrypted" => Ok(Self::Encrypted),
            "sent" => Ok(Self::Sent),
            "acked" => Ok(Self::Acked),
            other => Err(StorageError::Corrupt {
                context: format!("未知 sync_outbox.state {other:?}"),
            }),
        }
    }
}

/// 一条待处理出站消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutboxRow {
    pub message_id: String,
    pub batch_id: String,
    pub doc_id: String,
    pub seq: u64,
    pub key_epoch: u64,
    pub object_id: Option<[u8; 32]>,
    pub state: SyncOutboxState,
    pub cipher_digest: Option<[u8; 32]>,
}

/// 一个已加密的 MNE1 part。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncEncryptedPart {
    /// 0-based 连续下标（0..part_count-1）。
    pub part_no: u32,
    /// exact MNE1 envelope bytes（header+ciphertext）。
    pub envelope: Vec<u8>,
    /// 该 part envelope 的 SHA256（sync 计算，storage 不验哈希）。
    pub digest: [u8; 32],
}

/// 从数据库读出的 part。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutboxPart {
    pub message_id: String,
    pub epoch: u64,
    pub part_no: u32,
    pub envelope: Vec<u8>,
    pub digest: [u8; 32],
}

/// inbox 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncInboxState {
    Received,
    Verified,
    Integrated,
    Quarantine,
}

impl SyncInboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Verified => "verified",
            Self::Integrated => "integrated",
            Self::Quarantine => "quarantine",
        }
    }

    fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "received" => Ok(Self::Received),
            "verified" => Ok(Self::Verified),
            "integrated" => Ok(Self::Integrated),
            "quarantine" => Ok(Self::Quarantine),
            other => Err(StorageError::Corrupt {
                context: format!("未知 sync_inbox.state {other:?}"),
            }),
        }
    }
}

/// 一条入站对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncInboxRow {
    pub log_id: [u8; 16],
    pub relay_seq: u64,
    pub object_id: [u8; 32],
    pub epoch: u64,
    pub state: SyncInboxState,
    pub verified_payload: Option<Vec<u8>>,
}

/// 本地同步水位（`meta` 约定）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncCursorState {
    pub received_seq: u64,
    pub integrated_seq: u64,
    pub log_id: Option<[u8; 16]>,
    pub confirmed_highwater: Option<u64>,
}

/// 在 A 事务内追加 update 的同时写出站意图。
///
/// `intents.len()` 必须与 `records.len()` 相同（一 update 一意图）。
pub fn append_updates_with_sync_intents(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[store::UpdateRecord],
    intents: &[SyncIntent],
) -> Result<store::HeadState, StorageError> {
    if intents.len() != records.len() {
        return Err(StorageError::Corrupt {
            context: format!(
                "append_updates_with_sync_intents: records={} intents={} 必须一对一",
                records.len(),
                intents.len()
            ),
        });
    }
    store::append_updates_with_sync_intents(vault, doc_id, expected_head_seq, records, intents)
}

/// 单独记录一条出站意图（调用方已确认 update 落库）。
///
/// 若希望意图与 update 同事务，请用 [`append_updates_with_sync_intents`]。
pub fn enqueue_sync_intent(
    vault: &Vault,
    message_id: &str,
    batch_id: &str,
    doc_id: &str,
    seq: u64,
    key_epoch: u64,
    object_id: &[u8; 32],
) -> Result<(), StorageError> {
    validate_intent(message_id, batch_id, seq, key_epoch)?;
    let affected = vault.exec(
        "INSERT INTO sync_outbox(message_id,batch_id,doc_id,seq,key_epoch,object_id,state,ciphertext) \
         VALUES(?1,?2,?3,?4,?5,?6,'pending',NULL)",
        &[
            Value::Text(message_id.to_owned()),
            Value::Text(batch_id.to_owned()),
            Value::Text(doc_id.to_owned()),
            Value::Integer(i64_of(seq)?),
            Value::Integer(i64_of(key_epoch)?),
            Value::Blob(object_id.to_vec()),
        ],
    )?;
    if affected != 1 {
        return Err(StorageError::SyncConflict {
            context: format!("message {message_id} 已存在"),
        });
    }
    Ok(())
}

/// 读指定状态的 outbox 行。
pub fn outbox_rows(
    vault: &Vault,
    state: SyncOutboxState,
    limit: usize,
) -> Result<Vec<SyncOutboxRow>, StorageError> {
    let rows = vault.query(
        "SELECT message_id,batch_id,doc_id,seq,key_epoch,object_id,state,cipher_digest \
         FROM sync_outbox WHERE state=?1 ORDER BY doc_id,seq,message_id LIMIT ?2",
        &[
            Value::Text(state.as_str().to_owned()),
            Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)),
        ],
    )?;
    rows.iter().map(row_to_outbox).collect()
}

/// 待加密（state=pending）的意图。
pub fn pending_encryption(vault: &Vault, limit: usize) -> Result<Vec<SyncOutboxRow>, StorageError> {
    outbox_rows(vault, SyncOutboxState::Pending, limit)
}

/// 待发送（state=encrypted）的批次。
pub fn pending_send(vault: &Vault, limit: usize) -> Result<Vec<SyncOutboxRow>, StorageError> {
    outbox_rows(vault, SyncOutboxState::Encrypted, limit)
}

/// 按 message_id 读一行。
pub fn outbox_row(vault: &Vault, message_id: &str) -> Result<Option<SyncOutboxRow>, StorageError> {
    let rows = vault.query(
        "SELECT message_id,batch_id,doc_id,seq,key_epoch,object_id,state,cipher_digest \
         FROM sync_outbox WHERE message_id=?1",
        &[Value::Text(message_id.to_owned())],
    )?;
    rows.first().map(row_to_outbox).transpose()
}

/// 读某 message/epoch 的全部 parts，按 part_no 升序。
pub fn outbox_parts(
    vault: &Vault,
    message_id: &str,
    epoch: u64,
) -> Result<Vec<SyncOutboxPart>, StorageError> {
    let rows = vault.query(
        "SELECT message_id,epoch,part_no,envelope,digest FROM sync_outbox_parts \
         WHERE message_id=?1 AND epoch=?2 ORDER BY part_no",
        &[
            Value::Blob(message_id.as_bytes().to_vec()),
            Value::Integer(i64_of(epoch)?),
        ],
    )?;
    rows.iter()
        .map(|row| {
            Ok(SyncOutboxPart {
                message_id: text_from_blob(row, 0, "sync_outbox_parts.message_id")?,
                epoch: u64_of(row, 1, "sync_outbox_parts.epoch")?,
                part_no: u32_of(row, 2, "sync_outbox_parts.part_no")?,
                envelope: blob_of(row, 3, "sync_outbox_parts.envelope")?,
                digest: array32(row, 4, "sync_outbox_parts.digest")?,
            })
        })
        .collect()
}

/// CAS 写入一批加密 parts，并把 outbox 置为 encrypted。
///
/// 同一 `(message_id, epoch)` 已有 parts 时，必须与新提交的整批字节完全一致；
/// 否则返回 [`StorageError::SyncConflict`]（不会覆盖旧 parts）。
pub fn store_encrypted_parts(
    vault: &Vault,
    message_id: &str,
    epoch: u64,
    object_digest: &[u8; 32],
    parts: &[SyncEncryptedPart],
) -> Result<(), StorageError> {
    if message_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "message_id 不能为空".to_owned(),
        });
    }
    if parts.is_empty() {
        return Err(StorageError::Corrupt {
            context: format!("message {message_id} 没有 parts"),
        });
    }
    for (index, part) in parts.iter().enumerate() {
        if part.part_no != index as u32 {
            return Err(StorageError::Corrupt {
                context: format!(
                    "message {message_id} parts 必须从 0 连续：第 {index} 项是 {}",
                    part.part_no
                ),
            });
        }
        if part.envelope.is_empty() {
            return Err(StorageError::Corrupt {
                context: format!("message {message_id} 第 {index} 片信封为空"),
            });
        }
    }

    let outbox = outbox_row(vault, message_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("sync_outbox 缺少 message {message_id}"),
    })?;
    if outbox.key_epoch != epoch {
        return Err(StorageError::SyncConflict {
            context: format!(
                "message {message_id} epoch={epoch}，outbox 为 {}",
                outbox.key_epoch
            ),
        });
    }
    if outbox.state == SyncOutboxState::Acked {
        return Err(StorageError::SyncConflict {
            context: format!("message {message_id} 已 ACK，拒绝重加密"),
        });
    }

    let existing = outbox_parts(vault, message_id, epoch)?;
    let mut missing = Vec::new();
    if !existing.is_empty() {
        if let Some(old_digest) = outbox.cipher_digest {
            if old_digest != *object_digest {
                return Err(StorageError::SyncConflict {
                    context: format!("message {message_id}/{epoch} 整体 digest 不一致"),
                });
            }
        }
        if existing.len() != parts.len() {
            return Err(StorageError::SyncConflict {
                context: format!(
                    "message {message_id}/{epoch} 已有 {} parts，本次 {} parts",
                    existing.len(),
                    parts.len()
                ),
            });
        }
        for (old, new) in existing.iter().zip(parts) {
            if old.part_no != new.part_no
                || old.envelope != new.envelope
                || old.digest != new.digest
            {
                return Err(StorageError::SyncConflict {
                    context: format!(
                        "message {message_id}/{epoch} 第 {} 片字节不一致",
                        new.part_no
                    ),
                });
            }
        }
    } else {
        missing.extend(parts.iter());
    }

    let mut statements = Vec::with_capacity(1 + missing.len());
    statements.push(Statement::new(
        "UPDATE sync_outbox SET state='encrypted', cipher_digest=?2 \
         WHERE message_id=?1 AND key_epoch=?3 AND state IN('pending','encrypted','sent')",
        vec![
            Value::Text(message_id.to_owned()),
            Value::Blob(object_digest.to_vec()),
            Value::Integer(i64_of(epoch)?),
        ],
    ));
    for part in missing {
        statements.push(Statement::new(
            "INSERT INTO sync_outbox_parts(message_id,epoch,part_no,envelope,digest) \
             VALUES(?1,?2,?3,?4,?5)",
            vec![
                Value::Blob(message_id.as_bytes().to_vec()),
                Value::Integer(i64_of(epoch)?),
                Value::Integer(i64::from(part.part_no)),
                Value::Blob(part.envelope.clone()),
                Value::Blob(part.digest.to_vec()),
            ],
        ));
    }

    match vault.exec_transaction(&statements, &[AffectedGuard::exactly(0, 1)]) {
        Ok(_) => Ok(()),
        Err(StorageError::GuardFailed { .. }) => Err(StorageError::SyncConflict {
            context: format!("message {message_id}/{epoch} 状态已变化，未写入 parts"),
        }),
        Err(error) => Err(error),
    }
}

/// 把已加密批次标记为已发送（CAS digest）。
pub fn mark_sent(
    vault: &Vault,
    message_id: &str,
    epoch: u64,
    object_digest: &[u8; 32],
) -> Result<bool, StorageError> {
    let affected = vault.exec(
        "UPDATE sync_outbox SET state='sent' \
         WHERE message_id=?1 AND key_epoch=?2 AND cipher_digest=?3 \
           AND state IN('encrypted','sent')",
        &[
            Value::Text(message_id.to_owned()),
            Value::Integer(i64_of(epoch)?),
            Value::Blob(object_digest.to_vec()),
        ],
    )?;
    Ok(affected == 1)
}

/// durable ACK：CAS 成功后删除该 outbox 行及其 parts。
///
/// 旧 ACK 的 digest 对不上时只返回 `false`，**不会**删除同 doc/seq 的新 message。
pub fn mark_acked(
    vault: &Vault,
    message_id: &str,
    epoch: u64,
    object_digest: &[u8; 32],
) -> Result<bool, StorageError> {
    let statements = [
        Statement::new(
            "DELETE FROM sync_outbox_parts WHERE message_id=?1 AND epoch=?2",
            vec![
                Value::Blob(message_id.as_bytes().to_vec()),
                Value::Integer(i64_of(epoch)?),
            ],
        ),
        Statement::new(
            "DELETE FROM sync_outbox \
             WHERE message_id=?1 AND key_epoch=?2 AND cipher_digest=?3 \
               AND state IN('encrypted','sent')",
            vec![
                Value::Text(message_id.to_owned()),
                Value::Integer(i64_of(epoch)?),
                Value::Blob(object_digest.to_vec()),
            ],
        ),
    ];
    match vault.exec_transaction(&statements, &[AffectedGuard::exactly(1, 1)]) {
        Ok(_) => Ok(true),
        Err(StorageError::GuardFailed { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

/// `doc_id/seq` 是否仍被未 ACK 的 outbox 引用。
///
/// 只有返回 `false` 时才允许回收 `loro_updates`（经验证 snapshot VV 覆盖
/// 属于 sync 层的独立条件，本函数不解析 Loro）。
pub fn update_referenced_by_outbox(
    vault: &Vault,
    doc_id: &str,
    seq: u64,
) -> Result<bool, StorageError> {
    let rows = vault.query(
        "SELECT count(*) FROM sync_outbox WHERE doc_id=?1 AND seq=?2",
        &[Value::Text(doc_id.to_owned()), Value::Integer(i64_of(seq)?)],
    )?;
    Ok(rows
        .first()
        .and_then(|row| row.first())
        .and_then(|value| match value {
            Value::Integer(value) => Some(*value > 0),
            _ => None,
        })
        .unwrap_or(false))
}

/// `doc_id/seq` 的 update 是否已无未 ACK outbox 引用、可回收。
///
/// 这只是**必要**条件；调用方还必须确认“经验证 snapshot VV 已覆盖该 seq”。
pub fn can_recycle_update(vault: &Vault, doc_id: &str, seq: u64) -> Result<bool, StorageError> {
    Ok(!update_referenced_by_outbox(vault, doc_id, seq)?)
}

fn validate_intent(
    message_id: &str,
    batch_id: &str,
    seq: u64,
    key_epoch: u64,
) -> Result<(), StorageError> {
    if message_id.is_empty() || batch_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "sync intent message_id/batch_id 不能为空".to_owned(),
        });
    }
    if seq == 0 || key_epoch == 0 {
        return Err(StorageError::Corrupt {
            context: "sync intent seq/key_epoch 必须 >0".to_owned(),
        });
    }
    Ok(())
}

fn row_to_outbox(row: &crate::Row) -> Result<SyncOutboxRow, StorageError> {
    let object_id = row
        .get(5)
        .and_then(blob_value)
        .map(|bytes| array32_bytes(bytes, "sync_outbox.object_id"))
        .transpose()?;
    let cipher_digest = row
        .get(7)
        .and_then(blob_value)
        .map(|bytes| array32_bytes(bytes, "sync_outbox.cipher_digest"))
        .transpose()?;
    Ok(SyncOutboxRow {
        message_id: text_of(row, 0, "sync_outbox.message_id")?,
        batch_id: text_of(row, 1, "sync_outbox.batch_id")?,
        doc_id: text_of(row, 2, "sync_outbox.doc_id")?,
        seq: u64_of(row, 3, "sync_outbox.seq")?,
        key_epoch: u64_of(row, 4, "sync_outbox.key_epoch")?,
        object_id,
        state: SyncOutboxState::parse(&text_of(row, 6, "sync_outbox.state")?)?,
        cipher_digest,
    })
}

/// 入站：durable 保存已验签/已解密对象（依赖是否齐由 sync 层决定）。
pub fn store_inbox_verified(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seq: u64,
    object_id: &[u8; 32],
    epoch: u64,
    verified_payload: &[u8],
) -> Result<(), StorageError> {
    if relay_seq == 0 || epoch == 0 {
        return Err(StorageError::Corrupt {
            context: "inbox relay_seq/epoch 必须 >0".to_owned(),
        });
    }
    ensure_log_id(vault, log_id)?;

    let rows = vault.query(
        "SELECT object_id,epoch,state,verified_payload FROM sync_inbox \
         WHERE log_id=?1 AND relay_seq=?2",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(relay_seq)?),
        ],
    )?;
    if let Some(row) = rows.first() {
        let old_object = array32(row, 0, "sync_inbox.object_id")?;
        let old_epoch = u64_of(row, 1, "sync_inbox.epoch")?;
        let old_payload = row.get(3).and_then(blob_value).unwrap_or_default();
        if old_object != *object_id || old_epoch != epoch || old_payload != verified_payload {
            return Err(StorageError::SyncConflict {
                context: format!("inbox {log_id:?}/{relay_seq} 字节不一致"),
            });
        }
        advance_received_prefix(vault)?;
        return Ok(());
    }

    let changed = vault.exec(
        "INSERT INTO sync_inbox(log_id,relay_seq,object_id,epoch,state,verified_payload) \
         VALUES(?1,?2,?3,?4,'verified',?5)",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(relay_seq)?),
            Value::Blob(object_id.to_vec()),
            Value::Integer(i64_of(epoch)?),
            Value::Blob(verified_payload.to_vec()),
        ],
    )?;
    if changed != 1 {
        return Err(StorageError::SyncConflict {
            context: format!("inbox {log_id:?}/{relay_seq} 写入冲突"),
        });
    }
    advance_received_prefix(vault)?;
    Ok(())
}

/// 读当前已验证、且落在 durable 连续接收前缀内的 inbox 行。
///
/// 依赖完整性由 sync 层判断（本 crate 不解析 Loro VV）；这里的 "ready"
/// 指 cryptographically verified + durable + `relay_seq <= received_seq`，
/// 不在连续前缀内的行保持 durable，等前面的缺口补齐。排序为 `(log_id, relay_seq)`。
pub fn take_inbox_ready(vault: &Vault, limit: usize) -> Result<Vec<SyncInboxRow>, StorageError> {
    let received = read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0);
    let rows = vault.query(
        "SELECT log_id,relay_seq,object_id,epoch,state,verified_payload \
         FROM sync_inbox WHERE state='verified' AND relay_seq<=?1 \
         ORDER BY log_id,relay_seq LIMIT ?2",
        &[
            Value::Integer(i64_of(received)?),
            Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)),
        ],
    )?;
    rows.iter().map(row_to_inbox).collect()
}

/// 带调用方依赖判断的 ready 选择器；不满足的项保持 durable verified。
pub fn take_inbox_ready_where(
    vault: &Vault,
    limit: usize,
    mut is_ready: impl FnMut(&SyncInboxRow) -> bool,
) -> Result<Vec<SyncInboxRow>, StorageError> {
    let rows = take_inbox_ready(vault, limit.saturating_mul(16).max(limit))?;
    Ok(rows
        .into_iter()
        .filter(|row| is_ready(row))
        .take(limit)
        .collect())
}

/// 为 remote import 分配本地 `loro_updates.seq = head_seq + 1`。
///
/// **只分配，不写库**；调用方必须在同一 A 事务里把该 seq 用在
/// `loro_updates.seq`，禁止直接使用 sender_seq。
pub fn allocate_local_seq_for_import(vault: &Vault, doc_id: &str) -> Result<u64, StorageError> {
    let head = store::load_head(vault, doc_id)?;
    head.head_seq
        .checked_add(1)
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("{doc_id} 本地 seq 溢出"),
        })
}

/// 把一个 verified inbox 行标为 integrated，并推进连续 integrated 前缀。
pub fn mark_integrated(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seq: u64,
) -> Result<(), StorageError> {
    let changed = vault.exec(
        "UPDATE sync_inbox SET state='integrated' \
         WHERE log_id=?1 AND relay_seq=?2 AND state='verified'",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(relay_seq)?),
        ],
    )?;
    if changed == 0 {
        let rows = vault.query(
            "SELECT state FROM sync_inbox WHERE log_id=?1 AND relay_seq=?2",
            &[
                Value::Blob(log_id.to_vec()),
                Value::Integer(i64_of(relay_seq)?),
            ],
        )?;
        let Some(row) = rows.first() else {
            return Err(StorageError::Corrupt {
                context: format!("inbox {log_id:?}/{relay_seq} 不存在"),
            });
        };
        let state = text_of(row, 0, "sync_inbox.state")?;
        if state != SyncInboxState::Integrated.as_str() {
            return Err(StorageError::SyncConflict {
                context: format!("inbox {log_id:?}/{relay_seq} 当前 state={state}，不能 integrate"),
            });
        }
    }
    advance_integrated_prefix(vault)?;
    Ok(())
}

/// 把 inbox 行标为 quarantine（完整性失败/依赖永久缺失）。
pub fn mark_inbox_quarantine(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seq: u64,
) -> Result<(), StorageError> {
    let changed = vault.exec(
        "UPDATE sync_inbox SET state='quarantine' \
         WHERE log_id=?1 AND relay_seq=?2 AND state IN('received','verified')",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(relay_seq)?),
        ],
    )?;
    if changed == 0 {
        return Err(StorageError::SyncConflict {
            context: format!("inbox {log_id:?}/{relay_seq} 不能进入 quarantine"),
        });
    }
    Ok(())
}

fn row_to_inbox(row: &crate::Row) -> Result<SyncInboxRow, StorageError> {
    Ok(SyncInboxRow {
        log_id: array16(row, 0, "sync_inbox.log_id")?,
        relay_seq: u64_of(row, 1, "sync_inbox.relay_seq")?,
        object_id: array32(row, 2, "sync_inbox.object_id")?,
        epoch: u64_of(row, 3, "sync_inbox.epoch")?,
        state: SyncInboxState::parse(&text_of(row, 4, "sync_inbox.state")?)?,
        verified_payload: row.get(5).and_then(blob_value),
    })
}

/// 读本地同步水位。
pub fn sync_cursor_state(vault: &Vault) -> Result<SyncCursorState, StorageError> {
    Ok(SyncCursorState {
        received_seq: read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0),
        integrated_seq: read_meta_u64(vault, META_SYNC_INTEGRATED_SEQ)?.unwrap_or(0),
        log_id: read_meta(vault, META_SYNC_LOG_ID)?
            .map(|bytes| array16_bytes(bytes, "sync.log_id"))
            .transpose()?,
        confirmed_highwater: read_meta_u64(vault, META_SYNC_CONFIRMED_HIGHWATER)?,
    })
}

/// 写入 `sync.log_id`；已有不同值则拒绝（restore 必须显式迁移）。
pub fn set_sync_log_id(vault: &Vault, log_id: &[u8; 16]) -> Result<(), StorageError> {
    ensure_log_id(vault, log_id)
}

/// 写 confirmed highwater（BE64 BLOB）。
pub fn set_confirmed_highwater(vault: &Vault, value: u64) -> Result<(), StorageError> {
    write_meta(vault, META_SYNC_CONFIRMED_HIGHWATER, &value.to_be_bytes())
}

fn ensure_log_id(vault: &Vault, log_id: &[u8; 16]) -> Result<(), StorageError> {
    match read_meta(vault, META_SYNC_LOG_ID)? {
        Some(existing) => {
            if existing.as_slice() != log_id {
                return Err(StorageError::SyncLogMismatch {
                    expected: existing,
                    found: log_id.to_vec(),
                });
            }
        }
        None => write_meta(vault, META_SYNC_LOG_ID, log_id)?,
    }
    Ok(())
}

fn advance_received_prefix(vault: &Vault) -> Result<(), StorageError> {
    let Some(log_id) = read_meta(vault, META_SYNC_LOG_ID)? else {
        return Ok(());
    };
    let mut current = read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0);
    loop {
        let next = current
            .checked_add(1)
            .ok_or_else(|| StorageError::Corrupt {
                context: "received_seq 溢出".to_owned(),
            })?;
        let rows = vault.query(
            "SELECT state FROM sync_inbox WHERE log_id=?1 AND relay_seq=?2 \
             AND state IN('verified','integrated')",
            &[Value::Blob(log_id.clone()), Value::Integer(i64_of(next)?)],
        )?;
        if rows.is_empty() {
            break;
        }
        current = next;
        write_meta(vault, META_SYNC_RECEIVED_SEQ, &current.to_be_bytes())?;
    }
    Ok(())
}

fn advance_integrated_prefix(vault: &Vault) -> Result<(), StorageError> {
    let Some(log_id) = read_meta(vault, META_SYNC_LOG_ID)? else {
        return Ok(());
    };
    let mut current = read_meta_u64(vault, META_SYNC_INTEGRATED_SEQ)?.unwrap_or(0);
    loop {
        let next = current
            .checked_add(1)
            .ok_or_else(|| StorageError::Corrupt {
                context: "integrated_seq 溢出".to_owned(),
            })?;
        let rows = vault.query(
            "SELECT 1 FROM sync_inbox WHERE log_id=?1 AND relay_seq=?2 AND state='integrated'",
            &[Value::Blob(log_id.clone()), Value::Integer(i64_of(next)?)],
        )?;
        if rows.is_empty() {
            break;
        }
        current = next;
        write_meta(vault, META_SYNC_INTEGRATED_SEQ, &current.to_be_bytes())?;
    }
    Ok(())
}

fn read_meta(vault: &Vault, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
    let rows = vault.query(
        "SELECT value FROM meta WHERE key=?1",
        &[Value::Text(key.to_owned())],
    )?;
    Ok(rows
        .first()
        .and_then(|row| row.first())
        .and_then(blob_value))
}

fn read_meta_u64(vault: &Vault, key: &str) -> Result<Option<u64>, StorageError> {
    let Some(bytes) = read_meta(vault, key)? else {
        return Ok(None);
    };
    if bytes.len() != 8 {
        return Err(StorageError::Corrupt {
            context: format!("{key} 应为 BE64 8B，实际 {}B", bytes.len()),
        });
    }
    Ok(Some(u64::from_be_bytes(bytes.try_into().expect("长度8"))))
}

fn write_meta(vault: &Vault, key: &str, value: &[u8]) -> Result<(), StorageError> {
    vault.exec(
        "INSERT INTO meta(key,value) VALUES(?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        &[Value::Text(key.to_owned()), Value::Blob(value.to_vec())],
    )?;
    Ok(())
}

fn text_of(row: &crate::Row, index: usize, field: &str) -> Result<String, StorageError> {
    match row.get(index) {
        Some(Value::Text(value)) => Ok(value.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 类型异常：{other:?}"),
        }),
    }
}

fn text_from_blob(row: &crate::Row, index: usize, field: &str) -> Result<String, StorageError> {
    let bytes = blob_of(row, index, field)?;
    String::from_utf8(bytes).map_err(|_| StorageError::Corrupt {
        context: format!("{field} 不是 UTF-8 message id"),
    })
}

fn blob_of(row: &crate::Row, index: usize, field: &str) -> Result<Vec<u8>, StorageError> {
    row.get(index)
        .and_then(blob_value)
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("{field} 缺失或类型异常"),
        })
}

fn blob_value(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Blob(bytes) => Some(bytes.clone()),
        _ => None,
    }
}

fn array16(row: &crate::Row, index: usize, field: &str) -> Result<[u8; 16], StorageError> {
    array16_bytes(blob_of(row, index, field)?, field)
}

fn array32(row: &crate::Row, index: usize, field: &str) -> Result<[u8; 32], StorageError> {
    array32_bytes(blob_of(row, index, field)?, field)
}

fn array16_bytes(bytes: Vec<u8>, field: &str) -> Result<[u8; 16], StorageError> {
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
            context: format!("{field} 应为 16B，实际 {}B", bytes.len()),
        })
}

fn array32_bytes(bytes: Vec<u8>, field: &str) -> Result<[u8; 32], StorageError> {
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
            context: format!("{field} 应为 32B，实际 {}B", bytes.len()),
        })
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
    let value = u64_of(row, index, field)?;
    u32::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("{field} 超出 u32"),
    })
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER"),
    })
}

/// 出站 digest 的 map 形式；用于按 message 聚合时去重（本模块内部）。
#[allow(dead_code)]
fn group_parts_by_message(parts: Vec<SyncOutboxPart>) -> BTreeMap<String, Vec<SyncOutboxPart>> {
    let mut grouped = BTreeMap::new();
    for part in parts {
        grouped
            .entry(part.message_id.clone())
            .or_insert_with(Vec::new)
            .push(part);
    }
    grouped
}
