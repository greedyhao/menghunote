//! 同步元数据（`meta` 键）与「已消费连续前缀」的低层实现。
//!
//! 这里直接跑在 `rusqlite::Connection` 上，而不是走 [`crate::Vault`] 命令：
//! `rusqlite::Transaction` 通过 `Deref` 也是 `&Connection`，因此同一套扫描/写入既能被
//! 普通 Vault 命令复用（worker 线程的连接），也能被
//! `store_inbox_verified_batch` 放进**同一个**事务里执行——这是 Phase 1A-1
//! 「批量 inbox 原子化」的关键：新行插入与 `sync.received_seq` 推进一起提交/回滚。
//!
//! 语义与旧实现逐字等价：前缀只覆盖连续、已 durable 消费的序号；崩溃只会少推进
//! （下次扫描幂等重算），绝不会把未消费的事件算成已消费。

use rusqlite::types::Value;
use rusqlite::Connection;

use crate::error::StorageError;

/// `meta` 键：已 durable 连续接收前缀（BLOB BE64）。
pub const META_SYNC_RECEIVED_SEQ: &str = "sync.received_seq";
/// `meta` 键：已 integrate 连续前缀（BLOB BE64）。
pub const META_SYNC_INTEGRATED_SEQ: &str = "sync.integrated_seq";
/// `meta` 键：当前 relay log incarnation（BLOB 16B）。
pub const META_SYNC_LOG_ID: &str = "sync.log_id";
/// `meta` 键：已确认高水位（BLOB BE64）。
pub const META_SYNC_CONFIRMED_HIGHWATER: &str = "sync.confirmed_highwater";

/// `meta` 键前缀：**非 inbox 事件**（当前只有 ASSET）已 durable 消费的标记。
///
/// 完整键：`sync.skipped_received.<log_id 32 位十六进制>.<relay_seq 20 位十进制>`。
/// ASSET 事件不写 `sync_inbox`（它不是文档水位事件），但 `received_seq` 是"连续前缀"：
/// 若直接把 ASSET 当缺口，`received_seq` 会永久卡在该序号之前，后续 UPDATE 永远无法
/// 集成。pull 在把 ASSET 落成 durable skipped 记录后写本标记，前缀推进同时接受
/// inbox 行与这类标记，保证"已消费"的事件不会把前缀卡住。
pub const META_SYNC_SKIPPED_RECEIVED_PREFIX: &str = "sync.skipped_received.";

/// 一次前缀扫描中**每个来源**（inbox 行 / `skipped_received` 标记）各自的窗口大小。
/// 正常一页 256 条；4096 已远超单页，只作上限保护。
const PREFIX_SCAN_LIMIT: usize = 4096;

/// 要推进的连续前缀及其"已消费"判定。
///
/// 两个变体的条件与旧实现完全一致：
/// - `Received`：inbox `state IN('verified','integrated')`（读者尚未集成也算已接收）；
/// - `Integrated`：inbox `state='integrated'`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsumedPrefix {
    /// `sync.received_seq`。
    Received,
    /// `sync.integrated_seq`。
    Integrated,
}

impl ConsumedPrefix {
    /// 对应的 `meta` 键。
    pub(crate) fn meta_key(self) -> &'static str {
        match self {
            Self::Received => META_SYNC_RECEIVED_SEQ,
            Self::Integrated => META_SYNC_INTEGRATED_SEQ,
        }
    }

    /// inbox 行算作「已消费」的 SQL 条件。
    pub(crate) fn inbox_condition(self) -> &'static str {
        match self {
            Self::Received => "state IN('verified','integrated')",
            Self::Integrated => "state='integrated'",
        }
    }
}

/// 校验 `sync.log_id`：不存在则写入，已存在不同值则 [`StorageError::SyncLogMismatch`]。
///
/// 与 Vault 版 [`crate::sync_bridge::set_sync_log_id`] 同语义，区别只是直接在给定连接
/// （或事务）上执行，供批量 inbox 在同一事务里完成校验。
///
/// 既有值不是 BLOB 时 [`read_meta_on`] 返回 [`StorageError::Corrupt`]，本函数直接传播：
/// 损坏的 incarnation 记录**不能**当成"尚未绑定"覆盖掉（那会让旧传代的 inbox 行混进
/// 新一代）。
pub(crate) fn ensure_log_id_on(conn: &Connection, log_id: &[u8; 16]) -> Result<(), StorageError> {
    match read_meta_on(conn, META_SYNC_LOG_ID)? {
        Some(existing) => {
            if existing.as_slice() != log_id {
                return Err(StorageError::SyncLogMismatch {
                    expected: existing,
                    found: log_id.to_vec(),
                });
            }
        }
        None => write_meta_on(conn, META_SYNC_LOG_ID, log_id)?,
    }
    Ok(())
}

