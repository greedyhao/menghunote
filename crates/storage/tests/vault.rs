//! 存储层集成测试：建库与 PRAGMA 校验、幂等迁移、并发写入。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use menghunote_storage::{StorageError, Value, Vault, SCHEMA_VERSION};
use tempfile::TempDir;

/// ADR 0001 §7 附录 DDL 以及 v2 同步桥里应当出现的全部表。
const CORE_TABLES: [&str; 17] = [
    "asset_refs",
    "assets",
    "chunks",
    "index_outbox",
    "links",
    "loro_docs",
    "loro_updates",
    "meta",
    "note_tags",
    "notes",
    "projection_outbox",
    "sync_inbox",
    "sync_outbox",
    "sync_outbox_parts",
    "tags",
    "tree",
    "versions",
];

/// 测试用临时目录放在 cargo 为集成测试准备的 `target/tmp` 下。
///
/// 不使用 `tempfile::tempdir()`：本机构建环境里，cargo 启动的测试进程对 `%TEMP%`
/// 没有写权限（`Access denied, os error 5`），而 `CARGO_TARGET_TMPDIR` 一定可写。
fn temp_dir() -> TempDir {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(base).expect("创建 target/tmp");
    tempfile::Builder::new()
        .prefix("vault-test-")
        .tempdir_in(base)
        .expect("在 target/tmp 下创建临时目录")
}

/// 临时目录里一个**父目录尚不存在**的 vault 路径，用来顺带验证建目录逻辑。
fn vault_path(dir: &TempDir) -> PathBuf {
    dir.path().join("vaults").join("primary.sqlite3")
}

/// 打开 vault，失败直接让测试挂掉。
fn open_vault(path: &Path) -> Vault {
    Vault::open(path).expect("打开 vault 应当成功")
}

/// 查询第一行第一列。
fn query_value(vault: &Vault, sql: &str) -> Value {
    let mut rows = vault.query(sql, &[]).expect("查询应当成功");
    assert_eq!(rows.len(), 1, "SQL `{sql}` 期望恰好一行，实际 {rows:?}");
    let mut row = rows.remove(0);
    assert_eq!(row.len(), 1, "SQL `{sql}` 期望恰好一列，实际 {row:?}");
    row.remove(0)
}

/// 查询整数结果。
fn query_i64(vault: &Vault, sql: &str) -> i64 {
    match query_value(vault, sql) {
        Value::Integer(value) => value,
        other => panic!("SQL `{sql}` 期望整数结果，实际 {other:?}"),
    }
}

/// 查询文本结果。
fn query_text(vault: &Vault, sql: &str) -> String {
    match query_value(vault, sql) {
        Value::Text(value) => value,
        other => panic!("SQL `{sql}` 期望文本结果，实际 {other:?}"),
    }
}

/// 统计用户表数量（不算 SQLite 内部表）。
fn count_user_tables(vault: &Vault) -> i64 {
    query_i64(
        vault,
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )
}

#[test]
fn vault_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Vault>();
}

