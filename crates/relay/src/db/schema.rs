//! relay SQLite schema（ADR 0006 §6 + §13.2；多租户见 ADR 0014 §A）。
//!
//! 硬约束：
//! - **全部表 STRICT**（类型错误直接报错，不做 SQLite 隐式转换）；
//! - `journal_mode=WAL`、`synchronous=FULL`、`foreign_keys=ON`、`busy_timeout=5000`；
//! - schema 10 起同一库支持**多个 vault**（ADR 0014 §A）：`vaults(vault_id)` 一行 = 一个租户，
//!   所有**租户表**以 `vault_id` 为 PK/UNIQUE/FK 前缀，查询必须显式带 `vault_id`；
//! - 只有 `relay_meta`（instance/运维元数据）、`vaults`（租户注册表）与
//!   `create_tickets`（运维创建票据，ADR 0014 §A）是全局表；后者由
//!   `vault::resolve_scope`/`admin`/运维 CLI 读取，绝不作为匿名列表暴露给 HTTP 客户端；
//! - 写入顺序 = 一个 writer 事务（对象 body/parts/log seq/预算 ledger/幂等响应同事务，§6 末段）。
//!
//! 迁移 9 → 10 是**单事务维护窗口**：先 `VACUUM INTO` 一份 schema 9 快照（文件库），
//! 再在 `BEGIN IMMEDIATE` 里重建全部租户表（复合 PK/FK 不能原地 ALTER），逐表校验
//! 行数与多重集一致、`PRAGMA foreign_key_check` 无违规，最后才写 `schema_version=10`。
//!
//! 迁移 10 → 11 只**新增**全局表 `create_tickets`（运维发行的一次性创建票据；ADR 0014 §A）：
//! 单事务、幂等、不改任何已有租户表/数据；旧 binary 打开 schema 11 时按版本门禁拒绝。

use std::path::{Path, PathBuf};

use rusqlite::{Connection, TransactionBehavior};

use crate::error::{Error, Result};
use crate::ids::{random_id16, Id16};

/// 当前 schema 版本。任何破坏性变更都必须 +1 并写显式迁移。
///
/// 9 → 10：多租户（`vaults` + 全部租户表 `vault_id` 前缀，ADR 0014 §A）。
/// 10 → 11：新增全局表 `create_tickets`（`/v2/vaults` 的一次性创建票据）。
pub const SCHEMA_VERSION: i64 = 11;

/// relay_meta 中保存本实例 id 的键（cursor 的 `instance16`）。
pub const META_INSTANCE_ID: &str = "instance_id";
/// relay_meta 中保存 schema 版本的键。
pub const META_SCHEMA_VERSION: &str = "schema_version";
/// relay_meta 中标记一次性 setup secret 已被消费。
pub const META_SETUP_CONSUMED: &str = "setup.consumed";
/// `SHA256(setup secret)`；`init` 写、`serve` 读，重启后仍可完成一次 genesis。
pub const META_SETUP_SECRET_HASH: &str = "setup.secret_hash";

/// 每个连接都要设置的 PRAGMA（`journal_mode` 是持久属性，但重复设置无害）。
pub fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// 建表 + 写入实例元数据；已有旧库则逐级迁移到 [`SCHEMA_VERSION`]。
///
/// 版本**高于**本二进制时明确拒绝（旧 binary 打开新库时同样落到这条门禁上）：
/// relay 不支持自动降 schema，回滚只能恢复完整快照。
pub fn apply(conn: &mut Connection) -> Result<()> {
    configure(conn)?;
    if !table_exists(conn, "relay_meta")? {
        conn.execute_batch(&render_ddl())?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO relay_meta(key, value) VALUES (?1, ?2)",
            (META_INSTANCE_ID, random_id16()?.as_slice()),
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO relay_meta(key, value) VALUES (?1, ?2)",
            (META_SCHEMA_VERSION, SCHEMA_VERSION.to_string().as_bytes()),
        )?;
        tx.commit()?;
        return Ok(());
    }
    let mut version =
        match meta_text(conn, META_SCHEMA_VERSION)?.and_then(|t| t.parse::<i64>().ok()) {
            Some(version) => version,
            None => {
                // 表已建但元数据缺失（半初始化的库）：按表形状推断版本，不覆盖已有数据。
                // 旧 v9 形状 = 有 `vault(singleton)` 且没有 `vaults`；此时必须走 9→10 迁移，
                // 绝不能直接标成最新版（那会把单 vault 数据留成半迁移状态）。
                if table_exists(conn, "vault")? && !table_exists(conn, "vaults")? {
                    set_meta(conn, META_SCHEMA_VERSION, b"9")?;
                    9
                } else {
                    if !table_exists(conn, "vaults")? {
                        // 完全空的库：建表后按最新版初始化。
                        conn.execute_batch(&render_ddl())?;
                    } else {
                        // 已是多租户形状但元数据丢失：补齐 10 → 11 的全局表（IF NOT EXISTS 幂等）。
                        conn.execute_batch(CREATE_TICKETS_DDL)?;
                    }
                    // instance_id 只补缺失，不覆盖已有值（它是游标 `instance16`，被改会让
                    // 所有旧游标失效）。
                    conn.execute(
                        "INSERT OR IGNORE INTO relay_meta(key, value) VALUES (?1, ?2)",
                        (META_INSTANCE_ID, random_id16()?.as_slice()),
                    )?;
                    set_meta(
                        conn,
                        META_SCHEMA_VERSION,
                        SCHEMA_VERSION.to_string().as_bytes(),
                    )?;
                    return Ok(());
                }
            }
        };
    if version > SCHEMA_VERSION {
        return Err(Error::Invariant(
            "relay schema is newer than this binary; refusing to open (downgrade is not supported)",
        ));
    }
    while version < SCHEMA_VERSION {
        version = match version {
            1 => {
                migrate_v1_to_v2(conn)?;
                2
            }
            2 => {
                migrate_v2_to_v3(conn)?;
                3
            }
            3 => {
                migrate_v3_to_v4(conn)?;
                4
            }
            4 => {
                migrate_v4_to_v5(conn)?;
                5
            }
            5 => {
                migrate_v5_to_v6(conn)?;
                6
            }
            6 => {
                migrate_v6_to_v7(conn)?;
                7
            }
            7 => {
                migrate_v7_to_v8(conn)?;
                8
            }
            8 => {
                migrate_v8_to_v9(conn)?;
                9
            }
            9 => {
                migrate_v9_to_v10(conn)?;
                10
            }
            10 => {
                migrate_v10_to_v11(conn)?;
                11
            }
            _ => return Err(Error::Invariant("unsupported relay schema version")),
        };
    }
    Ok(())
}

/// v1 → v2：补齐 `objects.commit_signature` 与 `log.signature`（客户端验签需要）。
fn migrate_v1_to_v2(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    let has_commit_signature = column_exists(&tx, "objects", "commit_signature")?;
    if !has_commit_signature {
        tx.execute("ALTER TABLE objects ADD COLUMN commit_signature BLOB", [])?;
    }
    let has_log_signature = column_exists(&tx, "log", "signature")?;
    if !has_log_signature {
        tx.execute("ALTER TABLE log ADD COLUMN signature BLOB", [])?;
    }
    tx.execute(
        "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
        ("2".as_bytes(), META_SCHEMA_VERSION),
    )?;
    tx.commit()?;
    Ok(())
}

/// v2 → v3：`devices.certificate_signature`（旧行保持 NULL，客户端按缺失降级）。
fn migrate_v2_to_v3(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    if !column_exists(&tx, "devices", "certificate_signature")? {
        tx.execute(
            "ALTER TABLE devices ADD COLUMN certificate_signature BLOB",
            [],
        )?;
    }
    tx.execute(
        "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
        ("3".as_bytes(), META_SCHEMA_VERSION),
    )?;
    tx.commit()?;
    Ok(())
}

