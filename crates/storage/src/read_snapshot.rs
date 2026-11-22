//! 只读快照连接（S4-1a，ADR 0005 §1）。
//!
//! ## 为什么单独开连接
//!
//! 检索（search）要在后台线程反复扫全库。如果它也走写 actor，就会和编辑争同一条
//! 命令队列——一次大扫描能把写延迟拉长到用户可感知的程度。WAL 模式下**读不阻塞写、
//! 写也不阻塞读**，所以这里给检索开一条独立的 `SQLITE_OPEN_READ_ONLY` 连接：
//!
//! - 连接在创建它的线程里使用、随 [`ReadSnapshot`] 析构关闭（`rusqlite::Connection`
//!   不是 `Sync`，因此快照本身也不是 `Sync`）；
//! - 不参与迁移、不写任何东西，所以对 schema 版本只做**只读兼容性**判断；
//! - `busy_timeout` 与写连接一致（5s），但读路径**不做无限重试**：忙就返回可诊断错误。
//!
//! ## 读到的数据边界
//!
//! WAL 下读连接看到的是"最近一次已提交"的快照，**看不到**正在写的事务中间态；
//! 因此本模块只承诺"读到已提交数据"，不承诺"每次读到最新"——调用方（search）
//! 用 `meta_seq` / `body_seq` / `body_frontier` 自己做对账。

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags};

use crate::actor::Row;
use crate::error::StorageError;

/// 只读连接的 `busy_timeout`（毫秒），与写连接保持一致（ADR 0001 §3.9）。
const READ_BUSY_TIMEOUT_MS: u32 = 5_000;

/// 一条只读快照连接。
#[derive(Debug)]
pub struct ReadSnapshot {
    conn: Connection,
    path: PathBuf,
}

impl ReadSnapshot {
    /// 打开给定数据库文件的只读快照。
    ///
    /// 失败时返回可诊断的错误：文件不存在 → [`StorageError::Io`]；被写事务占满
    /// `busy_timeout` → [`StorageError::SnapshotBusy`]。
    pub(crate) fn open(path: &Path) -> Result<Self, StorageError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)
            .map_err(|error| map_read_error(error, "打开只读连接"))?;
        conn.busy_timeout(Duration::from_millis(u64::from(READ_BUSY_TIMEOUT_MS)))
            .map_err(|error| map_read_error(error, "设置只读连接的 busy_timeout"))?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    /// 数据库文件路径（与 [`crate::Vault::path`] 同一个文件）。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 预编译一条只读语句（强类型读取用；生命周期绑定本快照）。
    pub(crate) fn prepare(&self, sql: &str) -> Result<rusqlite::Statement<'_>, StorageError> {
        self.conn
            .prepare(sql)
            .map_err(|error| map_read_error(error, "准备只读查询"))
    }

    /// 执行一条只读 SQL，返回按列顺序排列的行。
    ///
    /// 语义与 [`crate::Vault::query`] 一致，只是走独立连接、不影响写 actor。
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, StorageError> {
        let mut statement = self
            .conn
            .prepare(sql)
            .map_err(|error| map_read_error(error, "准备只读查询"))?;
        let column_count = statement.column_count();
        let mut rows = statement
            .query(rusqlite::params_from_iter(params.iter()))
            .map_err(|error| map_read_error(error, "执行只读查询"))?;
        let mut out = Vec::new();
        loop {
            let row = rows
                .next()
                .map_err(|error| map_read_error(error, "读取只读查询结果"))?;
            let Some(row) = row else {
                break;
            };
            let mut values = Vec::with_capacity(column_count);
            for index in 0..column_count {
                let value = row
                    .get::<usize, Value>(index)
                    .map_err(|error| map_read_error(error, "解码只读查询列"))?;
                values.push(value);
            }
            out.push(values);
        }
        Ok(out)
    }
}

/// 把 SQLite 错误映射成可诊断的存储错误（BUSY/LOCKED 单独归类）。
pub(crate) fn map_read_error(error: rusqlite::Error, context: &'static str) -> StorageError {
    if let rusqlite::Error::SqliteFailure(code, _) = &error {
        use rusqlite::ErrorCode::{DatabaseBusy, DatabaseLocked};
        if matches!(code.code, DatabaseBusy | DatabaseLocked) {
            return StorageError::SnapshotBusy {
                context,
                busy_timeout_ms: READ_BUSY_TIMEOUT_MS,
            };
        }
    }
    StorageError::Sqlite(error)
}
