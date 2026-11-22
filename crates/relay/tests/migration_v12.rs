//! ADR 0015 **S4a2**：schema 11 → 12（授权/控制投影 + 迁移 + 约束）。
//!
//! 覆盖（任务书 §2）：
//! 1. 真实 schema 11 全量双租户 fixture（`fixtures/relay_schema11.sql`，冻结的 v11 DDL）升 12：
//!    逐表**按迁移前捕获的列集**的字节/行数/FK/rev/控制链不变；新列默认 `legacy/1/0`；
//! 2. 迁移中断回滚 + 重试 + 重复执行幂等；旧 binary/schema 版本门禁；
//! 3. 约束负例：authz_version/state 非法、active 缺 policy/recovery、cap 跨 vault FK、
//!    challenge/token 跨 vault、管理 token 无 cap、dedup 跨 vault、cert approval rev 不匹配；
//! 4. 旧 v1 全回归在其余测试文件（relay 246 基线）；本文件只碰存储层；
//! 5. 迁移前 WAL 快照（`<db>.schema11.bak`）可独立打开，回滚只恢复完整快照。
//!
//! 与 S2a/S2b 的口径一致：本文件**不**实现任何控制链/cap 鉴权语义（S4b），只证明存储形状、
//! 迁移原子性与数据库层兜底；v1 行为/wire 完全不变。

#![allow(clippy::type_complexity)]

mod support;

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use support::relay;
use support::relay::auth::Realm;
use support::relay::db::{schema, Db, DbConfig};
use support::relay::ids::{sha256_parts, Hash32, Id16};
use support::relay::time::FakeClock;

/// 冻结的 schema 11 DDL（迁移起点；由 schema 11 二进制的 `sqlite_master` 导出）。
const V11_DDL: &str = include_str!("fixtures/relay_schema11.sql");

/// 当前版本号的字符串形式（12→13→14 之后本文件的全量迁移测试都落在 14）。
const SCHEMA_VERSION_STR: &str = "14";

// ---------------------------------------------------------------- 固定 seed

const INSTANCE: Id16 = [0x22u8; 16];
const VAULT_A: Id16 = [0x11u8; 16];
const VAULT_B: Id16 = [0xB2u8; 16];
const DEVICE_A1: Id16 = [0xA1u8; 16];
const DEVICE_A2: Id16 = [0xA2u8; 16];
const DEVICE_A3: Id16 = [0xA3u8; 16];
const DEVICE_B1: Id16 = [0xB4u8; 16];
const MANAGER_A: [u8; 32] = [0x66u8; 32];
const MANAGER_B: [u8; 32] = [0x67u8; 32];
const INVITE_A: Id16 = [0x12u8; 16];
const INVITE_B: Id16 = [0x13u8; 16];
const INVITE_TOKEN_A: Hash32 = [0x1Au8; 32];
const INVITE_TOKEN_B: Hash32 = [0x1Bu8; 32];
const OBJECT_A_UPDATE: Hash32 = [0x21u8; 32];
const OBJECT_A_SLOT: Hash32 = [0x51u8; 32];
const OBJECT_B_UPDATE: Hash32 = [0x23u8; 32];
const OBJECT_B_SLOT: Hash32 = [0x53u8; 32];
const CHECKPOINT_A: Id16 = [0x41u8; 16];
const CHECKPOINT_B: Id16 = [0x42u8; 16];
const ROTATION_A: Id16 = [0x71u8; 16];
const ROTATION_B: Id16 = [0x72u8; 16];
const RESERVATION_A: Id16 = [0x14u8; 16];
const RESERVATION_B: Id16 = [0x17u8; 16];
const LEASE_A: Id16 = [0x81u8; 16];
const LEASE_B: Id16 = [0x82u8; 16];
const CHALLENGE_A: Id16 = [0xD1u8; 16];
const CHALLENGE_B: Id16 = [0xD2u8; 16];
const TOKEN_A: Hash32 = [0xF1u8; 32];
const TOKEN_B: Hash32 = [0xF2u8; 32];
const GRANT_ID_A: Id16 = [0x91u8; 16];
const GRANT_ID_B: Id16 = [0x92u8; 16];
const DEDUP_A: Id16 = [0x15u8; 16];
const DEDUP_B: Id16 = [0x16u8; 16];

/// schema 11 的 22 张租户表（顺序 = DDL 建表顺序）。
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

/// schema 12 新增的表（v11 里不存在）。
const V12_NEW_TABLES: &[&str] = &["device_credentials", "management_caps", "dedup_v2"];

/// schema 12 重建后会**清空**的表（旧凭据一律失效）。
const INVALIDATED_TABLES: &[&str] = &["challenges", "tokens"];

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

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    columns(conn, table).iter().any(|name| name == column)
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
///
/// 列集必须由调用方在**迁移前**捕获：schema 12 只追加列，旧列必须逐字节不变。
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

