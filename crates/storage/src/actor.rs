//! 单写者 actor：一个专用线程独占 `rusqlite::Connection`。
//!
//! 为什么需要它：
//!
//! - `rusqlite::Connection` 不是 `Sync`，多个线程各持一个连接写同一个文件时，
//!   SQLite 的写锁会退化成 `SQLITE_BUSY` 竞争；
//! - ADR 0001 §3.9 要求「每 vault 单 writer actor」。
//!
//! 实现方式：所有读写都变成消息排到同一个线程上顺序执行，外部通过 `mpsc` 发命令、
//! 通过容量为 1 的 `sync_channel` 收结果（等价 oneshot）。`Connection` 在工作线程内
//! 创建、也在工作线程内析构，**绝不跨线程移动**，因此关闭连接时的 WAL checkpoint
//! 也发生在它自己的线程上。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use rusqlite::types::Value;
use rusqlite::Connection;

use crate::connection;
use crate::error::StorageError;
use crate::migrate;

/// 查询结果中的一行：按 SQL 的列顺序排列的值。
pub type Row = Vec<Value>;

/// 单次请求的回复通道：容量 1，等价 oneshot。
type Reply<T> = SyncSender<Result<T, StorageError>>;

/// 事务里的一条语句。
#[derive(Debug, Clone)]
pub struct Statement {
    /// SQL 文本。
    pub sql: String,
    /// 绑定参数。
    pub params: Vec<Value>,
}

impl Statement {
    /// 构造一条语句。
    pub fn new(sql: impl Into<String>, params: Vec<Value>) -> Self {
        Self {
            sql: sql.into(),
            params,
        }
    }
}

/// 「这条语句必须影响 N 行」的事务守卫；不满足则整笔事务回滚。
///
/// 用来把 CAS 变成原子语义：例如 `UPDATE loro_docs ... WHERE head_seq = ?` 必须影响 1 行，
/// 否则说明 head 已被别人推进，此时**不能**提交本批 update。
#[derive(Debug, Clone, Copy)]
pub struct AffectedGuard {
    /// 语句下标（0 起）。
    pub statement: usize,
    /// 期望影响的行数。
    pub expected: usize,
}

impl AffectedGuard {
    /// 要求第 `statement` 条语句恰好影响 `expected` 行。
    pub fn exactly(statement: usize, expected: usize) -> Self {
        Self {
            statement,
            expected,
        }
    }
}

/// 工作线程命令。
enum Command {
    /// 执行单条写语句，返回受影响行数。
    Exec {
        /// SQL 文本。
        sql: String,
        /// 绑定参数。
        params: Vec<Value>,
        /// 结果回传通道。
        reply: Reply<usize>,
    },
    /// 执行多条语句（DDL / PRAGMA / 测试夹具），不关心行数。
    ExecBatch {
        /// SQL 文本。
        sql: String,
        /// 结果回传通道。
        reply: Reply<()>,
    },
    /// 执行单条查询，返回全部行。
    Query {
        /// SQL 文本。
        sql: String,
        /// 绑定参数。
        params: Vec<Value>,
        /// 结果回传通道。
        reply: Reply<Vec<Row>>,
    },
    /// 在**同一个事务**里执行多条语句，并按守卫校验影响行数。
    ExecTransaction {
        /// 语句列表。
        statements: Vec<Statement>,
        /// 守卫列表。
        guards: Vec<AffectedGuard>,
        /// 结果回传通道：每条语句的影响行数。
        reply: Reply<Vec<usize>>,
    },
    /// 读取 `PRAGMA user_version`。
    UserVersion {
        /// 结果回传通道。
        reply: Reply<i64>,
    },
}

/// vault 数据库句柄，克隆后共享同一个工作线程。
///
/// 它是 `Send + Sync`，可以跨线程共享；所有操作串行落在工作线程上。
#[derive(Debug, Clone)]
pub struct Vault {
    /// 共享状态，最后一个句柄析构时触发优雅关闭。
    inner: Arc<Shared>,
}

/// 句柄之间共享的状态。
#[derive(Debug)]
struct Shared {
    /// 命令发送端；关闭时取走并丢弃，工作线程的 `recv` 随之返回错误。
    tx: Mutex<Option<Sender<Command>>>,
    /// 工作线程句柄，只在最后一个句柄析构时 join。
    worker: Mutex<Option<JoinHandle<()>>>,
    /// 数据库文件路径。
    path: PathBuf,
}