/// 把连续前缀推进到最大已消费序号（**一次**写最终值）。
///
/// 语义与"从 `current+1` 起逐条检查、遇到第一个缺口停下"逐字等价；差别只在写盘次数：
/// 旧实现对每个前进的序号各写一次 `meta`（每次都是独立事务，WAL + `synchronous=FULL`
/// 下就是一次 fsync）。现在的做法：先用两次读（inbox 升序窗口 + 同 log 的
/// `skipped_received` 键范围扫描，**两个窗口各自**有 [`PREFIX_SCAN_LIMIT`] 条上限）
/// 在内存里走到第一个缺口，再**一次**写入最终值。
///
/// 两个来源的窗口可能分别被截断：只要还有来源没看到"缺口之后"的序号，缺口就不算
/// 证明完毕，必须从新的 `current` 继续下一窗口（见循环内注释）。
///
/// 崩溃语义没有变差：inbox 行与 skipped 标记都先 durable；前缀少推进只会让下一次调用
/// 重算（扫描幂等），绝不会把未消费的事件算成已消费。
///
/// 在 [`crate::Vault`] 命令里传入 worker 独占连接时是自动提交读 + 单条写；
/// 在批量 inbox 事务里传入 `&Transaction` 时，读/写都与新行插入处于同一事务。
pub(crate) fn advance_consumed_prefix_on(
    conn: &Connection,
    prefix: ConsumedPrefix,
) -> Result<(), StorageError> {
    let Some(log_id) = read_meta_on(conn, META_SYNC_LOG_ID)? else {
        return Ok(());
    };
    let log_id = array16_bytes(log_id, "sync.log_id")?;
    let meta_key = prefix.meta_key();
    let mut current = read_meta_u64_on(conn, meta_key)?.unwrap_or(0);
    loop {
        // 与旧实现同口径：前缀已经是 u64::MAX 时无法再前进（保留溢出错误）。
        current
            .checked_add(1)
            .ok_or_else(|| StorageError::Corrupt {
                context: format!("{meta_key} 溢出"),
            })?;
        let window = scan_consumed_after_on(conn, &log_id, current, prefix)?;
        if window.seqs.is_empty() {
            return Ok(());
        }
        // 在内存里从 current+1 走到第一个缺口（seqs 已升序去重）。
        let mut next = current;
        for &seq in &window.seqs {
            if Some(seq) == next.checked_add(1) {
                next = seq;
            } else {
                break;
            }
        }
        if next == current {
            // 首条观测序号就大于 current+1：缺口就在 current+1。
            // 两个窗口的下界都恰好是 current+1（inbox 用 relay_seq>current，skipped 用
            // key(current+1) 起扫），窗口又总是按升序取最小的若干条：若 current+1 确实
            // 已消费，它必然是窗口里最小的那一条，不可能被截断漏掉。缺口已证明，停止。
            return Ok(());
        }
        current = next;
        write_meta_on(conn, meta_key, &current.to_be_bytes())?;
        let gap = current
            .checked_add(1)
            .ok_or_else(|| StorageError::Corrupt {
                context: format!("{meta_key} 溢出"),
            })?;
        // 两个来源都没被截断 → 已看到 current 之后的全部已消费序号，缺口是真的。
        // 被截断但每个被截断来源都已观测到 ≥ 缺口位置的序号 → 缺口同样是真的
        // （未观测序号都大于各自的窗口最大值）。两种情况都不必再扫。
        if !window.truncated() || window.proves_gap_at(gap) {
            return Ok(());
        }
        // 否则继续下一窗口。典型场景：`skipped_received` 窗口被截断在 4096，而 inbox
        // 窗口里还有一条更大的序号（例如 10000）—— 若沿用「合并后的 truncated 且
        // current < last」判断，这里会因为 current(4096) < last(10000) 提前 return，
        // 把下一个窗口里 ≤ 缺口的 skipped 序号永久漏掉、前缀卡死。
        // 下一轮从新的 current+1 起扫；每个窗口只可能看到更大的序号，所以要么推进、
        // 要么在「首条观测序号 > current+1」处判定缺口并退出，不会死循环。
    }
}

/// 一次前缀扫描的窗口观测结果。
///
/// inbox 行与 `skipped_received` 标记是**两个独立的 `LIMIT` 窗口**：某个来源被截断只
/// 说明该来源还有更大的序号没读到（未观测序号一定大于它自己的最大观测序号），不能拿
/// 另一个来源的窗口去推断缺口是否成立。因此截断状态与最大观测序号都按来源记录。
struct ConsumedScanWindow {
    /// 两个来源合并、升序去重后的已消费序号。
    seqs: Vec<u64>,
    /// inbox 窗口是否被 `LIMIT` 截断。
    inbox_truncated: bool,
    /// `skipped_received` 窗口是否被 `LIMIT` 截断。
    skipped_truncated: bool,
    /// inbox 窗口观测到的最大序号（窗口按序号升序，末条即最大）。
    inbox_last: Option<u64>,
    /// `skipped_received` 窗口观测到的最大序号（键定宽，字典序即序号序）。
    skipped_last: Option<u64>,
}