/// 迁移前的完整快照：每张表（含 `vaults`）的列集 + 逐行字节 + 行数。
struct Snapshot {
    tables: Vec<(&'static str, Vec<String>, Vec<String>)>,
    counts: Vec<(&'static str, i64)>,
}

fn capture(conn: &Connection) -> Snapshot {
    let mut tables = vec![("vaults", columns(conn, "vaults"))];
    let mut names: Vec<&'static str> = Vec::new();
    for table in TENANT_TABLES {
        tables.push((*table, columns(conn, table)));
        names.push(*table);
    }
    // `vaults` 也参与逐字节比较（它是全局表，同样在 11→12 被重建）。
    let rows: Vec<(&'static str, Vec<String>, Vec<String>)> = tables
        .iter()
        .map(|(table, cols)| (*table, cols.clone(), dump_rows(conn, table, cols)))
        .collect();
    let counts = names
        .iter()
        .map(|table| (*table, count(conn, table)))
        .collect();
    Snapshot {
        tables: rows,
        counts,
    }
}

fn assert_preserved(conn: &Connection, before: &Snapshot) {
    for (table, cols, rows) in &before.tables {
        if INVALIDATED_TABLES.contains(table) {
            assert_eq!(count(conn, table), 0, "迁移必须让旧 {table} 失效");
            continue;
        }
        assert_eq!(
            &dump_rows(conn, table, cols),
            rows,
            "{table} 的旧列必须逐字节不变（行数/顺序/字节）"
        );
    }
    for (table, expected) in &before.counts {
        if INVALIDATED_TABLES.contains(table) {
            continue;
        }
        assert_eq!(count(conn, table), *expected, "{table} 行数必须不变");
    }
}

// ---------------------------------------------------------------- schema 11 fixture

fn create_v11(path: &Path) -> Connection {
    let conn = Connection::open(path).expect("open v11 db");
    conn.busy_timeout(std::time::Duration::from_millis(5_000))
        .expect("busy");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("wal");
    conn.pragma_update(None, "synchronous", "FULL")
        .expect("sync");
    conn.pragma_update(None, "foreign_keys", "ON").expect("fk");
    conn.execute_batch(V11_DDL).expect("v11 ddl");
    conn.execute(
        "INSERT INTO relay_meta(key, value) VALUES ('instance_id', ?1)",
        [INSTANCE.as_slice()],
    )
    .expect("instance");
    conn.execute(
        "INSERT INTO relay_meta(key, value) VALUES ('schema_version', ?1)",
        [b"11".as_slice()],
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

/// 写入**两个** vault 的全量租户数据（22 张表都有行；含旧 challenge/token/邀请事实）。
fn seed_two_vaults(conn: &Connection) {
    let mgmt_subject_b = Realm::management_subject(&MANAGER_B);
    // ---- vaults ----
    for (vault_id, log_id, inc, cursor, legacy, created, control_rev, auth_rev, head, floor) in [
        (
            VAULT_A,
            [0x33u8; 16],
            [0x44u8; 16],
            [0x55u8; 32],
            1i64,
            100i64,
            2i64,
            4i64,
            3i64,
            1i64,
        ),
        (
            VAULT_B,
            [0x34u8; 16],
            [0x45u8; 16],
            [0x56u8; 32],
            0,
            200,
            2,
            4,
            2,
            1,
        ),
    ] {
        conn.execute(
            "INSERT INTO vaults(vault_id, instance_id, log_id, log_incarnation, cursor_key,
                                state, active_epoch, next_epoch, manager_rev, control_rev, auth_rev,
                                head_seq, gc_floor, normal_limit, maintenance_limit,
                                legacy_default, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', 1, 3, 2, ?6, ?7, ?8, ?9, 1048576, 2097152,
                     ?10, ?11)",
            (
                vault_id.as_slice(),
                INSTANCE.as_slice(),
                log_id.as_slice(),
                inc.as_slice(),
                cursor.as_slice(),
                control_rev,
                auth_rev,
                head,
                floor,
                legacy,
                created,
            ),
        )
        .expect("vault");
    }
    // ---- manager_keys ----
    conn.execute(
        "INSERT INTO manager_keys(vault_id, rev, pubkey, transition, created_at)
         VALUES (?1, 1, ?2, ?3, 100)",
        (
            VAULT_A.as_slice(),
            MANAGER_A.as_slice(),
            b"genesis-cert-a".as_slice(),
        ),
    )
    .expect("manager a");
    conn.execute(
        "INSERT INTO manager_keys(vault_id, rev, pubkey, transition, created_at)
         VALUES (?1, 1, ?2, ?3, 200)",
        (
            VAULT_B.as_slice(),
            MANAGER_B.as_slice(),
            b"genesis-cert-b".as_slice(),
        ),
    )
    .expect("manager b");
    // ---- control_commits（两条链都按真实公式算）----
    let a1 = control_hash(&[0u8; 32], b"genesis-cert-a");
    let a2 = control_hash(a1.as_slice(), b"rotation-begin-a");
    conn.execute(
        "INSERT INTO control_commits(vault_id, rev, prev_hash, record_hash, signed_record,
                                     created_at)
         VALUES (?1, 1, ?2, ?3, ?4, 100)",
        (
            VAULT_A.as_slice(),
            [0u8; 32].as_slice(),
            a1.as_slice(),
            b"genesis-cert-a".as_slice(),
        ),
    )
    .expect("control a1");
    conn.execute(
        "INSERT INTO control_commits(vault_id, rev, prev_hash, record_hash, signed_record,
                                     created_at)
         VALUES (?1, 2, ?2, ?3, ?4, 150)",
        (
            VAULT_A.as_slice(),
            a1.as_slice(),
            a2.as_slice(),
            b"rotation-begin-a".as_slice(),
        ),
    )
    .expect("control a2");
    let b1 = control_hash(&[0u8; 32], b"genesis-cert-b");
    let b2 = control_hash(b1.as_slice(), b"rotation-begin-b");
    conn.execute(
        "INSERT INTO control_commits(vault_id, rev, prev_hash, record_hash, signed_record,
                                     created_at)
         VALUES (?1, 1, ?2, ?3, ?4, 200)",
        (
            VAULT_B.as_slice(),
            [0u8; 32].as_slice(),
            b1.as_slice(),
            b"genesis-cert-b".as_slice(),
        ),
    )
    .expect("control b1");
    conn.execute(
        "INSERT INTO control_commits(vault_id, rev, prev_hash, record_hash, signed_record,
                                     created_at)
         VALUES (?1, 2, ?2, ?3, ?4, 250)",
        (
            VAULT_B.as_slice(),
            b1.as_slice(),
            b2.as_slice(),
            b"rotation-begin-b".as_slice(),
        ),
    )
    .expect("control b2");
    // ---- epochs ----
    for (vault, epoch, state, revoked_once, created) in [
        (VAULT_A, 1i64, "active", 0i64, 100i64),
        (VAULT_A, 2, "staged", 1, 150),
        (VAULT_B, 1, "active", 0, 200),
        (VAULT_B, 2, "staged", 0, 250),
    ] {
        conn.execute(
            "INSERT INTO epochs(vault_id, epoch, state, revoked_once, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (vault.as_slice(), epoch, state, revoked_once, created),
        )
        .expect("epoch");
    }
    // ---- devices ----
    conn.execute(
        "INSERT INTO devices(vault_id, device_id, sign_pk, hpke_pk, certificate,
                             certificate_signature, status, created_at, revoked_at, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', 100, NULL, 200)",
        (
            VAULT_A.as_slice(),
            DEVICE_A1.as_slice(),
            [0xB1u8; 32].as_slice(),
            [0xC1u8; 32].as_slice(),
            b"cert-a1".as_slice(),
            b"sig-a1".as_slice(),
        ),
    )
    .expect("device a1");
    conn.execute(
        "INSERT INTO devices(vault_id, device_id, sign_pk, hpke_pk, certificate,
                             certificate_signature, status, created_at, revoked_at, last_seen)
         VALUES (?1, ?2, ?3, ?4, X'', NULL, 'pending', 110, NULL, NULL)",
        (
            VAULT_A.as_slice(),
            DEVICE_A2.as_slice(),
            [0xB2u8; 32].as_slice(),
            [0xC2u8; 32].as_slice(),
        ),
    )
    .expect("device a2");
    conn.execute(
        "INSERT INTO devices(vault_id, device_id, sign_pk, hpke_pk, certificate,
                             certificate_signature, status, created_at, revoked_at, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'revoked', 100, 120, 130)",
        (
            VAULT_A.as_slice(),
            DEVICE_A3.as_slice(),
            [0xB3u8; 32].as_slice(),
            [0xC3u8; 32].as_slice(),
            b"cert-a3".as_slice(),
            b"sig-a3".as_slice(),
        ),
    )
    .expect("device a3");
    conn.execute(
        "INSERT INTO devices(vault_id, device_id, sign_pk, hpke_pk, certificate,
                             certificate_signature, status, created_at, revoked_at, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', 200, NULL, 300)",
        (
            VAULT_B.as_slice(),
            DEVICE_B1.as_slice(),
            [0xB4u8; 32].as_slice(),
            [0xC4u8; 32].as_slice(),
            b"cert-b1".as_slice(),
            b"sig-b1".as_slice(),
        ),
    )
    .expect("device b1");
    // ---- challenges / tokens（v11：realm 是 TEXT；迁移必须清空）----
    conn.execute(
        "INSERT INTO challenges(vault_id, challenge_id, realm, subject, nonce, expires_at, used_at)
         VALUES (?1, ?2, 'device', ?3, ?4, 1000, NULL)",
        (
            VAULT_A.as_slice(),
            CHALLENGE_A.as_slice(),
            DEVICE_A1.as_slice(),
            [0xE1u8; 32].as_slice(),
        ),
    )
    .expect("challenge a");
    conn.execute(
        "INSERT INTO challenges(vault_id, challenge_id, realm, subject, nonce, expires_at, used_at)
         VALUES (?1, ?2, 'management', ?3, ?4, 1000, 900)",
        (
            VAULT_B.as_slice(),
            CHALLENGE_B.as_slice(),
            mgmt_subject_b.as_slice(),
            [0xE2u8; 32].as_slice(),
        ),
    )
    .expect("challenge b");
    conn.execute(
        "INSERT INTO tokens(vault_id, token_hash, realm, subject, auth_rev, expires_at, created_at)
         VALUES (?1, ?2, 'device', ?3, 4, 1000, 100)",
        (VAULT_A.as_slice(), TOKEN_A.as_slice(), DEVICE_A1.as_slice()),
    )
    .expect("token a");
    conn.execute(
        "INSERT INTO tokens(vault_id, token_hash, realm, subject, auth_rev, expires_at, created_at)
         VALUES (?1, ?2, 'management', ?3, 4, 1000, 200)",
        (
            VAULT_B.as_slice(),
            TOKEN_B.as_slice(),
            mgmt_subject_b.as_slice(),
        ),
    )
    .expect("token b");
    // ---- invites（已消费邀请事实必须保留）----
    conn.execute(
        "INSERT INTO invites(vault_id, invite_id, token_hash, expires_at, pending_device, used_at)
         VALUES (?1, ?2, ?3, 2000, ?4, 150)",
        (
            VAULT_A.as_slice(),
            INVITE_A.as_slice(),
            INVITE_TOKEN_A.as_slice(),
            DEVICE_A2.as_slice(),
        ),
    )
    .expect("invite a");
    conn.execute(
        "INSERT INTO invites(vault_id, invite_id, token_hash, expires_at, pending_device, used_at)
         VALUES (?1, ?2, ?3, 3000, NULL, NULL)",
        (
            VAULT_B.as_slice(),
            INVITE_B.as_slice(),
            INVITE_TOKEN_B.as_slice(),
        ),
    )
    .expect("invite b");
    // ---- maintenance_reservations ----
    conn.execute(
        "INSERT INTO maintenance_reservations(vault_id, reservation_id, purpose, resource_id,
                                               owner_device, reserved_bytes, consumed_bytes,
                                               state, expires_at)
         VALUES (?1, ?2, 'checkpoint', ?3, ?4, 1000, 400, 'active', 2000)",
        (
            VAULT_A.as_slice(),
            RESERVATION_A.as_slice(),
            [0x18u8; 16].as_slice(),
            DEVICE_A1.as_slice(),
        ),
    )
    .expect("reservation a");
    conn.execute(
        "INSERT INTO maintenance_reservations(vault_id, reservation_id, purpose, resource_id,
                                               owner_device, reserved_bytes, consumed_bytes,
                                               state, expires_at)
         VALUES (?1, ?2, 'rotation', ?3, ?4, 2048, 1024, 'complete', 3000)",
        (
            VAULT_B.as_slice(),
            RESERVATION_B.as_slice(),
            [0x19u8; 16].as_slice(),
            DEVICE_B1.as_slice(),
        ),
    )
    .expect("reservation b");
    // ---- usage ----
    conn.execute(
        "INSERT INTO usage(vault_id, normal_bytes, maintenance_bytes) VALUES (?1, 1234, 400)",
        [VAULT_A.as_slice()],
    )
    .expect("usage a");
    conn.execute(
        "INSERT INTO usage(vault_id, normal_bytes, maintenance_bytes) VALUES (?1, 77, 0)",
        [VAULT_B.as_slice()],
    )
    .expect("usage b");
    // ---- objects（含一个 staging + reservation，覆盖复合 FK）----
    conn.execute(
        "INSERT INTO objects(vault_id, object_id, message_id, producer, epoch, type, generation,
                             sender_seq, part_count, plain_len, state, budget_class, digest,
                             signed_commit, commit_signature, reservation_id, created_at,
                             touched_at, completed_at)
         VALUES (?1, ?2, ?3, ?4, 1, 1, 0, 1, 1, 128, 'complete', 'normal', ?5, ?6, ?7, NULL,
                 100, 200, 200)",
        (
            VAULT_A.as_slice(),
            OBJECT_A_UPDATE.as_slice(),
            [0x24u8; 16].as_slice(),
            DEVICE_A1.as_slice(),
            [0x25u8; 32].as_slice(),
            b"commit-a".as_slice(),
            b"csig-a".as_slice(),
        ),
    )
    .expect("object a update");
    conn.execute(
        "INSERT INTO objects(vault_id, object_id, message_id, producer, epoch, type, generation,
                             sender_seq, part_count, plain_len, state, budget_class, digest,
                             signed_commit, commit_signature, reservation_id, created_at,
                             touched_at, completed_at)
         VALUES (?1, ?2, ?3, ?4, 1, 4, 0, 2, 1, 64, 'complete', 'maintenance', ?5, NULL, NULL, ?6,
                 150, 200, 200)",
        (
            VAULT_A.as_slice(),
            OBJECT_A_SLOT.as_slice(),
            [0x26u8; 16].as_slice(),
            DEVICE_A1.as_slice(),
            [0x27u8; 32].as_slice(),
            RESERVATION_A.as_slice(),
        ),
    )
    .expect("object a slot");
    conn.execute(
        "INSERT INTO objects(vault_id, object_id, message_id, producer, epoch, type, generation,
                             sender_seq, part_count, plain_len, state, budget_class, digest,
                             signed_commit, commit_signature, reservation_id, created_at,
                             touched_at, completed_at)
         VALUES (?1, ?2, ?3, ?4, 1, 1, 0, 1, 1, 96, 'complete', 'normal', ?5, ?6, ?7, NULL,
                 200, 250, 250)",
        (
            VAULT_B.as_slice(),
            OBJECT_B_UPDATE.as_slice(),
            [0x28u8; 16].as_slice(),
            DEVICE_B1.as_slice(),
            [0x29u8; 32].as_slice(),
            b"commit-b".as_slice(),
            b"csig-b".as_slice(),
        ),
    )
    .expect("object b update");
    conn.execute(
        "INSERT INTO objects(vault_id, object_id, message_id, producer, epoch, type, generation,
                             sender_seq, part_count, plain_len, state, budget_class, digest,
                             signed_commit, commit_signature, reservation_id, created_at,
                             touched_at, completed_at)
         VALUES (?1, ?2, ?3, ?4, 1, 4, 0, 2, 1, 32, 'complete', 'normal', ?5, NULL, NULL, NULL,
                 200, 250, 250)",
        (
            VAULT_B.as_slice(),
            OBJECT_B_SLOT.as_slice(),
            [0x2Au8; 16].as_slice(),
            DEVICE_B1.as_slice(),
            [0x2Bu8; 32].as_slice(),
        ),
    )
    .expect("object b slot");
    // ---- object_parts ----
    for (vault, object_id, digest) in [
        (VAULT_A, OBJECT_A_UPDATE, [0x2Cu8; 32]),
        (VAULT_A, OBJECT_A_SLOT, [0x2Du8; 32]),
        (VAULT_B, OBJECT_B_UPDATE, [0x2Eu8; 32]),
        (VAULT_B, OBJECT_B_SLOT, [0x2Fu8; 32]),
    ] {
        conn.execute(
            "INSERT INTO object_parts(vault_id, object_id, part_no, envelope, digest)
             VALUES (?1, ?2, 0, ?3, ?4)",
            (
                vault.as_slice(),
                object_id.as_slice(),
                [0x5Au8; 300].as_slice(),
                digest.as_slice(),
            ),
        )
        .expect("object part");
    }
    // ---- log ----
    conn.execute(
        "INSERT INTO log(vault_id, relay_seq, kind, object_id, epoch, event, signature, created_at)
         VALUES (?1, 1, 'update', ?2, 1, ?3, ?4, 100)",
        (
            VAULT_A.as_slice(),
            OBJECT_A_UPDATE.as_slice(),
            b"event-a1".as_slice(),
            b"lsig-a1".as_slice(),
        ),
    )
    .expect("log a1");
    conn.execute(
        "INSERT INTO log(vault_id, relay_seq, kind, object_id, epoch, event, signature, created_at)
         VALUES (?1, 2, 'asset', ?2, 1, ?3, NULL, 200)",
        (
            VAULT_A.as_slice(),
            OBJECT_A_SLOT.as_slice(),
            b"event-a2".as_slice(),
        ),
    )
    .expect("log a2");
    conn.execute(
        "INSERT INTO log(vault_id, relay_seq, kind, object_id, epoch, event, signature, created_at)
         VALUES (?1, 1, 'update', ?2, 1, ?3, ?4, 200)",
        (
            VAULT_B.as_slice(),
            OBJECT_B_UPDATE.as_slice(),
            b"event-b1".as_slice(),
            b"lsig-b1".as_slice(),
        ),
    )
    .expect("log b1");
    // ---- checkpoints / checkpoint_refs ----
    conn.execute(
        "INSERT INTO checkpoints(vault_id, checkpoint_id, epoch, covered_seq, manifest_oid,
                                 refs_digest, signed_commit, commit_signature, state, role,
                                 validation_version, health, created_at, published_at)
         VALUES (?1, ?2, 1, 2, ?3, ?4, ?5, ?6, 'published', 'primary', 1, 'healthy', 100, 200)",
        (
            VAULT_A.as_slice(),
            CHECKPOINT_A.as_slice(),
            [0x71u8; 32].as_slice(),
            [0x72u8; 32].as_slice(),
            b"cp-a".as_slice(),
            b"cpsig-a".as_slice(),
        ),
    )
    .expect("checkpoint a");
    conn.execute(
        "INSERT INTO checkpoints(vault_id, checkpoint_id, epoch, covered_seq, manifest_oid,
                                 refs_digest, signed_commit, commit_signature, state, role,
                                 validation_version, health, created_at, published_at)
         VALUES (?1, ?2, 1, 1, ?3, NULL, ?4, NULL, 'published', 'fallback', 2, 'unknown', 200, 250)",
        (
            VAULT_B.as_slice(),
            CHECKPOINT_B.as_slice(),
            [0x73u8; 32].as_slice(),
            b"cp-b".as_slice(),
        ),
    )
    .expect("checkpoint b");
    for (vault, checkpoint, object_id, role) in [
        (VAULT_A, CHECKPOINT_A, OBJECT_A_UPDATE, "data"),
        (VAULT_A, CHECKPOINT_A, OBJECT_A_SLOT, "manifest"),
        (VAULT_B, CHECKPOINT_B, OBJECT_B_UPDATE, "data"),
    ] {
        conn.execute(
            "INSERT INTO checkpoint_refs(vault_id, checkpoint_id, object_id, role)
             VALUES (?1, ?2, ?3, ?4)",
            (
                vault.as_slice(),
                checkpoint.as_slice(),
                object_id.as_slice(),
                role,
            ),
        )
        .expect("checkpoint ref");
    }
    // ---- asset_objects ----
    conn.execute(
        "INSERT INTO asset_objects(vault_id, epoch, object_id, state, finalized_seq, rotation_id)
         VALUES (?1, 1, ?2, 'active', 2, NULL)",
        (VAULT_A.as_slice(), [0x74u8; 32].as_slice()),
    )
    .expect("asset a");
    conn.execute(
        "INSERT INTO asset_objects(vault_id, epoch, object_id, state, finalized_seq, rotation_id)
         VALUES (?1, 1, ?2, 'retired', 1, ?3)",
        (
            VAULT_B.as_slice(),
            [0x75u8; 32].as_slice(),
            ROTATION_B.as_slice(),
        ),
    )
    .expect("asset b");
    // ---- key_slots（FK → objects）----
    conn.execute(
        "INSERT INTO key_slots(vault_id, slot_id, epoch, kind, descriptor, state, slot_set_rev,
                               retire_at, created_at)
         VALUES (?1, ?2, 1, 'password', ?3, 'active', 1, NULL, 100)",
        (
            VAULT_A.as_slice(),
            OBJECT_A_SLOT.as_slice(),
            b"slot-a".as_slice(),
        ),
    )
    .expect("slot a");
    conn.execute(
        "INSERT INTO key_slots(vault_id, slot_id, epoch, kind, descriptor, state, slot_set_rev,
                               retire_at, created_at)
         VALUES (?1, ?2, 1, 'recovery', ?3, 'retiring', 2, 9000, 200)",
        (
            VAULT_B.as_slice(),
            OBJECT_B_SLOT.as_slice(),
            b"slot-b".as_slice(),
        ),
    )
    .expect("slot b");
    // ---- device_grants ----
    conn.execute(
        "INSERT INTO device_grants(vault_id, epoch, device_id, manager_rev, hpke_enc,
                                   hpke_ciphertext, signed_header, signature, grant_id,
                                   activation_ref)
         VALUES (?1, 1, ?2, 1, ?3, ?4, ?5, ?6, ?7, NULL)",
        (
            VAULT_A.as_slice(),
            DEVICE_A1.as_slice(),
            [0x61u8; 32].as_slice(),
            b"ct-a".as_slice(),
            b"hdr-a".as_slice(),
            b"gsig-a".as_slice(),
            GRANT_ID_A.as_slice(),
        ),
    )
    .expect("grant a");
    conn.execute(
        "INSERT INTO device_grants(vault_id, epoch, device_id, manager_rev, hpke_enc,
                                   hpke_ciphertext, signed_header, signature, grant_id,
                                   activation_ref)
         VALUES (?1, 1, ?2, 2, ?3, ?4, ?5, NULL, ?6, ?7)",
        (
            VAULT_B.as_slice(),
            DEVICE_B1.as_slice(),
            [0x62u8; 32].as_slice(),
            b"ct-b".as_slice(),
            b"hdr-b".as_slice(),
            GRANT_ID_B.as_slice(),
            ROTATION_B.as_slice(),
        ),
    )
    .expect("grant b");
    // ---- rotations ----
    conn.execute(
        "INSERT INTO rotations(vault_id, rotation_id, from_epoch, to_epoch, freeze_seq,
                               member_set_hash, asset_set_digest, revoked_count, state,
                               primary_checkpoint, fallback_checkpoint, activation_commit,
                               activated_at, activated_control_rev, created_at, updated_at)
         VALUES (?1, ?2, 1, 2, 1, ?3, ?4, 0, 'activated', ?5, NULL, ?6, 200, 2, 150, 200)",
        (
            VAULT_A.as_slice(),
            ROTATION_A.as_slice(),
            [0x76u8; 32].as_slice(),
            [0x77u8; 32].as_slice(),
            CHECKPOINT_A.as_slice(),
            [0x78u8; 32].as_slice(),
        ),
    )
    .expect("rotation a");
    conn.execute(
        "INSERT INTO rotations(vault_id, rotation_id, from_epoch, to_epoch, freeze_seq,
                               member_set_hash, asset_set_digest, revoked_count, state,
                               primary_checkpoint, fallback_checkpoint, activation_commit,
                               activated_at, activated_control_rev, created_at, updated_at)
         VALUES (?1, ?2, 1, 2, 0, NULL, NULL, 1, 'aborted', NULL, NULL, NULL, NULL, NULL, 250, 300)",
        (VAULT_B.as_slice(), ROTATION_B.as_slice()),
    )
    .expect("rotation b");
    // ---- bootstrap_leases ----
    conn.execute(
        "INSERT INTO bootstrap_leases(vault_id, lease_id, device_id, primary_checkpoint,
                                      fallback_checkpoint, floor_seq, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?4, 1, 200, 3000)",
        (
            VAULT_A.as_slice(),
            LEASE_A.as_slice(),
            DEVICE_A1.as_slice(),
            CHECKPOINT_A.as_slice(),
        ),
    )
    .expect("lease a");
    conn.execute(
        "INSERT INTO bootstrap_leases(vault_id, lease_id, device_id, primary_checkpoint,
                                      fallback_checkpoint, floor_seq, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?4, 1, 250, 4000)",
        (
            VAULT_B.as_slice(),
            LEASE_B.as_slice(),
            DEVICE_B1.as_slice(),
            CHECKPOINT_B.as_slice(),
        ),
    )
    .expect("lease b");
    // ---- device_acks ----
    conn.execute(
        "INSERT INTO device_acks(vault_id, device_id, received_seq, integrated_seq, epoch,
                                 updated_at)
         VALUES (?1, ?2, 3, 2, 1, 200)",
        (VAULT_A.as_slice(), DEVICE_A1.as_slice()),
    )
    .expect("ack a");
    conn.execute(
        "INSERT INTO device_acks(vault_id, device_id, received_seq, integrated_seq, epoch,
                                 updated_at)
         VALUES (?1, ?2, 2, 1, 1, 300)",
        (VAULT_B.as_slice(), DEVICE_B1.as_slice()),
    )
    .expect("ack b");
    // ---- dedup（v1 管理写幂等；11→12 不得动它）----
    conn.execute(
        "INSERT INTO dedup(vault_id, scope, id, request_hash, accepted_seq, response, created_at)
         VALUES (?1, 'management/begin', ?2, ?3, 1, ?4, 200)",
        (
            VAULT_A.as_slice(),
            DEDUP_A.as_slice(),
            [0x31u8; 32].as_slice(),
            b"ack-response-a".as_slice(),
        ),
    )
    .expect("dedup a");
    conn.execute(
        "INSERT INTO dedup(vault_id, scope, id, request_hash, accepted_seq, response, created_at)
         VALUES (?1, 'object-complete', ?2, ?3, 1, ?4, 300)",
        (
            VAULT_B.as_slice(),
            DEDUP_B.as_slice(),
            [0x32u8; 32].as_slice(),
            b"ack-response-b".as_slice(),
        ),
    )
    .expect("dedup b");
    // ---- gc_plans ----
    conn.execute(
        "INSERT INTO gc_plans(vault_id, epoch, cut_seq, eligible_at, proof, created_at)
         VALUES (?1, 1, 1, 5000, ?2, 200)",
        (VAULT_A.as_slice(), b"gc-a".as_slice()),
    )
    .expect("gc a");
    conn.execute(
        "INSERT INTO gc_plans(vault_id, epoch, cut_seq, eligible_at, proof, created_at)
         VALUES (?1, 1, 0, 6000, ?2, 300)",
        (VAULT_B.as_slice(), b"gc-b".as_slice()),
    )
    .expect("gc b");
}

