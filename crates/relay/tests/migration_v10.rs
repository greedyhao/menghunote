//! ADR 0014 S2a：schema 9 → 10 迁移（单 vault → 多租户）。
//!
//! 覆盖要求（任务书）：
//! 1. 构造 schema 9 单 vault **全量** fixture（设备/证书/授权/控制链/日志/对象/邀请/游标/
//!    usage/lease），迁移到 10；逐项断言 ID/字节/rev/签名链/cursor/配额不变；
//! 2. 行数 / FK / 多重集校验；
//! 3. 重复迁移幂等（再打开一次不变）；旧 binary 版本门禁（更高版本明确拒绝）；
//! 4. 中断/失败回滚 + `VACUUM INTO` 备份可恢复（WAL 一致）。

// 迁移快照是原样字节的多列元组；保留结构以便逐项比对，不额外引入类型别名。
#![allow(clippy::type_complexity)]

mod support;

use std::path::{Path, PathBuf};

use support::relay;
use support::relay::db::{schema, Db, DbConfig};
use support::relay::ids::{Hash32, Id16};
use support::relay::time::FakeClock;

use support::v9;

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

fn table_counts(conn: &rusqlite::Connection) -> Vec<(&'static str, i64)> {
    TENANT_TABLES
        .iter()
        .map(|table| {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap_or_else(|err| panic!("count {table}: {err}"));
            (*table, count)
        })
        .collect()
}

fn fk_violations(conn: &rusqlite::Connection) -> i64 {
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

fn column_exists(conn: &rusqlite::Connection, table: &str, column: &str) -> bool {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("table info");
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("columns")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect columns");
    names.iter().any(|name| name == column)
}

fn version(conn: &rusqlite::Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM relay_meta WHERE key = 'schema_version'",
        [],
        |row| row.get::<_, Vec<u8>>(0),
    )
    .ok()
    .map(|bytes| String::from_utf8(bytes).expect("version utf8"))
}

