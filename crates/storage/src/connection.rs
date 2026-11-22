//! 连接建立：打开文件 → 应用并校验 PRAGMA → 迁移到最新 schema。
//!
//! ADR 0001 §3.9 规定每个 vault 使用 `journal_mode=WAL`、`synchronous=FULL`、
//! `foreign_keys=ON`、`busy_timeout=5000`。这些 PRAGMA 里除了 `journal_mode` 都是
//! 连接级开关，只要连接是新建的就可能被重置，所以**设置完必须读回校验**，
//! 不做"设置了就算生效"的假设。

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use crate::error::StorageError;
use crate::migrate;

/// `busy_timeout` 毫秒数，与 ADR 0001 §3.9 一致。
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// `PRAGMA synchronous` 的 FULL 取值（SQLite 定义：OFF=0 / NORMAL=1 / FULL=2 / EXTRA=3）。
const SYNCHRONOUS_FULL: i64 = 2;

/// 打开（或创建）数据库、应用 PRAGMA、执行迁移。
///
/// 该函数只允许在工作线程上调用：`Connection` 由调用线程持有并在同一线程析构。
pub(crate) fn open_and_migrate(path: &Path) -> Result<Connection, StorageError> {
    let mut conn = Connection::open(path)?;
    configure(&conn)?;
    verify(&conn)?;
    migrate::migrate(&mut conn)?;
    Ok(conn)
}

/// 应用 ADR 0001 §3.9 要求的 PRAGMA。
fn configure(conn: &Connection) -> Result<(), StorageError> {
    // journal_mode 是持久化在库文件里的设置；设置语句本身会返回生效后的模式，必须读回。
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    check("journal_mode", "wal", &mode)?;

    // foreign_keys 是连接级开关，且必须在事务外设置（事务内设置是 no-op）。
    conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;

    conn.busy_timeout(Duration::from_millis(u64::from(BUSY_TIMEOUT_MS)))?;
    Ok(())
}

/// 读回校验：任何一项没生效都视为打开失败，而不是带着错误配置继续跑。
fn verify(conn: &Connection) -> Result<(), StorageError> {
    let journal_mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    check("journal_mode", "wal", &journal_mode)?;

    let foreign_keys: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    check("foreign_keys", "1", &foreign_keys.to_string())?;

    let busy_timeout: i64 = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
    check(
        "busy_timeout",
        &BUSY_TIMEOUT_MS.to_string(),
        &busy_timeout.to_string(),
    )?;

    // synchronous 不在任务书的硬性清单里，但 §3.9 明确要求 FULL，一并校验。
    let synchronous: i64 = conn.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    check(
        "synchronous",
        &SYNCHRONOUS_FULL.to_string(),
        &synchronous.to_string(),
    )?;

    Ok(())
}

/// 比较 PRAGMA 读回值；不一致时返回带上下文的错误。
fn check(name: &'static str, expected: &str, actual: &str) -> Result<(), StorageError> {
    if actual.eq_ignore_ascii_case(expected) {
        return Ok(());
    }
    Err(StorageError::PragmaMismatch {
        name,
        expected: expected.to_owned(),
        actual: actual.to_owned(),
    })
}