/// v3 → v4：`checkpoints.commit_signature`（§20.1：bootstrap 返回生产者签名，旧行保持 NULL）。
fn migrate_v3_to_v4(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    if !column_exists(&tx, "checkpoints", "commit_signature")? {
        tx.execute(
            "ALTER TABLE checkpoints ADD COLUMN commit_signature BLOB",
            [],
        )?;
    }
    tx.execute(
        "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
        ("4".as_bytes(), META_SCHEMA_VERSION),
    )?;
    tx.commit()?;
    Ok(())
}

/// v4 → v5：`epochs.revoked_once`（§23.1：冻结期间发生过吊销的旧 epoch 永久不可解冻）。
fn migrate_v4_to_v5(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    if !column_exists(&tx, "epochs", "revoked_once")? {
        tx.execute(
            "ALTER TABLE epochs ADD COLUMN revoked_once INTEGER NOT NULL DEFAULT 0
             CHECK(revoked_once IN (0, 1))",
            [],
        )?;
        // §27.1：旧库已经 begin(revoke)→abort 的 epoch 必须回填为 1，否则升级后一次 abort 就能解冻。
        tx.execute(
            "UPDATE epochs SET revoked_once = 1
             WHERE epoch IN (SELECT from_epoch FROM rotations WHERE revoked_count > 0)",
            [],
        )?;
    }
    if !column_exists(&tx, "manager_keys", "old_rev")? {
        tx.execute(
            "ALTER TABLE manager_keys ADD COLUMN old_rev INTEGER NOT NULL DEFAULT 0
             CHECK(old_rev >= 0)",
            [],
        )?;
        tx.execute("ALTER TABLE manager_keys ADD COLUMN old_signature BLOB", [])?;
        tx.execute("ALTER TABLE manager_keys ADD COLUMN new_signature BLOB", [])?;
    }
    tx.execute(
        "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
        ("5".as_bytes(), META_SCHEMA_VERSION),
    )?;
    tx.commit()?;
    Ok(())
}

/// v5 → v6（§24）：
/// - checkpoint 增加 `candidate`/`aborted` 生命周期（SQLite 无法改 CHECK → 重建表，FK 临时关闭）；
/// - `device_grants.signature` 持久化外层管理签名。
fn migrate_v5_to_v6(conn: &mut Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = (|| -> Result<()> {
        let tx = conn.transaction()?;
        tx.execute_batch(
            "CREATE TABLE checkpoints_v6(
               checkpoint_id      BLOB PRIMARY KEY CHECK(length(checkpoint_id) = 16),
               epoch              INTEGER NOT NULL CHECK(epoch > 0),
               covered_seq        INTEGER NOT NULL CHECK(covered_seq >= 0),
               manifest_oid       BLOB CHECK(manifest_oid IS NULL OR length(manifest_oid) = 32),
               refs_digest        BLOB CHECK(refs_digest IS NULL OR length(refs_digest) = 32),
               signed_commit      BLOB,
               commit_signature   BLOB,
               state              TEXT NOT NULL CHECK(state IN('staging','candidate','published','aborted','pruned')),
               validation_version INTEGER NOT NULL CHECK(validation_version >= 1),
               health             TEXT NOT NULL CHECK(health IN('unknown','healthy','unhealthy')),
               created_at         INTEGER NOT NULL,
               published_at       INTEGER
             ) STRICT;
             INSERT INTO checkpoints_v6
               (checkpoint_id, epoch, covered_seq, manifest_oid, refs_digest, signed_commit,
                commit_signature, state, validation_version, health, created_at, published_at)
             SELECT checkpoint_id, epoch, covered_seq, manifest_oid, refs_digest, signed_commit,
                    commit_signature, state, validation_version, health, created_at, published_at
             FROM checkpoints;
             DROP TABLE checkpoints;
             ALTER TABLE checkpoints_v6 RENAME TO checkpoints;
             CREATE INDEX IF NOT EXISTS checkpoints_state ON checkpoints(state, epoch);
             CREATE INDEX IF NOT EXISTS checkpoints_published ON checkpoints(published_at);",
        )?;
        if !column_exists(&tx, "device_grants", "signature")? {
            tx.execute("ALTER TABLE device_grants ADD COLUMN signature BLOB", [])?;
        }
        tx.execute(
            "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
            ("6".as_bytes(), META_SCHEMA_VERSION),
        )?;
        tx.commit()?;
        Ok(())
    })();
    conn.pragma_update(None, "foreign_keys", "ON")?;
    result
}