impl Vault {
    /// 打开（或创建）给定路径的 vault 数据库。
    ///
    /// 内部依次完成：
    ///
    /// 1. 父目录不存在则创建；
    /// 2. 在工作线程打开连接；
    /// 3. 应用并读回校验 PRAGMA（WAL / FULL / foreign_keys / busy_timeout）；
    /// 4. 迁移到最新 schema。
    ///
    /// 只有全部成功才返回 `Ok`；失败时工作线程已被回收，不会泄漏。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let (tx, rx) = mpsc::channel::<Command>();
        // 初始化握手：工作线程把「打开 + 配置 + 迁移」的结果回传，成功后才交出句柄。
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), StorageError>>(1);

        let worker_path = path.clone();
        let handle = thread::Builder::new()
            .name("menghunote-storage".to_owned())
            .spawn(move || worker_main(&worker_path, &rx, init_tx))?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                inner: Arc::new(Shared {
                    tx: Mutex::new(Some(tx)),
                    worker: Mutex::new(Some(handle)),
                    path,
                }),
            }),
            Ok(Err(error)) => {
                // 初始化失败：关掉通道并等线程退出，再把手上的错误原样上报。
                drop(tx);
                let _ = handle.join();
                Err(error)
            }
            Err(_) => {
                // 工作线程没回消息就没了（panic 或连接都放不下），按通道失效处理。
                drop(tx);
                let _ = handle.join();
                Err(StorageError::ChannelClosed {
                    context: "等待工作线程初始化",
                })
            }
        }
    }

    /// vault 数据库文件路径。
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// 当前 schema 版本（`PRAGMA user_version`）。
    pub fn user_version(&self) -> Result<i64, StorageError> {
        self.call(|reply| Command::UserVersion { reply })
    }

    /// 低层写命令：执行单条 SQL，返回受影响行数。
    ///
    /// 只用于写语句；会把结果集返回给调用方的语句请用 [`Vault::query`]。
    /// 参数用 [`Value`] 传递，调用方负责别把用户输入拼进 SQL。
    pub fn exec(&self, sql: &str, params: &[Value]) -> Result<usize, StorageError> {
        self.call(|reply| Command::Exec {
            sql: sql.to_owned(),
            params: params.to_vec(),
            reply,
        })
    }

    /// 低层批处理：执行多条语句（DDL、PRAGMA、测试夹具等），全部成功才返回 `Ok`。
    pub fn exec_batch(&self, sql: &str) -> Result<(), StorageError> {
        self.call(|reply| Command::ExecBatch {
            sql: sql.to_owned(),
            reply,
        })
    }

    /// 低层读命令：执行单条查询，返回全部行。
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, StorageError> {
        self.call(|reply| Command::Query {
            sql: sql.to_owned(),
            params: params.to_vec(),
            reply,
        })
    }

    /// 低层事务命令：多条语句在同一事务里执行，`guards` 不满足则整笔回滚。
    ///
    /// 返回每条语句的影响行数（顺序与 `statements` 一致）。
    pub fn exec_transaction(
        &self,
        statements: &[Statement],
        guards: &[AffectedGuard],
    ) -> Result<Vec<usize>, StorageError> {
        self.call(|reply| Command::ExecTransaction {
            statements: statements.to_vec(),
            guards: guards.to_vec(),
            reply,
        })
    }

    /// 把一条命令发给工作线程并等待回复。
    fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T, StorageError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<T, StorageError>>(1);
        {
            // 只锁住"取发送端 + 投递命令"这一段，等回复时不持锁。
            let guard = lock(&self.inner.tx);
            let Some(tx) = guard.as_ref() else {
                return Err(StorageError::ChannelClosed {
                    context: "提交命令，工作线程已关闭",
                });
            };
            tx.send(make(reply_tx))
                .map_err(|_| StorageError::ChannelClosed {
                    context: "提交命令，工作线程已退出",
                })?;
        }
        reply_rx.recv().map_err(|_| StorageError::ChannelClosed {
            context: "等待命令回复，工作线程已退出",
        })?
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // 丢弃发送端：工作线程的 recv 立刻返回错误，退出循环并在自己的线程上关闭连接
        // （关闭时 SQLite 会做 WAL checkpoint）。Drop 里只做清理，绝不 panic。
        drop(lock(&self.tx).take());
        if let Some(worker) = lock(&self.worker).take() {
            // 工作线程若已 panic，join 返回 Err；这里吞掉，避免析构路径上二次 panic。
            let _ = worker.join();
        }
    }
}

