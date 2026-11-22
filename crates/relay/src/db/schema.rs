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
//!
//! 迁移 12 → 13 是 ADR 0018 §2 的**权威生命周期**：单事务、`VACUUM INTO` 快照 + 中断回滚：
//! - 新建全局表 `vault_registry`（vault 生命周期权威；`RESERVED/ACTIVE/EXPIRED/CANCELLED/
//!   DELETING/DELETED/DELETE_FAILED`；`vault_id` 全局唯一含墓碑，终态 ID 禁止复用）；
//! - 存量 `vaults` 每行 → registry `ACTIVE`（`created_at` 取该行既有 `created_at`，不虚构）；
//! - 存量票据：未消费且未过期且无 `vaults` 行 → `RESERVED`；未消费已过期 → `EXPIRED`；
//!   已消费但无 `vaults` 行 → `DELETED` 墓碑（删除时刻未知，留 NULL，不虚构）；
//! - **重建** `create_tickets`：加 `created_at INTEGER NOT NULL`（旧行取**迁移时刻**，
//!   文档明确标注它不是原始创建时间）与 `FOREIGN KEY(vault_id) REFERENCES vault_registry`；
//!   其余列（hash/TTL/quota/consumed_at）逐字节保留（含旧库多额度历史值，不重写事实）。
//!
//! 迁移 11 → 12 是 ADR 0015 S4a2 的**授权投影**：单事务、`VACUUM INTO` 快照 + 中断回滚：
//! - `vaults` 加 V2 授权列（`authz_version/authz_rev/authority_rev/recovery_pub/policy_bytes/
//!   upgrade_control_rev/authz_state`），旧 vault 保持 `legacy/1/0`；
//! - 新建 `management_caps`（cap 公开授权事实）、`device_credentials`（V2 凭证历史，不 GC）、
//!   `dedup_v2`（V2 提交幂等，PK `(vault_id, request_id)`）；
//! - **重建** `devices`/`device_grants`/`invites`：加 `approval_control_rev`（复合 FK 到
//!   `control_commits(vault_id, rev)`）/`issued_authz_rev`/`issuer_cap_id`（SQLite 不能
//!   `ALTER TABLE` 加表级复合 FK 与跨列 CHECK）；
//! - **重建** `challenges`/`tokens`：`realm` 由 TEXT 改 INTEGER（1 设备 / 2 旧管理 / 3 管理设备 /
//!   4 恢复），加 `authz_version/authz_rev/authority_rev/cap_id`，管理 token 复合 FK 连 cap；
//!   旧 challenge/token 与 9→10 一样**全部失效**（不复制）。
//!
//! 本模块只提供**存储形状**：控制链验证、cap 门限、`/authz/*` 与 V2 提交路径属于 S4b，
//! schema 12 不启用 V2，v1 行为与 wire 完全不变。
//!
//! **版本推断**（ADR 0018 §2 修复）：元数据缺失时不能用 `authz_state` 存在性猜版本——
//! 那会把 schema 12 误标成最新版并跳过迁移。这里按**表形状标记**显式识别 11/12/13：
//! 12 标记（`vaults.authz_state` + `management_caps`/`device_credentials`/`dedup_v2` +
//! `challenges.realm` 已是 INTEGER）齐全才是 12；13 标记（`vault_registry` 表 +
//! `create_tickets.created_at` 列）齐全才是 13；标记部分存在 = 形状矛盾，fail closed，
//! 绝不猜版本。

use std::path::{Path, PathBuf};

use rusqlite::{Connection, TransactionBehavior};

use crate::error::{Error, Result};
use crate::ids::{random_id16, Id16};
use crate::time::{Clock, SystemClock};

/// 当前 schema 版本。任何破坏性变更都必须 +1 并写显式迁移。
///
/// 9 → 10：多租户（`vaults` + 全部租户表 `vault_id` 前缀，ADR 0014 §A）。
/// 10 → 11：新增全局表 `create_tickets`（`/v2/vaults` 的一次性创建票据）。
/// 11 → 12：ADR 0015 S4a2 的授权/control 投影（cap、challenge/token 扩展、凭证历史、dedup_v2）。
/// 12 → 13：ADR 0018 §2 的权威生命周期（`vault_registry`、票据 `created_at`/registry 关联、
///          版本推断修复；claim/cancel 原子语义）。
pub const SCHEMA_VERSION: i64 = 13;

/// relay_meta 中保存本实例 id 的键（cursor 的 `instance16`）。
pub const META_INSTANCE_ID: &str = "instance_id";
/// relay_meta 中保存 schema 版本的键。
pub const META_SCHEMA_VERSION: &str = "schema_version";
/// relay_meta 中标记一次性 setup secret 已被消费。
pub const META_SETUP_CONSUMED: &str = "setup.consumed";
/// `SHA256(setup secret)`；`init` 写、`serve` 读，重启后仍可完成一次 genesis。
pub const META_SETUP_SECRET_HASH: &str = "setup.secret_hash";
/// `SHA256(admin token 文本)`；ADR 0018 S1：内嵌 Web 管理面的独立凭证。
///
/// 与 setup secret **完全分离**（不同随机值、不同键、不同用途）：setup secret 只用于 v1
/// genesis / 旧 loopback 管理端点，admin token 只用于 `/admin` Web 登录。只存哈希；
/// 明文只在 `init` 的 stdout 出现一次，**不**写日志/数据库。
pub const META_ADMIN_TOKEN_HASH: &str = "admin.token_hash";

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
///
/// 迁移里需要的"迁移时刻"（12 → 13 给存量票据补 `created_at`）由调用方时钟提供：
/// [`Db::open`](crate::db::Db::open) 传 relay 时钟，测试用 `FakeClock` 可精确断言；
/// 直接调用本函数（单元测试/运维工具）时退化为系统时钟。**不得**用它虚构历史上的
/// 创建时间——迁移文档明确写清"这是迁移时刻"。
pub fn apply(conn: &mut Connection) -> Result<()> {
    apply_at(conn, None)
}