/// v6 → v7（§25.1）：不透明资产登记表 + rotations.asset_set_digest。
fn migrate_v6_to_v7(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS asset_objects(
           epoch         INTEGER NOT NULL CHECK(epoch > 0),
           object_id     BLOB NOT NULL CHECK(length(object_id) = 32),
           state         TEXT NOT NULL CHECK(state IN('active','retired')),
           finalized_seq INTEGER NOT NULL CHECK(finalized_seq >= 0),
           rotation_id   BLOB CHECK(rotation_id IS NULL OR length(rotation_id) = 16),
           PRIMARY KEY(epoch, object_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS asset_objects_epoch ON asset_objects(epoch, state, finalized_seq);",
    )?;
    if !column_exists(&tx, "rotations", "asset_set_digest")? {
        tx.execute(
            "ALTER TABLE rotations ADD COLUMN asset_set_digest BLOB
             CHECK(asset_set_digest IS NULL OR length(asset_set_digest) = 32)",
            [],
        )?;
    }
    tx.execute(
        "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
        ("7".as_bytes(), META_SCHEMA_VERSION),
    )?;
    tx.commit()?;
    Ok(())
}

/// v7 → v8（§28.1 条目 2/3/5）：把 activate 的事实、候选角色与 grant 绑定信息落库。
///
/// - `checkpoints.role`：候选 checkpoint 的固定角色（primary/fallback，activate 时落定）；
/// - `rotations.primary_checkpoint/fallback_checkpoint/activation_commit/activated_at`：
///   activate 指名并 promote 的**两个** cid 与激活 commit；
/// - `device_grants.grant_id/activation_ref`：grant 的 AAD grant_id 与所属轮换
///   （wire 必须能回读 `grant_id`/`activation_ref`，§24.2/评审 §三 4）。
fn migrate_v7_to_v8(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    if !column_exists(&tx, "checkpoints", "role")? {
        tx.execute(
            "ALTER TABLE checkpoints ADD COLUMN role TEXT
             CHECK(role IS NULL OR role IN('primary','fallback'))",
            [],
        )?;
    }
    if !column_exists(&tx, "rotations", "primary_checkpoint")? {
        tx.execute(
            "ALTER TABLE rotations ADD COLUMN primary_checkpoint BLOB
             CHECK(primary_checkpoint IS NULL OR length(primary_checkpoint) = 16)",
            [],
        )?;
        tx.execute(
            "ALTER TABLE rotations ADD COLUMN fallback_checkpoint BLOB
             CHECK(fallback_checkpoint IS NULL OR length(fallback_checkpoint) = 16)",
            [],
        )?;
        tx.execute(
            "ALTER TABLE rotations ADD COLUMN activation_commit BLOB
             CHECK(activation_commit IS NULL OR length(activation_commit) = 32)",
            [],
        )?;
        tx.execute("ALTER TABLE rotations ADD COLUMN activated_at INTEGER", [])?;
    }
    if !column_exists(&tx, "device_grants", "grant_id")? {
        tx.execute(
            "ALTER TABLE device_grants ADD COLUMN grant_id BLOB
             CHECK(grant_id IS NULL OR length(grant_id) = 16)",
            [],
        )?;
        tx.execute(
            "ALTER TABLE device_grants ADD COLUMN activation_ref BLOB
             CHECK(activation_ref IS NULL OR length(activation_ref) = 16)",
            [],
        )?;
    }
    tx.execute(
        "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
        ("8".as_bytes(), META_SCHEMA_VERSION),
    )?;
    tx.commit()?;
    Ok(())
}

/// v8 → v9：`rotations.activated_control_rev`（wire 要回读激活 commit 的 control_rev，
/// 与 `activation_commit` 的 seq 配对；旧行为 NULL）。
fn migrate_v8_to_v9(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    if !column_exists(&tx, "rotations", "activated_control_rev")? {
        tx.execute(
            "ALTER TABLE rotations ADD COLUMN activated_control_rev INTEGER
             CHECK(activated_control_rev IS NULL OR activated_control_rev >= 0)",
            [],
        )?;
    }
    tx.execute(
        "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
        ("9".as_bytes(), META_SCHEMA_VERSION),
    )?;
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------- v9 → v10（多租户）

/// 迁移 9 → 10 的故障注入点（只给测试用，生产路径永远是 [`MigrationFault::None`]）。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationFault {
    /// 不注入故障。
    None,
    /// 复制一半租户表后强制失败（验证事务回滚 + schema 9 快照仍可恢复）。
    Halfway,
}

/// v9 → v10：单 vault → 多租户（ADR 0014 §A）。
///
/// 步骤（维护窗口，单事务）：
/// 1. 文件库先 `VACUUM INTO <db>.schema9.bak`（WAL 一致快照；已存在则不再覆盖）；
/// 2. `PRAGMA foreign_keys=OFF`（复合 FK 不能原地改，重建后再统一校验）；
/// 3. `BEGIN IMMEDIATE`：逐表 `CREATE <t>__v10` → 复制并补 `vault_id` → `DROP` 旧表 →
///    `RENAME`；`vault` 行转 `vaults` 一行并标记 `legacy_default=1`；
/// 4. 每表校验行数与双向多重集一致；`PRAGMA foreign_key_check` 必须为空；
/// 5. 写 `schema_version=10` 并提交；任何一步失败 → 整体回滚（旧库仍是 schema 9）。
fn migrate_v9_to_v10(conn: &mut Connection) -> Result<()> {
    migrate_v9_to_v10_impl(conn, MigrationFault::None)
}

/// 测试入口：验证迁移中断/回滚/备份（见 `tests/migration_v10.rs`）。
#[doc(hidden)]
pub fn migrate_v9_to_v10_with_fault(conn: &mut Connection, fault: MigrationFault) -> Result<()> {
    migrate_v9_to_v10_impl(conn, fault)
}

/// 测试专用：重放 v4→v5 的 `revoked_once` 回填（§27.1），不依赖列是否存在。
///
/// 生产路径只在升级时执行一次；测试用它证明"旧库回填口径"与"重启后事实不丢"。
#[doc(hidden)]
pub fn replay_revoked_once_backfill(conn: &crate::scope::ScopeConn) -> Result<u64> {
    let changed = conn.execute(
        "UPDATE epochs SET revoked_once = 1
         WHERE vault_id = ?1
           AND epoch IN (SELECT from_epoch FROM rotations
                         WHERE revoked_count > 0 AND vault_id = ?1)",
        [conn.scope().as_slice()],
    )?;
    Ok(u64::try_from(changed).unwrap_or(u64::MAX))
}

fn migrate_v9_to_v10_impl(conn: &mut Connection, fault: MigrationFault) -> Result<()> {
    backup_before_v10(conn)?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let outcome = (|| -> Result<()> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let vault_id = legacy_vault_id_for_migration(&tx)?;
        let mut copied = 0usize;
        for (name, template, legacy_skip) in TENANT_TABLES {
            let staging = staging_name(name);
            tx.execute_batch(&template.replace("{table}", &staging))?;
            copy_rows(&tx, name, &staging, vault_id.as_ref(), legacy_skip)?;
            tx.execute_batch(&format!(
                "DROP TABLE {name};\nALTER TABLE {staging} RENAME TO {name};"
            ))?;
            copied += 1;
            if fault == MigrationFault::Halfway && copied * 2 >= TENANT_TABLES.len() {
                return Err(Error::Invariant("injected migration fault (test)"));
            }
        }
        migrate_vault_row_to_vaults(&tx, vault_id.as_ref())?;
        // ADR 0014 §A 维护窗口：旧 challenge/token 是短命凭据，升级后一律失效
        // （客户端重新认证即可；这与"保留已消费邀请事实"是两件事）。
        tx.execute("DELETE FROM challenges", [])?;
        tx.execute("DELETE FROM tokens", [])?;
        tx.execute_batch(INDEXES)?;
        if vault_id.is_none() {
            ensure_tenant_tables_empty(&tx)?;
        }
        verify_migration(&tx)?;
        // 这一步只负责到 10：后续 10 → 11 由 `apply` 的迁移循环继续执行。
        // （绝不能写 `SCHEMA_VERSION`——那会把库标成最新版却跳过 10→11 的新增表。）
        tx.execute(
            "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
            ("10".as_bytes(), META_SCHEMA_VERSION),
        )?;
        tx.commit()?;
        Ok(())
    })();
    conn.pragma_update(None, "foreign_keys", "ON")?;
    outcome
}

// ---------------------------------------------------------------- v10 → v11（创建票据）

/// v10 → 11：只新增全局表 `create_tickets`（ADR 0014 §A 运维创建票据）。
///
/// 单事务 + `IF NOT EXISTS`：重复执行幂等；不触碰任何已有租户表/数据，也不需要维护窗口
/// 之外的备份（表是新增的，回滚只需换回旧 binary 并恢复快照）。
fn migrate_v10_to_v11(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(CREATE_TICKETS_DDL)?;
    tx.execute(
        "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
        ("11".as_bytes(), META_SCHEMA_VERSION),
    )?;
    tx.commit()?;
    Ok(())
}

/// 迁移前备份：`VACUUM INTO` 生成 WAL 一致快照（内存库/已存在快照则跳过）。
fn backup_before_v10(conn: &Connection) -> Result<()> {
    let Some(path) = conn.path() else {
        return Ok(());
    };
    if path.is_empty() || path == ":memory:" {
        return Ok(());
    }
    let target = backup_path(Path::new(path));
    if target.exists() {
        // 已有维护窗口快照：保留最早的一份，不覆盖（回滚需要的是迁移前状态）。
        return Ok(());
    }
    conn.execute("VACUUM INTO ?1", [target.to_string_lossy().as_ref()])?;
    Ok(())
}

/// `<db>.schema9.bak`：迁移 9 → 10 之前的一致快照。
pub fn backup_path(db: &Path) -> PathBuf {
    PathBuf::from(format!("{}.schema9.bak", db.display()))
}

fn staging_name(name: &str) -> String {
    format!("{name}__v10")
}

/// 读旧库里唯一的 vault_id；没有任何 vault 行时返回 `None`（租户表必须为空）。
fn legacy_vault_id_for_migration(conn: &Connection) -> Result<Option<Id16>> {
    if !table_exists(conn, "vault")? {
        return Ok(None);
    }
    let mut stmt = conn.prepare("SELECT vault_id FROM vault")?;
    let mut rows = stmt.query([])?;
    let mut out = None;
    let mut count = 0usize;
    while let Some(row) = rows.next()? {
        let bytes: Vec<u8> = row.get(0)?;
        count += 1;
        if count > 1 {
            return Err(Error::Invariant(
                "migration: more than one legacy vault row",
            ));
        }
        out = Some(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("migration: vault_id is not 16 bytes"))?,
        );
    }
    Ok(out)
}

fn ensure_tenant_tables_empty(conn: &Connection) -> Result<()> {
    for (name, _, _) in TENANT_TABLES {
        let count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {name}"), [], |row| {
            row.get(0)
        })?;
        if count != 0 {
            return Err(Error::Invariant(
                "migration: tenant rows exist without a vault row",
            ));
        }
    }
    Ok(())
}

