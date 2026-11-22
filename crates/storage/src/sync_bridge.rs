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

// 同步 `meta` 键与「连续前缀」的低层实现放在 `sync_meta`（直接跑在 `Connection` 上，
// 这样批量 inbox 的插入与前缀推进能共用一个事务）；这里保持原有的公开路径。
use crate::sync_meta::{meta_bytes_of, skipped_received_key, ConsumedPrefix};
pub use crate::sync_meta::{
    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID,
    META_SYNC_RECEIVED_SEQ, META_SYNC_SKIPPED_RECEIVED_PREFIX,
};

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

/// 本地写入的**同步上下文**：本机 peer + 当前加密 epoch。
///
/// ADR 0009 切片 2b：真实本地写入（正文 session flush、结构提交）在 A 事务里
/// 顺手写 `sync_outbox` 意图。这里只放"派生意图所需的常量"：
///
/// - `peer_id`：来自持久设备身份（`LocalIdentityV1::loro_peer_id()`），**不是**常量 1；
/// - `key_epoch`：Phase 1 恒为 `1`（ADR 0006 genesis 初始代），**禁止**写成 0
///   （0 在 `validate_intent` 里就是非法值，会让整笔 A 事务失败）。
///
/// `None` 表示不产生意图（默认路径 / remote 导入路径），行为与切片 2a 之前逐字节一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyncContext {
    /// 本机 Loro peer id（`0` 非法：Loro 不接受 0 peer）。
    pub peer_id: u64,
    /// 当前加密 epoch（Phase 1 = 1；必须 > 0）。
    pub key_epoch: u64,
}

impl SyncContext {
    /// 构造上下文（调用方保证两个值都 > 0；不做隐式回退）。
    #[must_use]
    pub const fn new(peer_id: u64, key_epoch: u64) -> Self {
        Self { peer_id, key_epoch }
    }

    /// 校验：peer / epoch 都必须 > 0，否则明确报错（不静默替换成常量）。
    pub fn validate(self) -> Result<(), StorageError> {
        if self.peer_id == 0 {
            return Err(StorageError::Corrupt {
                context: "SyncContext.peer_id 必须 >0（0 不是合法 Loro peer）".to_owned(),
            });
        }
        if self.key_epoch == 0 {
            return Err(StorageError::Corrupt {
                context: "SyncContext.key_epoch 必须 >0（Phase 1 = 1，禁止常量 0）".to_owned(),
            });
        }
        Ok(())
    }
}

/// 本地意图 ID 的确定性派生域（版本化；改算法必须换常量）。
///
/// 三个派生都从这里起头：message id / object id 必须**纯函数**地由
/// `(doc_id, seq, peer, epoch)` 决定，重试才不会产生新 ID（ADR 0009）。
pub const SYNC_INTENT_ID_DOMAIN: &[u8] = b"MN/dev-sync-intent/v1\0";

/// 确定性 message id（32 位小写 hex = 16B wire id）。
///
/// **为什么不是可读串**：`crates/sync` 的 `engine::decode_message_id` 只接受
/// 16 字节的 hax/UUID 文本（32 个 hex 字符，允许带 `-`），可读的
/// `doc#seq@epoch` 会让 engine 直接报 `LocalCorrupt`。所以这里取
/// `sha256(domain‖"message"‖doc_id‖NUL‖seq_be‖peer_be‖epoch_be)` 的前 16B。
///
/// 纯函数语义：同一 `(doc_id, seq, peer_id, key_epoch)` 永远得到同一个 id；
/// 不同 seq / epoch 必然不同（hash 输入不同）。
#[must_use]
pub fn intent_message_id(doc_id: &str, seq: u64, ctx: SyncContext) -> String {
    let digest = intent_digest(
        b"message\0",
        &[
            doc_id.as_bytes(),
            &[0],
            &seq.to_be_bytes(),
            &ctx.peer_id.to_be_bytes(),
            &ctx.key_epoch.to_be_bytes(),
        ],
    );
    hex_lower(&digest[..16])
}

