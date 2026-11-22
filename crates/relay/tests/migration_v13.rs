//! ADR 0018 **S2**：schema 12 → 13（权威 `vault_registry` + 票据 `created_at`/registry 关联）。
//!
//! 覆盖（任务书 §3.1）：
//! 1. 真实 schema 12 全量 fixture（`fixtures/relay_schema12.sql`，冻结的 v12 DDL）升 13：
//!    两 vault/票据/设备/控制/租户表按迁移前捕获列集**逐字节不变**；registry ACTIVE 对齐
//!    `vaults`（created_at 取既有值）；未领取票据 → RESERVED/EXPIRED；已消费历史保留；
//! 2. 票据新列 `created_at` = **迁移时刻**（FakeClock 可精确断言；不虚构原始创建时间）；
//! 3. 迁移中断回滚 + 重试 + 重复打开幂等 + 迁移前快照（`<db>.schema12.bak`）；
//! 4. 版本推断修复：缺元数据的 **12** 库必须被识别为 12 并继续迁移（不得标 13 跳过）；
//!    13 形状（registry + created_at）才认 13；矛盾形状 fail closed；
//! 5. 旧 binary 门禁：更高版本号明确拒绝且不改写数据/不写快照。
//!
//! 全部临时文件库 + `FakeClock`，**无 sleep**；不碰 v1 wire/租户表语义。

#![allow(clippy::type_complexity)]

mod support;

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use support::relay;
use support::relay::db::{schema, Db, DbConfig};
use support::relay::ids::{sha256_parts, Hash32, Id16};
use support::relay::registry::LifecycleState;
use support::relay::time::FakeClock;

/// 冻结的 schema 12 DDL（12→13 迁移的起点；由 schema 12 二进制的 `sqlite_master` 导出）。
const V12_DDL: &str = include_str!("fixtures/relay_schema12.sql");

// ---------------------------------------------------------------- 固定 seed

const INSTANCE: Id16 = [0x22u8; 16];
const VAULT_A: Id16 = [0x11u8; 16];
const VAULT_B: Id16 = [0xB2u8; 16];
/// 未消费且未过期、没有 vaults 行 → 迁移后 RESERVED。
const VAULT_RESERVED: Id16 = [0xC1u8; 16];
/// 未消费但已过期、没有 vaults 行 → 迁移后 EXPIRED。
const VAULT_EXPIRED: Id16 = [0xC2u8; 16];
/// 已消费但没有 vaults 行 → 迁移后 DELETED 墓碑（删除时刻未知，不虚构）。
const VAULT_ORPHAN: Id16 = [0xC3u8; 16];

const DEVICE_A: Id16 = [0xA1u8; 16];
const DEVICE_B: Id16 = [0xB4u8; 16];

const TICKET_RESERVED: Hash32 = [0x41u8; 32];
const TICKET_EXPIRED: Hash32 = [0x42u8; 32];
const TICKET_CONSUMED_ORPHAN: Hash32 = [0x43u8; 32];
const TICKET_CONSUMED_ACTIVE: Hash32 = [0x44u8; 32];
const TICKET_RESERVED_FOR_ACTIVE: Hash32 = [0x45u8; 32];

/// 迁移/测试时钟：票据 `created_at` 迁移口径 = 这个时刻。
const MIGRATION_NOW: i64 = 1_000;
/// 预留票据的过期时刻（> MIGRATION_NOW）。
const RESERVED_EXPIRES: i64 = 5_000;
/// 过期票据的过期时刻（<= MIGRATION_NOW）。
const EXPIRED_AT: i64 = 900;

/// schema 12 的 25 张租户表（顺序与 schema 一致；逐字节比较用）。
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
    "device_credentials",
    "management_caps",
    "dedup_v2",
];

// ---------------------------------------------------------------- 通用 helper

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
    stmt.query_map([], |row| row.get::<_, String>(1))
        .expect("column names")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect columns")
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

