//! ADR 0014 **S2b**：schema 10 → 11（新增全局 `create_tickets`）。
//!
//! 覆盖（任务书 §3 第 5 条）：
//! 1. 旧 schema 10 数据完整：v9 全量 fixture → 9→10 迁移 → 10→11 后逐表 / 逐字节不变；
//! 2. 重复打开/重复 apply 幂等；
//! 3. 更新版本的库被拒绝（旧 binary 打开 schema 11 同样落到版本门禁，不会自动降 schema）；
//! 4. 元数据丢失的 v11 形状库能补齐 `create_tickets`（幂等 `IF NOT EXISTS`）；
//! 5. 新建库的 `create_tickets` 列形状与 ADR 0014 §A 完全一致。

#![allow(clippy::type_complexity)]

mod support;

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use support::relay;
use support::relay::db::{schema, Db, DbConfig};
use support::relay::time::FakeClock;

const TENANT_TABLES: &[&str] = &[
    "manager_keys",
    "control_commits",
    "epochs",
    "devices",
    "challenges",
    "tokens",
    "invites",
    "maintenance_reservations",
    "usage",
    "objects",
    "object_parts",
    "log",
    "checkpoints",
    "checkpoint_refs",
    "asset_objects",
    "key_slots",
    "device_grants",
    "rotations",
    "bootstrap_leases",
    "device_acks",
    "dedup",
    "gc_plans",
];

fn temp_path(label: &str) -> PathBuf {
    let path = support::temp_db_path(label);
    support::cleanup_db_dir(&path);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    path
}

fn cleanup(path: &Path) {
    support::cleanup_db_dir(path);
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

fn columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("table info");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("column names")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect columns");
    rows
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap_or_else(|err| panic!("count {table}: {err}"))
}

fn fk_violations(conn: &Connection) -> i64 {
    let mut stmt = conn
        .prepare("PRAGMA foreign_key_check")
        .expect("prepare fk check");
    let mut rows = stmt.query([]).expect("fk check");
    let mut count = 0i64;
    while rows.next().expect("fk row").is_some() {
        count += 1;
    }
    count
}

/// 一张表的**字节级**内容指纹：按列 `quote()`（BLOB → `X'..'`、NULL → `NULL`）拼接并排序。
///
/// 只用于"迁移前后完全一致"的相等断言；不依赖 rowid/物理顺序。
fn dump_table(conn: &Connection, table: &str) -> Vec<String> {
    let columns = columns(conn, table);
    assert!(!columns.is_empty(), "{table} 不存在");
    let expression = columns
        .iter()
        .map(|column| format!("quote({column})"))
        .collect::<Vec<_>>()
        .join("||'|'||");
    let mut stmt = conn
        .prepare(&format!("SELECT {expression} FROM {table} ORDER BY 1"))
        .unwrap_or_else(|err| panic!("prepare dump {table}: {err}"));
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap_or_else(|err| panic!("dump {table}: {err}"))
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap_or_else(|err| panic!("collect dump {table}: {err}"));
    rows
}

/// 全部租户表的内容快照（10→11 不得改动任何一条）。
fn dump_tenants(conn: &Connection) -> Vec<(&'static str, Vec<String>)> {
    TENANT_TABLES
        .iter()
        .map(|table| (*table, dump_table(conn, table)))
        .collect()
}

