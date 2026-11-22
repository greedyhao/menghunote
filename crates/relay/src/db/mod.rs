//! SQLite 访问层：**单有界 writer 线程** + 只读连接池（ADR 0006 §7 部署约束）。
//!
//! 为什么不是一个 `Arc<Mutex<Connection>>`：
//! - `synchronous=FULL` + WAL 下写事务是 fsync 瓶颈，必须**串行**且**有界排队**（背压），不能无限 spawn；
//! - handler 绝不能"拿到连接 = 拿住写事务再 await 别的 IO"：本模块把所有写操作变成
//!   `db.write(|conn| { ...同步短事务... })`，事务完整地活在 writer 线程内，async handler 只等一个
//!   oneshot 结果。这样**不存在跨 await 的写事务**（§7 硬要求）。
//!
//! 读走独立连接池（WAL 允许并发读），读事务不阻塞 writer。

pub mod schema;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;

use rusqlite::{Connection, TransactionBehavior};
use tokio::sync::{mpsc, oneshot};

use crate::error::{Error, Result};
use crate::time::{Clock, SystemClock};

/// writer 线程的任务：一个同步闭包，在写事务内运行。
type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

/// 打开数据库的参数。
#[derive(Debug, Clone)]
pub struct DbConfig {
    /// SQLite 文件路径。
    pub path: PathBuf,
    /// 正常预算 `Q`（ADR §7 默认 4GiB）。
    pub normal_limit: i64,
    /// 维护预算 `R`（ADR §7 默认 2Q）。
    pub maintenance_limit: i64,
    /// writer 队列容量（有界背压；满了 async 发送方会等待）。
    pub writer_queue: usize,
    /// 只读连接池上限。
    pub read_connections: usize,
}

impl DbConfig {
    /// 以默认配额（`Q=4GiB`、`R=2Q`）打开指定路径。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let normal_limit = 4 * 1024 * 1024 * 1024i64;
        Self {
            path: path.into(),
            normal_limit,
            maintenance_limit: 2 * normal_limit,
            writer_queue: 64,
            read_connections: 4,
        }
    }

    /// 覆盖预算（测试/运维 CLI 用）。
    pub fn with_limits(mut self, normal_limit: i64, maintenance_limit: i64) -> Self {
        self.normal_limit = normal_limit;
        self.maintenance_limit = maintenance_limit;
        self
    }
}

/// 只读连接池。
#[derive(Debug)]
struct ReadPool {
    path: PathBuf,
    max: usize,
    idle: Mutex<Vec<Connection>>,
}

impl ReadPool {
    fn new(path: PathBuf, max: usize) -> Self {
        Self {
            path,
            max: max.max(1),
            idle: Mutex::new(Vec::new()),
        }
    }

    fn open_conn(path: &Path) -> Result<Connection> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // 读连接禁止写，防止"读路径意外写库"。
        conn.pragma_update(None, "query_only", "ON")?;
        Ok(conn)
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let cached = self.lock_idle().and_then(|mut idle| idle.pop());
        let conn = match cached {
            Some(conn) => conn,
            None => Self::open_conn(&self.path)?,
        };
        let out = f(&conn);
        if let Some(mut idle) = self.lock_idle() {
            if idle.len() < self.max {
                idle.push(conn);
            }
        }
        out
    }

    fn lock_idle(&self) -> Option<MutexGuard<'_, Vec<Connection>>> {
        // 读锁中毒时降级为"每次新建连接"，不 panic（drop 期间也不能炸）。
        self.idle.lock().ok()
    }
}