/// 建库 + 写入两个 vault 的全量数据，返回迁移前快照。
fn build_v11(path: &Path) -> Snapshot {
    let conn = create_v11(path);
    seed_two_vaults(&conn);
    let snapshot = capture(&conn);
    // 迁移前自检：22 张表都有行（否则"逐字节不变"证明不了什么）。
    for (table, count) in &snapshot.counts {
        assert!(*count > 0, "fixture 的 {table} 必须有行");
    }
    snapshot
}

fn try_open_db(path: &Path) -> relay::Result<Db> {
    Db::open(
        DbConfig::new(path.to_path_buf()),
        std::sync::Arc::new(FakeClock::new(1)),
    )
}

fn open_db(path: &Path) -> Db {
    try_open_db(path).expect("open triggers 11 -> 12")
}

// ---------------------------------------------------------------- 1) 全量迁移

#[test]
fn v11_full_two_vault_fixture_migrates_to_v12_preserving_every_fact() {
    let path = temp_path("migration-v12-full");
    let before = build_v11(&path);
    let db = open_db(&path);

    db.read_instance(|conn| {
        assert_eq!(
            version(conn).as_deref(),
            Some(SCHEMA_VERSION_STR),
            "必须升到当前 schema（12→13 由同一打开流程继续执行）"
        );
        // ---- 旧数据逐字节不变（按迁移前捕获的列集）----
        assert_preserved(conn, &before);
        // ---- 新表：存在且为空（迁移不凭空造 cap/凭证/回执）----
        for table in V12_NEW_TABLES {
            assert!(table_exists(conn, table), "schema 12 必须建 {table}");
            assert_eq!(count(conn, table), 0, "迁移不得凭空写 {table}");
        }
        // ---- 旧 challenge/token 全部失效（两租户都清）----
        for table in INVALIDATED_TABLES {
            assert_eq!(count(conn, table), 0, "{table} 必须清空");
        }
        // ---- 旧 vault 一律 legacy / 1 / 0 ----
        for vault in [VAULT_A, VAULT_B] {
            let (state, version, authz_rev, authority_rev, recovery, policy, upgrade) = conn
                .query_row(
                    "SELECT authz_state, authz_version, authz_rev, authority_rev, recovery_pub,
                            policy_bytes, upgrade_control_rev
                     FROM vaults WHERE vault_id = ?1",
                    [vault.as_slice()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, Option<Vec<u8>>>(4)?,
                            row.get::<_, Option<Vec<u8>>>(5)?,
                            row.get::<_, Option<i64>>(6)?,
                        ))
                    },
                )
                .expect("vault authz columns");
            assert_eq!(state, "legacy", "旧 vault 必须是 legacy");
            assert_eq!(version, 1, "旧 vault 的 authz_version 必须是 1");
            assert_eq!(authz_rev, 0, "legacy vault 的 authz_rev 必须是 0");
            assert_eq!(authority_rev, 0, "legacy vault 的 authority_rev 必须是 0");
            assert_eq!(recovery, None, "legacy vault 不得有恢复钥");
            assert_eq!(policy, None, "legacy vault 不得有 policy");
            assert_eq!(upgrade, None, "legacy vault 不得有 upgrade rev");
        }
        // ---- 设备/证书/grant/邀请 的新列为 NULL（v1 行不改写）----
        for vault in [VAULT_A, VAULT_B] {
            let devices: Vec<(Option<i64>, Option<i64>)> = {
                let mut stmt = conn
                    .prepare(
                        "SELECT approval_control_rev, issued_authz_rev FROM devices
                         WHERE vault_id = ?1 ORDER BY device_id",
                    )
                    .expect("prepare devices");
                stmt.query_map([vault.as_slice()], |row| Ok((row.get(0)?, row.get(1)?)))
                    .expect("devices")
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .expect("collect devices")
            };
            assert!(!devices.is_empty());
            for (approval, issued) in devices {
                assert_eq!(approval, None, "v1 设备不得伪造 V2 批准点");
                assert_eq!(issued, None, "v1 设备不得伪造 V2 授权 rev");
            }
            let grants: Vec<(Option<i64>, Option<i64>)> = {
                let mut stmt = conn
                    .prepare(
                        "SELECT approval_control_rev, issued_authz_rev FROM device_grants
                         WHERE vault_id = ?1",
                    )
                    .expect("prepare grants");
                stmt.query_map([vault.as_slice()], |row| Ok((row.get(0)?, row.get(1)?)))
                    .expect("grants")
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .expect("collect grants")
            };
            assert!(!grants.is_empty());
            for (approval, issued) in grants {
                assert_eq!(approval, None);
                assert_eq!(issued, None);
            }
            let issuers: Vec<Option<Vec<u8>>> = {
                let mut stmt = conn
                    .prepare("SELECT issuer_cap_id FROM invites WHERE vault_id = ?1")
                    .expect("prepare invites");
                stmt.query_map([vault.as_slice()], |row| row.get(0))
                    .expect("invites")
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .expect("collect invites")
            };
            assert!(!issuers.is_empty());
            for issuer in issuers {
                assert_eq!(issuer, None, "v1 邀请不得挂 cap（legacy creator 语义）");
            }
        }
        // ---- 重建后的表形状必须是 schema 12 定义 ----
        assert!(column_exists(conn, "challenges", "cap_id"));
        assert!(column_exists(conn, "challenges", "authz_version"));
        assert!(column_exists(conn, "tokens", "authority_rev"));
        assert!(column_exists(conn, "devices", "approval_control_rev"));
        assert!(column_exists(conn, "device_grants", "issued_authz_rev"));
        assert!(column_exists(conn, "invites", "issuer_cap_id"));
        assert!(column_exists(conn, "vaults", "authz_state"));
        // ---- 控制链仍可逐条重算（迁移不得重写签名链）----
        for vault in [VAULT_A, VAULT_B] {
            let mut stmt = conn
                .prepare(
                    "SELECT rev, prev_hash, record_hash, signed_record FROM control_commits
                     WHERE vault_id = ?1 ORDER BY rev",
                )
                .expect("prepare control");
            let rows: Vec<(i64, Vec<u8>, Vec<u8>, Vec<u8>)> = stmt
                .query_map([vault.as_slice()], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .expect("control rows")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("collect control");
            assert_eq!(rows.len(), 2);
            let mut prev = vec![0u8; 32];
            for (rev, recorded_prev, recorded_hash, record) in rows {
                assert_eq!(
                    recorded_prev, prev,
                    "vault {vault:?} rev {rev} 的 prev_hash 断链"
                );
                assert_eq!(
                    recorded_hash,
                    control_hash(&prev, &record).to_vec(),
                    "rev {rev} 的 record_hash 不符"
                );
                prev = recorded_hash;
            }
        }
        // ---- FK 全部可解析 ----
        assert_eq!(fk_violations(conn), 0, "迁移后不得有 FK 违规");
        // ---- legacy 默认 vault 标记不变（v1 旧 API 仍只解析它）----
        let legacy: Vec<u8> = conn
            .query_row(
                "SELECT vault_id FROM vaults WHERE legacy_default = 1",
                [],
                |row| row.get(0),
            )
            .expect("legacy default");
        assert_eq!(legacy, VAULT_A.to_vec());
        Ok(())
    })
    .expect("post-migration asserts");
    drop(db);

    // ---- 迁移前快照：`<db>.schema11.bak` 是 schema 11，且保留迁移前的数据 ----
    let backup = schema::backup_path_v11(&path);
    assert!(backup.exists(), "11→12 必须留 schema 11 快照");
    let backup_conn = Connection::open(&backup).expect("open backup");
    assert_eq!(version(&backup_conn).as_deref(), Some("11"));
    assert!(
        !column_exists(&backup_conn, "vaults", "authz_state"),
        "快照必须是迁移前的 schema 11 形状"
    );
    assert_eq!(
        count(&backup_conn, "challenges"),
        2,
        "快照保留旧 challenge 事实"
    );
    for (table, expected) in &before.counts {
        assert_eq!(&count(&backup_conn, table), expected, "快照 {table} 行数");
    }
    drop(backup_conn);

    // ---- 重复打开幂等：不跑迁移、不改数据 ----
    let db = open_db(&path);
    db.read_instance(|conn| {
        assert_eq!(version(conn).as_deref(), Some(SCHEMA_VERSION_STR));
        assert_preserved(conn, &before);
        assert_eq!(fk_violations(conn), 0);
        Ok(())
    })
    .expect("idempotent reopen");
    db.close();
    cleanup(&path);
}

