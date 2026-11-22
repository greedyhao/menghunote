//! ADR 0016 **S4d2**：schema 13 → 14（relay 审批收件箱 `approval_jobs`/`approval_signatures`）。
//!
//! 覆盖：
//! 1. 真实 schema 13 库（含 vault/设备/控制链/票据等既有数据）**纯新增**升 14：
//!    所有非收件箱 schema 对象与既有行逐字节不变；两张新表 + 5 个索引到位；
//! 2. 迁移前 `VACUUM INTO <db>.schema13.bak` 快照可独立打开（仍是 schema 13、无收件箱表）；
//! 3. 中断注入 → 单事务整体回滚（版本仍 13、无半张表、既有数据不变）+ 重试成功；
//! 4. 重复打开幂等；缺元数据时按**表形状**识别 13（补跑 13→14）/ 14（不再迁移）；
//!    14 标记只出现一半 → fail closed；
//! 5. 版本门禁：更高版本明确拒绝且不改写数据。**旧 binary 拒 14** 走同一条门禁
//!    （SCHEMA_VERSION=13 的二进制看 14 > 13），这里以 15 验证门禁路径与"不改写数据"。
//!
//! 全部临时文件库 + `FakeClock`，**无 sleep**；不碰任何 wire/租户语义。

#![allow(clippy::type_complexity)]

mod support;

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use support::relay;
use support::relay::db::{schema, Db, DbConfig};
use support::relay::time::FakeClock;

/// 迁移时刻固定值（本迁移不消费时钟，但 `Db::open` 需要 FakeClock）。
const MIGRATION_NOW: i64 = 1_800_000_000;

/// 13→14 新增的索引名（ADR 0016 §收件箱 B）。
const APPROVAL_INDEXES: [&str; 5] = [
    "approval_jobs_recipient",
    "approval_jobs_initiator",
    "approval_jobs_expiry",
    "approval_jobs_terminal",
    "approval_signatures_received",
];

/// 迁移后必须逐字节不变的既有租户/全局事实表。
const PRESERVED_TABLES: [&str; 6] = [
    "vaults",
    "devices",
    "control_commits",
    "manager_keys",
    "device_credentials",
    "key_slots",
];

fn cleanup(path: &Path) {
    support::cleanup_db_dir(path);
}

fn open_db(path: &Path) -> relay::Result<Db> {
    Db::open(
        DbConfig::new(path.to_path_buf()),
        std::sync::Arc::new(FakeClock::new(MIGRATION_NOW)),
    )
}

fn version(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM relay_meta WHERE key = 'schema_version'",
        [],
        |row| row.get::<_, Vec<u8>>(0),
    )
    .ok()
    .map(|bytes| String::from_utf8(bytes).expect("version utf8"))
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .unwrap_or(false)
}

fn index_exists(conn: &Connection, index: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
        [index],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .unwrap_or(false)
}

fn fk_violations(conn: &Connection) -> i64 {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check").expect("prepare");
    let mut rows = stmt.query([]).expect("query");
    let mut count = 0;
    while rows.next().expect("next").is_some() {
        count += 1;
    }
    count
}

/// 一张表的全部行（列按 `SELECT *` 顺序；值用 rusqlite `Value` 的 Debug 形式比较）。
fn dump(conn: &Connection, table: &str) -> Vec<Vec<String>> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
        .expect("prepare dump");
    let names: Vec<String> = stmt.column_names().map(str::to_string).collect();
    let mut rows = stmt.query([]).expect("dump query");
    let mut out = Vec::new();
    while let Some(row) = rows.next().expect("dump row") {
        let mut values = Vec::with_capacity(names.len());
        for index in 0..names.len() {
            let value: rusqlite::types::Value = row.get(index).expect("dump value");
            values.push(format!("{value:?}"));
        }
        out.push(values);
    }
    out
}