#[derive(Debug)]
struct Inner {
    tx: Mutex<Option<mpsc::Sender<Job>>>,
    writer: Mutex<Option<JoinHandle<()>>>,
    reads: ReadPool,
    clock: std::sync::Arc<dyn Clock>,
    path: PathBuf,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // 先关 channel（writer 的 blocking_recv 返回 None），再 join，保证测试里"关闭后立刻重开"
        // 不会和残留 writer 抢锁。
        if let Ok(mut tx) = self.tx.lock() {
            let _ = tx.take();
        }
        if let Ok(mut writer) = self.writer.lock() {
            if let Some(handle) = writer.take() {
                let _ = handle.join();
            }
        }
    }
}

/// 数据库句柄（Clone 廉价；所有 clone 共享同一个 writer 线程）。
#[derive(Debug, Clone)]
pub struct Db {
    inner: std::sync::Arc<Inner>,
}

impl Db {
    /// 打开/建库并启动 writer 线程。
    pub fn open(cfg: DbConfig, clock: std::sync::Arc<dyn Clock>) -> Result<Self> {
        if let Some(parent) = cfg.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut conn = Connection::open(&cfg.path)?;
        schema::apply(&mut conn)?;
        // 建库后把连接交给 writer 线程独占。
        let (tx, mut rx) = mpsc::channel::<Job>(cfg.writer_queue.max(1));
        let writer = std::thread::Builder::new()
            .name("relay-sqlite-writer".to_string())
            .spawn(move || {
                while let Some(job) = rx.blocking_recv() {
                    job(&mut conn);
                }
                // channel 关闭：正常退出。
            })?;
        Ok(Self {
            inner: std::sync::Arc::new(Inner {
                tx: Mutex::new(Some(tx)),
                writer: Mutex::new(Some(writer)),
                reads: ReadPool::new(cfg.path.clone(), cfg.read_connections),
                clock,
                path: cfg.path,
            }),
        })
    }

    /// 用系统时钟打开。
    pub fn open_with_system_clock(cfg: DbConfig) -> Result<Self> {
        Self::open(cfg, std::sync::Arc::new(SystemClock))
    }

    /// 当前时钟。
    pub fn clock(&self) -> &dyn Clock {
        self.inner.clock.as_ref()
    }

    /// 时钟的拥有型句柄（供 `'static` 闭包/后台任务使用）。
    pub fn clock_handle(&self) -> std::sync::Arc<dyn Clock> {
        self.inner.clock.clone()
    }

    /// 数据库路径（诊断/测试）。
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    fn sender(&self) -> Result<mpsc::Sender<Job>> {
        self.inner
            .tx
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
            .ok_or(Error::WriterClosed)
    }

    /// 异步写：闭包在 writer 线程的一个 `BEGIN IMMEDIATE` 事务内运行，Ok 则 commit、Err 则 rollback。
    ///
    /// 这是 handler 唯一允许的写入口；闭包内不允许 `await`（类型上就不允许）。
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let sender = self.sender()?;
        let (reply_tx, reply_rx) = oneshot::channel::<Result<T>>();
        let job: Job = Box::new(move |conn: &mut Connection| {
            let outcome = run_in_transaction(conn, f);
            // 接收端可能已取消（客户端断开）；忽略发送失败。
            let _ = reply_tx.send(outcome);
        });
        sender.send(job).await.map_err(|_| Error::WriterClosed)?;
        reply_rx.await.map_err(|_| Error::WriterClosed)?
    }

    /// 同步写（测试/CLI 用；**不得**在 tokio runtime 线程上调用）。
    pub fn write_blocking<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let sender = self.sender()?;
        let (reply_tx, reply_rx) = oneshot::channel::<Result<T>>();
        let job: Job = Box::new(move |conn: &mut Connection| {
            let outcome = run_in_transaction(conn, f);
            let _ = reply_tx.send(outcome);
        });
        sender.blocking_send(job).map_err(|_| Error::WriterClosed)?;
        reply_rx.blocking_recv().map_err(|_| Error::WriterClosed)?
    }

    /// 只读查询（走只读连接池；短查询，可直接在 handler 内同步调用）。
    pub fn read<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        self.inner.reads.with_conn(f)
    }

    /// 显式收尾：关闭 writer 线程并等待退出（测试用它保证"重开同一个库"是干净的）。
    pub fn close(self) {
        drop(self);
    }
}