/// 与 [`apply`] 相同，但显式指定迁移时刻（`None` = 系统时钟）。
pub fn apply_at(conn: &mut Connection, now: Option<i64>) -> Result<()> {
    configure(conn)?;
    let migration_now = now.unwrap_or_else(|| SystemClock.now_secs());
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
                // 表已建但元数据缺失（半初始化的库）：按**表形状**显式推断版本，不覆盖已有数据。
                match infer_schema_version(conn)? {
                    InferredVersion::LegacySingleVault => {
                        // 旧 v9 形状 = 有 `vault(singleton)` 且没有 `vaults`；必须走 9→10，
                        // 绝不能直接标成最新版（那会把单 vault 数据留成半迁移状态）。
                        set_meta(conn, META_SCHEMA_VERSION, b"9")?;
                        9
                    }
                    InferredVersion::Empty => {
                        // 完全空的库：建表后按最新版初始化。
                        conn.execute_batch(&render_ddl())?;
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
                    InferredVersion::Known(version) => {
                        // 多租户形状：先把 10→11 的全局表及本版本应有的
                        // `create_tickets` 形状补齐（`IF NOT EXISTS`，只建空表），
                        // 再写回推断出的版本，让下面的迁移循环继续跑。
                        repair_global_tables(conn, version)?;
                        // instance_id 只补缺失，不覆盖已有值（它是游标 `instance16`，被改会让
                        // 所有旧游标失效）。
                        conn.execute(
                            "INSERT OR IGNORE INTO relay_meta(key, value) VALUES (?1, ?2)",
                            (META_INSTANCE_ID, random_id16()?.as_slice()),
                        )?;
                        set_meta(conn, META_SCHEMA_VERSION, version.to_string().as_bytes())?;
                        version
                    }
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
            11 => {
                migrate_v11_to_v12(conn)?;
                12
            }
            12 => {
                migrate_v12_to_v13(conn, migration_now)?;
                13
            }
            _ => return Err(Error::Invariant("unsupported relay schema version")),
        };
    }
    Ok(())
}

/// 元数据缺失时的形状推断结果（显式枚举，禁止"按某个列存不存在猜版本"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InferredVersion {
    /// schema 9：`vault(singleton)`，没有 `vaults`。
    LegacySingleVault,
    /// 完全没有多租户表：按最新版初始化。
    Empty,
    /// 明确识别出的 10/11/12/13 形状。
    Known(i64),
}

/// 按**表形状标记**推断 schema 版本（ADR 0018 §2 版本推断修复）。
///
/// - 多租户但既无 V2 授权投影也无 13 标记 → 10/11（10→11 只新增票据表，按 11 处理安全）；
/// - schema 12 标记**必须齐全**：`vaults.authz_state`、`management_caps`、
///   `device_credentials`、`dedup_v2`、`challenges.realm` 为 INTEGER；
/// - schema 13 标记**必须齐全**：`vault_registry` 表、`create_tickets.created_at` 列；
/// - 标记部分存在 = 半迁移/矛盾形状 → `Invariant`（fail closed，不猜）。
///
/// 关键修复：绝不因为 `vaults.authz_state` 存在就断定是最新版本——那会把 schema 12
/// 误标成 13 并跳过 12→13 迁移。
fn infer_schema_version(conn: &Connection) -> Result<InferredVersion> {
    let has_legacy_vault = table_exists(conn, "vault")?;
    let has_vaults = table_exists(conn, "vaults")?;
    if has_legacy_vault && has_vaults {
        return Err(Error::Invariant(
            "relay schema is ambiguous: both vault(singleton) and vaults exist",
        ));
    }
    if has_legacy_vault {
        return Ok(InferredVersion::LegacySingleVault);
    }
    if !has_vaults {
        return Ok(InferredVersion::Empty);
    }
    let v12_markers = [
        column_exists(conn, "vaults", "authz_state")?,
        table_exists(conn, "management_caps")?,
        table_exists(conn, "device_credentials")?,
        table_exists(conn, "dedup_v2")?,
        column_is_integer(conn, "challenges", "realm")?,
    ];
    let has_registry = table_exists(conn, "vault_registry")?;
    let tickets_exist = table_exists(conn, "create_tickets")?;
    let tickets_created_at = column_exists(conn, "create_tickets", "created_at")?;
    if has_registry {
        // registry 是 schema 13 独有的表。票据表整体缺失（空表被误删）→ 按 13 重建空表；
        // 票据表在但没有 `created_at` → 12/13 混合形状，fail closed（不猜）。
        if !tickets_exist {
            return Ok(InferredVersion::Known(13));
        }
        if !tickets_created_at {
            return Err(Error::Invariant(
                "relay schema is ambiguous: vault_registry without create_tickets.created_at",
            ));
        }
        if v12_markers.iter().all(|marker| *marker) {
            return Ok(InferredVersion::Known(13));
        }
        return Err(Error::Invariant(
            "relay schema is ambiguous: v13 markers without the v12 shape",
        ));
    }
    if tickets_created_at {
        // 有 13 的票据列却没有 registry = 半迁移/矛盾形状，绝不猜。
        return Err(Error::Invariant(
            "relay schema is ambiguous: create_tickets.created_at without vault_registry",
        ));
    }
    if v12_markers.iter().all(|marker| *marker) {
        return Ok(InferredVersion::Known(12));
    }
    if v12_markers.iter().any(|marker| *marker) {
        return Err(Error::Invariant(
            "relay schema is ambiguous: partial v12 markers",
        ));
    }
    // 多租户但无授权投影：schema 10 或 11（10→11 纯新增票据表，按 11 补齐即可）。
    Ok(InferredVersion::Known(11))
}

