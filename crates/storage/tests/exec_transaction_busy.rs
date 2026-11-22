//! P2-7：`exec_transaction` 用 `BEGIN IMMEDIATE` 开启事务（与批量 inbox 共用
//! `begin_immediate`）。
//!
//! 为什么必须是 Immediate：`exec_transaction` 承载 A 事务/资产/repack/finish，调用方
//! 习惯把写语句放在最前面，但**没有任何断言**保证这一点；只要事务里出现"先读后写"，
//! deferred 的锁升级就不走 `busy_timeout`（SQLite 的锁升级不调用 busy handler），会在
//! 预期之外立即 `SQLITE_BUSY`。
//!
//! 这些用例锁住外部可观测的后果：并发写者占着写锁时整笔事务失败、零半写、锁释放后
//! 同一批语句可重放且守卫照旧生效。

mod support;

use support::*;

use menghunote_storage::{AffectedGuard, Statement, StorageError, Value, Vault};

/// 断言错误是 `SQLITE_BUSY`（而不是守卫失败 / 约束冲突）。
fn assert_busy(error: &StorageError) {
    assert!(
        matches!(
            error,
            StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
                if code.code == rusqlite::ErrorCode::DatabaseBusy
        ),
        "并发写事务在场时必须整笔 SQLITE_BUSY，实际 {error:?}"
    );
}

/// 普通 `meta` 键的 BLOB 值（测试断言用）。
fn meta_blob(vault: &Vault, key: &str) -> Option<Vec<u8>> {
    vault.get_meta(key).expect("读 meta")
}

/// 外部写事务占着写锁时，`exec_transaction` 必须整笔 `SQLITE_BUSY` 且零半写；
/// 释放后同一批语句重放成功，语句顺序与守卫语义都不变。
#[test]
fn exec_transaction_under_foreign_write_lock_fails_busy_without_partial_write() {
    let (dir, vault) = fresh_vault();
    vault.set_meta("tx.cas", b"old").expect("预置 CAS 目标行");

    // 本连接不等锁（生产是 5s）：让 BEGIN IMMEDIATE 的失败立刻返回，测试无需 sleep。
    vault
        .exec_batch("PRAGMA busy_timeout=0")
        .expect("busy_timeout=0");

    // 第二个 Vault（= 第二个连接）先占住写锁。
    let holder = open_vault(&vault_path(&dir));
    holder
        .exec_batch("BEGIN IMMEDIATE")
        .expect("外部连接占写锁");

    // 第一条插入一行（半写的证据），第二条 CAS 更新既有行（守卫要求恰好 1 行）。
    let statements = [
        Statement::new(
            "INSERT INTO meta(key,value) VALUES(?1,?2)",
            vec![
                Value::Text("tx.half".to_owned()),
                Value::Blob(b"1".to_vec()),
            ],
        ),
        Statement::new(
            "UPDATE meta SET value=?2 WHERE key=?1",
            vec![
                Value::Text("tx.cas".to_owned()),
                Value::Blob(b"new".to_vec()),
            ],
        ),
    ];
    let guards = [AffectedGuard::exactly(0, 1), AffectedGuard::exactly(1, 1)];

    let error = vault
        .exec_transaction(&statements, &guards)
        .expect_err("写锁被占时必须整笔失败");
    assert_busy(&error);

    holder.exec_batch("ROLLBACK").expect("释放外部写锁");
    drop(holder);

    assert_eq!(meta_blob(&vault, "tx.half"), None, "失败事务不得留下半写");
    assert_eq!(
        meta_blob(&vault, "tx.cas"),
        Some(b"old".to_vec()),
        "失败事务不得改动 CAS 目标"
    );

    // 锁释放后同一批重放：整笔提交，每句影响行数与顺序都不变。
    let affected = vault
        .exec_transaction(&statements, &guards)
        .expect("锁释放后应成功");
    assert_eq!(affected, vec![1, 1], "语句顺序与每句影响行数必须保持原样");
    assert_eq!(meta_blob(&vault, "tx.half"), Some(b"1".to_vec()));
    assert_eq!(meta_blob(&vault, "tx.cas"), Some(b"new".to_vec()));
}

/// `exec_transaction` 在 `BEGIN` 阶段就取写锁（`BEGIN IMMEDIATE`），不是等第一条语句。
///
/// 有写语句时 deferred 也会在第一条语句撞上同样的 `SQLITE_BUSY`（`busy_timeout=0`），
/// 两种事务模式的结果无法区分；**空语句事务**是唯一能从外部观测到的差异：
///
/// - deferred：`BEGIN` 不碰写锁，`COMMIT` 空事务也成功 → `Ok(vec![])`；
/// - Immediate：`BEGIN` 就要写锁 → 占锁期间直接 `SQLITE_BUSY`。
///
/// 这也解释了为什么上面的"零半写"用例本身不足以证明模式切换。
#[test]
fn exec_transaction_takes_write_lock_at_begin() {
    let (dir, vault) = fresh_vault();
    vault
        .exec_batch("PRAGMA busy_timeout=0")
        .expect("busy_timeout=0");

    let holder = open_vault(&vault_path(&dir));
    holder
        .exec_batch("BEGIN IMMEDIATE")
        .expect("外部连接占写锁");

    let error = vault
        .exec_transaction(&[], &[])
        .expect_err("占锁期间 BEGIN IMMEDIATE 必须失败");
    assert_busy(&error);

    holder.exec_batch("ROLLBACK").expect("释放外部写锁");
    drop(holder);

    assert_eq!(
        vault
            .exec_transaction(&[], &[])
            .expect("释放后空事务应成功"),
        Vec::<usize>::new(),
        "空事务返回空的受影响行数列表"
    );
}

/// 占锁期间守卫错误也不该抢先暴露：整笔事务在 `BEGIN` 就失败，不会执行任何语句。
#[test]
fn exec_transaction_lock_failure_precedes_statement_effects() {
    let (dir, vault) = fresh_vault();
    vault.set_meta("tx.cas", b"old").expect("预置 CAS 目标行");
    vault
        .exec_batch("PRAGMA busy_timeout=0")
        .expect("busy_timeout=0");

    let holder = open_vault(&vault_path(&dir));
    holder
        .exec_batch("BEGIN IMMEDIATE")
        .expect("外部连接占写锁");

    // 守卫期望 0 行，但语句实际会影响 1 行：若事务能开始，会先写入再报 GuardFailed。
    let statements = [Statement::new(
        "UPDATE meta SET value=?2 WHERE key=?1",
        vec![
            Value::Text("tx.cas".to_owned()),
            Value::Blob(b"new".to_vec()),
        ],
    )];
    let guards = [AffectedGuard::exactly(0, 0)];
    let error = vault
        .exec_transaction(&statements, &guards)
        .expect_err("写锁被占时必须整笔失败");
    assert_busy(&error);

    holder.exec_batch("ROLLBACK").expect("释放外部写锁");
    drop(holder);

    assert_eq!(
        meta_blob(&vault, "tx.cas"),
        Some(b"old".to_vec()),
        "失败事务不得写入任何字节"
    );
}