/// 全局元数据（`schema_version` 单独断言，不参与逐字节比较）。
fn dump_meta(conn: &Connection) -> Vec<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT key, value FROM relay_meta WHERE key <> 'schema_version' ORDER BY key")
        .expect("prepare meta");
    let mut rows = stmt.query([]).expect("meta query");
    let mut out = Vec::new();
    while let Some(row) = rows.next().expect("meta row") {
        let key: String = row.get(0).expect("key");
        let value: rusqlite::types::Value = row.get(1).expect("value");
        out.push(vec![key, format!("{value:?}")]);
    }
    out
}

/// `sqlite_master` 里的 schema 对象（排除收件箱表/索引），用于证明 13→14 是纯新增。
fn non_approval_schema(conn: &Connection) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT type, name, sql FROM sqlite_master
              WHERE name NOT LIKE 'sqlite_%'
              ORDER BY type, name",
        )
        .expect("prepare schema");
    let mut rows = stmt.query([]).expect("schema query");
    let mut out = Vec::new();
    while let Some(row) = rows.next().expect("schema row") {
        let kind: String = row.get(0).expect("kind");
        let name: String = row.get(1).expect("name");
        if name.starts_with("approval_") {
            continue;
        }
        let sql: Option<String> = row.get(2).expect("sql");
        out.push((format!("{kind}:{name}"), sql.unwrap_or_default()));
    }
    out
}

/// 把真实 schema 14 库**改回纯 schema 13 形状**（删 14 的两张表 + 版本号 + 迁移快照）。
///
/// 13→14 是纯新增迁移，因此"14 库删掉收件箱表"就是 13 库的等价形状；测试用它验证
/// 迁移路径与纯新增性（非收件箱 schema 对象逐字节不变）。
fn strip_to_v13_shape(path: &Path) {
    let conn = Connection::open(path).expect("open raw");
    conn.execute_batch(
        "DROP TABLE IF EXISTS approval_signatures;
         DROP TABLE IF EXISTS approval_jobs;",
    )
    .expect("drop approval tables");
    conn.execute(
        "UPDATE relay_meta SET value = ?1 WHERE key = 'schema_version'",
        [b"13".as_slice()],
    )
    .expect("set version 13");
    drop(conn);
    for backup in [schema::backup_path_v13(path), schema::backup_path_v12(path)] {
        let _ = std::fs::remove_file(backup);
    }
}

/// 建立带真实数据的 schema 14 库，返回其路径与迁移前快照。
struct Snapshot {
    path: PathBuf,
    schema: Vec<(String, String)>,
    meta: Vec<Vec<String>>,
    tables: Vec<(String, Vec<Vec<String>>)>,
}

fn seeded_vault(label: &str) -> Snapshot {
    let fx = support::Fixture::with_real_manager(label);
    let path = fx.cfg.db.path.clone();
    let (schema_objects, meta, tables) = fx
        .db
        .read_instance(|conn| {
            let schema_objects = non_approval_schema(conn);
            let meta = dump_meta(conn);
            let tables = PRESERVED_TABLES
                .iter()
                .map(|table| ((*table).to_string(), dump(conn, table)))
                .collect::<Vec<_>>();
            Ok((schema_objects, meta, tables))
        })
        .expect("snapshot");
    let _path = fx.into_db_path();
    Snapshot {
        path,
        schema: schema_objects,
        meta,
        tables,
    }
}

fn assert_preserved(conn: &Connection, snapshot: &Snapshot) {
    assert_eq!(
        non_approval_schema(conn),
        snapshot.schema,
        "13→14 不得改动任何既有 schema 对象"
    );
    assert_eq!(dump_meta(conn), snapshot.meta, "relay_meta 既有键不得变化");
    for (table, rows) in &snapshot.tables {
        assert_eq!(&dump(conn, table), rows, "{table} 的既有行必须逐字节不变");
    }
}