/// 把旧 `vault` 单行转成 `vaults` 一行（保留全部字段，标记 `legacy_default=1`）。
fn migrate_vault_row_to_vaults(conn: &Connection, vault_id: Option<&Id16>) -> Result<()> {
    let staging = staging_name("vaults");
    conn.execute_batch(&vaults_template().replace("{table}", &staging))?;
    let mut columns = table_columns(conn, "vault")?;
    // v9 的 vault 以 `singleton=1` 为 PK；v10 改成 `vault_id` PK。
    columns.retain(|column| column != "singleton");
    let list = columns.join(", ");
    let old_count: i64 = conn.query_row("SELECT COUNT(*) FROM vault", [], |row| row.get(0))?;
    if old_count > 0 {
        conn.execute(
            &format!(
                "INSERT INTO {staging}(vault_id, {list}, legacy_default)
                 SELECT vault_id, {list}, 1 FROM vault"
            ),
            [],
        )?;
        // 迁移前逐行核对（vault 是单行表，`vault_id` 必须与租户表所用一致）。
        let expected = vault_id.ok_or(Error::Invariant("migration: vault row without id"))?;
        let copied_id: Vec<u8> =
            conn.query_row(&format!("SELECT vault_id FROM {staging}"), [], |row| {
                row.get(0)
            })?;
        if copied_id.as_slice() != expected.as_slice() {
            return Err(Error::Invariant("migration: vaults.vault_id mismatch"));
        }
        let mut check = String::new();
        for (index, column) in columns.iter().enumerate() {
            if index > 0 {
                check.push_str(" AND ");
            }
            check.push_str(&format!("n.{column} = o.{column}"));
        }
        let mismatched: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {staging} n JOIN vault o ON o.vault_id = n.vault_id WHERE NOT ({check})"),
            [],
            |row| row.get(0),
        )?;
        if mismatched != 0 {
            return Err(Error::Invariant("migration: vault row digest mismatch"));
        }
    }
    conn.execute_batch(&format!(
        "DROP TABLE vault;\nALTER TABLE {staging} RENAME TO vaults;\n\
         CREATE UNIQUE INDEX IF NOT EXISTS vaults_legacy_default \
         ON vaults(legacy_default) WHERE legacy_default = 1;"
    ))?;
    Ok(())
}

/// 复制一张租户表：补 `vault_id`，并校验行数与双向多重集完全一致。
fn copy_rows(
    conn: &Connection,
    old: &str,
    new: &str,
    vault_id: Option<&Id16>,
    legacy_skip: &[&str],
) -> Result<()> {
    let mut columns = table_columns(conn, old)?;
    columns.retain(|column| !legacy_skip.iter().any(|skip| skip == column));
    if columns.is_empty() {
        return Err(Error::Invariant("migration: missing tenant table"));
    }
    let list = columns.join(", ");
    let old_count: i64 =
        conn.query_row(&format!("SELECT COUNT(*) FROM {old}"), [], |row| row.get(0))?;
    if old_count > 0 {
        let id = vault_id.ok_or(Error::Invariant(
            "migration: tenant rows exist without a vault row",
        ))?;
        conn.execute(
            &format!("INSERT INTO {new}(vault_id, {list}) SELECT ?1, {list} FROM {old}"),
            [id.as_slice()],
        )?;
    }
    let new_count: i64 =
        conn.query_row(&format!("SELECT COUNT(*) FROM {new}"), [], |row| row.get(0))?;
    if old_count != new_count {
        return Err(Error::Invariant("migration: row count mismatch"));
    }
    if old_count == 0 {
        return Ok(());
    }
    // 双向 EXCEPT + 分组计数 = 多重集完全一致（BLOB 逐字节比较，NULL 视为相等）。
    for (left, right) in [(old, new), (new, old)] {
        let sql = format!(
            "SELECT COUNT(*) FROM (
               SELECT {list}, COUNT(*) AS multiplicity FROM {left} GROUP BY {list}
               EXCEPT
               SELECT {list}, COUNT(*) AS multiplicity FROM {right} GROUP BY {list}
             )"
        );
        let diff: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
        if diff != 0 {
            return Err(Error::Invariant("migration: row digest mismatch"));
        }
    }
    Ok(())
}