// ---------------------------------------------------------------- 2) 中断 / 重试 / 门禁

#[test]
fn interrupted_v12_migration_rolls_back_and_retry_succeeds() {
    let path = temp_path("migration-v12-fault");
    let before = build_v11(&path);
    let mut conn = Connection::open(&path).expect("open raw");
    let err = schema::migrate_v11_to_v12_with_fault(&mut conn, schema::MigrationFaultV12::Halfway)
        .expect_err("注入故障必须失败");
    assert!(format!("{err}").contains("injected migration fault"));
    // ---- 事务回滚：仍是 schema 11 形状，数据一条不少 ----
    assert_eq!(version(&conn).as_deref(), Some("11"), "失败必须回滚版本号");
    assert!(
        !column_exists(&conn, "vaults", "authz_state"),
        "回滚后不得残留 11→12 的新列"
    );
    assert!(
        !column_exists(&conn, "challenges", "cap_id"),
        "回滚后 challenges 必须还是 schema 11 形状"
    );
    assert!(
        !column_exists(&conn, "devices", "approval_control_rev"),
        "回滚后 devices 必须还是 schema 11 形状"
    );
    for (table, expected) in &before.counts {
        assert_eq!(
            count(&conn, table),
            *expected,
            "回滚后 {table} 行数必须不变"
        );
    }
    assert_eq!(fk_violations(&conn), 0);
    drop(conn);

    // ---- 中断时已写下维护窗口快照（回滚口径 = 只恢复完整快照）----
    let backup = schema::backup_path_v11(&path);
    assert!(backup.exists(), "中断前必须先留快照");
    let backup_conn = Connection::open(&backup).expect("open backup");
    assert_eq!(version(&backup_conn).as_deref(), Some("11"));
    assert_eq!(count(&backup_conn, "challenges"), 2);
    drop(backup_conn);

    // ---- 重试（正常路径）成功，数据仍完整 ----
    let db = open_db(&path);
    db.read_instance(|conn| {
        assert_eq!(version(conn).as_deref(), Some(SCHEMA_VERSION_STR));
        assert_preserved(conn, &before);
        assert_eq!(fk_violations(conn), 0);
        Ok(())
    })
    .expect("retry asserts");
    db.close();
    cleanup(&path);
}

