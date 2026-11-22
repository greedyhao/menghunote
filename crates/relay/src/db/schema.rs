//! relay SQLite schema（ADR 0006 §6 + §13.2）。
//!
//! 硬约束：
//! - **全部表 STRICT**（类型错误直接报错，不做 SQLite 隐式转换）；
//! - `journal_mode=WAL`、`synchronous=FULL`、`foreign_keys=ON`、`busy_timeout=5000`；
//! - **单实例只允许一个 vault**（`vault.singleton=1` + PK）；
//! - 写入顺序 = 一个 writer 事务（对象 body/parts/log seq/预算 ledger/幂等响应同事务，§6 末段）。

use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::ids::{random_id16, Id16};

/// 当前 schema 版本。任何破坏性变更都必须 +1 并写显式迁移（v1 只有 1）。
pub const SCHEMA_VERSION: i64 = 9;

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

/// 建表 + 写入实例元数据（幂等：已存在同版本则跳过）。
pub fn apply(conn: &mut Connection) -> Result<()> {
    configure(conn)?;
    if !table_exists(conn, "relay_meta")? {
        conn.execute_batch(DDL)?;
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
    let existing = meta_text(conn, META_SCHEMA_VERSION)?.and_then(|text| text.parse::<i64>().ok());
    match existing {
        Some(version) if version == SCHEMA_VERSION => Ok(()),
        // v1 没有 commit_signature / log.signature：先补到 v2，再逐级迁到最新。
        Some(version) if version == 1 => {
            let _ = version;
            migrate_v1_to_v2(conn)?;
            migrate_v2_to_v3(conn)?;
            migrate_v3_to_v4(conn)?;
            migrate_v4_to_v5(conn)?;
            migrate_v5_to_v6(conn)?;
            migrate_v6_to_v7(conn)?;
            migrate_v7_to_v8(conn)?;
            migrate_v8_to_v9(conn)
        }
        // v2 没有设备证书签名（§17.1 客户端验签需要）：补到 v3 再逐级到最新。
        Some(version) if version == 2 => {
            let _ = version;
            migrate_v2_to_v3(conn)?;
            migrate_v3_to_v4(conn)?;
            migrate_v4_to_v5(conn)?;
            migrate_v5_to_v6(conn)?;
            migrate_v6_to_v7(conn)?;
            migrate_v7_to_v8(conn)?;
            migrate_v8_to_v9(conn)
        }
        // v3 没有 checkpoint 生产者签名（§20.1 bootstrap 客户端验签需要）。
        Some(version) if version == 3 => {
            let _ = version;
            migrate_v3_to_v4(conn)?;
            migrate_v4_to_v5(conn)?;
            migrate_v5_to_v6(conn)?;
            migrate_v6_to_v7(conn)?;
            migrate_v7_to_v8(conn)?;
            migrate_v8_to_v9(conn)
        }
        // v4 没有 epoch 级"冻结期间吊销过"的持久事实（§23.1 abort 不得解冻）。
        Some(version) if version == 4 => {
            let _ = version;
            migrate_v4_to_v5(conn)?;
            migrate_v5_to_v6(conn)?;
            migrate_v6_to_v7(conn)?;
            migrate_v7_to_v8(conn)?;
            migrate_v8_to_v9(conn)
        }
        // v5 的 checkpoint 只有 staging/published/pruned，且 grant 未持久化外层签名（§24）。
        Some(version) if version == 5 => {
            let _ = version;
            migrate_v5_to_v6(conn)?;
            migrate_v6_to_v7(conn)?;
            migrate_v7_to_v8(conn)?;
            migrate_v8_to_v9(conn)
        }
        // v6 没有不透明资产登记表（§25.1：枚举不能依赖可回收 log）。
        Some(version) if version == 6 => {
            let _ = version;
            migrate_v6_to_v7(conn)?;
            migrate_v7_to_v8(conn)?;
            migrate_v8_to_v9(conn)
        }
        // v7 没有激活事实/候选角色列（§28.1 条目 2/3：activate 指名两 cid、wire 需回读）。
        Some(version) if version == 7 => {
            let _ = version;
            migrate_v7_to_v8(conn)?;
            migrate_v8_to_v9(conn)
        }
        // v8 没有激活时的 control_rev（wire 的 activated commit hash/seq，评审 §三 2）。
        Some(version) if version == 8 => {
            let _ = version;
            migrate_v8_to_v9(conn)
        }
        Some(_) => Err(Error::Invariant("unsupported relay schema version")),
        None => {
            // 表已建但元数据缺失（半初始化的库）：补齐，不覆盖已有数据。
            set_meta(conn, META_INSTANCE_ID, &random_id16()?)?;
            set_meta(
                conn,
                META_SCHEMA_VERSION,
                SCHEMA_VERSION.to_string().as_bytes(),
            )?;
            Ok(())
        }
    }
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
            (SCHEMA_VERSION.to_string().as_bytes(), META_SCHEMA_VERSION),
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
        (SCHEMA_VERSION.to_string().as_bytes(), META_SCHEMA_VERSION),
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
        (SCHEMA_VERSION.to_string().as_bytes(), META_SCHEMA_VERSION),
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
        (SCHEMA_VERSION.to_string().as_bytes(), META_SCHEMA_VERSION),
    )?;
    tx.commit()?;
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(names.iter().any(|name| name == column))
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

/// 全部 DDL。表顺序满足外键创建顺序。
pub const DDL: &str = r#"
-- ===== 实例元数据 =====
CREATE TABLE IF NOT EXISTS relay_meta(
  key   TEXT PRIMARY KEY,
  value BLOB NOT NULL
) STRICT;

-- ===== 单 vault（ADE 0006 §6：单实例只允许一个 vault）=====
CREATE TABLE IF NOT EXISTS vault(
  singleton         INTEGER PRIMARY KEY CHECK(singleton = 1),
  vault_id          BLOB NOT NULL CHECK(length(vault_id) = 16),
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
  created_at        INTEGER NOT NULL
) STRICT;

-- ===== 管理与控制链（不随密文 GC 消失，ADR §6 末段）=====
CREATE TABLE IF NOT EXISTS manager_keys(
  rev           INTEGER PRIMARY KEY CHECK(rev >= 1),
  pubkey        BLOB NOT NULL CHECK(length(pubkey) = 32),
  transition    BLOB NOT NULL,
  old_rev       INTEGER NOT NULL DEFAULT 0 CHECK(old_rev >= 0),
  old_signature BLOB,
  new_signature BLOB,
  created_at    INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS control_commits(
  rev           INTEGER PRIMARY KEY CHECK(rev >= 1),
  prev_hash     BLOB NOT NULL CHECK(length(prev_hash) = 32),
  record_hash   BLOB NOT NULL CHECK(length(record_hash) = 32),
  signed_record BLOB NOT NULL,
  created_at    INTEGER NOT NULL
) STRICT;

-- ===== epoch =====
CREATE TABLE IF NOT EXISTS epochs(
  epoch        INTEGER PRIMARY KEY CHECK(epoch > 0),
  state        TEXT NOT NULL CHECK(state IN('staged','active','retired','aborted')),
  revoked_once INTEGER NOT NULL DEFAULT 0 CHECK(revoked_once IN (0, 1)),
  created_at   INTEGER NOT NULL,
  retire_at    INTEGER
) STRICT;

-- ===== 设备 =====
CREATE TABLE IF NOT EXISTS devices(
  device_id   BLOB PRIMARY KEY CHECK(length(device_id) = 16),
  sign_pk     BLOB NOT NULL CHECK(length(sign_pk) = 32),
  hpke_pk     BLOB NOT NULL CHECK(length(hpke_pk) = 32),
  certificate BLOB NOT NULL,
  certificate_signature BLOB,
  status      TEXT NOT NULL CHECK(status IN('pending','active','revoked')),
  created_at  INTEGER NOT NULL,
  revoked_at  INTEGER,
  last_seen   INTEGER
) STRICT;
CREATE INDEX IF NOT EXISTS devices_status ON devices(status);

-- ===== 鉴权 =====
CREATE TABLE IF NOT EXISTS challenges(
  challenge_id BLOB PRIMARY KEY CHECK(length(challenge_id) = 16),
  realm        TEXT NOT NULL CHECK(realm IN('device','management')),
  subject      BLOB NOT NULL CHECK(length(subject) = 16),
  nonce        BLOB NOT NULL CHECK(length(nonce) = 32),
  expires_at   INTEGER NOT NULL,
  used_at      INTEGER
) STRICT;
CREATE INDEX IF NOT EXISTS challenges_expiry ON challenges(expires_at);

CREATE TABLE IF NOT EXISTS tokens(
  token_hash BLOB PRIMARY KEY CHECK(length(token_hash) = 32),
  realm      TEXT NOT NULL CHECK(realm IN('device','management')),
  subject    BLOB NOT NULL CHECK(length(subject) = 16),
  auth_rev   INTEGER NOT NULL CHECK(auth_rev >= 1),
  expires_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS tokens_expiry ON tokens(expires_at);

CREATE TABLE IF NOT EXISTS invites(
  invite_id       BLOB PRIMARY KEY CHECK(length(invite_id) = 16),
  token_hash      BLOB NOT NULL UNIQUE CHECK(length(token_hash) = 32),
  expires_at      INTEGER NOT NULL,
  pending_device  BLOB CHECK(pending_device IS NULL OR length(pending_device) = 16),
  used_at         INTEGER
) STRICT;

-- ===== 维护预算（ADR §13.2）=====
CREATE TABLE IF NOT EXISTS maintenance_reservations(
  reservation_id BLOB PRIMARY KEY CHECK(length(reservation_id) = 16),
  purpose        TEXT NOT NULL CHECK(purpose IN('checkpoint','rotation')),
  resource_id    BLOB NOT NULL UNIQUE,
  owner_device   BLOB NOT NULL REFERENCES devices(device_id),
  reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes >= 0),
  consumed_bytes INTEGER NOT NULL CHECK(consumed_bytes >= 0 AND consumed_bytes <= reserved_bytes),
  state          TEXT NOT NULL CHECK(state IN('active','complete','expired')),
  expires_at     INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS reservations_state ON maintenance_reservations(state, expires_at);

CREATE TABLE IF NOT EXISTS usage(
  singleton         INTEGER PRIMARY KEY CHECK(singleton = 1),
  normal_bytes      INTEGER NOT NULL CHECK(normal_bytes >= 0),
  maintenance_bytes INTEGER NOT NULL CHECK(maintenance_bytes >= 0)
) STRICT;

-- ===== 对象与分片 =====
CREATE TABLE IF NOT EXISTS objects(
  object_id      BLOB PRIMARY KEY CHECK(length(object_id) = 32),
  message_id     BLOB NOT NULL CHECK(length(message_id) = 16),
  producer       BLOB NOT NULL CHECK(length(producer) = 16),
  epoch          INTEGER NOT NULL CHECK(epoch > 0),
  type           INTEGER NOT NULL CHECK(type BETWEEN 1 AND 5),
  generation     INTEGER NOT NULL CHECK(generation >= 0),
  sender_seq     INTEGER NOT NULL CHECK(sender_seq >= 0),
  part_count     INTEGER NOT NULL CHECK(part_count >= 1),
  plain_len      INTEGER NOT NULL CHECK(plain_len >= 1),
  state          TEXT NOT NULL CHECK(state IN('staging','complete')),
  budget_class   TEXT NOT NULL CHECK(budget_class IN('normal','maintenance','retired')),
  digest         BLOB CHECK(digest IS NULL OR length(digest) = 32),
  signed_commit  BLOB,
  commit_signature BLOB,
  reservation_id BLOB REFERENCES maintenance_reservations(reservation_id),
  created_at     INTEGER NOT NULL,
  touched_at     INTEGER NOT NULL,
  completed_at   INTEGER,
  UNIQUE(epoch, message_id)
) STRICT;
CREATE INDEX IF NOT EXISTS objects_state ON objects(state, touched_at);
CREATE INDEX IF NOT EXISTS objects_epoch ON objects(epoch, type);

CREATE TABLE IF NOT EXISTS object_parts(
  object_id BLOB NOT NULL REFERENCES objects(object_id),
  part_no   INTEGER NOT NULL CHECK(part_no >= 0),
  envelope  BLOB NOT NULL CHECK(length(envelope) BETWEEN 212 AND 4194304),
  digest    BLOB NOT NULL CHECK(length(digest) = 32),
  PRIMARY KEY(object_id, part_no)
) STRICT;

-- ===== 日志（`relay_seq` 只表示送达顺序）=====
CREATE TABLE IF NOT EXISTS log(
  relay_seq  INTEGER PRIMARY KEY CHECK(relay_seq > 0),
  kind       TEXT NOT NULL CHECK(kind IN('update','asset','checkpoint','control')),
  object_id  BLOB CHECK(object_id IS NULL OR length(object_id) = 32),
  epoch      INTEGER NOT NULL CHECK(epoch > 0),
  event      BLOB NOT NULL,
  signature  BLOB,
  created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS log_epoch ON log(epoch);

-- ===== checkpoint =====
CREATE TABLE IF NOT EXISTS checkpoints(
  checkpoint_id      BLOB PRIMARY KEY CHECK(length(checkpoint_id) = 16),
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
  published_at       INTEGER
) STRICT;
CREATE INDEX IF NOT EXISTS checkpoints_state ON checkpoints(state, epoch);
CREATE INDEX IF NOT EXISTS checkpoints_published ON checkpoints(published_at);

CREATE TABLE IF NOT EXISTS checkpoint_refs(
  checkpoint_id BLOB NOT NULL REFERENCES checkpoints(checkpoint_id),
  object_id     BLOB NOT NULL CHECK(length(object_id) = 32),
  role          TEXT NOT NULL CHECK(role IN('data','manifest')),
  PRIMARY KEY(checkpoint_id, object_id)
) STRICT;
CREATE INDEX IF NOT EXISTS checkpoint_refs_object ON checkpoint_refs(object_id);

-- §25.1：不透明资产登记（只有 HMAC object_id/ObjectRefV1 元数据；真实 sha256/MIME/byte_len 不进 relay）。
CREATE TABLE IF NOT EXISTS asset_objects(
  epoch         INTEGER NOT NULL CHECK(epoch > 0),
  object_id     BLOB NOT NULL CHECK(length(object_id) = 32),
  state         TEXT NOT NULL CHECK(state IN('active','retired')),
  finalized_seq INTEGER NOT NULL CHECK(finalized_seq >= 0),
  rotation_id   BLOB CHECK(rotation_id IS NULL OR length(rotation_id) = 16),
  PRIMARY KEY(epoch, object_id)
) STRICT;
CREATE INDEX IF NOT EXISTS asset_objects_epoch ON asset_objects(epoch, state, finalized_seq);

-- ===== key slot =====
CREATE TABLE IF NOT EXISTS key_slots(
  slot_id      BLOB PRIMARY KEY CHECK(length(slot_id) = 32) REFERENCES objects(object_id),
  epoch        INTEGER NOT NULL CHECK(epoch > 0),
  kind         TEXT NOT NULL CHECK(kind IN('password','recovery')),
  descriptor   BLOB NOT NULL,
  state        TEXT NOT NULL CHECK(state IN('staging','active','retiring')),
  slot_set_rev INTEGER NOT NULL CHECK(slot_set_rev >= 0),
  retire_at    INTEGER,
  created_at   INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX IF NOT EXISTS active_slot_kind ON key_slots(kind) WHERE state = 'active';

-- ===== HPKE grants（按 epoch）=====
CREATE TABLE IF NOT EXISTS device_grants(
  epoch           INTEGER NOT NULL CHECK(epoch > 0),
  device_id       BLOB NOT NULL REFERENCES devices(device_id),
  manager_rev     INTEGER NOT NULL CHECK(manager_rev >= 1),
  hpke_enc        BLOB NOT NULL CHECK(length(hpke_enc) = 32),
  hpke_ciphertext BLOB NOT NULL,
  signed_header   BLOB NOT NULL,
  signature       BLOB,
  grant_id        BLOB CHECK(grant_id IS NULL OR length(grant_id) = 16),
  activation_ref  BLOB CHECK(activation_ref IS NULL OR length(activation_ref) = 16),
  PRIMARY KEY(epoch, device_id)
) STRICT;

-- ===== 轮换 =====
CREATE TABLE IF NOT EXISTS rotations(
  rotation_id    BLOB PRIMARY KEY CHECK(length(rotation_id) = 16),
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
  updated_at     INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS rotations_state ON rotations(state);

-- ===== bootstrap lease =====
CREATE TABLE IF NOT EXISTS bootstrap_leases(
  lease_id           BLOB PRIMARY KEY CHECK(length(lease_id) = 16),
  device_id          BLOB NOT NULL REFERENCES devices(device_id),
  primary_checkpoint BLOB NOT NULL CHECK(length(primary_checkpoint) = 16),
  fallback_checkpoint BLOB NOT NULL CHECK(length(fallback_checkpoint) = 16),
  floor_seq          INTEGER NOT NULL CHECK(floor_seq >= 0),
  created_at         INTEGER NOT NULL,
  expires_at         INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS leases_expiry ON bootstrap_leases(expires_at);

-- ===== 设备 ACK 水位 =====
CREATE TABLE IF NOT EXISTS device_acks(
  device_id     BLOB PRIMARY KEY REFERENCES devices(device_id),
  received_seq  INTEGER NOT NULL CHECK(received_seq >= 0),
  integrated_seq INTEGER NOT NULL CHECK(integrated_seq >= 0),
  epoch         INTEGER NOT NULL CHECK(epoch > 0),
  updated_at    INTEGER NOT NULL,
  CHECK(received_seq >= integrated_seq)
) STRICT;

-- ===== 幂等 / tombstone（不随密文 GC 消失，ADR §6 末段）=====
CREATE TABLE IF NOT EXISTS dedup(
  scope        TEXT NOT NULL,
  id           BLOB NOT NULL,
  request_hash BLOB NOT NULL CHECK(length(request_hash) = 32),
  accepted_seq INTEGER,
  response     BLOB NOT NULL,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY(scope, id)
) STRICT;

-- ===== GC 持久计划（ADR §7：后来的 checkpoint 不能重置已有 deadline）=====
CREATE TABLE IF NOT EXISTS gc_plans(
  epoch       INTEGER NOT NULL CHECK(epoch > 0),
  cut_seq     INTEGER NOT NULL CHECK(cut_seq >= 0),
  eligible_at INTEGER NOT NULL,
  proof       BLOB NOT NULL,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY(epoch, cut_seq)
) STRICT;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_applies_and_is_idempotent() {
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
    }

    #[test]
    fn v3_checkpoints_gain_commit_signature_column() {
        let mut conn = Connection::open_in_memory().expect("open");
        configure(&conn).expect("pragmas");
        apply(&mut conn).expect("apply");
        // 模拟 v3：删掉 §20.1 新列并把版本降回 3。
        conn.execute("ALTER TABLE checkpoints DROP COLUMN commit_signature", [])
            .expect("drop column");
        conn.execute(
            "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
            ("3".as_bytes(), META_SCHEMA_VERSION),
        )
        .expect("set v3");
        apply(&mut conn).expect("migrate v3 -> latest");
        let version = meta_text(&conn, META_SCHEMA_VERSION)
            .expect("meta")
            .expect("present");
        assert_eq!(version, SCHEMA_VERSION.to_string());
        assert!(
            column_exists(&conn, "checkpoints", "commit_signature").expect("column"),
            "v3 -> v4 必须补 commit_signature"
        );
        assert!(
            column_exists(&conn, "epochs", "revoked_once").expect("column"),
            "v4 -> v5 必须补 revoked_once"
        );
    }

    #[test]
    fn v8_gains_activated_control_rev_column() {
        let mut conn = Connection::open_in_memory().expect("open");
        configure(&conn).expect("pragmas");
        apply(&mut conn).expect("apply");
        conn.execute(
            "ALTER TABLE rotations DROP COLUMN activated_control_rev",
            [],
        )
        .expect("drop activated_control_rev");
        conn.execute(
            "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
            ("8".as_bytes(), META_SCHEMA_VERSION),
        )
        .expect("set v8");
        apply(&mut conn).expect("migrate v8 -> v9");
        assert!(
            column_exists(&conn, "rotations", "activated_control_rev").expect("column"),
            "v9 必须补 rotations.activated_control_rev"
        );
        assert_eq!(
            meta_text(&conn, META_SCHEMA_VERSION)
                .expect("meta")
                .expect("present"),
            SCHEMA_VERSION.to_string()
        );
    }

    #[test]
    fn v4_epochs_gain_revoked_once_column() {
        let mut conn = Connection::open_in_memory().expect("open");
        configure(&conn).expect("pragmas");
        apply(&mut conn).expect("apply");
        conn.execute("ALTER TABLE epochs DROP COLUMN revoked_once", [])
            .expect("drop column");
        conn.execute(
            "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
            ("4".as_bytes(), META_SCHEMA_VERSION),
        )
        .expect("set v4");
        apply(&mut conn).expect("migrate v4 -> v5");
        assert!(column_exists(&conn, "epochs", "revoked_once").expect("column"));
        let version = meta_text(&conn, META_SCHEMA_VERSION)
            .expect("meta")
            .expect("present");
        assert_eq!(version, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn v7_gains_activation_and_candidate_role_columns() {
        let mut conn = Connection::open_in_memory().expect("open");
        configure(&conn).expect("pragmas");
        apply(&mut conn).expect("apply");
        // 模拟 v7：删掉 v8 新增列并把版本降回 7。
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
        conn.execute(
            "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
            ("7".as_bytes(), META_SCHEMA_VERSION),
        )
        .expect("set v7");
        apply(&mut conn).expect("migrate v7 -> latest");
        assert_eq!(
            meta_text(&conn, META_SCHEMA_VERSION)
                .expect("meta")
                .expect("present"),
            SCHEMA_VERSION.to_string()
        );
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
    }

    #[test]
    fn v4_to_v5_backfills_revoked_once_from_rotations() {
        let mut conn = Connection::open_in_memory().expect("open");
        configure(&conn).expect("pragmas");
        apply(&mut conn).expect("apply");
        conn.execute(
            "INSERT INTO epochs(epoch, state, created_at) VALUES (1, 'active', 1)",
            [],
        )
        .expect("epoch 1");
        conn.execute(
            "INSERT INTO epochs(epoch, state, created_at) VALUES (2, 'staged', 1)",
            [],
        )
        .expect("epoch 2");
        conn.execute(
            "INSERT INTO rotations(rotation_id, from_epoch, to_epoch, freeze_seq, revoked_count,
                                   state, created_at, updated_at)
             VALUES (X'01010101010101010101010101010101', 1, 2, 0, 1, 'aborted', 1, 1)",
            [],
        )
        .expect("rotation");
        conn.execute("ALTER TABLE epochs DROP COLUMN revoked_once", [])
            .expect("drop");
        conn.execute(
            "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
            ("4".as_bytes(), META_SCHEMA_VERSION),
        )
        .expect("v4");
        apply(&mut conn).expect("migrate");
        let revoked: i64 = conn
            .query_row(
                "SELECT revoked_once FROM epochs WHERE epoch = 1",
                [],
                |row| row.get(0),
            )
            .expect("backfill");
        assert_eq!(revoked, 1, "旧库吊销过的 from_epoch 必须回填为 1");
    }
}