/// 迁移后的完整性校验：FK 全部可解析、且 `schema_version` 仍是旧值（由调用方最后改）。
fn verify_migration(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let mut rows = stmt.query([])?;
    if rows.next()?.is_some() {
        return Err(Error::Invariant(
            "migration: foreign_key_check reported violations",
        ));
    }
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(names.iter().any(|name| name == column))
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(names)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// 读取本实例 id。
pub fn instance_id(conn: &Connection) -> Result<Id16> {
    let mut stmt = conn.prepare("SELECT value FROM relay_meta WHERE key = ?1")?;
    let value: Vec<u8> = stmt
        .query_row([META_INSTANCE_ID], |row| row.get(0))
        .map_err(|_| Error::Invariant("relay_meta.instance_id missing"))?;
    value
        .as_slice()
        .try_into()
        .map_err(|_| Error::Invariant("relay_meta.instance_id is not 16 bytes"))
}

/// relay_meta 的 KV 读（字符串）。
pub fn meta_text(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM relay_meta WHERE key = ?1")?;
    let row = stmt.query_row([key], |row| row.get::<_, Vec<u8>>(0));
    match row {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| Error::Invariant("relay_meta value is not utf8")),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// relay_meta 的 KV 写。
pub fn set_meta(conn: &Connection, key: &str, value: &[u8]) -> Result<()> {
    conn.execute(
        "INSERT INTO relay_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    )?;
    Ok(())
}

/// 全局运维读：库内所有 vault_id（**不**面向 HTTP；GC/状态 CLI 用）。
pub fn list_vault_ids(conn: &Connection) -> Result<Vec<Id16>> {
    let mut stmt = conn.prepare("SELECT vault_id FROM vaults ORDER BY created_at, rowid")?;
    let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    let mut out = Vec::new();
    for row in rows {
        let bytes = row?;
        out.push(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("vaults.vault_id is not 16 bytes"))?,
        );
    }
    Ok(out)
}

/// 全局运维读：显式解析 v1 旧的 legacy 默认 vault（`legacy_default=1` 至多一行）。
pub fn legacy_default_vault_id(conn: &Connection) -> Result<Option<Id16>> {
    let value: Option<Vec<u8>> = conn
        .query_row(
            "SELECT vault_id FROM vaults WHERE legacy_default = 1",
            [],
            |row| row.get(0),
        )
        .ok();
    match value {
        None => Ok(None),
        Some(bytes) => {
            Ok(Some(bytes.as_slice().try_into().map_err(|_| {
                Error::Invariant("vaults.vault_id is not 16 bytes")
            })?))
        }
    }
}

/// `vaults` 表模板（迁移与新建共用同一份列定义）。
fn vaults_template() -> String {
    format!("CREATE TABLE {{table}}(\n{VAULTS_COLUMNS}\n) STRICT;")
}

const VAULTS_COLUMNS: &str = "  vault_id          BLOB PRIMARY KEY CHECK(length(vault_id) = 16),
  instance_id       BLOB NOT NULL CHECK(length(instance_id) = 16),
  log_id            BLOB NOT NULL CHECK(length(log_id) = 16),
  log_incarnation   BLOB NOT NULL CHECK(length(log_incarnation) = 16),
  cursor_key        BLOB NOT NULL CHECK(length(cursor_key) = 32),
  state             TEXT NOT NULL CHECK(state IN('initializing','active','rotating')),
  active_epoch      INTEGER NOT NULL CHECK(active_epoch > 0),
  next_epoch        INTEGER NOT NULL CHECK(next_epoch > active_epoch),
  manager_rev       INTEGER NOT NULL CHECK(manager_rev >= 1),
  control_rev       INTEGER NOT NULL CHECK(control_rev >= 1),
  auth_rev          INTEGER NOT NULL CHECK(auth_rev >= 1),
  head_seq          INTEGER NOT NULL CHECK(head_seq >= 0),
  gc_floor          INTEGER NOT NULL CHECK(gc_floor >= 0 AND gc_floor <= head_seq),
  normal_limit      INTEGER NOT NULL CHECK(normal_limit > 0),
  maintenance_limit INTEGER NOT NULL CHECK(maintenance_limit >= normal_limit),
  legacy_default    INTEGER NOT NULL DEFAULT 0 CHECK(legacy_default IN (0, 1)),
  created_at        INTEGER NOT NULL";

const GLOBAL_DDL: &str = r#"
-- ===== 实例元数据（全局）=====
CREATE TABLE IF NOT EXISTS relay_meta(
  key   TEXT PRIMARY KEY,
  value BLOB NOT NULL
) STRICT;

-- ===== 租户注册表（ADR 0014 §A）：一行 = 一个 vault = 一个租户 =====
CREATE TABLE IF NOT EXISTS vaults(
  vault_id          BLOB PRIMARY KEY CHECK(length(vault_id) = 16),
  instance_id       BLOB NOT NULL CHECK(length(instance_id) = 16),
  log_id            BLOB NOT NULL CHECK(length(log_id) = 16),
  log_incarnation   BLOB NOT NULL CHECK(length(log_incarnation) = 16),
  cursor_key        BLOB NOT NULL CHECK(length(cursor_key) = 32),
  state             TEXT NOT NULL CHECK(state IN('initializing','active','rotating')),
  active_epoch      INTEGER NOT NULL CHECK(active_epoch > 0),
  next_epoch        INTEGER NOT NULL CHECK(next_epoch > active_epoch),
  manager_rev       INTEGER NOT NULL CHECK(manager_rev >= 1),
  control_rev       INTEGER NOT NULL CHECK(control_rev >= 1),
  auth_rev          INTEGER NOT NULL CHECK(auth_rev >= 1),
  head_seq          INTEGER NOT NULL CHECK(head_seq >= 0),
  gc_floor          INTEGER NOT NULL CHECK(gc_floor >= 0 AND gc_floor <= head_seq),
  normal_limit      INTEGER NOT NULL CHECK(normal_limit > 0),
  maintenance_limit INTEGER NOT NULL CHECK(maintenance_limit >= normal_limit),
  legacy_default    INTEGER NOT NULL DEFAULT 0 CHECK(legacy_default IN (0, 1)),
  created_at        INTEGER NOT NULL
) STRICT;
-- v1 旧 API 的 legacy 默认 vault 至多一个（显式解析，禁止按 subject 猜租户）。
CREATE UNIQUE INDEX IF NOT EXISTS vaults_legacy_default ON vaults(legacy_default) WHERE legacy_default = 1;
"#;

/// 运维创建票据（ADR 0014 §A）：**全局表**，因为票据发行时目标 `vaults` 行还不存在。
///
/// - `token_hash = SHA256(32B 明文)`，主键；明文只在 `admin::issue_create_ticket` 返回一次，
///   不落库、不进日志；
/// - `vault_id` 是票据绑定的 128-bit **公开路由标识**（不是授权）；消费时 `route.vault` 必须与
///   它一致；
/// - `quota` = 该票据授权的创建次数（`>= 1`；一次性票据为 1），每次成功创建消费 1 次；
/// - `consumed_at` 只在额度归零时写入（一次性票据即首次成功消费），之后的任何使用都拒绝。
const CREATE_TICKETS_DDL: &str = r#"
-- ===== 运维创建票据（全局；ADR 0014 §A `/v2/vaults`）=====
CREATE TABLE IF NOT EXISTS create_tickets(
  token_hash  BLOB PRIMARY KEY CHECK(length(token_hash) = 32),
  vault_id    BLOB NOT NULL CHECK(length(vault_id) = 16),
  expires_at  INTEGER NOT NULL,
  quota       INTEGER NOT NULL,
  consumed_at INTEGER
) STRICT;
CREATE INDEX IF NOT EXISTS create_tickets_vault ON create_tickets(vault_id);
"#;

/// 租户表清单（顺序 = 建表/迁移顺序，满足 FK 依赖）。
///
/// 每张表都以 `vault_id BLOB NOT NULL CHECK(length(vault_id) = 16)` 开头，
/// PK/UNIQUE/FK 全部带 `vault_id` 前缀（ADR 0014 §A）。
const TENANT_TABLES: &[(&str, &str, &[&str])] = &[
    (
        "manager_keys",
        "CREATE TABLE {table}(
  vault_id      BLOB NOT NULL CHECK(length(vault_id) = 16),
  rev           INTEGER NOT NULL CHECK(rev >= 1),
  pubkey        BLOB NOT NULL CHECK(length(pubkey) = 32),
  transition    BLOB NOT NULL,
  old_rev       INTEGER NOT NULL DEFAULT 0 CHECK(old_rev >= 0),
  old_signature BLOB,
  new_signature BLOB,
  created_at    INTEGER NOT NULL,
  PRIMARY KEY(vault_id, rev)
) STRICT;",
        &[],
    ),
    (
        "control_commits",
        "CREATE TABLE {table}(
  vault_id     BLOB NOT NULL CHECK(length(vault_id) = 16),
  rev          INTEGER NOT NULL CHECK(rev >= 1),
  prev_hash    BLOB NOT NULL CHECK(length(prev_hash) = 32),
  record_hash  BLOB NOT NULL CHECK(length(record_hash) = 32),
  signed_record BLOB NOT NULL,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY(vault_id, rev)
) STRICT;",
        &[],
    ),
    (
        "epochs",
        "CREATE TABLE {table}(
  vault_id     BLOB NOT NULL CHECK(length(vault_id) = 16),
  epoch        INTEGER NOT NULL CHECK(epoch > 0),
  state        TEXT NOT NULL CHECK(state IN('staged','active','retired','aborted')),
  revoked_once INTEGER NOT NULL DEFAULT 0 CHECK(revoked_once IN (0, 1)),
  created_at   INTEGER NOT NULL,
  retire_at    INTEGER,
  PRIMARY KEY(vault_id, epoch)
) STRICT;",
        &[],
    ),
    (
        "devices",
        "CREATE TABLE {table}(
  vault_id              BLOB NOT NULL CHECK(length(vault_id) = 16),
  device_id             BLOB NOT NULL CHECK(length(device_id) = 16),
  sign_pk               BLOB NOT NULL CHECK(length(sign_pk) = 32),
  hpke_pk               BLOB NOT NULL CHECK(length(hpke_pk) = 32),
  certificate           BLOB NOT NULL,
  certificate_signature BLOB,
  status                TEXT NOT NULL CHECK(status IN('pending','active','revoked')),
  created_at            INTEGER NOT NULL,
  revoked_at            INTEGER,
  last_seen             INTEGER,
  PRIMARY KEY(vault_id, device_id)
) STRICT;",
        &[],
    ),
    (
        "challenges",
        "CREATE TABLE {table}(
  vault_id     BLOB NOT NULL CHECK(length(vault_id) = 16),
  challenge_id BLOB NOT NULL CHECK(length(challenge_id) = 16),
  realm        TEXT NOT NULL CHECK(realm IN('device','management')),
  subject      BLOB NOT NULL CHECK(length(subject) = 16),
  nonce        BLOB NOT NULL CHECK(length(nonce) = 32),
  expires_at   INTEGER NOT NULL,
  used_at      INTEGER,
  PRIMARY KEY(vault_id, challenge_id)
) STRICT;",
        &[],
    ),
    (
        "tokens",
        "CREATE TABLE {table}(
  vault_id   BLOB NOT NULL CHECK(length(vault_id) = 16),
  token_hash BLOB NOT NULL CHECK(length(token_hash) = 32),
  realm      TEXT NOT NULL CHECK(realm IN('device','management')),
  subject    BLOB NOT NULL CHECK(length(subject) = 16),
  auth_rev   INTEGER NOT NULL CHECK(auth_rev >= 1),
  expires_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY(vault_id, token_hash)
) STRICT;",
        &[],
    ),
    (
        "invites",
        "CREATE TABLE {table}(
  vault_id       BLOB NOT NULL CHECK(length(vault_id) = 16),
  invite_id      BLOB NOT NULL CHECK(length(invite_id) = 16),
  token_hash     BLOB NOT NULL CHECK(length(token_hash) = 32),
  expires_at     INTEGER NOT NULL,
  pending_device BLOB CHECK(pending_device IS NULL OR length(pending_device) = 16),
  used_at        INTEGER,
  PRIMARY KEY(vault_id, invite_id),
  UNIQUE(vault_id, token_hash)
) STRICT;",
        &[],
    ),
    (
        "maintenance_reservations",
        "CREATE TABLE {table}(
  vault_id       BLOB NOT NULL CHECK(length(vault_id) = 16),
  reservation_id BLOB NOT NULL CHECK(length(reservation_id) = 16),
  purpose        TEXT NOT NULL CHECK(purpose IN('checkpoint','rotation')),
  resource_id    BLOB NOT NULL,
  owner_device   BLOB NOT NULL CHECK(length(owner_device) = 16),
  reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes >= 0),
  consumed_bytes INTEGER NOT NULL CHECK(consumed_bytes >= 0 AND consumed_bytes <= reserved_bytes),
  state          TEXT NOT NULL CHECK(state IN('active','complete','expired')),
  expires_at     INTEGER NOT NULL,
  PRIMARY KEY(vault_id, reservation_id),
  UNIQUE(vault_id, resource_id),
  FOREIGN KEY(vault_id, owner_device) REFERENCES devices(vault_id, device_id)
) STRICT;",
        &[],
    ),
    (
        "usage",
        "CREATE TABLE {table}(
  vault_id          BLOB NOT NULL CHECK(length(vault_id) = 16),
  normal_bytes      INTEGER NOT NULL CHECK(normal_bytes >= 0),
  maintenance_bytes INTEGER NOT NULL CHECK(maintenance_bytes >= 0),
  PRIMARY KEY(vault_id)
) STRICT;",
        // v9 的 usage 用 `singleton=1` 当 PK；v10 改成 `vault_id`，复制时丢掉该列。
        &["singleton"],
    ),
    (
        "objects",
        "CREATE TABLE {table}(
  vault_id         BLOB NOT NULL CHECK(length(vault_id) = 16),
  object_id        BLOB NOT NULL CHECK(length(object_id) = 32),
  message_id       BLOB NOT NULL CHECK(length(message_id) = 16),
  producer         BLOB NOT NULL CHECK(length(producer) = 16),
  epoch            INTEGER NOT NULL CHECK(epoch > 0),
  type             INTEGER NOT NULL CHECK(type BETWEEN 1 AND 5),
  generation       INTEGER NOT NULL CHECK(generation >= 0),
  sender_seq       INTEGER NOT NULL CHECK(sender_seq >= 0),
  part_count       INTEGER NOT NULL CHECK(part_count >= 1),
  plain_len        INTEGER NOT NULL CHECK(plain_len >= 1),
  state            TEXT NOT NULL CHECK(state IN('staging','complete')),
  budget_class     TEXT NOT NULL CHECK(budget_class IN('normal','maintenance','retired')),
  digest           BLOB CHECK(digest IS NULL OR length(digest) = 32),
  signed_commit    BLOB,
  commit_signature BLOB,
  reservation_id   BLOB CHECK(reservation_id IS NULL OR length(reservation_id) = 16),
  created_at       INTEGER NOT NULL,
  touched_at       INTEGER NOT NULL,
  completed_at     INTEGER,
  PRIMARY KEY(vault_id, object_id),
  UNIQUE(vault_id, epoch, message_id),
  FOREIGN KEY(vault_id, reservation_id) REFERENCES maintenance_reservations(vault_id, reservation_id)
) STRICT;",
        &[],
    ),
    (
        "object_parts",
        "CREATE TABLE {table}(
  vault_id  BLOB NOT NULL CHECK(length(vault_id) = 16),
  object_id BLOB NOT NULL CHECK(length(object_id) = 32),
  part_no   INTEGER NOT NULL CHECK(part_no >= 0),
  envelope  BLOB NOT NULL CHECK(length(envelope) BETWEEN 212 AND 4194304),
  digest    BLOB NOT NULL CHECK(length(digest) = 32),
  PRIMARY KEY(vault_id, object_id, part_no),
  FOREIGN KEY(vault_id, object_id) REFERENCES objects(vault_id, object_id)
) STRICT;",
        &[],
    ),
    (
        "log",
        "CREATE TABLE {table}(
  vault_id   BLOB NOT NULL CHECK(length(vault_id) = 16),
  relay_seq  INTEGER NOT NULL CHECK(relay_seq > 0),
  kind       TEXT NOT NULL CHECK(kind IN('update','asset','checkpoint','control')),
  object_id  BLOB CHECK(object_id IS NULL OR length(object_id) = 32),
  epoch      INTEGER NOT NULL CHECK(epoch > 0),
  event      BLOB NOT NULL,
  signature  BLOB,
  created_at INTEGER NOT NULL,
  PRIMARY KEY(vault_id, relay_seq)
) STRICT;",
        &[],
    ),
    (
        "checkpoints",
        "CREATE TABLE {table}(
  vault_id           BLOB NOT NULL CHECK(length(vault_id) = 16),
  checkpoint_id      BLOB NOT NULL CHECK(length(checkpoint_id) = 16),
  epoch              INTEGER NOT NULL CHECK(epoch > 0),
  covered_seq        INTEGER NOT NULL CHECK(covered_seq >= 0),
  manifest_oid       BLOB CHECK(manifest_oid IS NULL OR length(manifest_oid) = 32),
  refs_digest        BLOB CHECK(refs_digest IS NULL OR length(refs_digest) = 32),
  signed_commit      BLOB,
  commit_signature   BLOB,
  state              TEXT NOT NULL CHECK(state IN('staging','candidate','published','aborted','pruned')),
  role               TEXT CHECK(role IS NULL OR role IN('primary','fallback')),
  validation_version INTEGER NOT NULL CHECK(validation_version >= 1),
  health             TEXT NOT NULL CHECK(health IN('unknown','healthy','unhealthy')),
  created_at         INTEGER NOT NULL,
  published_at       INTEGER,
  PRIMARY KEY(vault_id, checkpoint_id)
) STRICT;",
        &[],
    ),
    (
        "checkpoint_refs",
        "CREATE TABLE {table}(
  vault_id      BLOB NOT NULL CHECK(length(vault_id) = 16),
  checkpoint_id BLOB NOT NULL CHECK(length(checkpoint_id) = 16),
  object_id     BLOB NOT NULL CHECK(length(object_id) = 32),
  role          TEXT NOT NULL CHECK(role IN('data','manifest')),
  PRIMARY KEY(vault_id, checkpoint_id, object_id),
  FOREIGN KEY(vault_id, checkpoint_id) REFERENCES checkpoints(vault_id, checkpoint_id)
) STRICT;",
        &[],
    ),
    (
        "asset_objects",
        "CREATE TABLE {table}(
  vault_id      BLOB NOT NULL CHECK(length(vault_id) = 16),
  epoch         INTEGER NOT NULL CHECK(epoch > 0),
  object_id     BLOB NOT NULL CHECK(length(object_id) = 32),
  state         TEXT NOT NULL CHECK(state IN('active','retired')),
  finalized_seq INTEGER NOT NULL CHECK(finalized_seq >= 0),
  rotation_id   BLOB CHECK(rotation_id IS NULL OR length(rotation_id) = 16),
  PRIMARY KEY(vault_id, epoch, object_id)
) STRICT;",
        &[],
    ),
    (
        "key_slots",
        "CREATE TABLE {table}(
  vault_id     BLOB NOT NULL CHECK(length(vault_id) = 16),
  slot_id      BLOB NOT NULL CHECK(length(slot_id) = 32),
  epoch        INTEGER NOT NULL CHECK(epoch > 0),
  kind         TEXT NOT NULL CHECK(kind IN('password','recovery')),
  descriptor   BLOB NOT NULL,
  state        TEXT NOT NULL CHECK(state IN('staging','active','retiring')),
  slot_set_rev INTEGER NOT NULL CHECK(slot_set_rev >= 0),
  retire_at    INTEGER,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY(vault_id, slot_id),
  FOREIGN KEY(vault_id, slot_id) REFERENCES objects(vault_id, object_id)
) STRICT;",
        &[],
    ),
    (
        "device_grants",
        "CREATE TABLE {table}(
  vault_id        BLOB NOT NULL CHECK(length(vault_id) = 16),
  epoch           INTEGER NOT NULL CHECK(epoch > 0),
  device_id       BLOB NOT NULL CHECK(length(device_id) = 16),
  manager_rev     INTEGER NOT NULL CHECK(manager_rev >= 1),
  hpke_enc        BLOB NOT NULL CHECK(length(hpke_enc) = 32),
  hpke_ciphertext BLOB NOT NULL,
  signed_header   BLOB NOT NULL,
  signature       BLOB,
  grant_id        BLOB CHECK(grant_id IS NULL OR length(grant_id) = 16),
  activation_ref  BLOB CHECK(activation_ref IS NULL OR length(activation_ref) = 16),
  PRIMARY KEY(vault_id, epoch, device_id),
  FOREIGN KEY(vault_id, device_id) REFERENCES devices(vault_id, device_id)
) STRICT;",
        &[],
    ),
    (
        "rotations",
        "CREATE TABLE {table}(
  vault_id       BLOB NOT NULL CHECK(length(vault_id) = 16),
  rotation_id    BLOB NOT NULL CHECK(length(rotation_id) = 16),
  from_epoch     INTEGER NOT NULL CHECK(from_epoch > 0),
  to_epoch       INTEGER NOT NULL CHECK(to_epoch > from_epoch),
  freeze_seq     INTEGER NOT NULL CHECK(freeze_seq >= 0),
  member_set_hash BLOB CHECK(member_set_hash IS NULL OR length(member_set_hash) = 32),
  asset_set_digest BLOB CHECK(asset_set_digest IS NULL OR length(asset_set_digest) = 32),
  revoked_count  INTEGER NOT NULL CHECK(revoked_count >= 0),
  state          TEXT NOT NULL CHECK(state IN('frozen','staging','activated','aborted')),
  primary_checkpoint BLOB CHECK(primary_checkpoint IS NULL OR length(primary_checkpoint) = 16),
  fallback_checkpoint BLOB CHECK(fallback_checkpoint IS NULL OR length(fallback_checkpoint) = 16),
  activation_commit  BLOB CHECK(activation_commit IS NULL OR length(activation_commit) = 32),
  activated_at       INTEGER,
  activated_control_rev INTEGER CHECK(activated_control_rev IS NULL OR activated_control_rev >= 0),
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,
  PRIMARY KEY(vault_id, rotation_id)
) STRICT;",
        &[],
    ),
    (
        "bootstrap_leases",
        "CREATE TABLE {table}(
  vault_id           BLOB NOT NULL CHECK(length(vault_id) = 16),
  lease_id           BLOB NOT NULL CHECK(length(lease_id) = 16),
  device_id          BLOB NOT NULL CHECK(length(device_id) = 16),
  primary_checkpoint BLOB NOT NULL CHECK(length(primary_checkpoint) = 16),
  fallback_checkpoint BLOB NOT NULL CHECK(length(fallback_checkpoint) = 16),
  floor_seq          INTEGER NOT NULL CHECK(floor_seq >= 0),
  created_at         INTEGER NOT NULL,
  expires_at         INTEGER NOT NULL,
  PRIMARY KEY(vault_id, lease_id),
  FOREIGN KEY(vault_id, device_id) REFERENCES devices(vault_id, device_id)
) STRICT;",
        &[],
    ),
    (
        "device_acks",
        "CREATE TABLE {table}(
  vault_id       BLOB NOT NULL CHECK(length(vault_id) = 16),
  device_id      BLOB NOT NULL CHECK(length(device_id) = 16),
  received_seq   INTEGER NOT NULL CHECK(received_seq >= 0),
  integrated_seq INTEGER NOT NULL CHECK(integrated_seq >= 0),
  epoch          INTEGER NOT NULL CHECK(epoch > 0),
  updated_at     INTEGER NOT NULL,
  PRIMARY KEY(vault_id, device_id),
  FOREIGN KEY(vault_id, device_id) REFERENCES devices(vault_id, device_id),
  CHECK(received_seq >= integrated_seq)
) STRICT;",
        &[],
    ),
    (
        "dedup",
        "CREATE TABLE {table}(
  vault_id     BLOB NOT NULL CHECK(length(vault_id) = 16),
  scope        TEXT NOT NULL,
  id           BLOB NOT NULL,
  request_hash BLOB NOT NULL CHECK(length(request_hash) = 32),
  accepted_seq INTEGER,
  response     BLOB NOT NULL,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY(vault_id, scope, id)
) STRICT;",
        &[],
    ),
    (
        "gc_plans",
        "CREATE TABLE {table}(
  vault_id    BLOB NOT NULL CHECK(length(vault_id) = 16),
  epoch       INTEGER NOT NULL CHECK(epoch > 0),
  cut_seq     INTEGER NOT NULL CHECK(cut_seq >= 0),
  eligible_at INTEGER NOT NULL,
  proof       BLOB NOT NULL,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY(vault_id, epoch, cut_seq)
) STRICT;",
        &[],
    ),
];