fn run_in_transaction<T, F>(conn: &mut Connection, f: F) -> Result<T>
where
    F: FnOnce(&Connection) -> Result<T>,
{
    // IMMEDIATE：写事务一开始就拿写锁，避免 SQLITE_BUSY 在 commit 时才暴露。
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let out = f(&tx)?;
    tx.commit()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::FakeClock;

    /// 单元测试临时目录：cargo 只给集成测试定义 `CARGO_TARGET_TMPDIR`，
    /// lib 内联测试统一落到 `target/tmp`（AGENTS §6：不要用 tempfile/%TEMP%）。
    fn target_tmp() -> PathBuf {
        option_env!("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("..")
                    .join("..")
                    .join("target")
                    .join("tmp")
            })
    }

    fn test_path(label: &str) -> PathBuf {
        let dir = target_tmp().join("relay-unit");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(format!("{label}-{}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        path
    }

    #[test]
    fn write_is_serialized_and_visible_to_readers() {
        let db = Db::open(
            DbConfig::new(test_path("serialized")),
            std::sync::Arc::new(FakeClock::new(1_000)),
        )
        .expect("open");
        db.write_blocking(|conn| {
            conn.execute("INSERT INTO relay_meta(key, value) VALUES ('k', X'01')", [])?;
            Ok(())
        })
        .expect("write");
        let value: Vec<u8> = db
            .read(|conn| {
                Ok(
                    conn.query_row("SELECT value FROM relay_meta WHERE key='k'", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .expect("read");
        assert_eq!(value, vec![1u8]);

        // 闭包返回错误 → 事务回滚。
        let err = db
            .write_blocking(|conn| {
                conn.execute(
                    "INSERT INTO relay_meta(key, value) VALUES ('lost', X'02')",
                    [],
                )?;
                Err::<(), Error>(Error::Invariant("boom"))
            })
            .expect_err("must fail");
        assert!(matches!(err, Error::Invariant("boom")));
        let count: i64 = db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM relay_meta WHERE key='lost'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .expect("count");
        assert_eq!(count, 0);

        db.close();
        // 关闭后句柄不可再用。
        let closed = Db::open(
            DbConfig::new(test_path("serialized")),
            std::sync::Arc::new(FakeClock::new(1_000)),
        )
        .expect("reopen");
        closed.close();
    }

    #[test]
    fn bounded_queue_backpressure_does_not_lose_writes() {
        let db = Db::open(
            DbConfig {
                writer_queue: 1,
                ..DbConfig::new(test_path("bounded"))
            },
            std::sync::Arc::new(FakeClock::new(0)),
        )
        .expect("open");
        for i in 0..32 {
            db.write_blocking(move |conn| {
                conn.execute(
                    "INSERT INTO relay_meta(key, value) VALUES (?1, X'00')",
                    [format!("key-{i}")],
                )?;
                Ok(())
            })
            .expect("write");
        }
        let total: i64 = db
            .read(|conn| {
                Ok(conn.query_row("SELECT COUNT(*) FROM relay_meta", [], |row| row.get(0))?)
            })
            .expect("count");
        // 32 次写 + instance_id + schema_version。
        assert_eq!(total, 34);
        db.close();
    }

    #[test]
    fn read_connections_are_query_only() {
        let db = Db::open(
            DbConfig::new(test_path("query-only")),
            std::sync::Arc::new(FakeClock::new(0)),
        )
        .expect("open");
        let write_attempt = db.read(|conn| {
            conn.execute("INSERT INTO relay_meta(key, value) VALUES ('x', X'00')", [])?;
            Ok(())
        });
        assert!(write_attempt.is_err(), "read pool must reject writes");
        db.close();
    }
}