fn assert_approval_shape(conn: &Connection) {
    assert!(
        table_exists(conn, "approval_jobs"),
        "schema 14 必须有 approval_jobs"
    );
    assert!(
        table_exists(conn, "approval_signatures"),
        "schema 14 必须有 approval_signatures"
    );
    for index in APPROVAL_INDEXES {
        assert!(index_exists(conn, index), "缺少收件箱索引 {index}");
    }
    assert_eq!(fk_violations(conn), 0, "迁移后不得有 FK 违规");
}

// ---------------------------------------------------------------- 1) 全量迁移

#[test]
fn v13_database_migrates_to_v14_additively_preserving_every_fact() {
    let snapshot = seeded_vault("migration-v14-full");
    assert_eq!(
        schema::SCHEMA_VERSION,
        14,
        "本切片必须使用 schema 14（13 已被 web admin 的 vault_registry 占用）"
    );

    // 改回 13 形状后重新打开：必须升级且只**新增**收件箱对象。
    strip_to_v13_shape(&snapshot.path);
    let db = open_db(&snapshot.path).expect("open triggers 13 -> 14");
    db.read_instance(|conn| {
        assert_eq!(version(conn).as_deref(), Some("14"), "必须升到 schema 14");
        assert_preserved(conn, &snapshot);
        assert_approval_shape(conn);
        Ok(())
    })
    .expect("migration asserts");

    // 迁移前快照：可独立打开，仍是 schema 13 且**没有**收件箱表。
    let backup = schema::backup_path_v13(&snapshot.path);
    assert!(backup.exists(), "迁移前必须留 schema 13 快照");
    let backup_conn = Connection::open(&backup).expect("open backup");
    assert_eq!(version(&backup_conn).as_deref(), Some("13"));
    assert!(!table_exists(&backup_conn, "approval_jobs"));
    assert!(!table_exists(&backup_conn, "approval_signatures"));
    drop(backup_conn);

    // 重复打开幂等：不重跑迁移、不改任何数据、不覆盖最早一份快照。
    db.close();
    let snapshot_after = {
        let conn = Connection::open(&snapshot.path).expect("open raw");
        non_approval_schema(&conn)
    };
    let db = open_db(&snapshot.path).expect("reopen is idempotent");
    db.read_instance(|conn| {
        assert_eq!(version(conn).as_deref(), Some("14"));
        assert_eq!(non_approval_schema(conn), snapshot_after);
        assert_approval_shape(conn);
        Ok(())
    })
    .expect("idempotent asserts");
    db.close();
    cleanup(&snapshot.path);
}

// ---------------------------------------------------------------- 2) 中断/回滚

#[test]
fn interrupted_migration_rolls_back_and_retry_succeeds() {
    let snapshot = seeded_vault("migration-v14-fault");
    strip_to_v13_shape(&snapshot.path);
    let _ = std::fs::remove_file(schema::backup_path_v13(&snapshot.path));

    let mut conn = Connection::open(&snapshot.path).expect("open raw");
    schema::configure(&conn).expect("pragmas");
    let error =
        schema::migrate_v13_to_v14_with_fault(&mut conn, schema::MigrationFaultV14::Halfway)
            .expect_err("注入故障必须失败");
    assert!(format!("{error}").contains("injected"));
    // 单事务回滚：版本仍 13、没有任何半张表、既有数据不变。
    assert_eq!(version(&conn).as_deref(), Some("13"));
    assert!(!table_exists(&conn, "approval_jobs"));
    assert!(!table_exists(&conn, "approval_signatures"));
    for (table, rows) in &snapshot.tables {
        assert_eq!(&dump(&conn, table), rows, "{table} 的既有行必须逐字节不变");
    }
    assert_eq!(fk_violations(&conn), 0);
    // 中断发生前已写下维护窗口快照（回滚口径 = 恢复完整快照）。
    assert!(
        schema::backup_path_v13(&snapshot.path).exists(),
        "中断前必须先留 schema 13 快照"
    );

    // 重试（正常路径）成功。
    schema::apply(&mut conn).expect("retry succeeds");
    assert_eq!(version(&conn).as_deref(), Some("14"));
    assert_approval_shape(&conn);
    assert_preserved(&conn, &snapshot);
    drop(conn);
    cleanup(&snapshot.path);
}