#[test]
fn future_schema_is_rejected_and_version_is_not_rewritten() {
    let path = temp_path("migration-v12-gate");
    let before = build_v11(&path);
    let future = (schema::SCHEMA_VERSION + 1).to_string();
    let conn = Connection::open(&path).expect("open raw");
    conn.execute(
        "UPDATE relay_meta SET value = ?1 WHERE key = 'schema_version'",
        [future.as_bytes()],
    )
    .expect("bump");
    drop(conn);

    let Err(err) = try_open_db(&path) else {
        panic!("更新版本的库必须被拒绝（旧 binary 同理）");
    };
    assert!(
        format!("{err}").contains("newer than this binary"),
        "版本门禁文案必须明确: {err}"
    );
    // 拒绝时不得改写版本，也不得动数据（旧库保持原样，等运维恢复快照）。
    let conn = Connection::open(&path).expect("open raw");
    assert_eq!(version(&conn).as_deref(), Some(future.as_str()));
    assert!(!column_exists(&conn, "vaults", "authz_state"));
    for (table, expected) in &before.counts {
        assert_eq!(count(&conn, table), *expected, "{table} 行数");
    }
    assert!(
        !schema::backup_path_v11(&path).exists(),
        "门禁拒绝不得留快照"
    );
    drop(conn);
    cleanup(&path);
}

#[test]
fn missing_metadata_on_v11_shape_still_runs_the_v12_migration() {
    let path = temp_path("migration-v12-no-meta");
    let before = build_v11(&path);
    // 模拟"元数据丢失但表仍是 v11 形状"：不能直接标成最新版（那会留下半迁移的库）。
    let conn = Connection::open(&path).expect("open raw");
    conn.execute("DELETE FROM relay_meta WHERE key = 'schema_version'", [])
        .expect("drop version");
    drop(conn);

    let db = open_db(&path);
    db.read_instance(|conn| {
        assert_eq!(
            version(conn).as_deref(),
            Some(SCHEMA_VERSION_STR),
            "形状识别（vaults.authz_state 缺失 = schema 11）必须继续跑 11→12→13"
        );
        assert!(column_exists(conn, "vaults", "authz_state"));
        assert!(column_exists(conn, "challenges", "cap_id"));
        assert_preserved(conn, &before);
        assert_eq!(fk_violations(conn), 0);
        Ok(())
    })
    .expect("shape-aware migration asserts");
    db.close();
    cleanup(&path);
}