/// 补齐推断出版本应有的**全局表**（`create_tickets` 只建空表；不改任何租户数据）。
///
/// 形状按目标版本取：12 及更早用 v11/v12 的五列形状，让后续迁移（11→12 → 12→13）
/// 继续跑；13 已由 `infer_schema_version` 证明表与列都在，不需要补。
fn repair_global_tables(conn: &Connection, version: i64) -> Result<()> {
    if version < 13 {
        conn.execute_batch(CREATE_TICKETS_PRE_V13_DDL)?;
    } else if !table_exists(conn, "create_tickets")? {
        // schema 13 形状但票据表被误删：按当前形状补空表（registry 行/票据事实已不存在）。
        conn.execute_batch(&create_tickets_template().replace("{table}", "create_tickets"))?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS create_tickets_vault ON create_tickets(vault_id);",
        )?;
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
        // ADR 0014 §A 维护窗口：旧 challenge/token 是短命凭据，升级后一律失效
        // （客户端重新认证即可；这与"保留已消费邀请事实"是两件事）。
        //
        // 必须**在复制之前**删除：schema 12 起 `realm` 是 INTEGER（1/2/3/4），v9 的
        // TEXT realm 行不能直接写进 STRICT 表的新列。晚删（复制后再删新表）在 11→12
        // 之后语义相同，但那时已经写不进去了。
        tx.execute("DELETE FROM challenges", [])?;
        tx.execute("DELETE FROM tokens", [])?;
        let mut copied = 0usize;
        // 只**重建** schema 10 就存在的租户表：11→12 才新增的表没有 v9 来源可复制
        // （下面统一建成空表，供 11→12 之前的复合 FK 解析）。
        for (name, template, legacy_skip) in TENANT_TABLES
            .iter()
            .filter(|(name, _, _)| V10_TENANT_TABLES.contains(name))
        {
            let staging = staging_name(name);
            tx.execute_batch(&template.replace("{table}", &staging))?;
            copy_rows(&tx, name, &staging, vault_id.as_ref(), legacy_skip)?;
            tx.execute_batch(&format!(
                "DROP TABLE {name};\nALTER TABLE {staging} RENAME TO {name};"
            ))?;
            copied += 1;
            if fault == MigrationFault::Halfway && copied * 2 >= V10_TENANT_TABLES.len() {
                return Err(Error::Invariant("injected migration fault (test)"));
            }
        }
        migrate_vault_row_to_vaults(&tx, vault_id.as_ref())?;
        // schema 12 新增的租户表先建成**空表**：重建后的 `challenges`/`tokens`/`invites`
        // 带复合 FK 指向 `management_caps`，事务末尾的 `foreign_key_check` 必须能解析它。
        for (name, template, _) in TENANT_TABLES
            .iter()
            .filter(|(name, _, _)| V12_NEW_TENANT_TABLES.contains(name))
        {
            tx.execute_batch(&template.replace("{table}", name))?;
        }
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
///
/// `#[doc(hidden)] pub`：S2b/S4a2 的迁移测试要**分步**构造 schema 11 库（9→10→11→12）。
#[doc(hidden)]
pub fn migrate_v10_to_v11(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // 只建 schema 11 的**五列**形状；`created_at`/registry FK 由 12 → 13 迁移补齐。
    tx.execute_batch(CREATE_TICKETS_PRE_V13_DDL)?;
    tx.execute(
        "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
        ("11".as_bytes(), META_SCHEMA_VERSION),
    )?;
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------- v11 → v12（ADR 0015 S4a2）

/// 迁移 11 → 12 的故障注入点（只给测试用，生产路径永远是 [`MigrationFaultV12::None`]）。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationFaultV12 {
    /// 不注入故障。
    None,
    /// 重建完 `vaults` 后强制失败（验证事务回滚 + schema 11 快照仍可恢复）。
    Halfway,
}

/// v11 → v12：ADR 0015 S4a2 的授权/控制投影。
///
/// 步骤（维护窗口，单事务）：
/// 1. 文件库先 `VACUUM INTO <db>.schema11.bak`（WAL 一致快照；已存在则保留最早一份）；
/// 2. `PRAGMA foreign_keys=OFF`（SQLite 不能 `ALTER TABLE` 加表级复合 FK/跨列 CHECK）；
/// 3. `BEGIN IMMEDIATE`：**逐表重建** `vaults`/`devices`/`device_grants`/`invites`
///    （`CREATE {name}__v12` → 按新旧共有列复制 → 行数与多重集校验 → DROP → RENAME）；
/// 4. `challenges`/`tokens` 重建但**不复制任何行**（旧凭据一律失效，ADR 0015 §DDL）；
/// 5. 建/补齐 `management_caps`、`device_credentials`、`dedup_v2`（`IF NOT EXISTS`）；
/// 6. 重建索引、`PRAGMA foreign_key_check` 必须为空、写 `schema_version=12`、提交。
///
/// 任何一步失败 → 整体回滚（旧库仍是 schema 11，旧 binary 仍可打开）。
fn migrate_v11_to_v12(conn: &mut Connection) -> Result<()> {
    migrate_v11_to_v12_impl(conn, MigrationFaultV12::None)
}

/// 测试入口：验证迁移中断/回滚/备份（见 `tests/migration_v12.rs`）。
#[doc(hidden)]
pub fn migrate_v11_to_v12_with_fault(
    conn: &mut Connection,
    fault: MigrationFaultV12,
) -> Result<()> {
    migrate_v11_to_v12_impl(conn, fault)
}

fn migrate_v11_to_v12_impl(conn: &mut Connection, fault: MigrationFaultV12) -> Result<()> {
    backup_before_v12(conn)?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let outcome = (|| -> Result<()> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // `vaults` 是全局表（不在 TENANT_TABLES 里），单独重建。
        rebuild_table(&tx, "vaults", &vaults_template())?;
        if fault == MigrationFaultV12::Halfway {
            return Err(Error::Invariant("injected migration fault (test)"));
        }
        for (name, template, _) in TENANT_TABLES
            .iter()
            .filter(|(name, _, _)| V12_REBUILT_TENANT_TABLES.contains(name))
        {
            rebuild_table(&tx, name, template)?;
        }
        // 旧 challenge/token 一律失效：不复制（realm 由 TEXT 改 INTEGER，旧行为计数清零）。
        for (name, template, _) in TENANT_TABLES
            .iter()
            .filter(|(name, _, _)| V12_REPLACED_TENANT_TABLES.contains(name))
        {
            replace_table(&tx, name, template)?;
        }
        for (name, template, _) in TENANT_TABLES
            .iter()
            .filter(|(name, _, _)| V12_NEW_TENANT_TABLES.contains(name))
        {
            tx.execute_batch(&template.replace("{table}", name))?;
        }
        tx.execute_batch(INDEXES)?;
        verify_migration(&tx)?;
        tx.execute(
            "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
            ("12".as_bytes(), META_SCHEMA_VERSION),
        )?;
        tx.commit()?;
        Ok(())
    })();
    conn.pragma_update(None, "foreign_keys", "ON")?;
    outcome
}

// ---------------------------------------------------------------- v12 → v13（ADR 0018 §2）

/// 迁移 12 → 13 的故障注入点（只给测试用，生产路径永远是 [`MigrationFaultV13::None`]）。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationFaultV13 {
    /// 不注入故障。
    None,
    /// `vault_registry` 建好、存量 vault 已登记后强制失败（验证事务回滚 + schema 12 快照）。
    Halfway,
}