impl ConsumedScanWindow {
    /// 任一来源还有未观测的已消费序号（需要继续下一个窗口）。
    fn truncated(&self) -> bool {
        self.inbox_truncated || self.skipped_truncated
    }

    /// `gap` 处的缺口是否已被本窗口证明。
    ///
    /// 对每个被截断的来源，未观测序号都严格大于该来源窗口的最大观测序号：只要该最大值
    /// ≥ `gap`，未观测序号就不可能落在 `gap` 上。任一被截断来源不满足这条，缺口就没有
    /// 证明完毕，必须继续下一窗口。
    fn proves_gap_at(&self, gap: u64) -> bool {
        (!self.inbox_truncated || self.inbox_last.is_some_and(|last| last >= gap))
            && (!self.skipped_truncated || self.skipped_last.is_some_and(|last| last >= gap))
    }
}

/// 扫出 `current` 之后**已消费**的序号（升序去重），以及两个来源窗口的截断状态。
///
/// "已消费"= inbox 行满足 [`ConsumedPrefix::inbox_condition`] **或**存在
/// `sync.skipped_received` 标记（ASSET 这类不写 inbox 的事件）。`skipped_received`
/// 键是固定宽度的 `<prefix><hex log_id>.<20 位序号>`，字典序等于序号序，因此可以用
/// `meta` 主键做**范围扫描**，不需要 `LIKE` 全表匹配。
///
/// 两个来源各自带 [`PREFIX_SCAN_LIMIT`] 上界并各自报告截断状态：一次把某条日志的全部
/// 标记读进内存会在长日志上无界增长，而只合并成一个 `truncated` 标志又无法区分"哪个
/// 来源还有更大的序号没读到"，缺口证明就会出错（见 [`ConsumedScanWindow::proves_gap_at`]）。
fn scan_consumed_after_on(
    conn: &Connection,
    log_id: &[u8; 16],
    current: u64,
    prefix: ConsumedPrefix,
) -> Result<ConsumedScanWindow, StorageError> {
    let sql = format!(
        "SELECT relay_seq FROM sync_inbox \
         WHERE log_id=?1 AND relay_seq>?2 AND {} \
         ORDER BY relay_seq LIMIT ?3",
        prefix.inbox_condition()
    );
    let window_limit = i64::try_from(PREFIX_SCAN_LIMIT).unwrap_or(i64::MAX);
    let (mut seqs, inbox_truncated) = {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter([
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(current)?),
            Value::Integer(window_limit),
        ]))?;
        let mut seqs = Vec::new();
        while let Some(row) = rows.next()? {
            let value: i64 = row.get(0)?;
            seqs.push(u64::try_from(value).map_err(|_| StorageError::Corrupt {
                context: "sync_inbox.relay_seq 为负数".to_owned(),
            })?);
        }
        let truncated = seqs.len() == PREFIX_SCAN_LIMIT;
        (seqs, truncated)
    };
    let inbox_last = seqs.last().copied();

    let lower = skipped_received_key(log_id, current.saturating_add(1))?;
    let upper = skipped_received_key(log_id, u64::MAX)?;
    let mut skipped_last: Option<u64> = None;
    let skipped_truncated = {
        let mut statement = conn
            .prepare("SELECT key FROM meta WHERE key>=?1 AND key<=?2 ORDER BY key ASC LIMIT ?3")?;
        let mut rows = statement.query(rusqlite::params_from_iter([
            Value::Text(lower),
            Value::Text(upper),
            Value::Integer(window_limit),
        ]))?;
        let mut scanned = 0usize;
        while let Some(row) = rows.next()? {
            scanned = scanned.saturating_add(1);
            let key: String = row.get(0)?;
            // 格式不认识的键不参与前缀推进（fail-safe）：它既不算已消费，也不提供
            // 截断边界。可解析的未观测序号仍严格大于这里见过的最大可解析序号，所以
            // 用已解析最大值做下界是安全的。
            let Some(seq) = parse_skipped_received_seq(&key) else {
                continue;
            };
            skipped_last = Some(skipped_last.map_or(seq, |last| last.max(seq)));
            if seq > current {
                seqs.push(seq);
            }
        }
        scanned == PREFIX_SCAN_LIMIT
    };

    seqs.sort_unstable();
    seqs.dedup();
    Ok(ConsumedScanWindow {
        seqs,
        inbox_truncated,
        skipped_truncated,
        inbox_last,
        skipped_last,
    })
}