/// 租户表索引（表名已是最终名；每个索引都带 `vault_id` 前缀）。
const INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS devices_status ON devices(vault_id, status);
CREATE INDEX IF NOT EXISTS challenges_expiry ON challenges(vault_id, expires_at);
CREATE INDEX IF NOT EXISTS tokens_expiry ON tokens(vault_id, expires_at);
CREATE INDEX IF NOT EXISTS reservations_state ON maintenance_reservations(vault_id, state, expires_at);
CREATE INDEX IF NOT EXISTS objects_state ON objects(vault_id, state, touched_at);
CREATE INDEX IF NOT EXISTS objects_epoch ON objects(vault_id, epoch, type);
CREATE INDEX IF NOT EXISTS log_epoch ON log(vault_id, epoch);
CREATE INDEX IF NOT EXISTS checkpoints_state ON checkpoints(vault_id, state, epoch);
CREATE INDEX IF NOT EXISTS checkpoints_published ON checkpoints(vault_id, published_at);
CREATE INDEX IF NOT EXISTS checkpoint_refs_object ON checkpoint_refs(vault_id, object_id);
CREATE INDEX IF NOT EXISTS asset_objects_epoch ON asset_objects(vault_id, epoch, state, finalized_seq);
CREATE UNIQUE INDEX IF NOT EXISTS active_slot_kind ON key_slots(vault_id, kind) WHERE state = 'active';
CREATE INDEX IF NOT EXISTS rotations_state ON rotations(vault_id, state);
CREATE INDEX IF NOT EXISTS leases_expiry ON bootstrap_leases(vault_id, expires_at);
"#;