/// v12 → 13：权威 `vault_registry` + 票据 `created_at`/registry 关联（ADR 0018 §2）。
///
/// 步骤（维护窗口，单事务）：
/// 1. 文件库先 `VACUUM INTO <db>.schema12.bak`（WAL 一致快照；已存在则保留最早一份）；
/// 2. `PRAGMA foreign_keys=OFF`（重建 `create_tickets` 加 FK 不能在原地做）；
/// 3. `BEGIN IMMEDIATE`：
///    - 建 `vault_registry`（`IF NOT EXISTS`，幂等）；
///    - `vaults` 每行 → `ACTIVE`，`created_at` 取该行**既有**值（不虚构历史）；
///    - 存量票据补 registry 行（只补没有 `vaults` 行的 `vault_id`）：未消费且未过期 →
///      `RESERVED`；未消费已过期 → `EXPIRED`；已消费 → `DELETED` 墓碑（删除时刻未知，
///      `deleted_at` 留 NULL，不虚构；只保留最小元数据）；
///    - 重建 `create_tickets`：加 `created_at`（旧行取**迁移时刻**——文档明确它不是原始
///      创建时间）与 `FOREIGN KEY(vault_id) REFERENCES vault_registry`；原有列逐字节保留
///      （含旧库多额度历史值，不重写事实）；
///    - `ACTIVE` 数 == `vaults` 行数、票据 vault_id 全部可解析、`PRAGMA foreign_key_check`
///      为空、行数/多重集一致；
/// 4. 写 `schema_version=13` 并提交；任何一步失败 → 整体回滚（旧库仍是 schema 12）。
fn migrate_v12_to_v13(conn: &mut Connection, now: i64) -> Result<()> {
    migrate_v12_to_v13_impl(conn, MigrationFaultV13::None, now)
}

/// 测试入口：验证迁移中断/回滚/幂等（见 `tests/migration_v13.rs`）。
#[doc(hidden)]
pub fn migrate_v12_to_v13_with_fault(
    conn: &mut Connection,
    fault: MigrationFaultV13,
    now: i64,
) -> Result<()> {
    migrate_v12_to_v13_impl(conn, fault, now)
}

fn migrate_v12_to_v13_impl(
    conn: &mut Connection,
    fault: MigrationFaultV13,
    now: i64,
) -> Result<()> {
    backup_before_v13(conn)?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let outcome = (|| -> Result<()> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(VAULT_REGISTRY_DDL)?;
        // 存量 vault：生命周期从它既有的 `created_at` 开始（迁移不虚构历史）。
        tx.execute(
            "INSERT OR IGNORE INTO vault_registry(vault_id, state, created_at)
             SELECT vault_id, 'ACTIVE', created_at FROM vaults",
            [],
        )?;
        if fault == MigrationFaultV13::Halfway {
            return Err(Error::Invariant("injected migration fault (test)"));
        }
        // 存量票据：只给**没有 vaults 行**的 vault_id 补生命周期事实。
        // `created_at` 是迁移时刻（不虚构原始创建时间；见迁移文档）。
        tx.execute(
            "INSERT OR IGNORE INTO vault_registry(vault_id, state, created_at)
             SELECT t.vault_id,
                    CASE
                      WHEN MAX(CASE WHEN t.consumed_at IS NULL AND t.expires_at > ?1
                                    THEN 1 ELSE 0 END) = 1
                        THEN 'RESERVED'
                      WHEN MAX(CASE WHEN t.consumed_at IS NULL THEN 1 ELSE 0 END) = 1
                        THEN 'EXPIRED'
                      ELSE 'DELETED'
                    END,
                    ?1
               FROM create_tickets t
              WHERE NOT EXISTS (SELECT 1 FROM vaults v WHERE v.vault_id = t.vault_id)
              GROUP BY t.vault_id",
            [now],
        )?;
        rebuild_create_tickets(&tx, now)?;
        // ---- 迁移后自检（失败整体回滚）----
        let vault_count: i64 = tx.query_row("SELECT COUNT(*) FROM vaults", [], |row| row.get(0))?;
        let active_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM vault_registry WHERE state = 'ACTIVE'",
            [],
            |row| row.get(0),
        )?;
        if active_count != vault_count {
            return Err(Error::Invariant(
                "migration: ACTIVE registry rows != vaults rows",
            ));
        }
        let orphan_tickets: i64 = tx.query_row(
            "SELECT COUNT(*) FROM create_tickets t
              WHERE NOT EXISTS (SELECT 1 FROM vault_registry r WHERE r.vault_id = t.vault_id)",
            [],
            |row| row.get(0),
        )?;
        if orphan_tickets != 0 {
            return Err(Error::Invariant(
                "migration: create_tickets without vault_registry row",
            ));
        }
        verify_migration(&tx)?;
        tx.execute(
            "UPDATE relay_meta SET value = ?1 WHERE key = ?2",
            ("13".as_bytes(), META_SCHEMA_VERSION),
        )?;
        tx.commit()?;
        Ok(())
    })();
    conn.pragma_update(None, "foreign_keys", "ON")?;
    outcome
}

/// 重建 `create_tickets`：加 `created_at` + registry FK，原有列逐字节保留。
///
/// - 旧表没有 `created_at`（真实的 schema 11/12 库）：新列取**迁移时刻** `now`；
/// - 旧表已有该列（幂等重跑/形状已是 13）：按原值复制，不覆盖。
fn rebuild_create_tickets(conn: &Connection, now: i64) -> Result<()> {
    // 兜底：元数据 12 但票据表缺失（损坏库）时先建成 v12 形状，避免"迁移里建空表"歧义。
    conn.execute_batch(CREATE_TICKETS_PRE_V13_DDL)?;
    let staging = "create_tickets__v13";
    conn.execute_batch(&create_tickets_template().replace("{table}", staging))?;
    let new_columns = table_columns(conn, staging)?;
    let old_columns = table_columns(conn, "create_tickets")?;
    let shared: Vec<String> = new_columns
        .iter()
        .filter(|column| old_columns.contains(column))
        .cloned()
        .collect();
    if !shared.iter().any(|column| column == "token_hash") {
        return Err(Error::Invariant("migration: missing create_tickets"));
    }
    let list = shared.join(", ");
    let old_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM create_tickets", [], |row| row.get(0))?;
    if old_count > 0 {
        if shared.iter().any(|column| column == "created_at") {
            conn.execute(
                &format!("INSERT INTO {staging}({list}) SELECT {list} FROM create_tickets"),
                [],
            )?;
        } else {
            conn.execute(
                &format!(
                    "INSERT INTO {staging}({list}, created_at)
                     SELECT {list}, ?1 FROM create_tickets"
                ),
                [now],
            )?;
        }
    }
    let new_count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {staging}"), [], |row| {
        row.get(0)
    })?;
    if old_count != new_count {
        return Err(Error::Invariant("migration: row count mismatch"));
    }
    if old_count > 0 {
        // 旧列逐字节一致（`created_at` 不在旧列集里时自然不参与；幂等重跑时原值保留）。
        for (left, right) in [("create_tickets", staging), (staging, "create_tickets")] {
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
    }
    conn.execute_batch(&format!(
        "DROP TABLE create_tickets;\n\
         ALTER TABLE {staging} RENAME TO create_tickets;\n\
         CREATE INDEX IF NOT EXISTS create_tickets_vault ON create_tickets(vault_id);"
    ))?;
    Ok(())
}