/// 按给定列集做**字节级**内容指纹：`quote()`（BLOB → `X'..'`、NULL → `NULL`）拼接后排序。
fn dump_rows(conn: &Connection, table: &str, columns: &[String]) -> Vec<String> {
    let expression = columns
        .iter()
        .map(|column| format!("quote({column})"))
        .collect::<Vec<_>>()
        .join("||'|'||");
    let mut stmt = conn
        .prepare(&format!("SELECT {expression} FROM {table} ORDER BY 1"))
        .unwrap_or_else(|err| panic!("prepare dump {table}: {err}"));
    stmt.query_map([], |row| row.get::<_, String>(0))
        .unwrap_or_else(|err| panic!("dump {table}: {err}"))
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap_or_else(|err| panic!("collect dump {table}: {err}"))
}

/// 迁移前的完整快照：每张表（含 `vaults` 与 `create_tickets` 的旧列集）的逐行字节。
struct Snapshot {
    tables: Vec<(&'static str, Vec<String>, Vec<String>)>,
    tickets_columns: Vec<String>,
}

fn capture(conn: &Connection) -> Snapshot {
    let mut tables = vec![("vaults", columns(conn, "vaults"))];
    for table in TENANT_TABLES {
        tables.push((*table, columns(conn, table)));
    }
    let rows: Vec<(&'static str, Vec<String>, Vec<String>)> = tables
        .iter()
        .map(|(table, cols)| (*table, cols.clone(), dump_rows(conn, table, cols)))
        .collect();
    Snapshot {
        tables: rows,
        tickets_columns: columns(conn, "create_tickets"),
    }
}

// ---------------------------------------------------------------- schema 12 fixture

fn create_v12(path: &Path) -> Connection {
    let conn = Connection::open(path).expect("open v12 db");
    conn.busy_timeout(std::time::Duration::from_millis(5_000))
        .expect("busy");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("wal");
    conn.pragma_update(None, "synchronous", "FULL")
        .expect("sync");
    conn.pragma_update(None, "foreign_keys", "ON").expect("fk");
    conn.execute_batch(V12_DDL).expect("v12 ddl");
    conn.execute(
        "INSERT INTO relay_meta(key, value) VALUES ('instance_id', ?1)",
        [INSTANCE.as_slice()],
    )
    .expect("instance");
    conn.execute(
        "INSERT INTO relay_meta(key, value) VALUES ('schema_version', ?1)",
        [b"12".as_slice()],
    )
    .expect("version");
    conn.execute(
        "INSERT INTO relay_meta(key, value) VALUES ('setup.consumed', ?1)",
        [b"1".as_slice()],
    )
    .expect("setup consumed");
    conn
}

/// 控制链：`record_hash = SHA256(prev_hash || signed_record)`（genesis prev = 32×0）。
fn control_hash(prev: &[u8], record: &[u8]) -> Hash32 {
    sha256_parts(&[prev, record])
}

/// 写两个 vault 的租户事实 + 五种票据状态。
fn seed_two_vaults(conn: &Connection) {
    for (vault_id, log_id, inc, cursor, legacy, created) in [
        (
            VAULT_A,
            [0x33u8; 16],
            [0x44u8; 16],
            [0x55u8; 32],
            1i64,
            100i64,
        ),
        (VAULT_B, [0x34u8; 16], [0x45u8; 16], [0x56u8; 32], 0, 200),
    ] {
        conn.execute(
            "INSERT INTO vaults(vault_id, instance_id, log_id, log_incarnation, cursor_key,
                                state, active_epoch, next_epoch, manager_rev, control_rev, auth_rev,
                                head_seq, gc_floor, normal_limit, maintenance_limit,
                                legacy_default, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', 1, 2, 1, 2, 1, 0, 0, 1048576, 2097152,
                     ?6, ?7)",
            (
                vault_id.as_slice(),
                INSTANCE.as_slice(),
                log_id.as_slice(),
                inc.as_slice(),
                cursor.as_slice(),
                legacy,
                created,
            ),
        )
        .expect("vault");
        conn.execute(
            "INSERT INTO manager_keys(vault_id, rev, pubkey, transition, created_at)
             VALUES (?1, 1, ?2, ?3, ?4)",
            (
                vault_id.as_slice(),
                [0x66u8; 32].as_slice(),
                b"genesis-cert".as_slice(),
                created,
            ),
        )
        .expect("manager key");
        // 控制链两条，逐条可重算（迁移不得重写签名链）。
        let genesis = control_hash(&[0u8; 32], b"genesis-cert");
        conn.execute(
            "INSERT INTO control_commits(vault_id, rev, prev_hash, record_hash, signed_record,
                                        created_at)
             VALUES (?1, 1, ?2, ?3, ?4, ?5)",
            (
                vault_id.as_slice(),
                [0u8; 32].as_slice(),
                genesis.as_slice(),
                b"genesis-cert".as_slice(),
                created,
            ),
        )
        .expect("control 1");
        let second = control_hash(genesis.as_slice(), b"rotation-begin");
        conn.execute(
            "INSERT INTO control_commits(vault_id, rev, prev_hash, record_hash, signed_record,
                                        created_at)
             VALUES (?1, 2, ?2, ?3, ?4, ?5)",
            (
                vault_id.as_slice(),
                genesis.as_slice(),
                second.as_slice(),
                b"rotation-begin".as_slice(),
                created + 10,
            ),
        )
        .expect("control 2");
        conn.execute(
            "INSERT INTO epochs(vault_id, epoch, state, created_at)
             VALUES (?1, 1, 'active', ?2)",
            (vault_id.as_slice(), created),
        )
        .expect("epoch");
        conn.execute(
            "INSERT INTO usage(vault_id, normal_bytes, maintenance_bytes)
             VALUES (?1, 600, 400)",
            [vault_id.as_slice()],
        )
        .expect("usage");
    }
    // 设备：两个 vault 各一台（approval NULL = v1 语义）。
    for (vault_id, device_id, created) in [(VAULT_A, DEVICE_A, 100i64), (VAULT_B, DEVICE_B, 200)] {
        conn.execute(
            "INSERT INTO devices(vault_id, device_id, sign_pk, hpke_pk, certificate,
                                 status, created_at)
             VALUES (?1, ?2, ?3, ?3, ?4, 'active', ?5)",
            (
                vault_id.as_slice(),
                device_id.as_slice(),
                [0x77u8; 32].as_slice(),
                b"device-cert".as_slice(),
                created,
            ),
        )
        .expect("device");
    }
    // 邀请/挑战/token：12→13 不清空（与 11→12 的凭据失效是两件事）。
    conn.execute(
        "INSERT INTO invites(vault_id, invite_id, token_hash, expires_at)
         VALUES (?1, ?2, ?3, 5000)",
        (
            VAULT_A.as_slice(),
            [0x12u8; 16].as_slice(),
            [0x13u8; 32].as_slice(),
        ),
    )
    .expect("invite");
    conn.execute(
        "INSERT INTO challenges(vault_id, challenge_id, realm, subject, nonce, expires_at)
         VALUES (?1, ?2, 1, ?3, ?4, 5000)",
        (
            VAULT_A.as_slice(),
            [0xD1u8; 16].as_slice(),
            DEVICE_A.as_slice(),
            [0xE1u8; 32].as_slice(),
        ),
    )
    .expect("challenge");
    conn.execute(
        "INSERT INTO tokens(vault_id, token_hash, realm, subject, auth_rev, expires_at,
                            created_at)
         VALUES (?1, ?2, 1, ?3, 1, 5000, 100)",
        (
            VAULT_A.as_slice(),
            [0xF1u8; 32].as_slice(),
            DEVICE_A.as_slice(),
        ),
    )
    .expect("token");
    // 五种票据状态（旧五列；`created_at` 由 12→13 迁移补迁移时刻）。
    for (token_hash, vault_id, expires_at, quota, consumed_at) in [
        (
            TICKET_RESERVED,
            VAULT_RESERVED,
            RESERVED_EXPIRES,
            1i64,
            None,
        ),
        (TICKET_EXPIRED, VAULT_EXPIRED, EXPIRED_AT, 1, None),
        (TICKET_CONSUMED_ORPHAN, VAULT_ORPHAN, 4_000, 1, Some(950i64)),
        (TICKET_CONSUMED_ACTIVE, VAULT_A, 4_000, 3, Some(950)),
        (
            TICKET_RESERVED_FOR_ACTIVE,
            VAULT_B,
            RESERVED_EXPIRES,
            1,
            None,
        ),
    ] {
        conn.execute(
            "INSERT INTO create_tickets(token_hash, vault_id, expires_at, quota, consumed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                token_hash.as_slice(),
                vault_id.as_slice(),
                expires_at,
                quota,
                consumed_at,
            ),
        )
        .expect("ticket");
    }
}