/// 确定性 object id（32B）。
///
/// `sha256(domain‖"object"‖message_id‖peer_be‖epoch_be)`：对象名与 message 一一对应，
/// 重试/重打包时同样的 message 得到同样的对象名。
#[must_use]
pub fn intent_object_id(message_id: &str, ctx: SyncContext) -> [u8; 32] {
    intent_digest(
        b"object\0",
        &[
            message_id.as_bytes(),
            &ctx.peer_id.to_be_bytes(),
            &ctx.key_epoch.to_be_bytes(),
        ],
    )
}

/// 为一条**本地** update 派生一条出站意图。
///
/// `seq` 必须是这次 A 事务里该 update 真正落库的 `loro_updates.seq`
/// （= `expected_head_seq + 1 + index`），否则意图与数据对不上。
pub fn local_sync_intent(
    doc_id: &str,
    seq: u64,
    batch_id: &str,
    ctx: SyncContext,
) -> Result<SyncIntent, StorageError> {
    ctx.validate()?;
    if doc_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "sync intent doc_id 不能为空".to_owned(),
        });
    }
    let message_id = intent_message_id(doc_id, seq, ctx);
    validate_intent(&message_id, batch_id, seq, ctx.key_epoch)?;
    Ok(SyncIntent {
        batch_id: batch_id.to_owned(),
        object_id: intent_object_id(&message_id, ctx),
        message_id,
        key_epoch: ctx.key_epoch,
    })
}

/// 同事务写入「一 update 一**可选**意图」：`None` = 该条不产生意图（remote 导入）。
///
/// 一次 flush 的连续前缀可能**混**本地编辑与远端导入（`JournalOrigin`），而
/// remote/import/adopt **绝不能**产生出站意图（否则会把刚收到的数据再推回 relay）。
/// 这里仍是一条 A 事务、按位置一一对应，只是允许某些位置为空。
///
/// `slots` 空 = 本批没有**任何**出站意图；来源标记用 [`store::OriginMarks`]
/// （`AllLocal` / 与 records 等长的 `PerRecord`），**不再用空切片**表示「全部本地」。
pub(crate) fn append_updates_with_intent_slots(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[store::UpdateRecord],
    slots: &[Option<SyncIntent>],
    origin: store::OriginMarks<'_>,
) -> Result<store::HeadState, StorageError> {
    if !slots.is_empty() && slots.len() != records.len() {
        return Err(StorageError::Corrupt {
            context: format!(
                "append_updates_with_intent_slots: records={} slots={} 必须一对一",
                records.len(),
                slots.len()
            ),
        });
    }
    store::append_updates_with_intent_slots(
        vault,
        doc_id,
        expected_head_seq,
        records,
        slots,
        origin,
    )
}

fn intent_digest(tag: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(SYNC_INTENT_ID_DOMAIN);
    hasher.update(tag);
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
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

/// 批量写入的一行**已验签 / 已解密**入站事实（[`store_inbox_verified_batch`] 的输入）。
///
/// 字段与 [`store_inbox_verified`] 的参数一一对应；依赖是否齐仍由 sync 层判断，
/// storage 只负责 durable 落行与推进 `received` 连续前缀。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedInboxRow {
    /// relay 日志 incarnation（同一批必须一致）。
    pub log_id: [u8; 16],
    /// relay 日志序号（必须 > 0；同一批内不得重复）。
    pub relay_seq: u64,
    /// 密文对象名（32B）。
    pub object_id: [u8; 32],
    /// 加密 epoch（必须 > 0）。
    pub epoch: u64,
    /// 已解密并完成载荷校验的 `UpdateV1` 明文（写入 `sync_inbox.verified_payload`）。
    pub verified_payload: Vec<u8>,
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
    let slots: Vec<Option<SyncIntent>> = intents.iter().cloned().map(Some).collect();
    store::append_updates_with_intent_slots(
        vault,
        doc_id,
        expected_head_seq,
        records,
        &slots,
        store::OriginMarks::AllLocal,
    )
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

/// `meta` 键：轮换后必须 repack outbox 的持久屏障（sync 阶段 H 写入 `b"1"`）。
pub const META_SYNC_REPACK_REQUIRED: &str = "sync.repack_required";

/// 旧 outbox 行的 CAS 键：message/epoch/整体 digest 必须同时匹配。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxKey {
    pub message_id: String,
    pub epoch: u64,
    pub cipher_digest: [u8; 32],
}