// ---------------------------------------------------------------- 3) 约束负例

/// 断言一次写被数据库层拒绝，且错误文本含 `needle`（check/foreign key/unique）。
fn assert_rejected<T>(result: relay::Result<T>, needle: &str, label: &str) {
    let err = match result {
        Ok(_) => panic!("{label}：期望被数据库拒绝，实际成功"),
        Err(err) => err,
    };
    let text = match &err {
        relay::Error::Sqlite(inner) => format!("{inner}"),
        other => format!("{other}"),
    };
    assert!(
        text.to_lowercase().contains(needle),
        "{label}：期望错误含 `{needle}`，实际 `{text}`"
    );
}

/// 造一个已初始化（v1 genesis）的 schema 12 库，并额外插入第二个租户：
/// vault B + 设备 B + 只有 B 才有的控制提交 rev 5（跨租户断言用）。
fn fixture_with_second_vault(label: &str) -> (support::Fixture, Id16, Id16) {
    let fx = support::Fixture::new(label);
    let vault_b: Id16 = [0xB2u8; 16];
    let device_b: Id16 = [0xB4u8; 16];
    fx.raw_write(move |conn| {
        conn.execute(
            "INSERT INTO vaults(vault_id, instance_id, log_id, log_incarnation, cursor_key,
                                state, active_epoch, next_epoch, manager_rev, control_rev,
                                auth_rev, head_seq, gc_floor, normal_limit, maintenance_limit,
                                legacy_default, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', 1, 2, 1, 1, 1, 0, 0, 1048576, 2097152, 0, 1)",
            (
                vault_b.as_slice(),
                conn.scope().as_slice(),
                [0x34u8; 16].as_slice(),
                [0x45u8; 16].as_slice(),
                [0x56u8; 32].as_slice(),
            ),
        )?;
        conn.execute(
            "INSERT INTO devices(vault_id, device_id, sign_pk, hpke_pk, certificate, status,
                                 created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', 1)",
            (
                vault_b.as_slice(),
                device_b.as_slice(),
                [0xB4u8; 32].as_slice(),
                [0xC4u8; 32].as_slice(),
                b"cert-b".as_slice(),
            ),
        )?;
        // B 也有 genesis rev 1（任何真实 vault 都有）；**rev 5 只存在于 B**：
        // 跨租户断言必须失败于"vault_id 不配对"，而不是"rev 不存在"。
        conn.execute(
            "INSERT INTO control_commits(vault_id, rev, prev_hash, record_hash, signed_record,
                                         created_at)
             VALUES (?1, 1, ?2, ?3, ?4, 1)",
            (
                vault_b.as_slice(),
                [0x0Fu8; 32].as_slice(),
                [0x10u8; 32].as_slice(),
                b"genesis-b".as_slice(),
            ),
        )?;
        conn.execute(
            "INSERT INTO control_commits(vault_id, rev, prev_hash, record_hash, signed_record,
                                         created_at)
             VALUES (?1, 5, ?2, ?3, ?4, 1)",
            (
                vault_b.as_slice(),
                [0x11u8; 32].as_slice(),
                [0x12u8; 32].as_slice(),
                b"record-b5".as_slice(),
            ),
        )?;
        Ok(())
    })
    .expect("seed second vault");
    (fx, vault_b, device_b)
}

#[test]
fn vault_authz_state_and_version_constraints_hold() {
    let (fx, ..) = fixture_with_second_vault("migration-v12-vault-check");
    let device = fx.device;
    // authz_version 只能 1/2。
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute("UPDATE vaults SET authz_version = 3", [])?;
            Ok(())
        }),
        "check constraint failed",
        "authz_version=3",
    );
    // authz_state 只能 legacy/active/recovery_only。
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute("UPDATE vaults SET authz_state = 'bogus'", [])?;
            Ok(())
        }),
        "check constraint failed",
        "authz_state=bogus",
    );
    // ACTIVE 必须有 version=2 + policy + 恢复钥 + upgrade rev（缺一个都不行）。
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute("UPDATE vaults SET authz_state = 'active'", [])?;
            Ok(())
        }),
        "check constraint failed",
        "active 缺 policy/recovery/upgrade",
    );
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute(
                "UPDATE vaults SET authz_state = 'active', authz_version = 2, authz_rev = 1,
                                  authority_rev = 1, policy_bytes = X'02', upgrade_control_rev = 1",
                [],
            )?;
            Ok(())
        }),
        "check constraint failed",
        "active 缺 recovery_pub",
    );
    // upgrade rev 必须是真实控制提交（复合 FK）。
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute(
                "UPDATE vaults SET authz_state = 'active', authz_version = 2, authz_rev = 1,
                                  authority_rev = 1, policy_bytes = X'02',
                                  recovery_pub = ?1, upgrade_control_rev = 999",
                ([0x91u8; 32].as_slice(),),
            )?;
            Ok(())
        }),
        "foreign key constraint failed",
        "upgrade rev 指向不存在的提交",
    );
    // recovery_only 只要求 recovery_pub（不足两台只许离线 Reset）。
    fx.raw_write(move |conn| {
        conn.execute(
            "UPDATE vaults SET authz_state = 'recovery_only', authz_version = 2, authz_rev = 1,
                              authority_rev = 1, recovery_pub = ?1",
            ([0x92u8; 32].as_slice(),),
        )?;
        Ok(())
    })
    .expect("recovery_only 合法");
    // 完整的 ACTIVE 也必须能写进去（约束不能过度收紧），随后回到 legacy 证明可逆。
    fx.raw_write(move |conn| {
        conn.execute(
            "UPDATE vaults SET authz_state = 'active', authz_version = 2, authz_rev = 2,
                              authority_rev = 2, policy_bytes = X'02', recovery_pub = ?1,
                              upgrade_control_rev = 1",
            ([0x93u8; 32].as_slice(),),
        )?;
        Ok(())
    })
    .expect("完整 ACTIVE 可写");
    fx.raw_write(move |conn| {
        conn.execute(
            "UPDATE vaults SET authz_state = 'legacy', authz_version = 1, authz_rev = 0,
                              authority_rev = 0, policy_bytes = NULL, recovery_pub = NULL,
                              upgrade_control_rev = NULL",
            [],
        )?;
        Ok(())
    })
    .expect("active -> legacy 可逆");
    // legacy 不得带 V2 授权材料。
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute("UPDATE vaults SET policy_bytes = X'02'", [])?;
            Ok(())
        }),
        "check constraint failed",
        "legacy 带 policy",
    );
    assert_eq!(fx.device, device);
}

#[test]
fn management_cap_constraints_hold_across_vaults_and_revs() {
    let (fx, vault_b, _device_b) = fixture_with_second_vault("migration-v12-cap-check");
    let device_a = fx.device;
    let cap_a: Id16 = [0xC1u8; 16];
    let cap_b: Id16 = [0xC2u8; 16];
    let insert_cap = |cap: Id16,
                      subject: Id16,
                      scopes: i64,
                      issued: i64,
                      revoked: Option<i64>,
                      delegable: i64| {
        move |conn: &support::relay::ScopeConn<'_>| {
            conn.execute(
                "INSERT INTO management_caps(vault_id, cap_id, canonical_bytes, subject_device,
                                             subject_pk, scopes, not_before, expires_at,
                                             issued_control_rev, issued_authz_rev,
                                             issuer_authority_rev, revoked_control_rev,
                                             delegable, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 10, 1000, ?7, 1, 1, ?8, ?9, 10)",
                (
                    conn.scope().as_slice(),
                    cap.as_slice(),
                    b"cap-cde".as_slice(),
                    subject.as_slice(),
                    [0xD1u8; 32].as_slice(),
                    scopes,
                    issued,
                    revoked,
                    delegable,
                ),
            )?;
            Ok(())
        }
    };
    // 合法：A 的设备 A、issued rev=1（genesis 提交存在）。
    fx.raw_write(insert_cap(cap_a, device_a, 1 | 2, 1, None, 0))
        .expect("合法 cap");
    // 同一 (vault,cap) 不能重复。
    assert_rejected(
        fx.raw_write(insert_cap(cap_a, device_a, 1, 1, None, 0)),
        "unique constraint failed",
        "cap PK 重复",
    );
    // scopes 不得为零/未知位（1..63）。
    assert_rejected(
        fx.raw_write(insert_cap([0xC3u8; 16], device_a, 0, 1, None, 0)),
        "check constraint failed",
        "scopes=0",
    );
    assert_rejected(
        fx.raw_write(insert_cap([0xC4u8; 16], device_a, 64, 1, None, 0)),
        "check constraint failed",
        "scopes=64",
    );
    // delegable 必须 false。
    assert_rejected(
        fx.raw_write(insert_cap([0xC5u8; 16], device_a, 1, 1, None, 1)),
        "check constraint failed",
        "delegable=1",
    );
    // subject 必须属于**本 vault**（跨租户设备 → 复合 FK 失败）。
    assert_rejected(
        fx.raw_write(insert_cap([0xC6u8; 16], [0xB4u8; 16], 1, 1, None, 0)),
        "foreign key constraint failed",
        "cap 跨 vault 设备",
    );
    // issued rev 必须是本 vault 的真实提交（rev 5 只存在于 B）。
    assert_rejected(
        fx.raw_write(insert_cap([0xC7u8; 16], device_a, 1, 5, None, 0)),
        "foreign key constraint failed",
        "cap issued rev 跨 vault",
    );
    // revoked rev 必须 > issued rev，且必须解析到本 vault 的提交。
    assert_rejected(
        fx.raw_write(insert_cap([0xC8u8; 16], device_a, 1, 1, Some(1), 0)),
        "check constraint failed",
        "revoked rev 不大于 issued rev",
    );
    assert_rejected(
        fx.raw_write(insert_cap([0xC9u8; 16], device_a, 1, 1, Some(5), 0)),
        "foreign key constraint failed",
        "revoked rev 跨 vault",
    );
    // 另一个租户可以有同值的局部 cap id（vault 前缀隔离）。
    fx.raw_write(move |conn| {
        conn.execute(
            "INSERT INTO management_caps(vault_id, cap_id, canonical_bytes, subject_device,
                                         subject_pk, scopes, not_before, expires_at,
                                         issued_control_rev, issued_authz_rev,
                                         issuer_authority_rev, delegable, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, 10, 1000, 1, 1, 1, 0, 10)",
            (
                vault_b.as_slice(),
                cap_a.as_slice(),
                b"cap-cde-b".as_slice(),
                [0xB4u8; 16].as_slice(),
                [0xD2u8; 32].as_slice(),
            ),
        )?;
        Ok(())
    })
    .expect("同 cap id 在两个 vault 内互不冲突");
    let _ = cap_b;
}