/// 建库 + 写全量 seed，返回迁移前快照（含票据旧列集）。
fn build_v12(path: &Path) -> (Snapshot, Vec<String>) {
    let conn = create_v12(path);
    seed_two_vaults(&conn);
    let snapshot = capture(&conn);
    let ticket_rows_before = dump_rows(&conn, "create_tickets", &snapshot.tickets_columns);
    (snapshot, ticket_rows_before)
}

fn try_open_db(path: &Path) -> relay::Result<Db> {
    Db::open(
        DbConfig::new(path.to_path_buf()),
        std::sync::Arc::new(FakeClock::new(MIGRATION_NOW)),
    )
}

fn open_db(path: &Path) -> Db {
    // `apply` 会继续跑到当前 SCHEMA_VERSION（12 → 13 → 14）。
    try_open_db(path).expect("open triggers 12 -> 13 -> 14")
}

// ---------------------------------------------------------------- 1) 全量迁移

#[test]
fn v12_two_vault_fixture_migrates_to_v13_preserving_every_fact() {
    let path = temp_path("migration-v13-full");
    let (before, ticket_rows_before) = build_v12(&path);
    let db = open_db(&path);

    db.read_instance(|conn| {
        assert_eq!(
            version(conn).as_deref(),
            Some("14"),
            "必须升到当前 schema（12→13→14）"
        );
        // ---- 租户事实逐字节不变（registry 是唯一新增的生命周期事实来源）----
        for (table, cols, rows) in &before.tables {
            assert_eq!(
                &dump_rows(conn, table, cols),
                rows,
                "{table} 的旧列必须逐字节不变"
            );
        }
        // ---- 票据旧列事实逐字节不变（quota=3 的历史值也不重写）----
        assert_eq!(
            dump_rows(conn, "create_tickets", &before.tickets_columns),
            ticket_rows_before,
            "票据 hash/vault/TTL/quota/consumed_at 必须保留"
        );
        // ---- 新列 created_at = 迁移时刻（不是虚构的原始时间）----
        let created: Vec<(Vec<u8>, i64)> = {
            let mut stmt = conn
                .prepare("SELECT token_hash, created_at FROM create_tickets ORDER BY token_hash")
                .expect("prepare");
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .expect("query")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("collect")
        };
        assert_eq!(created.len(), 5);
        for (hash, created_at) in created {
            assert_eq!(
                created_at, MIGRATION_NOW,
                "token {hash:?} 的 created_at 必须是明确的迁移时刻"
            );
        }
        // ---- registry：存量 vault → ACTIVE，created_at 取既有值 ----
        for (vault_id, expected_created) in [(VAULT_A, 100i64), (VAULT_B, 200i64)] {
            let row = relay::registry::load_raw(conn, &vault_id)
                .expect("registry")
                .expect("registry row");
            assert_eq!(row.state, LifecycleState::Active, "存量 vault 必须 ACTIVE");
            assert_eq!(
                row.created_at, expected_created,
                "ACTIVE 的 created_at 取 vaults 既有值（不虚构）"
            );
            let vaults: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM vaults WHERE vault_id = ?1",
                    [vault_id.as_slice()],
                    |row| row.get(0),
                )
                .expect("vaults count");
            assert_eq!(vaults, 1, "ACTIVE 必须对应 vaults 行");
        }
        // ---- registry：票据来源的生命周期事实 ----
        let reserved = relay::registry::load_raw(conn, &VAULT_RESERVED)
            .expect("registry")
            .expect("row");
        assert_eq!(reserved.state, LifecycleState::Reserved);
        assert_eq!(reserved.created_at, MIGRATION_NOW);
        let expired = relay::registry::load_raw(conn, &VAULT_EXPIRED)
            .expect("registry")
            .expect("row");
        assert_eq!(expired.state, LifecycleState::Expired);
        assert_eq!(expired.created_at, MIGRATION_NOW);
        let orphan = relay::registry::load_raw(conn, &VAULT_ORPHAN)
            .expect("registry")
            .expect("row");
        assert_eq!(
            orphan.state,
            LifecycleState::Deleted,
            "已消费但无 vault 行的票据 → DELETED 墓碑（ID 禁止复用）"
        );
        assert_eq!(
            orphan.deleted_at, None,
            "删除时刻未知：迁移不得虚构 deleted_at"
        );
        assert_eq!(orphan.delete_job_id, None);
        assert_eq!(orphan.delete_progress, 0);
        assert_eq!(orphan.delete_error, None);
        // ---- 已消费票据不会把已 ACTIVE 的 vault 降级/覆盖 ----
        let active_a = relay::registry::load_raw(conn, &VAULT_A)
            .expect("registry")
            .expect("row");
        assert_eq!(active_a.state, LifecycleState::Active);
        // ---- 新列形状 + FK ----
        assert_eq!(
            columns(conn, "create_tickets"),
            vec![
                "token_hash".to_string(),
                "vault_id".to_string(),
                "created_at".to_string(),
                "expires_at".to_string(),
                "quota".to_string(),
                "consumed_at".to_string(),
            ]
        );
        assert_eq!(
            fk_violations(conn),
            0,
            "迁移后 FK 必须干净（票据 → registry）"
        );
        assert_eq!(count(conn, "create_tickets"), 5, "票据行数不变");
        Ok(())
    })
    .expect("post-migration asserts");
    drop(db);

    // ---- 迁移前快照：`<db>.schema12.bak` 是 schema 12 且保留迁移前数据 ----
    let backup = schema::backup_path_v12(&path);
    assert!(backup.exists(), "12→13 必须留 schema 12 快照");
    let backup_conn = Connection::open(&backup).expect("open backup");
    assert_eq!(version(&backup_conn).as_deref(), Some("12"));
    assert!(
        !table_exists(&backup_conn, "vault_registry"),
        "快照必须是迁移前的 schema 12 形状"
    );
    assert_eq!(count(&backup_conn, "create_tickets"), 5);
    assert_eq!(
        columns(&backup_conn, "create_tickets"),
        vec![
            "token_hash".to_string(),
            "vault_id".to_string(),
            "expires_at".to_string(),
            "quota".to_string(),
            "consumed_at".to_string(),
        ],
        "快照里票据仍是旧五列"
    );
    drop(backup_conn);

    // ---- 重复打开幂等：不跑迁移、不改 registry/票据 ----
    let registry_before = {
        let conn = Connection::open(&path).expect("open raw");
        let snapshot = dump_rows(&conn, "vault_registry", &columns(&conn, "vault_registry"));
        let tickets = dump_rows(&conn, "create_tickets", &columns(&conn, "create_tickets"));
        (snapshot, tickets)
    };
    let db = open_db(&path);
    db.read_instance(|conn| {
        assert_eq!(version(conn).as_deref(), Some("14"));
        assert_eq!(
            dump_rows(conn, "vault_registry", &columns(conn, "vault_registry")),
            registry_before.0
        );
        assert_eq!(
            dump_rows(conn, "create_tickets", &columns(conn, "create_tickets")),
            registry_before.1
        );
        assert_eq!(fk_violations(conn), 0);
        Ok(())
    })
    .expect("idempotent reopen");
    db.close();
    cleanup(&path);
}