/// 工作线程入口：独占连接，顺序处理命令。
fn worker_main(path: &Path, rx: &Receiver<Command>, init: Reply<()>) {
    let conn = match connection::open_and_migrate(path) {
        Ok(conn) => conn,
        Err(error) => {
            let _ = init.send(Err(error));
            return;
        }
    };

    if init.send(Ok(())).is_err() {
        // 主线程已放弃初始化；连接在这里随函数返回于本线程析构。
        return;
    }

    while let Ok(command) = rx.recv() {
        handle_command(&conn, command);
    }
}

/// 执行一条命令并回传结果；调用方可能已经走了，回复失败只忽略。
fn handle_command(conn: &Connection, command: Command) {
    match command {
        Command::Exec { sql, params, reply } => {
            let _ = reply.send(exec(conn, &sql, &params));
        }
        Command::ExecBatch { sql, reply } => {
            let _ = reply.send(conn.execute_batch(&sql).map_err(StorageError::from));
        }
        Command::Query { sql, params, reply } => {
            let _ = reply.send(query(conn, &sql, &params));
        }
        Command::ExecTransaction {
            statements,
            guards,
            reply,
        } => {
            let _ = reply.send(exec_transaction(conn, &statements, &guards));
        }
        Command::UserVersion { reply } => {
            let _ = reply.send(migrate::user_version(conn));
        }
    }
}

/// 执行单条写语句。
fn exec(conn: &Connection, sql: &str, params: &[Value]) -> Result<usize, StorageError> {
    let mut statement = conn.prepare(sql)?;
    Ok(statement.execute(rusqlite::params_from_iter(params))?)
}

/// 执行单条查询，把每一行转成 [`Row`]。
fn query(conn: &Connection, sql: &str, params: &[Value]) -> Result<Vec<Row>, StorageError> {
    let mut statement = conn.prepare(sql)?;
    let column_count = statement.column_count();
    let mut rows = statement.query(rusqlite::params_from_iter(params))?;

    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::with_capacity(column_count);
        for index in 0..column_count {
            values.push(row.get::<_, Value>(index)?);
        }
        result.push(values);
    }
    Ok(result)
}

/// 在一个事务里执行多条语句；任一语句报错或守卫不满足 → 整笔回滚。
fn exec_transaction(
    conn: &Connection,
    statements: &[Statement],
    guards: &[AffectedGuard],
) -> Result<Vec<usize>, StorageError> {
    // 连接由工作线程独占，不会有嵌套事务，所以用 &self 版本的 unchecked_transaction
    let tx = conn.unchecked_transaction()?;

    // 守卫必须在**该语句执行后立刻**校验：否则后续语句可能先撞 UNIQUE/FK，
    // 把「head 被人推进了」这个真正原因盖掉（CAS 语义要求先于任何写入冲突暴露）。
    for guard in guards {
        if guard.statement >= statements.len() {
            return Err(StorageError::GuardFailed {
                statement: guard.statement,
                expected: guard.expected,
                actual: 0,
            });
        }
    }

    let mut affected = Vec::with_capacity(statements.len());
    for (index, statement) in statements.iter().enumerate() {
        let mut prepared = tx.prepare(&statement.sql)?;
        let rows = prepared.execute(rusqlite::params_from_iter(&statement.params))?;
        affected.push(rows);

        for guard in guards.iter().filter(|guard| guard.statement == index) {
            if rows != guard.expected {
                // 不 commit：tx 在此 drop，SQLite 自动回滚整笔事务
                return Err(StorageError::GuardFailed {
                    statement: index,
                    expected: guard.expected,
                    actual: rows,
                });
            }
        }
    }

    tx.commit()?;
    Ok(affected)
}

/// 获取互斥锁；即使锁被毒化也继续使用内部数据（清理路径不允许 panic）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
