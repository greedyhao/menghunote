//! schema 迁移框架。
//!
//! 规则（ADR 0001 §3.9）：
//!
//! - 用 `PRAGMA user_version` 记录当前 schema 版本，**逐级向上**迁移；
//! - 每一级迁移单独一个事务，`PRAGMA user_version` 在**同一事务的最后**设置，
//!   保证「建表」与「版本号」原子生效，不会出现"表建了一半但版本号已经前进"的半成品库；
//! - 已是目标版本的库直接跳过，重复打开幂等；
//! - 库版本高于本程序支持的版本时拒绝打开（旧程序不得写新 schema）。

use rusqlite::{Connection, Transaction};

use crate::error::StorageError;

/// 本程序支持的最高 schema 版本。
pub const SCHEMA_VERSION: i64 = 1;

/// 单步迁移：把库从 `version - 1` 升到 `version`。
struct Migration {
    /// 目标版本号，也写入 `PRAGMA user_version`。
    version: i64,
    /// 迁移的可读名称，只用于错误信息。
    name: &'static str,
    /// 迁移主体，在事务内执行。
    run: fn(&Transaction<'_>) -> rusqlite::Result<()>,
}

/// 迁移表，必须按 `version` 升序且连续（v1、v2、v3…）。
const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "初始 schema",
    run: migrate_v1,
}];

/// 读取当前 schema 版本（`PRAGMA user_version`）。
pub(crate) fn user_version(conn: &Connection) -> Result<i64, StorageError> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// 把库迁移到 [`SCHEMA_VERSION`]；已是最新版本时什么都不做（幂等）。
pub(crate) fn migrate(conn: &mut Connection) -> Result<(), StorageError> {
    let current = user_version(conn)?;
    if current > SCHEMA_VERSION {
        return Err(StorageError::UnsupportedSchemaVersion {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }

    for migration in MIGRATIONS.iter().filter(|item| item.version > current) {
        apply(conn, migration)?;
    }
    Ok(())
}

/// 执行一级迁移：DDL 与版本号同一个事务提交。
fn apply(conn: &mut Connection, migration: &Migration) -> Result<(), StorageError> {
    let tx = conn.transaction()?;

    (migration.run)(&tx).map_err(|source| migration_failed(migration, source))?;

    // 关键顺序：先建表，最后才推进 user_version，二者同事务。
    tx.pragma_update(None, "user_version", migration.version)
        .map_err(|source| migration_failed(migration, source))?;

    tx.commit()
        .map_err(|source| migration_failed(migration, source))?;
    Ok(())
}

/// 把 SQLite 错误包装成带迁移上下文的错误。
fn migration_failed(migration: &Migration, source: rusqlite::Error) -> StorageError {
    StorageError::Migration {
        version: migration.version,
        name: migration.name,
        source,
    }
}

/// migration v1：ADR 0001 §7 附录 DDL 全量建表。
///
/// DDL 原样放在 `schema/v1.sql` 里，避免把建表语句拆碎在 Rust 字符串中而走样。
fn migrate_v1(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(include_str!("schema/v1.sql"))
}