// ---------------------------------------------------------------- 2) 中断 / 重试

#[test]
fn interrupted_v13_migration_rolls_back_and_retry_succeeds() {
    let path = temp_path("migration-v13-fault");
    let (before, ticket_rows_before) = build_v12(&path);
    let mut conn = Connection::open(&path).expect("open raw");
    let err = schema::migrate_v12_to_v13_with_fault(
        &mut conn,
        schema::MigrationFaultV13::Halfway,
        MIGRATION_NOW,
    )
    .expect_err("注入故障必须失败");
    assert!(format!("{err}").contains("injected migration fault"));
    // ---- 事务回滚：仍是 schema 12 形状，数据一条不少 ----
    assert_eq!(version(&conn).as_deref(), Some("12"), "失败必须回滚版本号");
    assert!(
        !table_exists(&conn, "vault_registry"),
        "回滚后不得残留 registry 表"
    );
    assert_eq!(
        columns(&conn, "create_tickets"),
        before.tickets_columns,
        "回滚后票据仍是旧五列"
    );
    for (table, cols, rows) in &before.tables {
        assert_eq!(&dump_rows(&conn, table, cols), rows, "回滚后 {table} 数据");
    }
    assert_eq!(
        dump_rows(&conn, "create_tickets", &before.tickets_columns),
        ticket_rows_before
    );
    assert_eq!(fk_violations(&conn), 0);
    drop(conn);

    // ---- 中断前已写下维护窗口快照 ----
    let backup = schema::backup_path_v12(&path);
    assert!(backup.exists(), "中断前必须先留快照");
    let backup_conn = Connection::open(&backup).expect("open backup");
    assert_eq!(version(&backup_conn).as_deref(), Some("12"));
    drop(backup_conn);

    // ---- 重试（正常路径）成功，数据仍完整 ----
    let db = open_db(&path);
    db.read_instance(|conn| {
        assert_eq!(version(conn).as_deref(), Some("14"));
        assert!(table_exists(conn, "vault_registry"));
        assert_eq!(fk_violations(conn), 0);
        assert_eq!(count(conn, "create_tickets"), 5);
        assert_eq!(
            relay::registry::load_raw(conn, &VAULT_RESERVED)
                .expect("registry")
                .expect("row")
                .state,
            LifecycleState::Reserved
        );
        Ok(())
    })
    .expect("retry asserts");
    db.close();
    cleanup(&path);
}