/// 解析 `sync.skipped_received.<hex log_id>.<20 位序号>` 的序号。
///
/// 格式不认识就返回 `None`：不是本函数写下的键不参与前缀推进，宁可让前缀停在这里
/// （fail-safe），也不把来路不明的 meta 键当成"已消费"。
fn parse_skipped_received_seq(key: &str) -> Option<u64> {
    let rest = key.strip_prefix(META_SYNC_SKIPPED_RECEIVED_PREFIX)?;
    let (hex, digits) = rest.split_once('.')?;
    if hex.len() != 32 || digits.len() != 20 {
        return None;
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `sync.skipped_received.<hex log_id>.<seq 20 位>`。
pub(crate) fn skipped_received_key(log_id: &[u8], relay_seq: u64) -> Result<String, StorageError> {
    if log_id.len() != 16 {
        return Err(StorageError::Corrupt {
            context: format!("sync.log_id 长度不是 16：{}", log_id.len()),
        });
    }
    let mut hex = String::with_capacity(32);
    for byte in log_id {
        use core::fmt::Write as _;
        write!(hex, "{byte:02x}").map_err(|_| StorageError::Corrupt {
            context: "sync.log_id 十六进制编码失败".to_owned(),
        })?;
    }
    Ok(format!(
        "{META_SYNC_SKIPPED_RECEIVED_PREFIX}{hex}.{relay_seq:020}"
    ))
}

/// `meta.value` 的**唯一**解读口径：只认 BLOB，其它 SQLite 类型一律是损坏。
///
/// 三个读取入口共享本函数，避免"同一份库读出两套结论"漂移：
///
/// - [`crate::Vault::get_meta`]（actor 线程上的命令）；
/// - `sync_bridge::read_meta`（同步桥经 Vault 命令读取）；
/// - 本模块的 [`read_meta_on`]（worker 独占连接 / 事务内的批量 inbox 校验）。
///
/// 为什么不把其它类型「当字节」或「当缺失」：
///
/// - **当缺失会静默绕过持久屏障**：`sync.repack_required`（轮换后必须重打包）、
///   `sync.log_id`（incarnation 绑定）、`sync.received_seq` 这类键"读不到"就等价于
///   "没有屏障/没有游标"，把一处损坏放大成绕过安全前置条件；
/// - **当字节与 schema 相悖**：`meta.value` 是 `BLOB NOT NULL`（STRICT 表），能读出
///   TEXT/INTEGER/REAL 本身就说明库被外部改坏或 schema 漂移，继续按字节解释只会让
///   损坏值流入上层语义。
///
/// 正常写入路径写不出非 BLOB（STRICT 表会直接拒绝），所以这里命中的都是
/// "库被外部改坏"的场景：宁可 fail-closed 报损坏，也不猜。
pub(crate) fn meta_bytes_of(key: &str, value: Value) -> Result<Vec<u8>, StorageError> {
    match value {
        Value::Blob(bytes) => Ok(bytes),
        other => Err(StorageError::Corrupt {
            context: format!(
                "meta[{key}] 应为 BLOB，实际 {}（库被外部改坏或 schema 漂移）",
                sqlite_type_name(&other)
            ),
        }),
    }
}

/// SQLite 值的类型名（只用于错误信息）。
fn sqlite_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Integer(_) => "INTEGER",
        Value::Real(_) => "REAL",
        Value::Text(_) => "TEXT",
        Value::Blob(_) => "BLOB",
    }
}

/// 在给定连接（或事务）上读一条 meta；键不存在返回 `None`。
///
/// 类型判定走 [`meta_bytes_of`]：只认 BLOB，其它类型是 [`StorageError::Corrupt`]
/// （不再静默当作缺失）。
pub(crate) fn read_meta_on(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
    let mut statement = conn.prepare("SELECT value FROM meta WHERE key=?1")?;
    let mut rows = statement.query([key])?;
    match rows.next()? {
        None => Ok(None),
        Some(row) => Ok(Some(meta_bytes_of(key, row.get::<usize, Value>(0)?)?)),
    }
}

fn read_meta_u64_on(conn: &Connection, key: &str) -> Result<Option<u64>, StorageError> {
    let Some(bytes) = read_meta_on(conn, key)? else {
        return Ok(None);
    };
    if bytes.len() != 8 {
        return Err(StorageError::Corrupt {
            context: format!("{key} 应为 BE64 8B，实际 {}B", bytes.len()),
        });
    }
    Ok(Some(u64::from_be_bytes(bytes.try_into().expect("长度8"))))
}

fn write_meta_on(conn: &Connection, key: &str, value: &[u8]) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO meta(key,value) VALUES(?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

fn array16_bytes(bytes: Vec<u8>, field: &str) -> Result<[u8; 16], StorageError> {
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
            context: format!("{field} 应为 16B，实际 {}B", bytes.len()),
        })
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER"),
    })
}