/// repack 后要写入的一个 MNE1 part。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackPart {
    pub part_no: u32,
    pub envelope: Vec<u8>,
    pub digest: [u8; 32],
}

/// 一条 outbox 重打包目标（保留旧行的 doc_id/seq/batch_id，只换消息元数据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackSpec {
    pub message_id: String,
    pub object_id: [u8; 32],
    pub epoch: u64,
    pub parts: Vec<RepackPart>,
    pub cipher_digest: [u8; 32],
}

/// repack 结果；`NotFound`/`Conflict` 都保证**没有改动任何行**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepackOutcome {
    Repacked,
    NotFound,
    Conflict,
}

/// candidate 里的 parts 摘要（不把 envelope 全量搬进列表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackPartSummary {
    pub part_no: u32,
    pub envelope_len: usize,
    pub digest: [u8; 32],
}

/// 一条待 repack 的旧 outbox 及其 parts 摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRepackCandidate {
    pub message_id: String,
    pub batch_id: String,
    pub doc_id: String,
    pub seq: u64,
    pub key_epoch: u64,
    pub object_id: Option<[u8; 32]>,
    pub state: SyncOutboxState,
    pub cipher_digest: Option<[u8; 32]>,
    pub parts: Vec<RepackPartSummary>,
}

/// `sync.repack_required` 屏障读取；未知值按损坏处理，避免静默漏 repack。
pub fn repack_required(vault: &Vault) -> Result<bool, StorageError> {
    match vault.get_meta(META_SYNC_REPACK_REQUIRED)? {
        None => Ok(false),
        Some(value) if value == b"0" || value.is_empty() => Ok(false),
        Some(value) if value == b"1" => Ok(true),
        Some(value) => Err(StorageError::Corrupt {
            context: format!(
                "meta {META_SYNC_REPACK_REQUIRED} 非法值：{}",
                String::from_utf8_lossy(&value)
            ),
        }),
    }
}

/// 写 `sync.repack_required` 屏障；与 sync 现有 `vault.set_meta` 使用同一 meta 键。
pub fn set_repack_required(vault: &Vault, required: bool) -> Result<(), StorageError> {
    vault.set_meta(
        META_SYNC_REPACK_REQUIRED,
        if required { b"1" } else { b"0" },
    )
}

/// 列出仍未 ACK（pending/encrypted/sent）、且 key_epoch 不是给定当前 epoch 的 outbox 行。
///
/// `encrypted` 表示 parts 已 durable 但尚未发送；换 epoch 后同样必须重包，不能漏掉。
pub fn outbox_repack_candidates(
    vault: &Vault,
    current_epoch: u64,
) -> Result<Vec<OutboxRepackCandidate>, StorageError> {
    let rows = vault.query(
        "SELECT message_id,batch_id,doc_id,seq,key_epoch,object_id,state,cipher_digest \
         FROM sync_outbox WHERE state IN('pending','encrypted','sent') AND key_epoch<>?1 \
         ORDER BY doc_id,seq,message_id",
        &[Value::Integer(i64_of(current_epoch)?)],
    )?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let row = row_to_outbox(row)?;
        let parts = outbox_parts(vault, &row.message_id, row.key_epoch)?
            .into_iter()
            .map(|part| RepackPartSummary {
                part_no: part.part_no,
                envelope_len: part.envelope.len(),
                digest: part.digest,
            })
            .collect();
        out.push(OutboxRepackCandidate {
            message_id: row.message_id,
            batch_id: row.batch_id,
            doc_id: row.doc_id,
            seq: row.seq,
            key_epoch: row.key_epoch,
            object_id: row.object_id,
            state: row.state,
            cipher_digest: row.cipher_digest,
            parts,
        });
    }
    Ok(out)
}