#[test]
fn challenge_and_token_constraints_require_cap_and_stay_in_their_vault() {
    let (fx, vault_b, _device_b) = fixture_with_second_vault("migration-v12-auth-check");
    let device_a = fx.device;
    let cap_a: Id16 = [0xE1u8; 16];
    let cap_b: Id16 = [0xE2u8; 16];
    // 准备两个租户各一个 cap（B 用 B 自己的设备/提交）。
    fx.raw_write(move |conn| {
        conn.execute(
            "INSERT INTO management_caps(vault_id, cap_id, canonical_bytes, subject_device,
                                         subject_pk, scopes, not_before, expires_at,
                                         issued_control_rev, issued_authz_rev,
                                         issuer_authority_rev, delegable, created_at)
             VALUES (?1, ?2, CAST('cap-a' AS BLOB), ?3, ?4, 1, 10, 1000, 1, 1, 1, 0, 10)",
            (
                conn.scope().as_slice(),
                cap_a.as_slice(),
                device_a.as_slice(),
                [0xF1u8; 32].as_slice(),
            ),
        )?;
        conn.execute(
            "INSERT INTO management_caps(vault_id, cap_id, canonical_bytes, subject_device,
                                         subject_pk, scopes, not_before, expires_at,
                                         issued_control_rev, issued_authz_rev,
                                         issuer_authority_rev, delegable, created_at)
             VALUES (?1, ?2, CAST('cap-b' AS BLOB), ?3, ?4, 1, 10, 1000, 1, 1, 1, 0, 10)",
            (
                vault_b.as_slice(),
                cap_b.as_slice(),
                [0xB4u8; 16].as_slice(),
                [0xF2u8; 32].as_slice(),
            ),
        )?;
        Ok(())
    })
    .expect("seed caps");

    let insert_challenge = |id: Id16, realm: i64, cap: Option<Id16>| {
        move |conn: &support::relay::ScopeConn<'_>| {
            conn.execute(
                "INSERT INTO challenges(vault_id, challenge_id, realm, subject, nonce, expires_at,
                                        used_at, authz_version, authz_rev, authority_rev, cap_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1000, NULL, 2, 1, 1, ?6)",
                (
                    conn.scope().as_slice(),
                    id.as_slice(),
                    realm,
                    conn.scope().as_slice(),
                    [0xE9u8; 32].as_slice(),
                    cap.map(|cap| cap.to_vec()),
                ),
            )?;
            Ok(())
        }
    };
    // v1 设备 challenge（realm 1）不需要 cap。
    fx.raw_write(insert_challenge([0xD1u8; 16], 1, None))
        .expect("realm1 无 cap");
    // realm 3（管理设备）必须绑定 cap。
    assert_rejected(
        fx.raw_write(insert_challenge([0xD2u8; 16], 3, None)),
        "check constraint failed",
        "realm3 无 cap",
    );
    // realm 3 的 cap 必须属于本 vault。
    assert_rejected(
        fx.raw_write(insert_challenge([0xD3u8; 16], 3, Some(cap_b))),
        "foreign key constraint failed",
        "realm3 cap 跨 vault",
    );
    fx.raw_write(insert_challenge([0xD4u8; 16], 3, Some(cap_a)))
        .expect("realm3 带本 vault cap");
    // realm 只能 1..4。
    assert_rejected(
        fx.raw_write(insert_challenge([0xD5u8; 16], 5, None)),
        "check constraint failed",
        "realm=5",
    );

    let insert_token = |hash: Hash32, realm: i64, cap: Option<Id16>| {
        move |conn: &support::relay::ScopeConn<'_>| {
            conn.execute(
                "INSERT INTO tokens(vault_id, token_hash, realm, subject, auth_rev, expires_at,
                                    created_at, authz_version, authz_rev, authority_rev, cap_id)
                 VALUES (?1, ?2, ?3, ?4, 1, 1000, 10, 2, 1, 1, ?5)",
                (
                    conn.scope().as_slice(),
                    hash.as_slice(),
                    realm,
                    conn.scope().as_slice(),
                    cap.map(|cap| cap.to_vec()),
                ),
            )?;
            Ok(())
        }
    };
    // v1 旧管理 token（realm 2）仍是合法 legacy 行，不要求 cap。
    fx.raw_write(insert_token([0xA9u8; 32], 2, None))
        .expect("realm2 legacy");
    // 管理 token（realm 3）没有 cap → 拒绝（ADR：管理 token 必须绑定 cap）。
    assert_rejected(
        fx.raw_write(insert_token([0xAAu8; 32], 3, None)),
        "check constraint failed",
        "管理 token 无 cap",
    );
    // 管理 token 的 cap 跨 vault → 复合 FK 拒绝。
    assert_rejected(
        fx.raw_write(insert_token([0xABu8; 32], 3, Some(cap_b))),
        "foreign key constraint failed",
        "管理 token cap 跨 vault",
    );
    fx.raw_write(insert_token([0xACu8; 32], 3, Some(cap_a)))
        .expect("管理 token 带本 vault cap");
}

