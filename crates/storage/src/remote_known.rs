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
//! ## 单调性怎么强制（P2-5）
//!
//! 「只读旧值、在事务外算并集、再无条件覆盖」有两个写者互相覆盖的窗口（普通 pull 的
//! A 事务与 baseline apply 批次都可能写同一个键）。因此所有写入都改成**值 CAS**：
//!
//! - 生成语句时带上读到的旧值 `expected_old`，SQL 是
//!   `INSERT ... ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE meta.value=?old`；
//!   旧值不存在时用 `INSERT ... SELECT ... WHERE NOT EXISTS`；
//! - 语句带 `AffectedGuard::exactly(index, 1)`：CAS 不命中就是 0 行 → 整笔事务回滚，
//!   调用方拿到明确的冲突错误（重读后重试即收敛到并集），**绝不会写入更小的值**。
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
        // meta 值的统一口径（P2-6）：只认 BLOB；NULL/INTEGER/TEXT 都视为库被改坏。
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

/// 生成一条**值 CAS** upsert 语句（调用方负责把它放进 A 事务，并加
/// `AffectedGuard::exactly(语句下标, 1)`）。
///
/// - `expected = Some(old)`：`INSERT ... ON CONFLICT(key) DO UPDATE ... WHERE meta.value=?old`。
///   只有库中值仍是读到的旧值时才写入；被并发写者改过就是 0 行 → 守卫失败 → 整笔回滚；
/// - `expected = None`：`INSERT ... SELECT ... WHERE NOT EXISTS`。只有键仍不存在时才插入；
///   并发插入过就是 0 行 → 同样回滚。
///
/// 两种写法都不会用旧并集覆盖（更小的）并发结果，因此值只能单调扩大。
pub(crate) fn cas_statement(
    doc_id: &str,
    expected: Option<&VersionVector>,
    merged: &VersionVector,
) -> Statement {
    assert_monotone(expected, merged);
    let key = meta_key(doc_id);
    let encoded = merged.encode();
    match expected {
        Some(old) => Statement::new(
            "INSERT INTO meta(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value \
             WHERE meta.value=?3",
            vec![
                Value::Text(key),
                Value::Blob(encoded),
                Value::Blob(old.encode()),
            ],
        ),
        None => Statement::new(
            "INSERT INTO meta(key,value) SELECT ?1,?2 \
             WHERE NOT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
            vec![Value::Text(key), Value::Blob(encoded)],
        ),
    }
}

/// 调试断言：CAS 的目标值必须包含读到的旧值（`old ⊆ merged`）。
///
/// 生产路径由 [`union`] 构造 merged，这里只是把「只能扩大」的契约钉在生成语句处；
/// 一旦将来有人改成直接写入更小的值，debug/test 构建会立刻失败。
fn assert_monotone(expected: Option<&VersionVector>, merged: &VersionVector) {
    if let Some(old) = expected {
        debug_assert!(
            old.sub_iter(merged).next().is_none(),
            "remote known VV 只能单调扩大：旧值必须是 merged 的子集"
        );
    }
}