/// 原子地把一条旧 outbox 重打包为新的 message/object/epoch/parts。
///
/// 先做无写入的旧行 CAS 校验；校验通过后在**同一个事务**里：
/// 删除旧 parts → 删除旧 outbox 行 → 插入新 outbox 行 → 插入新 parts。
/// 旧行已被 ACK、epoch/digest 不匹配或事务中途失败时，旧行与旧 parts 都保持不变。
pub fn repack_outbox(
    vault: &Vault,
    old: &OutboxKey,
    new: &RepackSpec,
) -> Result<RepackOutcome, StorageError> {
    validate_repack_spec(new)?;
    if old.message_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "repack old.message_id 不能为空".to_owned(),
        });
    }

    let old_row = outbox_row(vault, &old.message_id)?;
    let Some(old_row) = old_row else {
        return Ok(RepackOutcome::NotFound);
    };
    if old_row.state == SyncOutboxState::Acked
        || old_row.key_epoch != old.epoch
        || old_row.cipher_digest != Some(old.cipher_digest)
    {
        return Ok(RepackOutcome::Conflict);
    }
    // 若新旧 CAS 键完全相同，迟到的旧 ACK 仍可能命中新行；这不是一次有效的重包。
    if new.message_id == old.message_id
        && new.epoch == old.epoch
        && new.cipher_digest == old.cipher_digest
    {
        return Ok(RepackOutcome::Conflict);
    }

    // 旧 parts 必须是连续 0..n-1；数据库损坏时不能静默搬走。
    let old_parts = outbox_parts(vault, &old.message_id, old.epoch)?;
    for (index, part) in old_parts.iter().enumerate() {
        if part.part_no != index as u32 {
            return Err(StorageError::Corrupt {
                context: format!(
                    "旧 outbox {}/{} parts 不连续：第 {index} 项是 {}",
                    old.message_id, old.epoch, part.part_no
                ),
            });
        }
    }

    // 同一 message_id 只能有一行；复用旧 message 时由事务内“先删后插”保证。
    if new.message_id != old.message_id && outbox_row(vault, &new.message_id)?.is_some() {
        return Ok(RepackOutcome::Conflict);
    }

    let mut statements = Vec::with_capacity(2 + 1 + new.parts.len());
    statements.push(Statement::new(
        "DELETE FROM sync_outbox_parts WHERE message_id=?1 AND epoch=?2",
        vec![
            Value::Blob(old.message_id.as_bytes().to_vec()),
            Value::Integer(i64_of(old.epoch)?),
        ],
    ));
    statements.push(Statement::new(
        "DELETE FROM sync_outbox \
         WHERE message_id=?1 AND key_epoch=?2 AND cipher_digest=?3 \
           AND state IN('pending','encrypted','sent')",
        vec![
            Value::Text(old.message_id.clone()),
            Value::Integer(i64_of(old.epoch)?),
            Value::Blob(old.cipher_digest.to_vec()),
        ],
    ));
    statements.push(Statement::new(
        "INSERT INTO sync_outbox(message_id,batch_id,doc_id,seq,key_epoch,ciphertext,object_id,state,cipher_digest) \
         VALUES(?1,?2,?3,?4,?5,NULL,?6,'encrypted',?7)",
        vec![
            Value::Text(new.message_id.clone()),
            Value::Text(old_row.batch_id.clone()),
            Value::Text(old_row.doc_id.clone()),
            Value::Integer(i64_of(old_row.seq)?),
            Value::Integer(i64_of(new.epoch)?),
            Value::Blob(new.object_id.to_vec()),
            Value::Blob(new.cipher_digest.to_vec()),
        ],
    ));
    for part in &new.parts {
        statements.push(Statement::new(
            "INSERT INTO sync_outbox_parts(message_id,epoch,part_no,envelope,digest) \
             VALUES(?1,?2,?3,?4,?5)",
            vec![
                Value::Blob(new.message_id.as_bytes().to_vec()),
                Value::Integer(i64_of(new.epoch)?),
                Value::Integer(i64::from(part.part_no)),
                Value::Blob(part.envelope.clone()),
                Value::Blob(part.digest.to_vec()),
            ],
        ));
    }

    let mut guards = vec![
        AffectedGuard::exactly(0, old_parts.len()),
        AffectedGuard::exactly(1, 1),
        AffectedGuard::exactly(2, 1),
    ];
    guards.extend((0..new.parts.len()).map(|index| AffectedGuard::exactly(3 + index, 1)));

    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => Ok(RepackOutcome::Repacked),
        Err(StorageError::GuardFailed { .. }) => Ok(RepackOutcome::Conflict),
        Err(error) => Err(error),
    }
}