// ---------------------------------------------------------------- 3) 版本推断

#[test]
fn missing_metadata_is_inferred_from_shape_and_never_guesses() {
    let snapshot = seeded_vault("migration-v14-no-meta");
    // (a) 14 形状 + 缺元数据 → 识别为 14，不重跑迁移、不产生新快照。
    {
        let conn = Connection::open(&snapshot.path).expect("open raw");
        conn.execute("DELETE FROM relay_meta WHERE key = 'schema_version'", [])
            .expect("drop version");
    }
    let db = open_db(&snapshot.path).expect("14 shape must open");
    db.read_instance(|conn| {
        assert_eq!(
            version(conn).as_deref(),
            Some("14"),
            "14 形状必须写回版本且不再迁移"
        );
        assert_eq!(non_approval_schema(conn), snapshot.schema);
        Ok(())
    })
    .expect("14 shape asserts");
    assert!(
        !schema::backup_path_v13(&snapshot.path).exists(),
        "没有跑 13→14 就不应产生 schema 13 快照"
    );
    db.close();

    // (b) 13 形状 + 缺元数据 → 识别为 13 并补跑 13→14。
    strip_to_v13_shape(&snapshot.path);
    {
        let conn = Connection::open(&snapshot.path).expect("open raw");
        conn.execute("DELETE FROM relay_meta WHERE key = 'schema_version'", [])
            .expect("drop version");
    }
    let db = open_db(&snapshot.path).expect("13 shape must upgrade");
    db.read_instance(|conn| {
        assert_eq!(version(conn).as_deref(), Some("14"));
        assert_approval_shape(conn);
        assert_preserved(conn, &snapshot);
        Ok(())
    })
    .expect("13 shape asserts");
    assert!(
        schema::backup_path_v13(&snapshot.path).exists(),
        "识别为 13 才会执行迁移并留快照"
    );
    db.close();

    // (c) 14 标记只出现一半（两张表中丢一张）→ 矛盾形状 fail closed。
    {
        let conn = Connection::open(&snapshot.path).expect("open raw");
        conn.execute("DROP TABLE approval_signatures", [])
            .expect("drop signatures");
        conn.execute("DELETE FROM relay_meta WHERE key = 'schema_version'", [])
            .expect("drop version");
    }
    let error = open_db(&snapshot.path).expect_err("partial v14 markers must fail closed");
    assert!(
        format!("{error}").contains("partial v14 approval tables"),
        "unexpected error: {error}"
    );
    cleanup(&snapshot.path);
}

// ---------------------------------------------------------------- 4) 版本门禁

#[test]
fn newer_schema_is_rejected_without_touching_data() {
    let snapshot = seeded_vault("migration-v14-newer");
    {
        let conn = Connection::open(&snapshot.path).expect("open raw");
        // 15 > 当前 14：与"13 的旧 binary 看到 14"走**同一条**版本门禁。
        conn.execute(
            "UPDATE relay_meta SET value = ?1 WHERE key = 'schema_version'",
            [b"15".as_slice()],
        )
        .expect("bump");
    }
    let error = open_db(&snapshot.path).expect_err("must refuse newer schema");
    assert!(
        format!("{error}").contains("newer than this binary"),
        "unexpected error: {error}"
    );
    // 拒绝时不得改写版本/数据（等运维恢复快照）。
    let conn = Connection::open(&snapshot.path).expect("open raw");
    assert_eq!(version(&conn).as_deref(), Some("15"));
    assert_eq!(non_approval_schema(&conn), snapshot.schema);
    assert_approval_shape(&conn);
    drop(conn);
    cleanup(&snapshot.path);
}