/// 重建一张表：`CREATE {name}__v12` → 复制**新旧共有的列**（新列取默认值）→ 校验 →
/// `DROP` 旧表 → `RENAME`。
///
/// 旧表已有的新列（例如新 binary 先跑过 9→10 的库）也一并复制，因此对"已经是 v12 形状"
/// 的表重复执行是幂等的；旧表没有的列（真实的 schema 11 库）取列默认值。
fn rebuild_table(conn: &Connection, name: &str, template: &str) -> Result<()> {
    let staging = rebuild_name(name);
    conn.execute_batch(&template.replace("{table}", &staging))?;
    let new_columns = table_columns(conn, &staging)?;
    let old_columns = table_columns(conn, name)?;
    let shared: Vec<String> = new_columns
        .iter()
        .filter(|column| old_columns.contains(column))
        .cloned()
        .collect();
    if shared.is_empty() {
        return Err(Error::Invariant("migration: missing tenant table"));
    }
    let list = shared.join(", ");
    let old_count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {name}"), [], |row| {
        row.get(0)
    })?;
    if old_count > 0 {
        conn.execute(
            &format!("INSERT INTO {staging}({list}) SELECT {list} FROM {name}"),
            [],
        )?;
    }
    let new_count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {staging}"), [], |row| {
        row.get(0)
    })?;
    if old_count != new_count {
        return Err(Error::Invariant("migration: row count mismatch"));
    }
    if old_count > 0 {
        // 双向 EXCEPT + 分组计数 = 多重集完全一致（BLOB 逐字节比较，NULL 视为相等）。
        for (left, right) in [(name, staging.as_str()), (staging.as_str(), name)] {
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
    }
    conn.execute_batch(&format!(
        "DROP TABLE {name};\nALTER TABLE {staging} RENAME TO {name};"
    ))?;
    Ok(())
}

/// 用新模板替换一张表，**不保留任何行**（旧凭据一律失效）。
fn replace_table(conn: &Connection, name: &str, template: &str) -> Result<()> {
    let staging = rebuild_name(name);
    conn.execute_batch(&template.replace("{table}", &staging))?;
    conn.execute_batch(&format!(
        "DROP TABLE {name};\nALTER TABLE {staging} RENAME TO {name};"
    ))?;
    Ok(())
}

fn rebuild_name(name: &str) -> String {
    format!("{name}__v12")
}

/// 迁移前备份：`VACUUM INTO` 生成 WAL 一致快照（内存库/已存在快照则跳过）。
fn backup_before_v10(conn: &Connection) -> Result<()> {
    vacuum_snapshot(conn, &backup_path)
}

/// 迁移 11 → 12 前的 `<db>.schema11.bak` 快照。
fn backup_before_v12(conn: &Connection) -> Result<()> {
    vacuum_snapshot(conn, &backup_path_v11)
}

/// 迁移 12 → 13 前的 `<db>.schema12.bak` 快照。
fn backup_before_v13(conn: &Connection) -> Result<()> {
    vacuum_snapshot(conn, &backup_path_v12)
}

fn vacuum_snapshot(conn: &Connection, target_of: &dyn Fn(&Path) -> PathBuf) -> Result<()> {
    let Some(path) = conn.path() else {
        return Ok(());
    };
    if path.is_empty() || path == ":memory:" {
        return Ok(());
    }
    let target = target_of(Path::new(path));
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

/// `<db>.schema11.bak`：迁移 11 → 12 之前的一致快照（ADR 0015 §DDL 回滚口径）。
pub fn backup_path_v11(db: &Path) -> PathBuf {
    PathBuf::from(format!("{}.schema11.bak", db.display()))
}

/// `<db>.schema12.bak`：迁移 12 → 13 之前的一致快照（ADR 0018 §2 回滚口径）。
pub fn backup_path_v12(db: &Path) -> PathBuf {
    PathBuf::from(format!("{}.schema12.bak", db.display()))
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
    Ok(column_info(conn, table, column)?.is_some())
}

/// `PRAGMA table_info` 里该列的声明类型（不存在 = `None`）。
fn column_info(conn: &Connection, table: &str, column: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    })?;
    for row in rows {
        let (name, kind) = row?;
        if name == column {
            return Ok(Some(kind));
        }
    }
    Ok(None)
}