#[test]
fn v10_data_survives_migration_to_v11_and_reopen_is_idempotent() {
    let path = temp_path("migration-v11-full");
    let conn = support::v9::fixture(&path);
    drop(conn);

    // 先真实走 9 → 10（复用 S2a 的迁移入口 + 全量 v9 fixture）。
    let mut conn = Connection::open(&path).expect("open raw");
    schema::migrate_v9_to_v10_with_fault(&mut conn, schema::MigrationFault::None).expect("9 -> 10");
    assert_eq!(version(&conn).as_deref(), Some("10"));
    assert!(
        !table_exists(&conn, "create_tickets"),
        "schema 10 时尚无 create_tickets（10→11 才新增）"
    );
    let before = dump_tenants(&conn);
    let before_counts: Vec<(&'static str, i64)> = TENANT_TABLES
        .iter()
        .map(|table| (*table, count(&conn, table)))
        .collect();
    let legacy: Option<Vec<u8>> = conn
        .query_row(
            "SELECT vault_id FROM vaults WHERE legacy_default = 1",
            [],
            |row| row.get(0),
        )
        .ok();
    drop(conn);

    // 打开 = apply → 10→11。
    let db = Db::open(
        DbConfig::new(path.clone()),
        std::sync::Arc::new(FakeClock::new(1)),
    )
    .expect("open triggers 10 -> 11");
    db.read_instance(|conn| {
        assert_eq!(version(conn).as_deref(), Some("11"));
        assert!(table_exists(conn, "create_tickets"), "10→11 必须新增票据表");
        assert_eq!(count(conn, "create_tickets"), 0, "迁移不得凭空发票据");
        assert_eq!(columns(conn, "create_tickets"), {
            vec![
                "token_hash".to_string(),
                "vault_id".to_string(),
                "expires_at".to_string(),
                "quota".to_string(),
                "consumed_at".to_string(),
            ]
        });
        assert_eq!(
            dump_tenants(conn),
            before,
            "10→11 不得改动任何租户表（逐字节）"
        );
        assert_eq!(
            TENANT_TABLES
                .iter()
                .map(|table| (*table, count(conn, table)))
                .collect::<Vec<_>>(),
            before_counts
        );
        assert_eq!(fk_violations(conn), 0, "迁移后 FK 必须干净");
        let legacy_after: Option<Vec<u8>> = conn.query_row(
            "SELECT vault_id FROM vaults WHERE legacy_default = 1",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(legacy_after, legacy, "legacy 默认 vault 不变");
        Ok(())
    })
    .expect("schema/version asserts");
    drop(db);

    // 迁移前快照仍是 schema 9（10→11 是纯新增，不需要再备份一遍）。
    let backup = schema::backup_path(&path);
    assert!(backup.exists(), "9→10 备份仍在");
    let backup_conn = Connection::open(&backup).expect("open backup");
    assert_eq!(version(&backup_conn).as_deref(), Some("9"));
    drop(backup_conn);

    // 重复打开幂等：不跑迁移、不改数据。
    let db = Db::open(
        DbConfig::new(path.clone()),
        std::sync::Arc::new(FakeClock::new(1)),
    )
    .expect("reopen");
    db.read_instance(|conn| {
        assert_eq!(version(conn).as_deref(), Some("11"));
        assert_eq!(dump_tenants(conn), before);
        assert_eq!(count(conn, "create_tickets"), 0);
        assert_eq!(fk_violations(conn), 0);
        Ok(())
    })
    .expect("idempotent reopen");
    drop(db);
    cleanup(&path);
}

#[test]
fn fresh_schema_11_has_ticket_table_and_admin_can_issue() {
    let path = temp_path("migration-v11-fresh");
    let db = Db::open(
        DbConfig::new(path.clone()),
        std::sync::Arc::new(FakeClock::new(1_000)),
    )
    .expect("fresh open");
    db.read_instance(|conn| {
        assert_eq!(version(conn).as_deref(), Some("11"));
        assert!(table_exists(conn, "create_tickets"));
        assert_eq!(
            columns(conn, "create_tickets"),
            vec![
                "token_hash".to_string(),
                "vault_id".to_string(),
                "expires_at".to_string(),
                "quota".to_string(),
                "consumed_at".to_string(),
            ]
        );
        Ok(())
    })
    .expect("schema asserts");

    let vault_id = [0x42u8; 16];
    let ticket =
        relay::admin::issue_create_ticket(&db, vault_id, std::time::Duration::from_secs(60), 1)
            .expect("issue");
    let row = relay::admin::load_create_ticket(&db, &ticket)
        .expect("load")
        .expect("row");
    assert_eq!(row.vault_id, vault_id);
    assert_eq!(row.expires_at, 1_060);
    assert_eq!(row.quota, 1);
    assert_eq!(row.consumed_at, None);
    db.close();
    cleanup(&path);
}

#[test]
fn missing_metadata_on_v11_shape_repairs_ticket_table_without_touching_data() {
    let path = temp_path("migration-v11-no-meta");
    let db = Db::open(
        DbConfig::new(path.clone()),
        std::sync::Arc::new(FakeClock::new(1)),
    )
    .expect("open");
    let vault_id = [0x31u8; 16];
    db.write_instance_blocking(move |conn| {
        conn.execute(
            "INSERT INTO create_tickets(token_hash, vault_id, expires_at, quota, consumed_at)
             VALUES (?1, ?2, 1000, 1, NULL)",
            ([0x77u8; 32].as_slice(), vault_id.as_slice()),
        )?;
        Ok(())
    })
    .expect("seed ticket");
    db.close();
    // 模拟"元数据丢失 + 10→11 的 DDL 没跑"：删版本、删票据表，但 vaults 形状仍在。
    let conn = Connection::open(&path).expect("open raw");
    conn.execute("DELETE FROM relay_meta WHERE key = 'schema_version'", [])
        .expect("drop version");
    conn.execute("DROP TABLE create_tickets", [])
        .expect("drop ticket table");
    drop(conn);

    let db = Db::open(
        DbConfig::new(path.clone()),
        std::sync::Arc::new(FakeClock::new(1)),
    )
    .expect("shape-aware repair");
    db.read_instance(|conn| {
        assert_eq!(version(conn).as_deref(), Some("11"));
        assert!(
            table_exists(conn, "create_tickets"),
            "vaults 形状的库必须补齐 10→11 的全局表"
        );
        assert_eq!(count(conn, "create_tickets"), 0, "补救只建空表");
        assert_eq!(count(conn, "vaults"), 0);
        assert_eq!(fk_violations(conn), 0);
        Ok(())
    })
    .expect("repair asserts");
    drop(db);
    cleanup(&path);
}

#[test]
fn future_schema_is_rejected_and_version_is_not_rewritten() {
    let path = temp_path("migration-v11-gate");
    let db = Db::open(
        DbConfig::new(path.clone()),
        std::sync::Arc::new(FakeClock::new(1)),
    )
    .expect("open");
    db.close();
    let future = (schema::SCHEMA_VERSION + 1).to_string();
    let conn = Connection::open(&path).expect("open raw");
    conn.execute(
        "UPDATE relay_meta SET value = ?1 WHERE key = 'schema_version'",
        [future.as_bytes()],
    )
    .expect("bump");
    drop(conn);

    let Err(err) = Db::open(
        DbConfig::new(path.clone()),
        std::sync::Arc::new(FakeClock::new(1)),
    ) else {
        panic!("更新版本的库必须被拒绝（旧 binary 同理）");
    };
    assert!(
        format!("{err}").contains("newer than this binary"),
        "版本门禁文案必须明确: {err}"
    );
    let conn = Connection::open(&path).expect("open raw");
    assert_eq!(version(&conn).as_deref(), Some(future.as_str()));
    assert!(table_exists(&conn, "create_tickets"));
    drop(conn);
    cleanup(&path);
}