#[test]
fn v2_dedup_and_credential_history_constraints_hold() {
    let (fx, vault_b, device_b) = fixture_with_second_vault("migration-v12-dedup-check");
    let device_a = fx.device;
    let request: Id16 = [0xAAu8; 16];
    // 合法回执：control_rev=1 是 genesis 提交。
    fx.raw_write(move |conn| {
        conn.execute(
            "INSERT INTO dedup_v2(vault_id, request_id, request_hash, control_rev, accepted_seq,
                                  response, created_at)
             VALUES (?1, ?2, ?3, 1, 1, CAST('receipt-a' AS BLOB), 10)",
            (
                conn.scope().as_slice(),
                request.as_slice(),
                [0xABu8; 32].as_slice(),
            ),
        )?;
        Ok(())
    })
    .expect("合法 V2 回执");
    // 同 vault 同 request_id 不能重复（复合 PK）。
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute(
                "INSERT INTO dedup_v2(vault_id, request_id, request_hash, control_rev, response,
                                      created_at)
                 VALUES (?1, ?2, ?3, 1, CAST('receipt-a2' AS BLOB), 11)",
                (
                    conn.scope().as_slice(),
                    request.as_slice(),
                    [0xACu8; 32].as_slice(),
                ),
            )?;
            Ok(())
        }),
        "unique constraint failed",
        "同 request_id 重复",
    );
    // control_rev 必须解析到本 vault 的提交（rev 5 只存在于 B）。
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute(
                "INSERT INTO dedup_v2(vault_id, request_id, request_hash, control_rev, response,
                                      created_at)
                 VALUES (?1, ?2, ?3, 5, CAST('receipt-x' AS BLOB), 12)",
                (
                    conn.scope().as_slice(),
                    [0xADu8; 16].as_slice(),
                    [0xAEu8; 32].as_slice(),
                ),
            )?;
            Ok(())
        }),
        "foreign key constraint failed",
        "dedup 跨 vault 提交",
    );
    // 另一个租户可以用同一个 request_id（vault 前缀隔离）。
    fx.raw_write(move |conn| {
        conn.execute(
            "INSERT INTO dedup_v2(vault_id, request_id, request_hash, control_rev, response,
                                  created_at)
             VALUES (?1, ?2, ?3, NULL, CAST('receipt-b' AS BLOB), 13)",
            (
                vault_b.as_slice(),
                request.as_slice(),
                [0xAFu8; 32].as_slice(),
            ),
        )?;
        Ok(())
    })
    .expect("同 request_id 在两个 vault 内互不冲突");

    // ---- 凭证历史：serial 递增、跨 vault 拒绝、approval rev 必须真实 ----
    fx.raw_write(move |conn| {
        conn.execute(
            "INSERT INTO device_credentials(vault_id, device_id, serial, authz_version, sign_pk,
                                            hpke_pk, certificate, issued_authz_rev,
                                            approval_control_rev, created_at)
             VALUES (?1, ?2, 2, 2, ?3, ?4, CAST('cert-a2' AS BLOB), 1, 1, 10)",
            (
                conn.scope().as_slice(),
                device_a.as_slice(),
                [0xB7u8; 32].as_slice(),
                [0xC7u8; 32].as_slice(),
            ),
        )?;
        Ok(())
    })
    .expect("合法 V2 凭证历史");
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute(
                "INSERT INTO device_credentials(vault_id, device_id, serial, authz_version, sign_pk,
                                                hpke_pk, certificate, created_at)
                 VALUES (?1, ?2, 2, 2, ?3, ?4, CAST('dup' AS BLOB), 11)",
                (
                    conn.scope().as_slice(),
                    device_a.as_slice(),
                    [0xB8u8; 32].as_slice(),
                    [0xC8u8; 32].as_slice(),
                ),
            )?;
            Ok(())
        }),
        "unique constraint failed",
        "凭证 serial 重复",
    );
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute(
                "INSERT INTO device_credentials(vault_id, device_id, serial, authz_version, sign_pk,
                                                hpke_pk, certificate, created_at)
                 VALUES (?1, ?2, 1, 3, ?3, ?4, CAST('bad-version' AS BLOB), 11)",
                (
                    conn.scope().as_slice(),
                    device_a.as_slice(),
                    [0xB9u8; 32].as_slice(),
                    [0xC9u8; 32].as_slice(),
                ),
            )?;
            Ok(())
        }),
        "check constraint failed",
        "authz_version=3",
    );
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute(
                "INSERT INTO device_credentials(vault_id, device_id, serial, authz_version, sign_pk,
                                                hpke_pk, certificate, approval_control_rev,
                                                created_at)
                 VALUES (?1, ?2, 3, 2, ?3, ?4, CAST('bad-rev' AS BLOB), 999, 11)",
                (
                    conn.scope().as_slice(),
                    device_a.as_slice(),
                    [0xBAu8; 32].as_slice(),
                    [0xCAu8; 32].as_slice(),
                ),
            )?;
            Ok(())
        }),
        "foreign key constraint failed",
        "凭证 approval rev 不存在",
    );
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute(
                "INSERT INTO device_credentials(vault_id, device_id, serial, authz_version, sign_pk,
                                                hpke_pk, certificate, created_at)
                 VALUES (?1, ?2, 1, 2, ?3, ?4, CAST('cross' AS BLOB), 12)",
                (
                    vault_b.as_slice(),
                    device_a.as_slice(),
                    [0xBBu8; 32].as_slice(),
                    [0xCBu8; 32].as_slice(),
                ),
            )?;
            Ok(())
        }),
        "foreign key constraint failed",
        "凭证跨 vault 设备",
    );
    // cert approval rev 不匹配：设备行的批准点必须解析到本 vault 的提交。
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute(
                "UPDATE devices SET approval_control_rev = 999 WHERE device_id = ?1",
                [device_a.as_slice()],
            )?;
            Ok(())
        }),
        "foreign key constraint failed",
        "设备 approval rev 不存在",
    );
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute(
                "UPDATE devices SET approval_control_rev = 5 WHERE device_id = ?1",
                [device_a.as_slice()],
            )?;
            Ok(())
        }),
        "foreign key constraint failed",
        "设备 approval rev 跨 vault",
    );
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute(
                "UPDATE devices SET issued_authz_rev = -1 WHERE device_id = ?1",
                [device_a.as_slice()],
            )?;
            Ok(())
        }),
        "check constraint failed",
        "设备 issued_authz_rev 负数",
    );
    fx.raw_write(move |conn| {
        conn.execute(
            "UPDATE devices SET approval_control_rev = 1 WHERE device_id = ?1",
            [device_a.as_slice()],
        )?;
        Ok(())
    })
    .expect("设备 approval rev = 真实提交");

    // ---- 邀请的 issuer_cap_id：必须解析到本 vault 的 cap ----
    let cap_a: Id16 = [0xE1u8; 16];
    let cap_b: Id16 = [0xE2u8; 16];
    fx.raw_write(move |conn| {
        for (vault, cap, subject, pk) in [
            (*conn.scope().vault_id(), cap_a, device_a, [0xF1u8; 32]),
            (vault_b, cap_b, device_b, [0xF2u8; 32]),
        ] {
            conn.execute(
                "INSERT INTO management_caps(vault_id, cap_id, canonical_bytes, subject_device,
                                             subject_pk, scopes, not_before, expires_at,
                                             issued_control_rev, issued_authz_rev,
                                             issuer_authority_rev, delegable, created_at)
                 VALUES (?1, ?2, CAST('cap' AS BLOB), ?3, ?4, 1, 10, 1000, 1, 1, 1, 0, 10)",
                (
                    vault.as_slice(),
                    cap.as_slice(),
                    subject.as_slice(),
                    pk.as_slice(),
                ),
            )?;
        }
        conn.execute(
            "INSERT INTO invites(vault_id, invite_id, token_hash, expires_at, pending_device,
                                 used_at, issuer_cap_id)
             VALUES (?1, ?2, ?3, 1000, NULL, NULL, ?4)",
            (
                conn.scope().as_slice(),
                [0xE5u8; 16].as_slice(),
                [0xE6u8; 32].as_slice(),
                cap_a.as_slice(),
            ),
        )?;
        Ok(())
    })
    .expect("邀请绑定本 vault cap");
    assert_rejected(
        fx.raw_write(move |conn| {
            conn.execute(
                "UPDATE invites SET issuer_cap_id = ?1 WHERE invite_id = ?2",
                ([0xE2u8; 16].as_slice(), [0xE5u8; 16].as_slice()),
            )?;
            Ok(())
        }),
        "foreign key constraint failed",
        "邀请 issuer cap 跨 vault",
    );
}

// ---------------------------------------------------------------- 4) 新建库形状

#[test]
fn fresh_schema_13_has_the_adr_shape() {
    let path = temp_path("migration-v12-fresh");
    let db = Db::open(
        DbConfig::new(path.clone()),
        std::sync::Arc::new(FakeClock::new(1)),
    )
    .expect("fresh open");
    db.read_instance(|conn| {
        assert_eq!(version(conn).as_deref(), Some(SCHEMA_VERSION_STR));
        // 新表齐全且都是 STRICT。
        for table in V12_NEW_TABLES {
            assert!(table_exists(conn, table), "缺少 {table}");
            let strict: i64 = conn
                .query_row(
                    "SELECT strict FROM pragma_table_list WHERE schema = 'main' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("strict flag");
            assert_eq!(strict, 1, "{table} 必须 STRICT");
        }
        // 关键列形状（与 ADR 0015 §DDL 对齐）。
        assert_eq!(
            columns(conn, "management_caps"),
            vec![
                "vault_id",
                "cap_id",
                "canonical_bytes",
                "subject_device",
                "subject_pk",
                "scopes",
                "not_before",
                "expires_at",
                "issued_control_rev",
                "issued_authz_rev",
                "issuer_authority_rev",
                "revoked_control_rev",
                "delegable",
                "created_at",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
        for column in [
            "authz_version",
            "authz_rev",
            "authority_rev",
            "recovery_pub",
            "policy_bytes",
            "upgrade_control_rev",
            "authz_state",
        ] {
            assert!(
                column_exists(conn, "vaults", column),
                "vaults 缺少 {column}"
            );
        }
        for (table, columns_expected) in [
            (
                "challenges",
                vec![
                    "vault_id",
                    "challenge_id",
                    "realm",
                    "subject",
                    "nonce",
                    "expires_at",
                    "used_at",
                    "authz_version",
                    "authz_rev",
                    "authority_rev",
                    "cap_id",
                ],
            ),
            (
                "tokens",
                vec![
                    "vault_id",
                    "token_hash",
                    "realm",
                    "subject",
                    "auth_rev",
                    "expires_at",
                    "created_at",
                    "authz_version",
                    "authz_rev",
                    "authority_rev",
                    "cap_id",
                ],
            ),
            (
                "dedup_v2",
                vec![
                    "vault_id",
                    "request_id",
                    "request_hash",
                    "control_rev",
                    "accepted_seq",
                    "response",
                    "created_at",
                ],
            ),
        ] {
            assert_eq!(
                columns(conn, table),
                columns_expected
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
                "{table} 列形状"
            );
        }
        // 每个新 PK/UNIQUE/FK 都带 vault_id（试点：新表的主键第一列）。
        for table in V12_NEW_TABLES {
            let mut stmt = conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .expect("table info");
            let first = stmt
                .query_map([], |row| row.get::<_, String>(1))
                .expect("names")
                .next()
                .expect("pk")
                .expect("name");
            assert_eq!(first, "vault_id", "{table} 的 PK 必须以 vault_id 开头");
        }
        Ok(())
    })
    .expect("fresh shape asserts");
    // 空库不应有 authz 状态机以外的行。
    db.close();
    cleanup(&path);
}
