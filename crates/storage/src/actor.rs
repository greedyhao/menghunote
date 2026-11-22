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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use rusqlite::types::Value;
use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::connection;
use crate::error::StorageError;
use crate::migrate;
use crate::sync_meta::{self, ConsumedPrefix};

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

/// 批量 inbox 原子写入的一行。
///
/// 由 `sync_bridge::store_inbox_verified_batch` 构造，送到 worker 线程后在同一事务里
/// 完成「既有行比对 + 新行插入 + `received` 连续前缀推进」；字段含义与
/// [`crate::VerifiedInboxRow`] 一一对应，只是在 actor 层不依赖 sync 语义。
#[derive(Debug, Clone)]
pub(crate) struct InboxBatchRow {
    /// relay 日志 incarnation（同一批必须一致）。
    pub(crate) log_id: [u8; 16],
    /// relay 日志序号（必须 > 0；同一批内不得重复）。
    pub(crate) relay_seq: u64,
    /// 密文对象名（32B）。
    pub(crate) object_id: [u8; 32],
    /// 加密 epoch（必须 > 0）。
    pub(crate) epoch: u64,
    /// 已解密并完成载荷校验的 `UpdateV1` 明文。
    pub(crate) verified_payload: Vec<u8>,
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
    /// 读取一条 meta（键值，BLOB）。
    GetMeta {
        /// 键（命名空间由调用方约定，storage 不理解语义）。
        key: String,
        /// 结果回传通道。
        reply: Reply<Option<Vec<u8>>>,
    },
    /// 读取 `PRAGMA user_version`。
    UserVersion {
        /// 结果回传通道。
        reply: Reply<i64>,
    },
    /// 原子批量写 inbox：校验、既有行冲突检测、新行插入、`received` 连续前缀推进
    /// 全部在**同一个事务**里完成。
    ExecInboxBatch {
        /// 待写入行（worker 内重新校验/排序）。
        rows: Vec<InboxBatchRow>,
        /// 结果回传通道：本次真正新插入的行数（幂等命中不计）。
        reply: Reply<usize>,
    },
    /// 在同一条命令里推进连续前缀（读窗口 + 一次写最终值都在 worker 连接上完成）。
    AdvanceConsumedPrefix {
        /// 要推进的前缀（received / integrated）。
        prefix: ConsumedPrefix,
        /// 结果回传通道。
        reply: Reply<()>,
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
pub(crate) struct Shared {
    /// 命令发送端；关闭时取走并丢弃，工作线程的 `recv` 随之返回错误。
    tx: Mutex<Option<Sender<Command>>>,
    /// 工作线程句柄，只在最后一个句柄析构时 join。
    worker: Mutex<Option<JoinHandle<()>>>,
    /// 数据库文件路径。
    path: PathBuf,
    /// 变更通知的订阅者槽位（`(id, sender)`）。
    pub(crate) event_slots: Mutex<Vec<crate::events::EventSlot>>,
    /// 分配订阅者 id 用。
    pub(crate) next_event_slot: std::sync::atomic::AtomicU64,
    /// 「事件送不出去、请做全量对账」的粘性标记。
    pub(crate) reconcile_required: std::sync::atomic::AtomicBool,
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
                    event_slots: crate::events::new_slots(),
                    next_event_slot: std::sync::atomic::AtomicU64::new(0),
                    reconcile_required: std::sync::atomic::AtomicBool::new(false),
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

    /// 打开一条**只读快照**连接（WAL 下与写路径并发，不占写 actor）。
    ///
    /// 语义见 [`crate::ReadSnapshot`]：只保证读到"已提交"的数据；忙则返回
    /// [`StorageError::SnapshotBusy`]，不做无限重试。
    pub fn open_read_snapshot(&self) -> Result<crate::ReadSnapshot, StorageError> {
        crate::ReadSnapshot::open(&self.inner.path)
    }

    /// 订阅轻量变更事件（容量 = 队列上限）。
    ///
    /// 发布走 `try_send`：队列满 / 无订阅者时不会阻塞写，而是置"需要全量对账"标记。
    pub fn subscribe_events(&self, capacity: usize) -> crate::StorageEventSubscriber {
        self.inner.subscribe_events(capacity)
    }

    /// 取走并清除"事件没送到、请做全量对账"的标记。
    pub fn take_reconcile_required(&self) -> bool {
        self.inner.take_reconcile_required()
    }

    /// 读一条 meta（键值 BLOB）；键不存在返回 `None`。
    ///
    /// 判定与另两个入口共享同一个 helper（`sync_meta::meta_bytes_of`）：**只认 BLOB**，
    /// TEXT/INTEGER/REAL 都是 [`StorageError::Corrupt`]，不会静默当成"没有这条 meta"
    /// ——否则 `sync.repack_required` / `sync.log_id` 之类持久屏障会被绕过。
    pub fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        self.call(|reply| Command::GetMeta {
            key: key.to_owned(),
            reply,
        })
    }

    /// 写一条 meta：**走写 actor 的小事务**（`INSERT ... ON CONFLICT`），不绕过 actor。
    pub fn set_meta(&self, key: &str, value: &[u8]) -> Result<(), StorageError> {
        self.exec_transaction(
            &[Statement::new(
                "INSERT INTO meta(key, value) VALUES(?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                vec![Value::Text(key.to_owned()), Value::Blob(value.to_vec())],
            )],
            &[AffectedGuard::exactly(0, 1)],
        )?;
        Ok(())
    }

    /// 发布一条变更事件（内部用；B 物化与结构投影提交后调用）。
    ///
    /// 返回成功投递的订阅者数量；`0` 说明没人收到，已置"需要全量对账"标记。
    pub(crate) fn publish_event(&self, event: crate::StorageEvent) -> usize {
        self.inner.publish_event(event)
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
    ///
    /// 事务以 `BEGIN IMMEDIATE` 显式开启（与批量 inbox 入口共用
    /// [`begin_immediate`]）：写锁在事务开始就取，并发写者按 `busy_timeout` 排队，
    /// 而不是在第一条语句处撞上"锁升级不走 busy handler"的立即失败。
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

    /// 原子批量写 inbox（storage 内部入口；外部请走
    /// [`crate::sync_bridge::store_inbox_verified_batch`]）。
    ///
    /// 空批次是合法 no-op（与逐条入口一致），不会打开事务。
    pub(crate) fn exec_inbox_batch(&self, rows: Vec<InboxBatchRow>) -> Result<usize, StorageError> {
        if rows.is_empty() {
            return Ok(0);
        }
        self.call(|reply| Command::ExecInboxBatch { rows, reply })
    }

    /// 推进连续前缀（`received` / `integrated`），同一条命令内完成读窗口与写最终值。
    pub(crate) fn advance_consumed_prefix(
        &self,
        prefix: ConsumedPrefix,
    ) -> Result<(), StorageError> {
        self.call(|reply| Command::AdvanceConsumedPrefix { prefix, reply })
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
        Command::GetMeta { key, reply } => {
            // 与 sync_bridge / sync_meta 同一个读取 helper（只认 BLOB，其它类型报损坏）。
            let _ = reply.send(sync_meta::read_meta_on(conn, &key));
        }
        Command::UserVersion { reply } => {
            let _ = reply.send(migrate::user_version(conn));
        }
        Command::ExecInboxBatch { rows, reply } => {
            let _ = reply.send(exec_inbox_batch(conn, rows));
        }
        Command::AdvanceConsumedPrefix { prefix, reply } => {
            let _ = reply.send(sync_meta::advance_consumed_prefix_on(conn, prefix));
        }
    }
}

/// 在连接上显式开启一个 **Immediate** 写事务（`BEGIN IMMEDIATE`）。
///
/// 为什么不用 deferred（`conn.unchecked_transaction()` 的默认行为）：
///
/// - deferred 的 `BEGIN` 只是登记，第一条 SELECT 拿共享锁，等到第一条写语句才尝试把
///   共享锁升级成写锁；而 SQLite 的锁升级**不走 busy handler**——此刻若已有别的写事务
///   在途，不会用 `busy_timeout` 等待，而是直接返回 `SQLITE_BUSY`，让调用方在"最多等
///   5s"的预期之外立刻失败；
/// - `BEGIN IMMEDIATE` 在事务开始就取写锁，这一步**照常**受 `busy_timeout` 约束，与
///   ADR 0001 §3.9「每 vault 单 writer actor」的模型一致：写事务不该和别的写事务撞在
///   升级点上。
///
/// 两个写事务入口都走这里，论证只写在这一处（避免注释在两个调用点各改一半而漂移）：
///
/// - [`exec_inbox_batch`]：先读后写，deferred 会在锁升级处失败，且读到的旧快照还会先
///   报出误导性的 `SyncConflict`；
/// - [`exec_transaction`]：承载 A 事务/资产/repack/finish，调用方习惯把写语句放最前，
///   但**没有任何断言**保证这一点（以后多一条只读语句就退化成升级路径），所以同样
///   显式 Immediate。
fn begin_immediate(conn: &Connection) -> Result<Transaction<'_>, StorageError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    Ok(tx)
}

/// 批量 inbox 的原子执行。
///
/// 全部动作在 worker 持有独占连接上的**同一个事务**里：
///
/// 1. 校验批内行：同 `log_id`、`relay_seq/epoch > 0`、`relay_seq` 不重复；按序号排序；
/// 2. 校验 `sync.log_id`（不存在则写，存在不同则 [`StorageError::SyncLogMismatch`]）；
/// 3. 读该序号范围既有行，逐行比较 `object_id/epoch/verified_payload`：
///    - 完全相同 → 幂等跳过（不插入、不触发任何守卫）；
///    - 任意不同 → [`StorageError::SyncConflict`]，`tx` 在此 drop，整笔回滚（一行不写）；
/// 4. 新行在同一事务插入（每行要求恰好影响 1 行）；
/// 5. **同一事务内**推进 `received` 连续前缀（与普通路径同一套扫描语义）；
/// 6. 提交后返回新插入行数（幂等行不计）。
///
/// 先读后写的竞态因此被消除：快照与插入属于同一个写事务（SQLite 的单写者锁），
/// 且一次提交只产生一次 fsync；崩溃/冲突只会留下「什么都没发生」或「行+前缀一起提交」。
///
/// 事务用 [`begin_immediate`] 显式开启（而不是 deferred）：本事务先读后写，deferred
/// 的锁升级不走 busy handler，完整论证见该函数。
fn exec_inbox_batch(
    conn: &Connection,
    mut rows: Vec<InboxBatchRow>,
) -> Result<usize, StorageError> {
    if rows.is_empty() {
        return Ok(0);
    }
    let log_id = rows[0].log_id;
    for row in &rows {
        if row.log_id != log_id {
            return Err(StorageError::Corrupt {
                context: "批量 inbox 行必须属于同一 log_id".to_owned(),
            });
        }
        if row.relay_seq == 0 || row.epoch == 0 {
            return Err(StorageError::Corrupt {
                context: "inbox relay_seq/epoch 必须 >0".to_owned(),
            });
        }
    }
    rows.sort_by_key(|row| row.relay_seq);
    if rows
        .windows(2)
        .any(|pair| pair[0].relay_seq == pair[1].relay_seq)
    {
        return Err(StorageError::Corrupt {
            context: "批量 inbox 出现重复 relay_seq".to_owned(),
        });
    }

    // 事务模式与锁升级论证收在 [`begin_immediate`]：本事务先读后写，deferred 会在锁
    // 升级处立即 SQLITE_BUSY（见该函数的注释）。
    let tx = begin_immediate(conn)?;
    sync_meta::ensure_log_id_on(&tx, &log_id)?;

    let first_seq = rows.first().map_or(0, |row| row.relay_seq);
    let last_seq = rows.last().map_or(0, |row| row.relay_seq);
    let existing = read_existing_inbox(&tx, &log_id, first_seq, last_seq)?;
    let mut fresh: Vec<&InboxBatchRow> = Vec::with_capacity(rows.len());
    for row in &rows {
        match existing.get(&row.relay_seq) {
            Some((object_id, epoch, payload)) => {
                if *object_id != row.object_id
                    || *epoch != row.epoch
                    || *payload != row.verified_payload
                {
                    // 不 commit：tx 在此 drop，SQLite 自动回滚整笔事务（包括已插入的新行）。
                    return Err(StorageError::SyncConflict {
                        context: format!("inbox {log_id:?}/{} 字节不一致", row.relay_seq),
                    });
                }
            }
            None => fresh.push(row),
        }
    }

    if !fresh.is_empty() {
        insert_inbox_rows(&tx, &log_id, &fresh)?;
    }

    // 与插入处于同一事务：提交后 received 一定覆盖本批新行；崩溃/冲突则一起回滚。
    sync_meta::advance_consumed_prefix_on(&tx, ConsumedPrefix::Received)?;
    tx.commit()?;
    Ok(fresh.len())
}

/// 既有 inbox 行的比较字段：`(object_id, epoch, verified_payload)`。
type ExistingInboxFacts = ([u8; 32], u64, Vec<u8>);

/// 读 `[first_seq, last_seq]` 范围内的既有 inbox 行，按序号建索引。
fn read_existing_inbox(
    conn: &Connection,
    log_id: &[u8; 16],
    first_seq: u64,
    last_seq: u64,
) -> Result<BTreeMap<u64, ExistingInboxFacts>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT relay_seq,object_id,epoch,verified_payload FROM sync_inbox \
         WHERE log_id=?1 AND relay_seq>=?2 AND relay_seq<=?3",
    )?;
    let mut rows = statement.query(rusqlite::params_from_iter([
        Value::Blob(log_id.to_vec()),
        Value::Integer(i64_of(first_seq)?),
        Value::Integer(i64_of(last_seq)?),
    ]))?;
    let mut existing = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let seq = u64::try_from(seq).map_err(|_| StorageError::Corrupt {
            context: "sync_inbox.relay_seq 为负数".to_owned(),
        })?;
        let object_id: Vec<u8> = row.get(1)?;
        let object_id = object_id
            .try_into()
            .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
                context: format!("sync_inbox.object_id 应为 32B，实际 {}B", bytes.len()),
            })?;
        let epoch: i64 = row.get(2)?;
        let epoch = u64::try_from(epoch).map_err(|_| StorageError::Corrupt {
            context: "sync_inbox.epoch 为负数".to_owned(),
        })?;
        let payload: Option<Vec<u8>> = row.get(3)?;
        existing.insert(seq, (object_id, epoch, payload.unwrap_or_default()));
    }
    Ok(existing)
}