// ---------------------------------------------------------------- 3) 版本推断 / 门禁

#[test]
fn missing_metadata_on_v12_shape_is_recognised_as_12_and_migrated() {
    let path = temp_path("migration-v13-no-meta-12");
    let (_before, _tickets) = build_v12(&path);
    // 模拟"元数据丢失但表仍是 v12 形状"：绝不能因为 `vaults.authz_state` 存在就标 13。
    let conn = Connection::open(&path).expect("open raw");
    conn.execute("DELETE FROM relay_meta WHERE key = 'schema_version'", [])
        .expect("drop version");
    drop(conn);

    let db = open_db(&path);
    db.read_instance(|conn| {
        assert_eq!(version(conn).as_deref(), Some("14"));
        assert!(table_exists(conn, "vault_registry"), "12→13 必须真的跑过");
        assert_eq!(
            relay::registry::load_raw(conn, &VAULT_RESERVED)
                .expect("registry")
                .expect("row")
                .state,
            LifecycleState::Reserved,
            "缺元数据的 12 库必须按 12 迁移（不是标 13 跳过）"
        );
        assert_eq!(fk_violations(conn), 0);
        Ok(())
    })
    .expect("shape-aware migration asserts");
    // 真的跑了迁移 → 有 schema 12 快照。
    assert!(
        schema::backup_path_v12(&path).exists(),
        "识别为 12 才会执行迁移并留快照"
    );
    db.close();
    cleanup(&path);
}