/// 该列是否声明为 `INTEGER`（schema 12 起 `challenges/tokens.realm` 由 TEXT 改 INTEGER，
/// 是版本形状标记之一，不能只看列名存在）。
fn column_is_integer(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(column_info(conn, table, column)?.is_some_and(|kind| kind.eq_ignore_ascii_case("INTEGER")))
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
  created_at        INTEGER NOT NULL,
  authz_version     INTEGER NOT NULL DEFAULT 1 CHECK(authz_version IN (1, 2)),
  authz_rev         INTEGER NOT NULL DEFAULT 0 CHECK(authz_rev >= 0),
  authority_rev     INTEGER NOT NULL DEFAULT 0 CHECK(authority_rev >= 0),
  recovery_pub      BLOB CHECK(recovery_pub IS NULL OR length(recovery_pub) = 32),
  policy_bytes      BLOB CHECK(policy_bytes IS NULL OR length(policy_bytes) > 0),
  upgrade_control_rev INTEGER CHECK(upgrade_control_rev IS NULL OR upgrade_control_rev >= 1),
  authz_state       TEXT NOT NULL DEFAULT 'legacy'
                    CHECK(authz_state IN ('legacy', 'active', 'recovery_only')),
  -- ADR 0015：旧 vault 一律 legacy/1/0；ACTIVE 必须有 V2 授权（policy + 恢复钥 +
  -- 指向真实控制提交的 upgrade rev）；RECOVERY_ONLY 只要求恢复钥（只许离线 Reset）。
  -- upgrade_control_rev 是复合 FK 到 control_commits(vault_id, rev)：不能指向不存在的提交。
  CHECK(
    (authz_state = 'legacy' AND authz_version = 1 AND authz_rev = 0 AND authority_rev = 0
     AND recovery_pub IS NULL AND policy_bytes IS NULL AND upgrade_control_rev IS NULL)
    OR (authz_state = 'active' AND authz_version = 2 AND authz_rev >= 1 AND authority_rev >= 1
        AND recovery_pub IS NOT NULL AND policy_bytes IS NOT NULL
        AND upgrade_control_rev IS NOT NULL)
    OR (authz_state = 'recovery_only' AND authz_version = 2 AND authz_rev >= 1
        AND authority_rev >= 1 AND recovery_pub IS NOT NULL)
  ),
  FOREIGN KEY(vault_id, upgrade_control_rev) REFERENCES control_commits(vault_id, rev)";

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
  created_at        INTEGER NOT NULL,
  authz_version     INTEGER NOT NULL DEFAULT 1 CHECK(authz_version IN (1, 2)),
  authz_rev         INTEGER NOT NULL DEFAULT 0 CHECK(authz_rev >= 0),
  authority_rev     INTEGER NOT NULL DEFAULT 0 CHECK(authority_rev >= 0),
  recovery_pub      BLOB CHECK(recovery_pub IS NULL OR length(recovery_pub) = 32),
  policy_bytes      BLOB CHECK(policy_bytes IS NULL OR length(policy_bytes) > 0),
  upgrade_control_rev INTEGER CHECK(upgrade_control_rev IS NULL OR upgrade_control_rev >= 1),
  authz_state       TEXT NOT NULL DEFAULT 'legacy'
                    CHECK(authz_state IN ('legacy', 'active', 'recovery_only')),
  CHECK(
    (authz_state = 'legacy' AND authz_version = 1 AND authz_rev = 0 AND authority_rev = 0
     AND recovery_pub IS NULL AND policy_bytes IS NULL AND upgrade_control_rev IS NULL)
    OR (authz_state = 'active' AND authz_version = 2 AND authz_rev >= 1 AND authority_rev >= 1
        AND recovery_pub IS NOT NULL AND policy_bytes IS NOT NULL
        AND upgrade_control_rev IS NOT NULL)
    OR (authz_state = 'recovery_only' AND authz_version = 2 AND authz_rev >= 1
        AND authority_rev >= 1 AND recovery_pub IS NOT NULL)
  ),
  FOREIGN KEY(vault_id, upgrade_control_rev) REFERENCES control_commits(vault_id, rev)
) STRICT;
-- v1 旧 API 的 legacy 默认 vault 至多一个（显式解析，禁止按 subject 猜租户）。
CREATE UNIQUE INDEX IF NOT EXISTS vaults_legacy_default ON vaults(legacy_default) WHERE legacy_default = 1;
"#;

/// 权威 vault 生命周期注册表（ADR 0018 §2）：**全局表**，`vaults` 之外的唯一生命周期权威。
///
/// - `vault_id` 全局唯一（含墓碑）；终态 ID 禁止复用；
/// - `RESERVED`（已发票据、未 genesis）/`ACTIVE`（有 `vaults` 行）/`EXPIRED`/`CANCELLED`
///   是领取/取消可见的状态；`DELETING/DELETED/DELETE_FAILED` 留给 S3 的在线删除作业；
/// - 墓碑行只保留最小管理元数据（状态/时间/作业 id/进度/错误），**不含**租户内容/密钥；
/// - `ACTIVE` 必须对应 `vaults` 行（同一写事务保证；DB 层无法跨表 CHECK，由迁移自检 +
///   `registry::claim_reserved` 保证）。
const VAULT_REGISTRY_DDL: &str = r#"
-- ===== vault 生命周期注册表（全局；ADR 0018 §2）=====
CREATE TABLE IF NOT EXISTS vault_registry(
  vault_id        BLOB PRIMARY KEY CHECK(length(vault_id) = 16),
  state           TEXT NOT NULL CHECK(state IN (
                    'RESERVED','ACTIVE','EXPIRED','CANCELLED',
                    'DELETING','DELETED','DELETE_FAILED')),
  created_at      INTEGER NOT NULL,
  deleted_at      INTEGER,
  delete_job_id   BLOB,
  delete_progress INTEGER NOT NULL DEFAULT 0,
  delete_error    TEXT,
  -- 活状态不得带删除元数据；删除元数据只属于删除作业状态。
  CHECK(state NOT IN ('RESERVED','ACTIVE') OR deleted_at IS NULL),
  CHECK(delete_job_id IS NULL OR length(delete_job_id) = 16),
  CHECK(delete_job_id IS NULL
        OR state IN ('DELETING','DELETED','DELETE_FAILED')),
  CHECK(delete_progress >= 0),
  CHECK(deleted_at IS NULL OR deleted_at >= created_at),
  CHECK(delete_error IS NULL OR state IN ('DELETING','DELETE_FAILED'))
) STRICT;
CREATE INDEX IF NOT EXISTS vault_registry_state ON vault_registry(state, created_at);
"#;

/// 运维创建票据（ADR 0014 §A / ADR 0018 §2）：**全局表**，因为票据发行时目标 `vaults`
/// 行还不存在（只有 registry 的 `RESERVED` 行）。
///
/// - `token_hash = SHA256(32B 明文)`，主键；明文只在 `admin::create_reservation(_for)`
///   返回一次，不落库、不进日志；
/// - `vault_id` 是票据绑定的 128-bit 公开路由标识（不是授权），**FK 到 `vault_registry`**：
///   生命周期（RESERVED/ACTIVE/…）以 registry 为准；
/// - `created_at`/`expires_at` 在**写事务内**由 relay 时钟生成（ADR 0018 §2：不再用进队前
///   采样）；迁移出来的旧票据 `created_at` 是**迁移时刻**，不是原始创建时间；
/// - `quota` 只 0/1（新发放固定 1；旧库历史值按事实保留，迁移不重写）；
/// - `consumed_at` 只在票据被消费/作废时写入，之后的任何使用都拒绝；
/// - 同 vault 兄弟票据（含未消费/过期/已消费）都可在删除时清理（S3）。
const CREATE_TICKETS_COLUMNS: &str =
    "  token_hash  BLOB PRIMARY KEY CHECK(length(token_hash) = 32),
  vault_id    BLOB NOT NULL CHECK(length(vault_id) = 16),
  created_at  INTEGER NOT NULL,
  expires_at  INTEGER NOT NULL,
  quota       INTEGER NOT NULL CHECK(quota >= 0),
  consumed_at INTEGER,
  FOREIGN KEY(vault_id) REFERENCES vault_registry(vault_id)";

/// `create_tickets` 建表模板（`{table}` 供迁移 staging 复用）。
fn create_tickets_template() -> String {
    format!("CREATE TABLE {{table}}(\n{CREATE_TICKETS_COLUMNS}\n) STRICT;")
}