fn validate_repack_spec(spec: &RepackSpec) -> Result<(), StorageError> {
    if spec.message_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "repack new.message_id 不能为空".to_owned(),
        });
    }
    if spec.epoch == 0 {
        return Err(StorageError::Corrupt {
            context: "repack new.epoch 必须 >0".to_owned(),
        });
    }
    if spec.parts.is_empty() {
        return Err(StorageError::Corrupt {
            context: "repack new.parts 不能为空".to_owned(),
        });
    }
    for (index, part) in spec.parts.iter().enumerate() {
        if part.part_no != index as u32 {
            return Err(StorageError::Corrupt {
                context: format!(
                    "repack new.parts 必须从 0 连续：第 {index} 项是 {}",
                    part.part_no
                ),
            });
        }
        if part.envelope.is_empty() {
            return Err(StorageError::Corrupt {
                context: format!("repack new.parts 第 {index} 片信封为空"),
            });
        }
        if part.digest.len() != 32 {
            return Err(StorageError::Corrupt {
                context: format!("repack new.parts 第 {index} 片 digest 不是 32B"),
            });
        }
    }
    Ok(())
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

/// 入站批量入口：**一个事务**完成「既有行比对 + 新行插入 + `received` 前缀推进」。
///
/// 语义与逐条入口**完全一致**：
///
/// - 已存在同 `(log_id, relay_seq)` 行、且 `object_id/epoch/verified_payload` 完全相同 →
///   幂等跳过（不重写、不报错，也不触发任何写入守卫）；
/// - 已存在同行但字节不同 → [`StorageError::SyncConflict`]，**整批一行都不写**；
/// - 新行插入与 `received` 连续前缀推进在**同一个** SQLite 事务里（Phase 1A-1），
///   一次提交只产生一次 fsync；冲突/崩溃只会整批回滚，不存在"行写了、前缀没推进"的
///   半提交状态；前缀只覆盖已消费的连续序号（含 `sync.skipped_received` 的 ASSET 标记）。
///
/// 输入先按 `relay_seq` 升序**稳定排序**（不要求调用方预排序；允许序号不连续，例如一页里
/// 混有已消费的 ASSET 序号）；同一批里出现重复 `relay_seq` 直接拒绝——一条日志不可能有
/// 两个同序号事件，静默去重会掩盖上游错误。全部校验/排序在 worker 的事务入口
/// （`actor` 模块的 `ExecInboxBatch` 命令）再做一遍，这里只负责搬运数据。
///
/// 返回**本次新插入**的行数（幂等跳过的行不计）。
pub fn store_inbox_verified_batch(
    vault: &Vault,
    rows: &[VerifiedInboxRow],
) -> Result<usize, StorageError> {
    if rows.is_empty() {
        return Ok(0);
    }
    let batch = rows
        .iter()
        .map(|row| crate::actor::InboxBatchRow {
            log_id: row.log_id,
            relay_seq: row.relay_seq,
            object_id: row.object_id,
            epoch: row.epoch,
            verified_payload: row.verified_payload.clone(),
        })
        .collect();
    vault.exec_inbox_batch(batch)
}