#[test]
fn missing_metadata_on_v13_shape_is_recognised_as_13_and_upgrades_to_14() {
    let path = temp_path("migration-v13-no-meta-13");
    let _ = build_v12(&path);
    let db = open_db(&path);
    drop(db);
    // 记录迁移后的 registry 行；把库改回**纯 schema 13 形状**（删掉 14 的两张收件箱表、
    // 版本号与维护窗口快照），再打开——必须被识别成 13（而不是 12/14）并补跑 13→14，
    // 不得重跑 12→13。
    let conn = Connection::open(&path).expect("open raw");
    let registry_rows = dump_rows(&conn, "vault_registry", &columns(&conn, "vault_registry"));
    conn.execute_batch(
        "DROP TABLE approval_signatures; DROP TABLE approval_jobs;
         DELETE FROM relay_meta WHERE key = 'schema_version';",
    )
    .expect("shape back to v13");
    drop(conn);
    std::fs::remove_file(schema::backup_path_v12(&path)).expect("remove snapshot");

    let db = try_open_db(&path).expect("v13 shape must upgrade to 14");
    db.read_instance(|conn| {
        assert_eq!(
            version(conn).as_deref(),
            Some("14"),
            "必须显式识别 13 并补跑 13→14"
        );
        assert!(
            table_exists(conn, "approval_jobs") && table_exists(conn, "approval_signatures"),
            "识别为 13 才会创建收件箱表"
        );
        assert_eq!(
            dump_rows(conn, "vault_registry", &columns(conn, "vault_registry")),
            registry_rows,
            "不得重跑 12→13 改写 registry"
        );
        Ok(())
    })
    .expect("v13 shape asserts");
    assert!(
        !schema::backup_path_v12(&path).exists(),
        "没有重跑 12→13，就不应产生新的 schema 12 快照"
    );
    assert!(
        schema::backup_path_v13(&path).exists(),
        "真的跑了 13→14，必须留 schema 13 快照"
    );
    db.close();
    cleanup(&path);
}