#[test]
fn open_creates_full_schema_and_verifies_pragmas() {
    let dir = temp_dir();
    let path = vault_path(&dir);
    assert!(!path.exists(), "起手不应存在数据库文件");

    let vault = Vault::open(&path).expect("首次打开应当建库成功");

    // 1. 迁移到 v2
    assert_eq!(vault.user_version().expect("读取 user_version"), 2);
    assert_eq!(vault.path(), path.as_path(), "句柄应记录真实路径");

    // 2. 父目录与库文件都被创建出来
    assert!(path.exists(), "数据库文件应当已创建");
    assert!(path.parent().is_some_and(Path::exists), "父目录应当已创建");

    // 3. 核心表齐全
    let rows = vault
        .query(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            &[],
        )
        .expect("查询 sqlite_master");
    let tables: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.first() {
            Some(Value::Text(name)) => Some(name.clone()),
            _ => None,
        })
        .collect();
    for expected in CORE_TABLES {
        assert!(
            tables.iter().any(|name| name == expected),
            "缺少核心表 `{expected}`，实际：{tables:?}"
        );
    }
    assert_eq!(tables.len(), CORE_TABLES.len(), "表数量不符：{tables:?}");

    // 4. 全部是 STRICT 表（ADR §7 要求 SQLite STRICT）
    let non_strict = query_i64(
        &vault,
        "SELECT count(*) FROM sqlite_master \
         WHERE type='table' AND name NOT LIKE 'sqlite_%' AND sql NOT LIKE '%STRICT%'",
    );
    assert_eq!(non_strict, 0, "存在非 STRICT 的表");

    // 5. PRAGMA 读回校验：journal_mode=wal / foreign_keys=1 / busy_timeout=5000
    assert_eq!(
        query_text(&vault, "PRAGMA journal_mode").to_ascii_lowercase(),
        "wal"
    );
    assert_eq!(query_i64(&vault, "PRAGMA foreign_keys"), 1);
    assert_eq!(query_i64(&vault, "PRAGMA busy_timeout"), 5_000);
    // synchronous: 0=OFF / 1=NORMAL / 2=FULL
    assert_eq!(query_i64(&vault, "PRAGMA synchronous"), 2);
}

#[test]
fn reopen_same_path_is_idempotent() {
    let dir = temp_dir();
    let path = vault_path(&dir);

    let table_count_before;
    {
        let vault = open_vault(&path);
        assert_eq!(vault.user_version().expect("读取 user_version"), 2);
        table_count_before = count_user_tables(&vault);
    } // 这里触发 Drop：优雅关闭工作线程、关闭连接

    let vault = open_vault(&path);
    assert_eq!(
        vault.user_version().expect("第二次读取 user_version"),
        2,
        "重复打开不应改变 schema 版本"
    );
    assert_eq!(
        count_user_tables(&vault),
        table_count_before,
        "重复打开不应重复建表"
    );

    // 再开一次，确认数据与版本都稳定
    drop(vault);
    let vault = open_vault(&path);
    assert_eq!(vault.user_version().expect("第三次读取 user_version"), 2);
    assert_eq!(count_user_tables(&vault), table_count_before);
}

#[test]
fn two_threads_write_concurrently_without_busy_failure() {
    let dir = temp_dir();
    let vault = open_vault(&vault_path(&dir));
    let barrier = Arc::new(Barrier::new(2));

    let mut handles = Vec::new();
    for writer in 0..2u32 {
        let vault = vault.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            // 尽量让两个线程同时挤进写入阶段
            barrier.wait();
            for index in 0..100u32 {
                let key = format!("writer-{writer}-row-{index:03}");
                let affected = vault
                    .exec(
                        "INSERT INTO meta(key, value) VALUES(?1, ?2)",
                        &[Value::Text(key.clone()), Value::Blob(key.into_bytes())],
                    )
                    .expect("并发写入不应出现 SQLITE_BUSY 失败（busy_timeout 应吸收等待）");
                assert_eq!(affected, 1, "每次插入应恰好影响一行");
            }
        }));
    }

    for handle in handles {
        handle.join().expect("写入线程不应 panic");
    }

    assert_eq!(query_i64(&vault, "SELECT count(*) FROM meta"), 200);
    assert_eq!(
        query_i64(
            &vault,
            "SELECT count(*) FROM meta WHERE key LIKE 'writer-0-%'"
        ),
        100
    );
    assert_eq!(
        query_i64(
            &vault,
            "SELECT count(*) FROM meta WHERE key LIKE 'writer-1-%'"
        ),
        100
    );
}