/// 读当前已验证、且落在 durable 连续接收前缀内的 inbox 行。
///
/// 依赖完整性由 sync 层判断（本 crate 不解析 Loro VV）；这里的 "ready"
/// 指 cryptographically verified + durable + **属于当前 `sync.log_id`** +
/// `relay_seq <= received_seq`，不在连续前缀内的行保持 durable，等前面的缺口补齐。
/// 排序为 `(log_id, relay_seq)`。
///
/// 清理已被 `integrated` 前缀覆盖的 inbox 行（Phase 1B 追赶后）。
///
/// **安全边界**：只删除 `relay_seq <= up_to_seq` 且同一 SQL 内证明
/// `sync.integrated_seq >= up_to_seq` 的行；证明不成立时一行不删。调用方还必须保证
/// `log_id` 是当前 relay incarnation，且 `up_to_seq` 是实际成功的 checkpoint S。
///
/// 为什么不用先读后删：读 `integrated` 与 DELETE 之间可能被并发推进/回退；
/// `EXISTS` 子查询把证明和删除放在同一条原子语句里。BLOB 按 8 字节 BE 比较。
pub fn prune_covered_inbox(
    vault: &Vault,
    log_id: &[u8; 16],
    up_to_seq: u64,
) -> Result<usize, StorageError> {
    let up_to = i64_of(up_to_seq)?;
    let statements = [Statement::new(
        "DELETE FROM sync_inbox \
         WHERE log_id=?1 AND relay_seq<=?2 \
           AND EXISTS (SELECT 1 FROM meta \
                        WHERE key=?3 AND typeof(value)='blob' AND length(value)=8 \
                          AND value>=?4)",
        vec![
            Value::Blob(log_id.to_vec()),
            Value::Integer(up_to),
            Value::Text(META_SYNC_INTEGRATED_SEQ.to_owned()),
            Value::Blob(up_to_seq.to_be_bytes().to_vec()),
        ],
    )];
    let changed = vault.exec_transaction(&statements, &[])?;
    Ok(changed.first().copied().unwrap_or(0))
}

/// **incarnation 过滤**（ADR 0006 §5）：`sync_inbox` 的 `log_id` 是写入时那代 relay
/// 日志的 incarnation。restore 迁移 `sync.log_id` 后，旧 incarnation 的行仍然 durable
/// （不能在迁移时静默删数据），但绝不能进入 ready——`relay_seq` 只在同一 incarnation
/// 内有序，跨代比较没有意义。过滤条件里的 `log_id` 由 [`read_current_log_id`] 读出后
/// 作为 SQL 参数绑定，不做子查询以便调用方/测试都看到同一个显式值。
///
/// `sync.log_id` 不存在（新库尚未绑定 relay 日志代）时返回空：没有当前 incarnation
/// 就没有可匹配的行；新库此时 `received` 也是 0，所以与旧实现（不按 log 过滤）的结果
/// 一致，不会把其它 incarnation 的遗留行漏出来。`sync.log_id` 存在但不是 16B →
/// [`StorageError::Corrupt`]（与 [`sync_cursor_state`] 同口径），不静默当作"没有 log"。
pub fn take_inbox_ready(vault: &Vault, limit: usize) -> Result<Vec<SyncInboxRow>, StorageError> {
    let Some(log_id) = read_current_log_id(vault)? else {
        return Ok(Vec::new());
    };
    let received = read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0);
    let rows = vault.query(
        "SELECT log_id,relay_seq,object_id,epoch,state,verified_payload \
         FROM sync_inbox WHERE log_id=?1 AND state='verified' AND relay_seq<=?2 \
         ORDER BY log_id,relay_seq LIMIT ?3",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(received)?),
            Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)),
        ],
    )?;
    rows.iter().map(row_to_inbox).collect()
}