/// 在 A 事务**之外**读旧值并返回合并后的 **CAS** 语句；`added` 为空时返回 `None`。
///
/// 旧值只作为 CAS 的期望值；若事务执行时值已被并发写者扩大，语句影响 0 行、守卫失败，
/// 调用方必须重读（重新调用本函数）后重试。这里**不会**用读到的旧并集覆盖库里的新值。
pub(crate) fn merge_statement(
    vault: &Vault,
    doc_id: &str,
    added: &VersionVector,
) -> Result<Option<Statement>, StorageError> {
    if added.is_empty() {
        return Ok(None);
    }
    let existing = load(vault, doc_id)?;
    let merged = match &existing {
        Some(existing) => union(existing, added),
        None => added.clone(),
    };
    Ok(Some(cas_statement(doc_id, existing.as_ref(), &merged)))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::mpsc;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;
    use crate::actor::AffectedGuard;

    /// 临时 vault：目录建在 `target/tmp` 下（AGENTS §6：不用 `%TEMP%`）。
    ///
    /// lib 单元测试拿不到 `CARGO_TARGET_TMPDIR`（只有集成测试有），所以从
    /// `CARGO_MANIFEST_DIR` 拼出同一个位置。
    fn fresh_vault(prefix: &str) -> (tempfile::TempDir, Vault) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("remote-known-{prefix}"));
        std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录");
        let dir = tempfile::Builder::new()
            .prefix("vault-")
            .tempdir_in(&root)
            .expect("创建临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
        (dir, vault)
    }

    fn vv(peer: u64, counter: i32) -> VersionVector {
        let mut vv = VersionVector::new();
        vv.insert(peer, counter);
        vv
    }

    fn read(vault: &Vault, doc_id: &str) -> Option<VersionVector> {
        load(vault, doc_id).expect("读取远端已知 VV")
    }

    fn exec_cas(
        vault: &Vault,
        doc_id: &str,
        expected: Option<&VersionVector>,
        merged: &VersionVector,
    ) -> Result<(), StorageError> {
        vault
            .exec_transaction(
                &[cas_statement(doc_id, expected, merged)],
                &[AffectedGuard::exactly(0, 1)],
            )
            .map(|_| ())
    }

    /// 值 CAS 的三种状态：不存在时插入 → 陈旧期望被拒绝且不缩小 → 读到新值后并集成功。
    #[test]
    fn cas_statement_rejects_stale_expected_and_never_shrinks() {
        let (_dir, vault) = fresh_vault("stale");
        let doc_id = "doc-cas";
        let a = vv(0xA1, 2);
        let b = vv(0xB2, 3);

        // 读时不存在：直接插入。
        exec_cas(&vault, doc_id, None, &a).expect("首次插入");
        assert_eq!(read(&vault, doc_id), Some(a.clone()));

        // 另一个写者仍以为「不存在」，拿着旧的 None 来写 B：必须被 CAS 拒绝。
        let error = exec_cas(&vault, doc_id, None, &b).expect_err("陈旧期望必须失败");
        assert!(
            matches!(error, StorageError::GuardFailed { .. }),
            "CAS 不命中应当是 GuardFailed（调用方映射成明确冲突），实际 {error:?}"
        );
        assert_eq!(
            read(&vault, doc_id),
            Some(a.clone()),
            "CAS 失败绝不能把已有的 A 覆盖成只有 B"
        );

        // 重读后按并集重试：成功且值单调扩大。
        let current = read(&vault, doc_id).expect("已存在");
        let merged = union(&current, &b);
        exec_cas(&vault, doc_id, Some(&current), &merged).expect("并集重试");

        // 再拿一个更旧、且值更小的期望来写：必须继续被拒绝，值不得缩小。
        let stale = vv(0xA1, 1);
        let smaller = vv(0xA1, 1);
        let error = exec_cas(&vault, doc_id, Some(&stale), &smaller).expect_err("更小值必须失败");
        assert!(matches!(error, StorageError::GuardFailed { .. }));
        assert_eq!(
            read(&vault, doc_id),
            Some(merged),
            "失败后必须保持并集，绝不能写入更小的值"
        );
    }

    /// 两路并发写者从**同一旧值**出发写不同 `added`：barrier 对齐后同时 CAS。
    ///
    /// 结果只能是：恰好一方首次成功、另一方拿到明确的 `GuardFailed`；失败方重读后按
    /// 并集重试成功。最终值 = 两方新增的并集（不丢证据、不静默变小）。
    #[test]
    fn concurrent_writers_from_same_old_converge_to_union() {
        let (_dir, vault) = fresh_vault("concurrent");
        let doc_id = "doc-concurrent";
        let base = vv(0x10, 1);
        exec_cas(&vault, doc_id, None, &base).expect("初始值");

        let barrier = Arc::new(Barrier::new(2));
        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();
        for peer in [0xA1_u64, 0xB2_u64] {
            let vault = vault.clone();
            let barrier = Arc::clone(&barrier);
            let tx = tx.clone();
            handles.push(thread::spawn(move || {
                let added = vv(peer, 5);
                // 两方都在 barrier 之前读到同一个旧值。
                let old = load(&vault, doc_id).expect("读旧值").expect("已存在");
                let merged = union(&old, &added);
                let statement = cas_statement(doc_id, Some(&old), &merged);
                barrier.wait();
                let first = vault.exec_transaction(&[statement], &[AffectedGuard::exactly(0, 1)]);
                // 失败方（陈旧旧值）重读后重试一次：必须成功并收敛到并集。
                let (first_failed, retried) = match first {
                    Ok(_) => (false, true),
                    Err(StorageError::GuardFailed { .. }) => {
                        let current = load(&vault, doc_id).expect("重读").expect("已存在");
                        let merged = union(&current, &added);
                        (
                            true,
                            exec_cas(&vault, doc_id, Some(&current), &merged).is_ok(),
                        )
                    }
                    Err(other) => panic!("并发 CAS 只允许守卫失败，实际 {other:?}"),
                };
                tx.send((peer, first_failed, retried)).expect("回传结果");
            }));
        }
        drop(tx);
        let results: Vec<(u64, bool, bool)> = rx.iter().take(2).collect();
        for handle in handles {
            handle.join().expect("写者线程不应 panic");
        }
        assert_eq!(results.len(), 2, "两路写者都必须有明确结果");
        assert_eq!(
            results.iter().filter(|(_, failed, _)| *failed).count(),
            1,
            "同一旧值下只允许一方首次成功，另一方必须拿到明确的 CAS 冲突"
        );
        for (peer, _, retried) in &results {
            assert!(*retried, "peer {peer:#x} 重读重试后必须成功");
        }

        let final_vv = read(&vault, doc_id).expect("最终值");
        let expected = union(&union(&base, &vv(0xA1, 5)), &vv(0xB2, 5));
        assert_eq!(final_vv, expected, "最终值必须包含两路 added 的并集");
        assert_eq!(final_vv.get(&0x10).copied(), Some(1), "旧前缀不得丢失");
    }
}