/// 构建新建库的 DDL（表模板按 [`TENANT_TABLES`] 顺序替换成真实表名）。
pub(crate) fn render_ddl() -> String {
    let mut out = String::from(GLOBAL_DDL);
    out.push_str(CREATE_TICKETS_DDL);
    for (name, template, _) in TENANT_TABLES {
        out.push_str(&template.replace("{table}", name));
    }
    out.push_str(INDEXES);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_applies_and_is_idempotent() {
        let conn = Connection::open_in_memory().expect("open");
        configure(&conn).expect("pragmas");
        conn.execute_batch(&render_ddl()).expect("ddl");
        let version = meta_text(&conn, META_SCHEMA_VERSION)
            .expect("meta")
            .is_none();
        assert!(version, "fresh ddl 不写 schema_version，由 apply 写");
        let mut conn = Connection::open_in_memory().expect("open");
        configure(&conn).expect("pragmas");
        apply(&mut conn).expect("first apply");
        apply(&mut conn).expect("second apply is a no-op");
        let version = meta_text(&conn, META_SCHEMA_VERSION)
            .expect("meta")
            .expect("present");
        assert_eq!(version, SCHEMA_VERSION.to_string());
        let instance = instance_id(&conn).expect("instance");
        assert_ne!(instance, [0u8; 16]);

        // 所有表必须是 STRICT（ADR §6）。
        let mut stmt = conn
            .prepare("SELECT name, strict FROM pragma_table_list WHERE schema = 'main' AND type = 'table' AND name NOT LIKE 'sqlite_%'")
            .expect("prepare");
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect");
        assert!(
            rows.len() >= 20,
            "expected full table set, got {}",
            rows.len()
        );
        for (name, strict) in rows {
            assert_eq!(strict, 1, "table {name} must be STRICT");
        }
        // 每张租户表都有 vault_id 列（ADR 0014 §A：不得遗漏）。
        for (name, _, _) in TENANT_TABLES {
            assert!(
                column_exists(&conn, name, "vault_id").expect("columns"),
                "tenant table {name} 缺少 vault_id"
            );
        }
        assert!(column_exists(&conn, "vaults", "legacy_default").expect("legacy_default"));
    }

    #[test]
    fn newer_schema_is_rejected_without_touching_data() {
        let mut conn = Connection::open_in_memory().expect("open");
        apply(&mut conn).expect("apply");
        conn.execute(
            "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
            (
                (SCHEMA_VERSION + 1).to_string().as_bytes(),
                META_SCHEMA_VERSION,
            ),
        )
        .expect("bump");
        let error = apply(&mut conn).expect_err("must refuse newer schema");
        assert!(format!("{error}").contains("newer than this binary"));
        // 拒绝时不得改写版本（旧库保持原样，等运维恢复快照）。
        let version = meta_text(&conn, META_SCHEMA_VERSION)
            .expect("meta")
            .expect("present");
        assert_eq!(version, (SCHEMA_VERSION + 1).to_string());
    }

    /// v1..v9 的历史迁移步骤仍可单独执行（v9→v10 是整库重建，不再从当前 schema
    /// "降级模拟"整条链——那会要求 v9 的 `vault` 单行表存在，见 `tests/migration_v10.rs`）。
    #[test]
    fn v3_to_v4_adds_commit_signature_column() {
        let mut conn = Connection::open_in_memory().expect("open");
        configure(&conn).expect("pragmas");
        apply(&mut conn).expect("apply");
        conn.execute("ALTER TABLE checkpoints DROP COLUMN commit_signature", [])
            .expect("drop column");
        migrate_v3_to_v4(&mut conn).expect("v3 -> v4");
        assert!(column_exists(&conn, "checkpoints", "commit_signature").expect("column"));
        assert_eq!(
            meta_text(&conn, META_SCHEMA_VERSION)
                .expect("meta")
                .expect("present"),
            "4".to_string(),
            "每一步迁移必须只写自己的版本号（否则 v9->v10 会被误判为已完成）"
        );
    }

    #[test]
    fn v4_to_v5_adds_revoked_once_and_manager_transition_columns() {
        let mut conn = Connection::open_in_memory().expect("open");
        configure(&conn).expect("pragmas");
        apply(&mut conn).expect("apply");
        conn.execute("ALTER TABLE epochs DROP COLUMN revoked_once", [])
            .expect("drop column");
        migrate_v4_to_v5(&mut conn).expect("v4 -> v5");
        assert!(column_exists(&conn, "epochs", "revoked_once").expect("column"));
        assert_eq!(
            meta_text(&conn, META_SCHEMA_VERSION)
                .expect("meta")
                .expect("present"),
            "5".to_string()
        );
    }

    #[test]
    fn v7_to_v8_adds_activation_and_candidate_role_columns() {
        let mut conn = Connection::open_in_memory().expect("open");
        configure(&conn).expect("pragmas");
        apply(&mut conn).expect("apply");
        conn.execute("ALTER TABLE checkpoints DROP COLUMN role", [])
            .expect("drop role");
        for column in [
            "primary_checkpoint",
            "fallback_checkpoint",
            "activation_commit",
            "activated_at",
        ] {
            conn.execute(&format!("ALTER TABLE rotations DROP COLUMN {column}"), [])
                .unwrap_or_else(|err| panic!("drop rotations.{column}: {err}"));
        }
        for column in ["grant_id", "activation_ref"] {
            conn.execute(
                &format!("ALTER TABLE device_grants DROP COLUMN {column}"),
                [],
            )
            .unwrap_or_else(|err| panic!("drop device_grants.{column}: {err}"));
        }
        migrate_v7_to_v8(&mut conn).expect("v7 -> v8");
        assert!(column_exists(&conn, "checkpoints", "role").expect("role"));
        for column in [
            "primary_checkpoint",
            "fallback_checkpoint",
            "activation_commit",
            "activated_at",
        ] {
            assert!(
                column_exists(&conn, "rotations", column).expect("column"),
                "v8 必须补 rotations.{column}"
            );
        }
        for column in ["grant_id", "activation_ref"] {
            assert!(
                column_exists(&conn, "device_grants", column).expect("column"),
                "v8 必须补 device_grants.{column}"
            );
        }
        assert_eq!(
            meta_text(&conn, META_SCHEMA_VERSION)
                .expect("meta")
                .expect("present"),
            "8".to_string()
        );
    }

    #[test]
    fn v8_to_v9_adds_activated_control_rev() {
        let mut conn = Connection::open_in_memory().expect("open");
        configure(&conn).expect("pragmas");
        apply(&mut conn).expect("apply");
        conn.execute(
            "ALTER TABLE rotations DROP COLUMN activated_control_rev",
            [],
        )
        .expect("drop activated_control_rev");
        migrate_v8_to_v9(&mut conn).expect("v8 -> v9");
        assert!(column_exists(&conn, "rotations", "activated_control_rev").expect("column"));
        assert_eq!(
            meta_text(&conn, META_SCHEMA_VERSION)
                .expect("meta")
                .expect("present"),
            "9".to_string()
        );
    }

    #[test]
    fn v4_to_v5_backfills_revoked_once_from_rotations() {
        let mut conn = Connection::open_in_memory().expect("open");
        configure(&conn).expect("pragmas");
        apply(&mut conn).expect("apply");
        let vault_id: Id16 = [0x31u8; 16];
        conn.execute(
            "INSERT INTO vaults(vault_id, instance_id, log_id, log_incarnation, cursor_key,
                                state, active_epoch, next_epoch, manager_rev, control_rev,
                                auth_rev, head_seq, gc_floor, normal_limit, maintenance_limit,
                                legacy_default, created_at)
             VALUES (?1, ?2, ?2, ?2, ?3, 'active', 1, 2, 1, 1, 1, 0, 0, 1048576, 2097152, 1, 0)",
            (
                vault_id.as_slice(),
                [0u8; 16].as_slice(),
                [0u8; 32].as_slice(),
            ),
        )
        .expect("vault");
        conn.execute(
            "INSERT INTO epochs(vault_id, epoch, state, created_at)
             VALUES (?1, 1, 'active', 1)",
            [vault_id.as_slice()],
        )
        .expect("epoch 1");
        conn.execute(
            "INSERT INTO epochs(vault_id, epoch, state, created_at)
             VALUES (?1, 2, 'staged', 1)",
            [vault_id.as_slice()],
        )
        .expect("epoch 2");
        conn.execute(
            "INSERT INTO rotations(vault_id, rotation_id, from_epoch, to_epoch, freeze_seq,
                                   revoked_count, state, created_at, updated_at)
             VALUES (?1, X'01010101010101010101010101010101', 1, 2, 0, 1, 'aborted', 1, 1)",
            [vault_id.as_slice()],
        )
        .expect("rotation");
        conn.execute("ALTER TABLE epochs DROP COLUMN revoked_once", [])
            .expect("drop");
        migrate_v4_to_v5(&mut conn).expect("migrate");
        let revoked: i64 = conn
            .query_row(
                "SELECT revoked_once FROM epochs WHERE epoch = 1 AND vault_id = ?1",
                [vault_id.as_slice()],
                |row| row.get(0),
            )
            .expect("backfill");
        assert_eq!(revoked, 1, "旧库吊销过的 from_epoch 必须回填为 1");
    }
}