/// 统计「已验证、属于当前 incarnation、落在 durable 连续接收前缀内、尚未集成」的行数。
///
/// 语义（ADR 0012 S2b）：这些行不是"丢失"——它们要么依赖未就绪（deferred），要么被
/// 批次预算留在本轮之后。调用方用它上报"已接收·待应用 N"，并据此判断"还有没有
/// 可继续的工作"，而不是循环刷最早的一批。
///
/// 与 [`take_inbox_ready`] 同口径：只计当前 `sync.log_id` 的行，旧 incarnation 的
/// verified 行即使 `relay_seq <= received` 也不计。`sync.log_id` 不存在返回 0
/// （没有当前 incarnation 就没有 ready 行）；存在但长度非法 → [`StorageError::Corrupt`]。
pub fn count_inbox_ready(vault: &Vault) -> Result<u64, StorageError> {
    let Some(log_id) = read_current_log_id(vault)? else {
        return Ok(0);
    };
    let received = read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0);
    let rows = vault.query(
        "SELECT COUNT(*) FROM sync_inbox \
         WHERE log_id=?1 AND state='verified' AND relay_seq<=?2",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(received)?),
        ],
    )?;
    rows.first()
        .map(|row| u64_of(row, 0, "sync_inbox.count"))
        .transpose()?
        .ok_or_else(|| StorageError::Corrupt {
            context: "sync_inbox 计数缺失".to_owned(),
        })
}

/// 带调用方依赖判断的 ready 选择器；不满足的项保持 durable verified。
///
/// 底层走 [`take_inbox_ready`]，因此同样只选择当前 `sync.log_id` 的行。
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

/// 批量把同一 log 的多条 verified inbox 行标为 integrated（**一个事务**）。
///
/// Phase 1A-1 为聚合 integrate 准备的 API：调用方在整批正文/依赖都落库后一次性标记，
/// 再统一调用 [`advance_integrated_prefix`]（本函数**不推进**前缀）。
///
/// 严格语义：`relay_seqs` 中每一行都必须存在且当前 `state='verified'`；任何一行
/// 不存在 / 已 integrated / 其它 state → 整批回滚并返回 [`StorageError::SyncConflict`]，
/// 不会留下半批已 integrated 的行。重复序号同理（第二次更新影响 0 行）整批回滚。
///
/// 返回标记的行数（= 输入行数；空输入返回 0）。
pub fn mark_integrated_batch(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seqs: &[u64],
) -> Result<usize, StorageError> {
    if relay_seqs.is_empty() {
        return Ok(0);
    }
    let mut statements = Vec::with_capacity(relay_seqs.len());
    let mut guards = Vec::with_capacity(relay_seqs.len());
    for (index, relay_seq) in relay_seqs.iter().enumerate() {
        statements.push(Statement::new(
            "UPDATE sync_inbox SET state='integrated' \
             WHERE log_id=?1 AND relay_seq=?2 AND state='verified'",
            vec![
                Value::Blob(log_id.to_vec()),
                Value::Integer(i64_of(*relay_seq)?),
            ],
        ));
        guards.push(AffectedGuard::exactly(index, 1));
    }
    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => Ok(relay_seqs.len()),
        // 守卫不满足 = 某行不存在或已不是 verified；exec_transaction 已整笔回滚。
        Err(StorageError::GuardFailed { .. }) => Err(StorageError::SyncConflict {
            context: format!(
                "inbox {log_id:?} 批量 integrate 失败：某行不存在或当前不是 verified（整批已回滚）"
            ),
        }),
        Err(error) => Err(error),
    }
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
        log_id: read_current_log_id(vault)?,
        confirmed_highwater: read_meta_u64(vault, META_SYNC_CONFIRMED_HIGHWATER)?,
    })
}