/// schema 11/12 的**冻结**票据表形状（10 → 11 与"元数据丢失修复"只用它建空表；
/// 12 → 13 负责加 `created_at` 与 registry FK）。不要随当前 schema 修改。
const CREATE_TICKETS_PRE_V13_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS create_tickets(
  token_hash  BLOB PRIMARY KEY CHECK(length(token_hash) = 32),
  vault_id    BLOB NOT NULL CHECK(length(vault_id) = 16),
  expires_at  INTEGER NOT NULL,
  quota       INTEGER NOT NULL,
  consumed_at INTEGER
) STRICT;
CREATE INDEX IF NOT EXISTS create_tickets_vault ON create_tickets(vault_id);
"#;

/// schema 10 起就存在的 22 张租户表（9→10 迁移**逐张重建**；顺序 = FK 依赖顺序）。
///
/// 9→10 必须按**目标版本**（10）的形状重建，不能把 schema 11/12 才新增的表一起造出来；
/// 反过来，模板本身用当前（12）定义——新增列都有默认值/NULL，旧行复制后自然满足新约束。
const V10_TENANT_TABLES: &[&str] = &[
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

/// schema 12 **重建**（新增表级复合 FK / 跨列 CHECK）的租户表；`vaults` 另见
/// [`vaults_template`]（它是全局表，不在 [`TENANT_TABLES`] 里）。
const V12_REBUILT_TENANT_TABLES: &[&str] = &["devices", "device_grants", "invites"];

/// schema 12 **替换并清空**的租户表：`realm` 由 TEXT 改 INTEGER，旧凭据一律失效。
const V12_REPLACED_TENANT_TABLES: &[&str] = &["challenges", "tokens"];

/// schema 12 新增的租户表（cap 公开授权事实 / V2 凭证历史 / V2 提交幂等）。
const V12_NEW_TENANT_TABLES: &[&str] = &["device_credentials", "management_caps", "dedup_v2"];

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
  approval_control_rev  INTEGER CHECK(approval_control_rev IS NULL OR approval_control_rev >= 1),
  issued_authz_rev      INTEGER CHECK(issued_authz_rev IS NULL OR issued_authz_rev >= 0),
  PRIMARY KEY(vault_id, device_id),
  FOREIGN KEY(vault_id, approval_control_rev) REFERENCES control_commits(vault_id, rev)
) STRICT;",
        &[],
    ),
    (
        // ADR 0015 §DDL：V2 设备凭证历史（每台设备每次改权一份，**不 GC**）。
        // v1 行不写（历史只有 v2 证书）；`approval_control_rev` 指批准它的控制提交。
        "device_credentials",
        "CREATE TABLE IF NOT EXISTS {table}(
  vault_id             BLOB NOT NULL CHECK(length(vault_id) = 16),
  device_id            BLOB NOT NULL CHECK(length(device_id) = 16),
  serial               INTEGER NOT NULL CHECK(serial >= 1),
  authz_version        INTEGER NOT NULL CHECK(authz_version IN (1, 2)),
  sign_pk              BLOB NOT NULL CHECK(length(sign_pk) = 32),
  hpke_pk              BLOB NOT NULL CHECK(length(hpke_pk) = 32),
  certificate          BLOB NOT NULL,
  certificate_signature BLOB,
  issued_authz_rev     INTEGER CHECK(issued_authz_rev IS NULL OR issued_authz_rev >= 0),
  approval_control_rev INTEGER CHECK(approval_control_rev IS NULL OR approval_control_rev >= 1),
  created_at           INTEGER NOT NULL,
  PRIMARY KEY(vault_id, device_id, serial),
  FOREIGN KEY(vault_id, device_id) REFERENCES devices(vault_id, device_id),
  FOREIGN KEY(vault_id, approval_control_rev) REFERENCES control_commits(vault_id, rev)
) STRICT;",
        &[],
    ),
    (
        // ADR 0015 §DDL：`ManagementCapV2` 的公开授权事实（不是私钥、不含 root）。
        // `canonical_bytes` 是客户端签名的原始 CDE（服务端只存不重编码）；
        // `issued_control_rev`/`revoked_control_rev` 是复合 FK 到控制提交，跨 vault 必然失败。
        "management_caps",
        "CREATE TABLE IF NOT EXISTS {table}(
  vault_id             BLOB NOT NULL CHECK(length(vault_id) = 16),
  cap_id               BLOB NOT NULL CHECK(length(cap_id) = 16),
  canonical_bytes      BLOB NOT NULL CHECK(length(canonical_bytes) > 0),
  subject_device       BLOB NOT NULL CHECK(length(subject_device) = 16),
  subject_pk           BLOB NOT NULL CHECK(length(subject_pk) = 32),
  scopes               INTEGER NOT NULL CHECK(scopes BETWEEN 1 AND 63),
  not_before           INTEGER NOT NULL,
  expires_at           INTEGER NOT NULL CHECK(expires_at > not_before),
  issued_control_rev   INTEGER NOT NULL CHECK(issued_control_rev >= 1),
  issued_authz_rev     INTEGER NOT NULL CHECK(issued_authz_rev >= 1),
  issuer_authority_rev INTEGER NOT NULL CHECK(issuer_authority_rev >= 1),
  revoked_control_rev  INTEGER CHECK(revoked_control_rev IS NULL OR revoked_control_rev >= 1),
  delegable            INTEGER NOT NULL DEFAULT 0 CHECK(delegable = 0),
  created_at           INTEGER NOT NULL,
  PRIMARY KEY(vault_id, cap_id),
  FOREIGN KEY(vault_id, subject_device) REFERENCES devices(vault_id, device_id),
  FOREIGN KEY(vault_id, issued_control_rev) REFERENCES control_commits(vault_id, rev),
  FOREIGN KEY(vault_id, revoked_control_rev) REFERENCES control_commits(vault_id, rev),
  CHECK(revoked_control_rev IS NULL OR revoked_control_rev > issued_control_rev)
) STRICT;",
        &[],
    ),
    (
        "challenges",
        // ADR 0015：`realm` = 1 设备 / 2 旧管理（仅 legacy）/ 3 管理设备 / 4 恢复；
        // `cap_id` 只在 realm 3 必填（管理 token 必须绑定 cap，复合 FK 连 `management_caps`）。
        "CREATE TABLE {table}(
  vault_id      BLOB NOT NULL CHECK(length(vault_id) = 16),
  challenge_id  BLOB NOT NULL CHECK(length(challenge_id) = 16),
  realm         INTEGER NOT NULL CHECK(realm IN (1, 2, 3, 4)),
  subject       BLOB NOT NULL CHECK(length(subject) = 16),
  nonce         BLOB NOT NULL CHECK(length(nonce) = 32),
  expires_at    INTEGER NOT NULL,
  used_at       INTEGER,
  authz_version INTEGER NOT NULL DEFAULT 1 CHECK(authz_version IN (1, 2)),
  authz_rev     INTEGER NOT NULL DEFAULT 0 CHECK(authz_rev >= 0),
  authority_rev INTEGER NOT NULL DEFAULT 0 CHECK(authority_rev >= 0),
  cap_id        BLOB CHECK(cap_id IS NULL OR length(cap_id) = 16),
  PRIMARY KEY(vault_id, challenge_id),
  FOREIGN KEY(vault_id, cap_id) REFERENCES management_caps(vault_id, cap_id),
  CHECK(realm <> 3 OR cap_id IS NOT NULL)
) STRICT;",
        &[],
    ),
    (
        "tokens",
        "CREATE TABLE {table}(
  vault_id      BLOB NOT NULL CHECK(length(vault_id) = 16),
  token_hash    BLOB NOT NULL CHECK(length(token_hash) = 32),
  realm         INTEGER NOT NULL CHECK(realm IN (1, 2, 3, 4)),
  subject       BLOB NOT NULL CHECK(length(subject) = 16),
  auth_rev      INTEGER NOT NULL CHECK(auth_rev >= 1),
  expires_at    INTEGER NOT NULL,
  created_at    INTEGER NOT NULL,
  authz_version INTEGER NOT NULL DEFAULT 1 CHECK(authz_version IN (1, 2)),
  authz_rev     INTEGER NOT NULL DEFAULT 0 CHECK(authz_rev >= 0),
  authority_rev INTEGER NOT NULL DEFAULT 0 CHECK(authority_rev >= 0),
  cap_id        BLOB CHECK(cap_id IS NULL OR length(cap_id) = 16),
  PRIMARY KEY(vault_id, token_hash),
  FOREIGN KEY(vault_id, cap_id) REFERENCES management_caps(vault_id, cap_id),
  CHECK(realm <> 3 OR cap_id IS NOT NULL)
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
  issuer_cap_id  BLOB CHECK(issuer_cap_id IS NULL OR length(issuer_cap_id) = 16),
  PRIMARY KEY(vault_id, invite_id),
  UNIQUE(vault_id, token_hash),
  FOREIGN KEY(vault_id, issuer_cap_id) REFERENCES management_caps(vault_id, cap_id)
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
  approval_control_rev INTEGER CHECK(approval_control_rev IS NULL OR approval_control_rev >= 1),
  issued_authz_rev     INTEGER CHECK(issued_authz_rev IS NULL OR issued_authz_rev >= 0),
  PRIMARY KEY(vault_id, epoch, device_id),
  FOREIGN KEY(vault_id, device_id) REFERENCES devices(vault_id, device_id),
  FOREIGN KEY(vault_id, approval_control_rev) REFERENCES control_commits(vault_id, rev)
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
        // ADR 0015 §鉴权与事务：V2 控制提交的幂等/回执，键固定为 `(vault_id, request_id)`
        // （v1 管理写的 `dedup` 仍按 scope+id，两者互不影响）。
        // 同 request_id + 同 body 摘要 → 回放原回执；异摘要 → 409（S4b 实现判定）。
        "dedup_v2",
        "CREATE TABLE IF NOT EXISTS {table}(
  vault_id     BLOB NOT NULL CHECK(length(vault_id) = 16),
  request_id   BLOB NOT NULL CHECK(length(request_id) = 16),
  request_hash BLOB NOT NULL CHECK(length(request_hash) = 32),
  control_rev  INTEGER CHECK(control_rev IS NULL OR control_rev >= 1),
  accepted_seq INTEGER,
  response     BLOB NOT NULL,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY(vault_id, request_id),
  FOREIGN KEY(vault_id, control_rev) REFERENCES control_commits(vault_id, rev)
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

/// 租户表名（**权威清单**：与 [`TENANT_TABLES`] 一一对应，顺序 = 建表顺序）。
///
/// ADR 0018 S3 的分批 purge 用 [`crate::deletion::PURGE_ORDER`]（按 FK 子→父顺序），
/// 两者集合必须完全一致；`deletion` 模块的单元测试守这条不变量（防新表漏删）。
pub const TENANT_TABLE_NAMES: &[&str] = &[
    "manager_keys",
    "control_commits",
    "epochs",
    "devices",
    "device_credentials",
    "management_caps",
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
    "dedup_v2",
    "gc_plans",
];

/// 租户表索引（表名已是最终名；每个索引都带 `vault_id` 前缀）。
const INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS devices_status ON devices(vault_id, status);
CREATE INDEX IF NOT EXISTS devices_approval ON devices(vault_id, approval_control_rev);
CREATE INDEX IF NOT EXISTS challenges_expiry ON challenges(vault_id, expires_at);
CREATE INDEX IF NOT EXISTS challenges_cap ON challenges(vault_id, cap_id, expires_at);
CREATE INDEX IF NOT EXISTS tokens_expiry ON tokens(vault_id, expires_at);
CREATE INDEX IF NOT EXISTS tokens_cap ON tokens(vault_id, cap_id, expires_at);
CREATE INDEX IF NOT EXISTS management_caps_subject
  ON management_caps(vault_id, subject_device, expires_at);
CREATE INDEX IF NOT EXISTS management_caps_revoked
  ON management_caps(vault_id, revoked_control_rev);
CREATE INDEX IF NOT EXISTS device_credentials_approval
  ON device_credentials(vault_id, approval_control_rev);
CREATE INDEX IF NOT EXISTS invites_issuer_cap ON invites(vault_id, issuer_cap_id);
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
CREATE INDEX IF NOT EXISTS device_grants_approval
  ON device_grants(vault_id, approval_control_rev);
-- `vaults` 在 11→12 被重建（DROP + RENAME 会带走索引），这里幂等重建。
CREATE UNIQUE INDEX IF NOT EXISTS vaults_legacy_default ON vaults(legacy_default) WHERE legacy_default = 1;
"#;

/// 构建新建库的 DDL（表模板按 [`TENANT_TABLES`] 顺序替换成真实表名）。
pub(crate) fn render_ddl() -> String {
    let mut out = String::from(GLOBAL_DDL);
    // 票据表 FK 指向 `vault_registry`，必须先建 registry（SQLite 的 FK 解析延后到 DML，
    // 但按依赖顺序建表更直观，也让 `foreign_key_check` 一次通过）。
    out.push_str(VAULT_REGISTRY_DDL);
    out.push_str(&create_tickets_template().replace("{table}", "create_tickets"));
    out.push_str("CREATE INDEX IF NOT EXISTS create_tickets_vault ON create_tickets(vault_id);\n");
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