/// 在同一事务里插入新行；每行都要求恰好影响 1 行，否则整笔回滚。
fn insert_inbox_rows(
    conn: &Connection,
    log_id: &[u8; 16],
    rows: &[&InboxBatchRow],
) -> Result<(), StorageError> {
    let mut statement = conn.prepare(
        "INSERT INTO sync_inbox(log_id,relay_seq,object_id,epoch,state,verified_payload) \
         VALUES(?1,?2,?3,?4,'verified',?5)",
    )?;
    for row in rows {
        let affected = statement.execute(rusqlite::params_from_iter([
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(row.relay_seq)?),
            Value::Blob(row.object_id.to_vec()),
            Value::Integer(i64_of(row.epoch)?),
            Value::Blob(row.verified_payload.clone()),
        ]))?;
        if affected != 1 {
            return Err(StorageError::SyncConflict {
                context: format!("inbox {log_id:?}/{} 写入冲突", row.relay_seq),
            });
        }
    }
    Ok(())
}

/// SQLite INTEGER（有符号 64 位）到 u64 的转换；负数说明库/入参不自洽。
fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER"),
    })
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
    // 连接由工作线程独占，不会有嵌套事务；事务模式与批量 inbox 共用 begin_immediate
    // （锁升级论证见该函数），语句顺序/守卫/CAS 语义都不变。
    let tx = begin_immediate(conn)?;

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