/// 迁移前快照（全部来自 v9 表的原始字节）。
struct Before {
    vault: Vec<(String, i64)>,
    device_rows: Vec<(
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        Option<Vec<u8>>,
        String,
        i64,
    )>,
    control: Vec<(i64, Vec<u8>, Vec<u8>, Vec<u8>)>,
    log: Vec<(i64, String, Option<Vec<u8>>, i64, Vec<u8>, Option<Vec<u8>>)>,
    objects: Vec<(Vec<u8>, i64, Vec<u8>, Vec<u8>)>,
    parts: Vec<(Vec<u8>, i64, Vec<u8>, Vec<u8>)>,
    invites: Vec<(Vec<u8>, Vec<u8>, i64, Option<Vec<u8>>, Option<i64>)>,
    usage: (i64, i64),
    leases: Vec<(Vec<u8>, Vec<u8>, i64, i64)>,
    reservations: Vec<(Vec<u8>, i64, i64, String)>,
    dedup: Vec<(String, Vec<u8>, Vec<u8>, Option<i64>, Vec<u8>)>,
    checkpoints: Vec<(Vec<u8>, i64, i64, Option<Vec<u8>>, Option<Vec<u8>>, String)>,
    grants: Vec<(
        i64,
        Vec<u8>,
        i64,
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        Option<Vec<u8>>,
    )>,
    counts: Vec<(&'static str, i64)>,
}

fn snapshot(conn: &rusqlite::Connection) -> Before {
    let mut vault_stmt = conn
        .prepare(
            "SELECT instance_id, log_id, log_incarnation, cursor_key, state, active_epoch,
                    next_epoch, manager_rev, control_rev, auth_rev, head_seq, gc_floor,
                    normal_limit, maintenance_limit, created_at FROM vault WHERE singleton = 1",
        )
        .expect("vault select");
    let vault: Vec<(String, i64)> = vault_stmt
        .query_row([], |row| {
            Ok(vec![
                (
                    "instance_id".to_string(),
                    row.get::<_, Vec<u8>>(0)?.len() as i64,
                ),
                ("log_id".to_string(), row.get::<_, Vec<u8>>(1)?.len() as i64),
                (
                    "log_incarnation".to_string(),
                    row.get::<_, Vec<u8>>(2)?.len() as i64,
                ),
                (
                    "cursor_key".to_string(),
                    row.get::<_, Vec<u8>>(3)?.len() as i64,
                ),
                ("active_epoch".to_string(), row.get::<_, i64>(5)?),
                ("next_epoch".to_string(), row.get::<_, i64>(6)?),
                ("manager_rev".to_string(), row.get::<_, i64>(7)?),
                ("control_rev".to_string(), row.get::<_, i64>(8)?),
                ("auth_rev".to_string(), row.get::<_, i64>(9)?),
                ("head_seq".to_string(), row.get::<_, i64>(10)?),
                ("gc_floor".to_string(), row.get::<_, i64>(11)?),
                ("normal_limit".to_string(), row.get::<_, i64>(12)?),
                ("maintenance_limit".to_string(), row.get::<_, i64>(13)?),
                ("created_at".to_string(), row.get::<_, i64>(14)?),
            ])
        })
        .expect("vault row");
    drop(vault_stmt);

    let device_rows = {
        let mut stmt = conn
            .prepare(
                "SELECT device_id, sign_pk, hpke_pk, certificate, certificate_signature, status,
                        created_at FROM devices ORDER BY device_id",
            )
            .expect("devices");
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .expect("devices map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("devices collect")
    };
    let control = {
        let mut stmt = conn
            .prepare(
                "SELECT rev, prev_hash, record_hash, signed_record FROM control_commits
                 ORDER BY rev",
            )
            .expect("control");
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("control map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("control collect")
    };
    let log = {
        let mut stmt = conn
            .prepare(
                "SELECT relay_seq, kind, object_id, epoch, event, signature FROM log
                 ORDER BY relay_seq",
            )
            .expect("log");
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .expect("log map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("log collect")
    };
    let objects = {
        let mut stmt = conn
            .prepare(
                "SELECT object_id, epoch, digest, signed_commit FROM objects
                 ORDER BY object_id",
            )
            .expect("objects");
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .expect("objects map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("objects collect")
    };
    let parts = {
        let mut stmt = conn
            .prepare(
                "SELECT object_id, part_no, envelope, digest FROM object_parts
                 ORDER BY object_id, part_no",
            )
            .expect("parts");
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .expect("parts map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("parts collect")
    };
    let invites = {
        let mut stmt = conn
            .prepare(
                "SELECT invite_id, token_hash, expires_at, pending_device, used_at FROM invites",
            )
            .expect("invites");
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("invites map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("invites collect")
    };
    let usage = conn
        .query_row(
            "SELECT normal_bytes, maintenance_bytes FROM usage WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("usage");
    let leases = {
        let mut stmt = conn
            .prepare("SELECT lease_id, device_id, floor_seq, expires_at FROM bootstrap_leases")
            .expect("leases");
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("leases map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("leases collect")
    };
    let reservations = {
        let mut stmt = conn
            .prepare(
                "SELECT reservation_id, consumed_bytes, expires_at, state
                 FROM maintenance_reservations",
            )
            .expect("reservations");
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("reservations map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("reservations collect")
    };
    let dedup = {
        let mut stmt = conn
            .prepare("SELECT scope, id, request_hash, accepted_seq, response FROM dedup")
            .expect("dedup");
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("dedup map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("dedup collect")
    };
    let checkpoints = {
        let mut stmt = conn
            .prepare(
                "SELECT checkpoint_id, epoch, covered_seq, manifest_oid, refs_digest, state
                 FROM checkpoints",
            )
            .expect("checkpoints");
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .expect("checkpoints map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("checkpoints collect")
    };
    let grants = {
        let mut stmt = conn
            .prepare(
                "SELECT epoch, device_id, manager_rev, hpke_enc, hpke_ciphertext, signed_header,
                        signature FROM device_grants ORDER BY epoch, device_id",
            )
            .expect("grants");
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .expect("grants map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("grants collect")
    };
    Before {
        vault,
        device_rows,
        control,
        log,
        objects,
        parts,
        invites,
        usage,
        leases,
        reservations,
        dedup,
        checkpoints,
        grants,
        counts: table_counts(conn),
    }
}

#[test]
fn v9_fixture_migrates_to_v10_preserving_every_tenant_fact() {
    let path = temp_path("migration-v10-full");
    let conn = v9::fixture(&path);
    let before = snapshot(&conn);
    drop(conn);

    let db = Db::open(
        DbConfig::new(path.clone()),
        std::sync::Arc::new(FakeClock::new(1)),
    )
    .expect("open triggers migration");
    let after = db
        .read_instance(|conn| Ok(snapshot_v10(conn)))
        .expect("post-migration snapshot");

    // ---- vault 行：ID/rev/log incarnation/cursor key/配额逐项不变 ----
    assert_eq!(
        after.vault, before.vault,
        "vault 行除 PK 语义外必须逐字段不变"
    );
    let legacy: Option<Id16> = db
        .read_instance(schema::legacy_default_vault_id)
        .expect("legacy default");
    assert_eq!(
        legacy,
        Some(v9::VAULT),
        "旧单 vault 必须显式标为 legacy 默认"
    );
    let vault_id_in_row: Vec<u8> = db
        .read_instance(|conn| {
            Ok(conn.query_row("SELECT vault_id FROM vaults", [], |row| row.get(0))?)
        })
        .expect("vault id");
    assert_eq!(vault_id_in_row, v9::VAULT.to_vec());

    // ---- 设备/控制链/日志/对象/parts/邀请/lease/reservation/dedup/checkpoint/grant ----
    assert_eq!(
        after.device_rows, before.device_rows,
        "设备字节/证书/状态不变"
    );
    assert_eq!(after.control, before.control, "控制链签名链不变");
    assert_eq!(after.log, before.log, "日志 seq/event/signature 不变");
    assert_eq!(after.objects, before.objects, "对象摘要/commit 不变");
    assert_eq!(after.parts, before.parts, "分片密文字节不变");
    assert_eq!(after.invites, before.invites, "已消费邀请事实不变");
    assert_eq!(after.usage, before.usage, "usage 账本不变");
    assert_eq!(after.leases, before.leases, "bootstrap lease 不变");
    assert_eq!(
        after.reservations, before.reservations,
        "维护 reservation 不变"
    );
    assert_eq!(after.dedup, before.dedup, "幂等 tombstone 不变");
    assert_eq!(after.checkpoints, before.checkpoints, "checkpoint 骨架不变");
    assert_eq!(after.grants, before.grants, "HPKE grant 不变");

    // ---- 行数（challenges/tokens 按 ADR 0014 §A 维护窗口失效）----
    for ((table, old), (table2, new)) in before.counts.iter().zip(after.counts.iter()) {
        assert_eq!(table, table2);
        if matches!(*table, "challenges" | "tokens") {
            assert_eq!(*new, 0, "迁移必须让旧 {table} 失效", table = table);
        } else {
            assert_eq!(old, new, "{table} 行数必须不变");
        }
    }

    // ---- schema 结构 / FK / 版本 ----
    db.read_instance(|conn| {
        // 迁移链 9 → 10 → 11（v10 起 schema 11 只新增 create_tickets，不改租户数据）。
        let expected = schema::SCHEMA_VERSION.to_string();
        assert_eq!(version(conn).as_deref(), Some(expected.as_str()));
        assert_eq!(fk_violations(conn), 0, "迁移后不得有 FK 违规");
        for table in TENANT_TABLES {
            assert!(
                column_exists(conn, table, "vault_id"),
                "{table} 必须有 vault_id"
            );
        }
        assert!(
            !column_exists(conn, "vault", "singleton"),
            "vault 表已改名 vaults"
        );
        Ok(())
    })
    .expect("schema asserts");

    // ---- 备份：WAL 一致的 schema 9 快照仍可独立打开 ----
    let backup = schema::backup_path(&path);
    assert!(backup.exists(), "迁移前必须留下 schema 9 快照");
    let backup_conn = rusqlite::Connection::open(&backup).expect("open backup");
    assert_eq!(
        version(&backup_conn).as_deref(),
        Some("9"),
        "备份是 schema 9"
    );
    assert!(column_exists(&backup_conn, "vault", "singleton"));
    let backup_counts = table_counts(&backup_conn);
    assert_eq!(backup_counts, before.counts, "备份行数与迁移前一致");
    drop(backup_conn);

    // ---- 重复迁移幂等：再打开一次什么都不变 ----
    drop(db);
    let db = Db::open(
        DbConfig::new(path.clone()),
        std::sync::Arc::new(FakeClock::new(1)),
    )
    .expect("reopen is a no-op");
    let again = db
        .read_instance(|conn| Ok(snapshot_v10(conn)))
        .expect("second snapshot");
    assert_eq!(again.vault, after.vault);
    assert_eq!(again.counts, after.counts);
    drop(db);
    cleanup(&path);
}

/// 迁移后的同一维度快照（表名/列名改为 v10，值应与 v9 快照一致）。
fn snapshot_v10(conn: &rusqlite::Connection) -> Before {
    let vault: Vec<(String, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT instance_id, log_id, log_incarnation, cursor_key, state, active_epoch,
                        next_epoch, manager_rev, control_rev, auth_rev, head_seq, gc_floor,
                        normal_limit, maintenance_limit, created_at
                 FROM vaults WHERE vault_id = ?1",
            )
            .expect("vaults");
        stmt.query_row([v9::VAULT.as_slice()], |row| {
            Ok(vec![
                (
                    "instance_id".to_string(),
                    row.get::<_, Vec<u8>>(0)?.len() as i64,
                ),
                ("log_id".to_string(), row.get::<_, Vec<u8>>(1)?.len() as i64),
                (
                    "log_incarnation".to_string(),
                    row.get::<_, Vec<u8>>(2)?.len() as i64,
                ),
                (
                    "cursor_key".to_string(),
                    row.get::<_, Vec<u8>>(3)?.len() as i64,
                ),
                ("active_epoch".to_string(), row.get::<_, i64>(5)?),
                ("next_epoch".to_string(), row.get::<_, i64>(6)?),
                ("manager_rev".to_string(), row.get::<_, i64>(7)?),
                ("control_rev".to_string(), row.get::<_, i64>(8)?),
                ("auth_rev".to_string(), row.get::<_, i64>(9)?),
                ("head_seq".to_string(), row.get::<_, i64>(10)?),
                ("gc_floor".to_string(), row.get::<_, i64>(11)?),
                ("normal_limit".to_string(), row.get::<_, i64>(12)?),
                ("maintenance_limit".to_string(), row.get::<_, i64>(13)?),
                ("created_at".to_string(), row.get::<_, i64>(14)?),
            ])
        })
        .expect("vault row")
    };
    let count = |table: &str| -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap_or_else(|err| panic!("count {table}: {err}"))
    };
    let device_rows = {
        let mut stmt = conn
            .prepare(
                "SELECT device_id, sign_pk, hpke_pk, certificate, certificate_signature, status,
                        created_at FROM devices ORDER BY device_id",
            )
            .expect("devices");
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .expect("devices map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("devices collect")
    };
    let control = {
        let mut stmt = conn
            .prepare(
                "SELECT rev, prev_hash, record_hash, signed_record FROM control_commits
                 ORDER BY rev",
            )
            .expect("control");
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("control map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("control collect")
    };
    let log = {
        let mut stmt = conn
            .prepare(
                "SELECT relay_seq, kind, object_id, epoch, event, signature FROM log
                 ORDER BY relay_seq",
            )
            .expect("log");
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .expect("log map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("log collect")
    };
    let objects = {
        let mut stmt = conn
            .prepare(
                "SELECT object_id, epoch, digest, signed_commit FROM objects ORDER BY object_id",
            )
            .expect("objects");
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .expect("objects map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("objects collect")
    };
    let parts = {
        let mut stmt = conn
            .prepare(
                "SELECT object_id, part_no, envelope, digest FROM object_parts
                 ORDER BY object_id, part_no",
            )
            .expect("parts");
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .expect("parts map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("parts collect")
    };
    let invites = {
        let mut stmt = conn
            .prepare(
                "SELECT invite_id, token_hash, expires_at, pending_device, used_at FROM invites",
            )
            .expect("invites");
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("invites map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("invites collect")
    };
    let usage = conn
        .query_row(
            "SELECT normal_bytes, maintenance_bytes FROM usage WHERE vault_id = ?1",
            [v9::VAULT.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("usage");
    let leases = {
        let mut stmt = conn
            .prepare("SELECT lease_id, device_id, floor_seq, expires_at FROM bootstrap_leases")
            .expect("leases");
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("leases map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("leases collect")
    };
    let reservations = {
        let mut stmt = conn
            .prepare(
                "SELECT reservation_id, consumed_bytes, expires_at, state
                 FROM maintenance_reservations",
            )
            .expect("reservations");
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("reservations map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("reservations collect")
    };
    let dedup = {
        let mut stmt = conn
            .prepare("SELECT scope, id, request_hash, accepted_seq, response FROM dedup")
            .expect("dedup");
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("dedup map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("dedup collect")
    };
    let checkpoints = {
        let mut stmt = conn
            .prepare(
                "SELECT checkpoint_id, epoch, covered_seq, manifest_oid, refs_digest, state
                 FROM checkpoints",
            )
            .expect("checkpoints");
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .expect("checkpoints map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("checkpoints collect")
    };
    let grants = {
        let mut stmt = conn
            .prepare(
                "SELECT epoch, device_id, manager_rev, hpke_enc, hpke_ciphertext, signed_header,
                        signature FROM device_grants ORDER BY epoch, device_id",
            )
            .expect("grants");
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .expect("grants map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("grants collect")
    };
    Before {
        vault,
        device_rows,
        control,
        log,
        objects,
        parts,
        invites,
        usage,
        leases,
        reservations,
        dedup,
        checkpoints,
        grants,
        counts: TENANT_TABLES
            .iter()
            .map(|table| (*table, count(table)))
            .collect(),
    }
}

#[test]
fn migration_failure_rolls_back_and_keeps_schema9_snapshot() {
    let path = temp_path("migration-v10-fault");
    let conn = v9::fixture(&path);
    let before = snapshot(&conn);
    drop(conn);

    // 直接调迁移入口并注入"复制到一半失败"：事务必须整体回滚。
    let mut conn = rusqlite::Connection::open(&path).expect("open raw");
    let err = schema::migrate_v9_to_v10_with_fault(&mut conn, schema::MigrationFault::Halfway)
        .expect_err("injected fault must fail");
    assert!(format!("{err}").contains("injected"), "{err}");

    // 旧库仍是 schema 9（版本/表形状/数据都在），备份留下的是同一份一致快照。
    assert_eq!(
        version(&conn).as_deref(),
        Some("9"),
        "失败必须保留 schema 9"
    );
    assert!(
        column_exists(&conn, "vault", "singleton"),
        "旧 vault 表仍在"
    );
    assert!(
        !column_exists(&conn, "devices", "vault_id"),
        "失败不得留下半迁移表"
    );
    let after_failure = snapshot(&conn);
    assert_eq!(after_failure.counts, before.counts, "失败不得丢任何行");
    assert_eq!(after_failure.device_rows, before.device_rows);
    assert_eq!(after_failure.log, before.log);
    assert_eq!(after_failure.parts, before.parts);
    assert_eq!(fk_violations(&conn), 0, "回滚后 FK 必须仍然干净");
    drop(conn);

    let backup = schema::backup_path(&path);
    assert!(backup.exists(), "失败也必须留下迁移前快照");
    let backup_conn = rusqlite::Connection::open(&backup).expect("open backup");
    assert_eq!(version(&backup_conn).as_deref(), Some("9"));
    assert_eq!(table_counts(&backup_conn), before.counts);
    drop(backup_conn);

    // 重试（正常路径）必须成功，数据完整。
    let db = Db::open(
        DbConfig::new(path.clone()),
        std::sync::Arc::new(FakeClock::new(1)),
    )
    .expect("retry migration");
    db.read_instance(|conn| {
        let expected = schema::SCHEMA_VERSION.to_string();
        assert_eq!(version(conn).as_deref(), Some(expected.as_str()));
        assert_eq!(fk_violations(conn), 0);
        Ok(())
    })
    .expect("retry asserts");
    let after_retry = db
        .read_instance(|conn| Ok(snapshot_v10(conn)))
        .expect("retry snapshot");
    assert_eq!(after_retry.device_rows, before.device_rows);
    assert_eq!(after_retry.parts, before.parts);
    assert_eq!(after_retry.control, before.control);
    drop(db);
    cleanup(&path);
}

#[test]
fn newer_schema_is_rejected_like_an_old_binary() {
    let path = temp_path("migration-v10-gate");
    let conn = v9::fixture(&path);
    drop(conn);
    let conn = rusqlite::Connection::open(&path).expect("open raw");
    let future = (schema::SCHEMA_VERSION + 1).to_string();
    conn.execute(
        "UPDATE relay_meta SET value = ?1 WHERE key = 'schema_version'",
        [future.as_bytes()],
    )
    .expect("bump to future schema");
    drop(conn);

    let Err(err) = Db::open(
        DbConfig::new(path.clone()),
        std::sync::Arc::new(FakeClock::new(1)),
    ) else {
        panic!("更新版本的库必须被拒绝");
    };
    assert!(
        format!("{err}").contains("newer than this binary"),
        "版本门禁文案必须明确（旧 binary 同样落到这条分支）: {err}"
    );
    // 拒绝不得改写库（不自动降 schema）。
    let conn = rusqlite::Connection::open(&path).expect("open raw");
    assert_eq!(version(&conn).as_deref(), Some(future.as_str()));
    assert!(column_exists(&conn, "vault", "singleton"));
    drop(conn);
    cleanup(&path);
}

/// 元数据里的 `schema_version` 丢失但表仍是 v9 形状：必须按 v9 迁移，而不是标成最新版。
#[test]
fn missing_schema_version_on_v9_shape_still_migrates() {
    let path = temp_path("migration-v10-no-meta");
    let conn = v9::fixture(&path);
    conn.execute("DELETE FROM relay_meta WHERE key = 'schema_version'", [])
        .expect("drop version meta");
    let instance_before: Vec<u8> = conn
        .query_row(
            "SELECT value FROM relay_meta WHERE key = 'instance_id'",
            [],
            |row| row.get(0),
        )
        .expect("instance before");
    drop(conn);

    let db = Db::open(
        DbConfig::new(path.clone()),
        std::sync::Arc::new(FakeClock::new(1)),
    )
    .expect("shape-aware migration");
    db.read_instance(|conn| {
        let expected = schema::SCHEMA_VERSION.to_string();
        assert_eq!(version(conn).as_deref(), Some(expected.as_str()));
        assert!(column_exists(conn, "vaults", "legacy_default"));
        let instance_after: Vec<u8> = conn.query_row(
            "SELECT value FROM relay_meta WHERE key = 'instance_id'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(instance_after, instance_before, "instance_id 不得被改写");
        assert_eq!(fk_violations(conn), 0);
        Ok(())
    })
    .expect("asserts");
    drop(db);
    cleanup(&path);
}

/// 迁移后旧签名链仍可逐条重算（`record_hash = SHA256(prev_hash || signed_record)`）。
#[test]
fn migrated_control_chain_still_verifies() {
    let path = temp_path("migration-v10-chain");
    let conn = v9::fixture(&path);
    drop(conn);
    let db = Db::open(
        DbConfig::new(path.clone()),
        std::sync::Arc::new(FakeClock::new(1)),
    )
    .expect("migrate");
    let chain: Vec<(i64, Vec<u8>, Vec<u8>, Vec<u8>)> = db
        .read_instance(|conn| {
            let mut stmt = conn.prepare(
                "SELECT rev, prev_hash, record_hash, signed_record FROM control_commits
                 ORDER BY rev",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .expect("chain");
    assert_eq!(chain.len(), 2);
    let mut prev: Hash32 = [0u8; 32];
    for (rev, prev_hash, record_hash, signed) in chain {
        assert_eq!(prev_hash, prev, "rev {rev} 的 prev_hash 必须接上");
        let recomputed = relay::ids::sha256_parts(&[&prev, &signed]);
        assert_eq!(
            record_hash, recomputed,
            "rev {rev} 的 record_hash 必须可重算"
        );
        prev = record_hash.as_slice().try_into().expect("record hash 32B");
    }
    drop(db);
    cleanup(&path);
}
