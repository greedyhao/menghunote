//! 每篇文档「远端已知 VV」的本地元数据（**非破坏性本地 meta**，不新增表/列、不参与 wire）。
//!
//! ## 为什么需要它
//!
//! `baseline_apply` 的远端 tombstone 需要回答：**这篇本地文档里有没有远端从未见过的 op？**
//! 有就必须保留（可能是 local-only 内容或尚未同步的本地编辑）；全都见过则可以按远端
//! tombstone 标记删除。仅凭 `loro_docs.head_seq>0` 或 `sync_outbox` 为空都不够：
//!
//! - 已有设备普通 pull/integrate 同步过的文档，`head_seq>0`、outbox 为空，
//!   但它的内容确实全部来自远端；
//! - 启用同步前就存在的 local-only 文档同样 `head_seq>0`（且没有 outbox 行），
//!   却从未离开本机。
//!
//! 因此这里持久化一个**只能单调扩大**的「远端已确证见过的 oplog 前缀」：
//! `meta` 键 `sync.remote_known_vv.<doc_id>`，值为 `VersionVector::encode()`。
//!
//! ## 谁会扩大它
//!
//! 1. 应用已验证基线快照：快照自带的 VV 就是远端在该 checkpoint 上已知的 ops
//!    （`baseline_apply` 在同一事务里写入）；
//! 2. 集成远端 update：`after_vv - before_vv` 就是这条远端消息实际带来的 ops
//!    （`store::append_updates_inner` 在同一 A 事务里写入）。
//!
//! 本地写入**不会**扩大它——即使已经 ACK 上传也不扩大。这是刻意的保守取舍：
//! tombstone 判定只会更倾向于 `kept_local_edit`，绝不会把本地未确证同步过的内容删掉。
//!
//! ## 判定
//!
//! `local_vv.sub_iter(remote_known).next().is_none()` 表示本地没有任何远端未知的 op，
//! 才允许按 tombstone 删除。没有这条记录时一律视为「无证据」，保守保留。

use loro::VersionVector;
use rusqlite::types::Value;

use crate::actor::Statement;
use crate::error::StorageError;
use crate::Vault;

/// 每篇文档「远端已知 VV」的 meta 键前缀。
pub const META_REMOTE_KNOWN_VV_PREFIX: &str = "sync.remote_known_vv.";

/// 某篇文档的 meta 键（doc_id 由仓库生成，形如 `note-<peer>-<seq>`，不含前缀歧义）。
pub(crate) fn meta_key(doc_id: &str) -> String {
    format!("{META_REMOTE_KNOWN_VV_PREFIX}{doc_id}")
}

/// 读某篇文档的远端已知 VV；没有记录返回 `None`（= 无证据）。
pub(crate) fn load(vault: &Vault, doc_id: &str) -> Result<Option<VersionVector>, StorageError> {
    let key = meta_key(doc_id);
    let rows = vault.query(
        "SELECT value FROM meta WHERE key=?1",
        &[Value::Text(key.clone())],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    match row.first() {
        Some(Value::Blob(bytes)) => Ok(Some(decode(bytes)?)),
        Some(Value::Null) | None => Ok(None),
        other => Err(StorageError::Corrupt {
            context: format!("{key} 读回异常：{other:?}"),
        }),
    }
}

/// 解码 VV；空字节等价于空版本向量（与 `HeadState::version_vector` 同口径）。
pub(crate) fn decode(bytes: &[u8]) -> Result<VersionVector, StorageError> {
    if bytes.is_empty() {
        return Ok(VersionVector::default());
    }
    VersionVector::decode(bytes).map_err(StorageError::from)
}

/// 逐 peer 取 max（单调并集）。
pub(crate) fn union(existing: &VersionVector, added: &VersionVector) -> VersionVector {
    let mut merged = existing.clone();
    for (peer, &counter) in added.iter() {
        let counter = counter.max(0);
        if counter <= 0 {
            continue;
        }
        let current = merged.get(peer).copied().unwrap_or(0).max(0);
        if counter > current {
            merged.insert(*peer, counter);
        }
    }
    merged
}

/// `after` 中不在 `before` 里的 ops（远端消息真正带来的那一段）。
///
/// 结果 VV 对某个 peer 的值是 `after[peer]`，即「远端已知 [0, after)」，与 `before`
/// 里已有的前缀合并是单调的。
pub(crate) fn added_since(after: &VersionVector, before: &VersionVector) -> VersionVector {
    let mut added = VersionVector::new();
    for span in after.sub_iter(before) {
        added.insert(span.peer, span.counter.end);
    }
    added
}

/// 纯 SQL 语句：把某篇文档的远端已知 VV 覆盖为 `merged`（调用方负责事务与单调并集）。
pub(crate) fn upsert_statement(doc_id: &str, merged: &VersionVector) -> Statement {
    Statement::new(
        "INSERT INTO meta(key,value) VALUES(?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        vec![Value::Text(meta_key(doc_id)), Value::Blob(merged.encode())],
    )
}

/// 在 A 事务**之外**读旧值并返回合并后的 upsert 语句；`added` 为空时返回 `None`。
///
/// 事务内的单调性由「先读后写 + 单写者」保证：所有写都经 Vault actor 串行执行。
pub(crate) fn merge_statement(
    vault: &Vault,
    doc_id: &str,
    added: &VersionVector,
) -> Result<Option<Statement>, StorageError> {
    if added.is_empty() {
        return Ok(None);
    }
    let merged = match load(vault, doc_id)? {
        Some(existing) => union(&existing, added),
        None => added.clone(),
    };
    Ok(Some(upsert_statement(doc_id, &merged)))
}