#[test]
fn separate_handles_serialize_by_busy_timeout() {
    // 两个独立 Vault（= 两个连接）同时写同一个库：WAL + busy_timeout 应让写锁等待，
    // 而不是直接抛 SQLITE_BUSY。
    let dir = temp_dir();
    let path = vault_path(&dir);
    let vault_a = open_vault(&path);
    let vault_b = Vault::open(&path).expect("同一路径第二次打开应当成功");
    let barrier = Arc::new(Barrier::new(2));

    let mut handles = Vec::new();
    for (writer, vault) in [vault_a.clone(), vault_b.clone()].into_iter().enumerate() {
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            for index in 0..50u32 {
                let key = format!("conn-{writer}-row-{index:03}");
                assert_eq!(
                    insert_with_busy_retry(&vault, &key),
                    1,
                    "每次插入应恰好影响一行"
                );
            }
        }));
    }
    for handle in handles {
        handle.join().expect("写入线程不应 panic");
    }

    assert_eq!(
        query_i64(&vault_a, "SELECT count(*) FROM meta"),
        100,
        "两个连接各写 50 行，合计 100 行"
    );
}

/// 有界重试：`busy_timeout=5000` 是**单次等锁上限**，不是「主机极端过载时必定成功」的保证。
///
/// task-28 实测（一边跑整包、一边跑文件级循环的双重重负载）：两个连接各自 autocommit 的
/// 100 次 INSERT，在 `synchronous=FULL` 下每次提交约 94ms；后到的写者会连续抢不到 WAL 写锁
/// （写者饥饿），失败样本全部是 `insert 0` 在 **≈5.40s** 后返回 `DatabaseBusy`（extended 5，
/// 不是 517 BUSY_SNAPSHOT）——即 busy_timeout 到期，而不是锁升级死锁。
///
/// 生产只有**一个** Vault / 一条写连接，不会自己跟自己抢锁；所以这里只让测试对 BUSY 做
/// 有界退避重试（不改生产默认值），把「两个连接最终都能写完」这个性质测住。
fn insert_with_busy_retry(vault: &Vault, key: &str) -> usize {
    const ATTEMPTS: u32 = 4;
    let mut delay = Duration::from_millis(20);
    for attempt in 1..=ATTEMPTS {
        match vault.exec(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)",
            &[
                Value::Text(key.to_owned()),
                Value::Blob(key.as_bytes().to_vec()),
            ],
        ) {
            Ok(affected) => return affected,
            Err(error) if is_sqlite_busy(&error) && attempt < ATTEMPTS => {
                thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(500));
            }
            Err(error) => panic!("插入 {key} 第 {attempt} 次失败：{error:?}"),
        }
    }
    unreachable!("循环内必定 return 或 panic")
}

/// 只把 SQLITE_BUSY 视为可重试（其它错误原样暴露）。
fn is_sqlite_busy(error: &StorageError) -> bool {
    matches!(
        error,
        StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
            if code.code == rusqlite::ErrorCode::DatabaseBusy
    )
}

#[test]
fn rejects_database_from_newer_schema() {
    let dir = temp_dir();
    let path = vault_path(&dir);
    {
        let vault = open_vault(&path);
        // 模拟"被更新版本的程序升级过"的库；与当前 schema 版本解耦。
        vault
            .exec_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
            .expect("设置 user_version");
    }

    let error = Vault::open(&path).expect_err("更高版本的库必须拒绝打开");
    assert!(
        matches!(
            error,
            StorageError::UnsupportedSchemaVersion {
                found,
                supported
            } if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION
        ),
        "错误类型不符：{error}"
    );
}

#[test]
fn failed_statement_does_not_kill_worker() {
    let dir = temp_dir();
    let vault = open_vault(&vault_path(&dir));

    let error = vault
        .query("SELECT * FROM no_such_table", &[])
        .expect_err("不存在的表应返回错误");
    assert!(
        matches!(error, StorageError::Sqlite(_)),
        "错误类型不符：{error}"
    );

    // 单条语句失败不应影响工作线程，句柄仍可继续使用
    assert_eq!(vault.user_version().expect("读取 user_version"), 2);
    let affected = vault
        .exec(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)",
            &[
                Value::Text("health".to_owned()),
                Value::Blob(b"ok".to_vec()),
            ],
        )
        .expect("工作线程应仍然可用");
    assert_eq!(affected, 1);
    assert_eq!(query_i64(&vault, "SELECT count(*) FROM meta"), 1);
}