/// 读当前 relay 日志 incarnation（`sync.log_id`，BLOB 16B）。
///
/// - 不存在（新库/尚未绑定）：`Ok(None)`，调用方按"没有当前 incarnation"处理
///   （ready 视为空，绝不回退成"不过滤 log"）；
/// - 存在但不是 16B：[`StorageError::Corrupt`]，与 [`sync_cursor_state`] 的既有口径一致。
fn read_current_log_id(vault: &Vault) -> Result<Option<[u8; 16]>, StorageError> {
    read_meta(vault, META_SYNC_LOG_ID)?
        .map(|bytes| array16_bytes(bytes, "sync.log_id"))
        .transpose()
}

/// 写入 `sync.log_id`；已有不同值则拒绝（restore 必须显式迁移）。
pub fn set_sync_log_id(vault: &Vault, log_id: &[u8; 16]) -> Result<(), StorageError> {
    ensure_log_id(vault, log_id)
}

/// 写 confirmed highwater（BE64 BLOB）。
pub fn set_confirmed_highwater(vault: &Vault, value: u64) -> Result<(), StorageError> {
    write_meta(vault, META_SYNC_CONFIRMED_HIGHWATER, &value.to_be_bytes())
}

/// 单条入口的 `sync.log_id` 校验（走 Vault 命令，独立事务）。
///
/// 批量 inbox 事务内的同语义校验在 `sync_meta::ensure_log_id_on`（直接跑在连接/事务上）；
/// 两处必须保持一致：不存在则写，存在不同则 `SyncLogMismatch`。
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
    vault.advance_consumed_prefix(ConsumedPrefix::Received)
}

/// 把已集成连续前缀推进到最大已消费序号（**一次**写最终值）。
///
/// 供聚合 integrate 的调用方在整批正文都落库后调用一次（Phase 1A-1）。语义与旧实现
/// 一致：只认 inbox `state='integrated'` 或 `sync.skipped_received` 标记，扫窗口后
/// 一次写最终值；崩溃只会少推进，下次调用重算。
pub fn advance_integrated_prefix(vault: &Vault) -> Result<(), StorageError> {
    vault.advance_consumed_prefix(ConsumedPrefix::Integrated)
}

/// 登记一个**不写 inbox**、但已 durable 消费的事件（ASSET），并推进连续接收前缀。
///
/// 调用方必须先把该事件的事实（对 ASSET 是 skipped-asset HMAC 记录）durable 化，
/// 再调用本函数；顺序保证"received 只覆盖已消费的事件"。
pub fn mark_skipped_event_received(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seq: u64,
) -> Result<(), StorageError> {
    let key = skipped_received_key(log_id, relay_seq)?;
    write_meta(vault, &key, b"1")?;
    advance_received_prefix(vault)
}

/// 读一条 meta（同步桥经 Vault 命令的读取路径）。
///
/// 类型判定与 [`Vault::get_meta`] / `sync_meta::read_meta_on` 共用同一个 helper
/// [`meta_bytes_of`]：**只认 BLOB**，其它类型是 [`StorageError::Corrupt`]。
/// 这里以前把 TEXT 当"缺失"，会让 `sync.log_id` / `sync.repack_required` /
/// `sync.received_seq` 这些持久屏障与游标被静默绕过。
fn read_meta(vault: &Vault, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
    let rows = vault.query(
        "SELECT value FROM meta WHERE key=?1",
        &[Value::Text(key.to_owned())],
    )?;
    let Some(value) = rows.first().and_then(|row| row.first()) else {
        return Ok(None);
    };
    Ok(Some(meta_bytes_of(key, value.clone())?))
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