#[test]
fn contradictory_shape_fails_closed() {
    let path = temp_path("migration-v13-ambiguous");
    let _ = build_v12(&path);
    let db = open_db(&path);
    drop(db);
    // 去掉 registry 但留下 13 的 `create_tickets.created_at` → 半迁移形状。
    let conn = Connection::open(&path).expect("open raw");
    // 手工制造矛盾形状（模拟损坏/半迁移）：删版本号让 apply 走形状推断；
    // 关 FK 才能删被票据表引用的 registry 表。
    conn.execute("DELETE FROM relay_meta WHERE key = 'schema_version'", [])
        .expect("drop version");
    conn.pragma_update(None, "foreign_keys", "OFF")
        .expect("fk off");
    conn.execute("DROP TABLE vault_registry", [])
        .expect("drop registry");
    drop(conn);
    let Err(err) = try_open_db(&path) else {
        panic!("矛盾形状必须被拒绝（不能猜版本）");
    };
    assert!(
        format!("{err}").contains("ambiguous"),
        "必须明确报形状矛盾: {err}"
    );
    cleanup(&path);
}

#[test]
fn future_schema_is_rejected_and_version_is_not_rewritten() {
    let path = temp_path("migration-v13-gate");
    let (before, _ticket_rows_before) = build_v12(&path);
    let future = (schema::SCHEMA_VERSION + 1).to_string();
    let conn = Connection::open(&path).expect("open raw");
    conn.execute(
        "UPDATE relay_meta SET value = ?1 WHERE key = 'schema_version'",
        [future.as_bytes()],
    )
    .expect("bump");
    drop(conn);

    let Err(err) = try_open_db(&path) else {
        panic!("更新版本的库必须被拒绝（旧 12 binary 同理）");
    };
    assert!(
        format!("{err}").contains("newer than this binary"),
        "版本门禁文案必须明确: {err}"
    );
    // 拒绝时不得改写版本、不得动数据、不得留快照。
    let conn = Connection::open(&path).expect("open raw");
    assert_eq!(version(&conn).as_deref(), Some(future.as_str()));
    assert!(!table_exists(&conn, "vault_registry"));
    for (table, cols, rows) in &before.tables {
        assert_eq!(&dump_rows(&conn, table, cols), rows, "{table} 行数/字节");
    }
    assert!(!schema::backup_path_v12(&path).exists());
    drop(conn);
    cleanup(&path);
}
