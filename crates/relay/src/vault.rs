//! vault 行、初始化、控制链（ADR 0006 §6；多租户 ADR 0014 §A）。
//!
//! schema 10 起 `vaults` 可以有多行，每行是一个租户；**任何**访问都必须带显式
//! [`VaultScope`]（`vault_id`），不存在 `singleton = 1` 的全表读写，也不按 token subject
//! 猜租户。v1 旧 API 的 legacy 默认 vault 由 [`resolve_legacy_scope`] 显式解析
//! （部署配置覆盖，或 `vaults.legacy_default = 1`）。

use rusqlite::{Connection, OptionalExtension};

use crate::config::RelayConfig;
use crate::db::schema;
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{ct_eq, random_hash32, random_id16, sha256, sha256_parts, Hash32, Id16};
use crate::scope::{ScopeConn, VaultScope};
use crate::sig::{require, Expectation, SignDomain, SignatureCheck, SignatureHook};
use crate::time::Clock;

/// vault 状态机（ADR 0006 §6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultState {
    /// 尚未完成 genesis。
    Initializing,
    /// 正常。
    Active,
    /// 轮换中：旧 epoch 追加一律 409，读仍可用。
    Rotating,
}

impl VaultState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Initializing => "initializing",
            Self::Active => "active",
            Self::Rotating => "rotating",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "initializing" => Some(Self::Initializing),
            "active" => Some(Self::Active),
            "rotating" => Some(Self::Rotating),
            _ => None,
        }
    }
}

/// 单行 vault（某个租户）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vault {
    pub vault_id: Id16,
    pub instance_id: Id16,
    pub log_id: Id16,
    pub log_incarnation: Id16,
    pub cursor_key: Hash32,
    pub state: VaultState,
    pub active_epoch: u64,
    pub next_epoch: u64,
    pub manager_rev: u64,
    pub control_rev: u64,
    pub auth_rev: u64,
    pub head_seq: u64,
    pub gc_floor: u64,
    pub normal_limit: i64,
    pub maintenance_limit: i64,
    pub created_at: i64,
}

/// 资产/对象里的 epoch 宽度是 u64，但 SQLite 存 INTEGER（i64）；统一在这里收口。
fn u64_from_i64(value: i64, field: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Invariant(field))
}

fn i64_from_u64(value: u64, field: &'static str) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Invariant(field))
}

fn id16_from_blob(bytes: Vec<u8>, field: &'static str) -> Result<Id16> {
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Invariant(field))
}

fn hash32_from_blob(bytes: Vec<u8>, field: &'static str) -> Result<Hash32> {
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Invariant(field))
}

/// 读已落库的 setup secret 哈希（`init` 写入；**实例级**元数据）。
pub fn setup_secret_hash(conn: &Connection) -> Result<Option<Hash32>> {
    let value: Option<Vec<u8>> = conn
        .query_row(
            "SELECT value FROM relay_meta WHERE key = ?1",
            [schema::META_SETUP_SECRET_HASH],
            |row| row.get(0),
        )
        .optional()?;
    match value {
        None => Ok(None),
        Some(bytes) => {
            Ok(Some(bytes.as_slice().try_into().map_err(|_| {
                Error::Invariant("setup secret hash length")
            })?))
        }
    }
}

/// 写入 setup secret 哈希（`init` 调用；只落哈希，不存明文）。**实例级**。
pub fn store_setup_secret_hash(conn: &Connection, hash: &Hash32) -> Result<()> {
    conn.execute(
        "INSERT INTO relay_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (schema::META_SETUP_SECRET_HASH, hash.as_slice()),
    )?;
    Ok(())
}

/// 显式解析 v1 旧 API 的 legacy 默认 vault scope。
///
/// 解析顺序（两者都是**显式**来源，绝不"挑第一个 vault"）：
/// 1. 部署配置 `legacy_vault_id`（必须真实存在，否则 404）；
/// 2. 库里 `vaults.legacy_default = 1` 的唯一一行（迁移把旧单 vault 标成它）。
///
/// 都没有 → 404（等同旧行为"库还没 genesis"）。
pub fn resolve_legacy_scope(conn: &Connection, cfg: &RelayConfig) -> Result<VaultScope> {
    if let Some(vault_id) = cfg.legacy_vault_id {
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM vaults WHERE vault_id = ?1",
            [vault_id.as_slice()],
            |row| row.get(0),
        )?;
        if exists != 1 {
            return Err(Error::api(ErrorCode::NotFound));
        }
        return Ok(VaultScope::new(vault_id));
    }
    legacy_scope_from_db(conn)
}

/// 只按库内标记解析 legacy 默认 scope（`vaults.legacy_default = 1`）。
///
/// 供运维/测试脚手架使用；HTTP 层始终走 [`resolve_legacy_scope`]（含配置覆盖）。
pub fn legacy_scope_from_db(conn: &Connection) -> Result<VaultScope> {
    match schema::legacy_default_vault_id(conn)? {
        Some(vault_id) => Ok(VaultScope::new(vault_id)),
        None => Err(Error::api(ErrorCode::NotFound)),
    }
}

/// 读取作用域内的 vault 行；不存在返回 404。
pub fn load(conn: &ScopeConn) -> Result<Vault> {
    load_opt(conn)?.ok_or_else(|| Error::api(ErrorCode::NotFound))
}

/// 读取作用域内的 vault 行（可不存在）。
pub fn load_opt(conn: &ScopeConn) -> Result<Option<Vault>> {
    let mut stmt = conn.prepare(
        "SELECT vault_id, instance_id, log_id, log_incarnation, cursor_key, state,
                active_epoch, next_epoch, manager_rev, control_rev, auth_rev,
                head_seq, gc_floor, normal_limit, maintenance_limit, created_at
         FROM vaults WHERE vault_id = ?1",
    )?;
    let row = stmt
        .query_row([conn.scope().as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, i64>(13)?,
                row.get::<_, i64>(14)?,
                row.get::<_, i64>(15)?,
            ))
        })
        .optional()?;
    let Some((
        vault_id,
        instance_id,
        log_id,
        log_incarnation,
        cursor_key,
        state,
        active_epoch,
        next_epoch,
        manager_rev,
        control_rev,
        auth_rev,
        head_seq,
        gc_floor,
        normal_limit,
        maintenance_limit,
        created_at,
    )) = row
    else {
        return Ok(None);
    };
    Ok(Some(Vault {
        vault_id: id16_from_blob(vault_id, "vaults.vault_id")?,
        instance_id: id16_from_blob(instance_id, "vaults.instance_id")?,
        log_id: id16_from_blob(log_id, "vaults.log_id")?,
        log_incarnation: id16_from_blob(log_incarnation, "vaults.log_incarnation")?,
        cursor_key: hash32_from_blob(cursor_key, "vaults.cursor_key")?,
        state: VaultState::parse(&state).ok_or(Error::Invariant("vaults.state"))?,
        active_epoch: u64_from_i64(active_epoch, "vaults.active_epoch")?,
        next_epoch: u64_from_i64(next_epoch, "vaults.next_epoch")?,
        manager_rev: u64_from_i64(manager_rev, "vaults.manager_rev")?,
        control_rev: u64_from_i64(control_rev, "vaults.control_rev")?,
        auth_rev: u64_from_i64(auth_rev, "vaults.auth_rev")?,
        head_seq: u64_from_i64(head_seq, "vaults.head_seq")?,
        gc_floor: u64_from_i64(gc_floor, "vaults.gc_floor")?,
        normal_limit,
        maintenance_limit,
        created_at,
    }))
}

/// 改 vault 状态。
pub fn set_state(conn: &ScopeConn, state: VaultState) -> Result<()> {
    let changed = conn.execute(
        "UPDATE vaults SET state = ?1 WHERE vault_id = ?",
        (state.as_str(), conn.scope().as_slice()),
    )?;
    if changed != 1 {
        return Err(Error::Invariant("vault row missing"));
    }
    Ok(())
}

/// 吊销/轮换后推进 auth_rev：**该 vault 的所有旧 token 立即失效**（ADR 0006 §4）。
pub fn bump_auth_rev(conn: &ScopeConn) -> Result<u64> {
    let changed = conn.execute(
        "UPDATE vaults SET auth_rev = auth_rev + 1 WHERE vault_id = ?",
        [conn.scope().as_slice()],
    )?;
    if changed != 1 {
        return Err(Error::Invariant("vault row missing"));
    }
    let rev: i64 = conn.query_row(
        "SELECT auth_rev FROM vaults WHERE vault_id = ?1",
        [conn.scope().as_slice()],
        |row| row.get(0),
    )?;
    u64_from_i64(rev, "vaults.auth_rev")
}

/// 分配下一个 epoch（单调递增，废弃 epoch 永不复用；ADR §8 步 3）。
pub fn allocate_epoch(conn: &ScopeConn, clock_now: i64) -> Result<u64> {
    let vault = load(conn)?;
    let epoch = vault.next_epoch;
    conn.execute(
        "UPDATE vaults SET next_epoch = next_epoch + 1 WHERE vault_id = ?",
        [conn.scope().as_slice()],
    )?;
    conn.execute(
        "INSERT INTO epochs(vault_id, epoch, state, created_at) VALUES (?1, ?2, 'staged', ?3)",
        (
            conn.scope().as_slice(),
            i64_from_u64(epoch, "epoch")?,
            clock_now,
        ),
    )?;
    Ok(epoch)
}

/// 换 log incarnation（备份 restore 时显式调用；ADR §7：不偷偷回滚游标）。
///
/// 换完后该 vault 的所有旧 cursor 都会得到 `RELAY_RESET`，客户端必须显式 reconcile。
pub fn reincarnate_log(conn: &ScopeConn) -> Result<Id16> {
    let incarnation = random_id16()?;
    conn.execute(
        "UPDATE vaults SET log_incarnation = ?1 WHERE vault_id = ?",
        (incarnation.as_slice(), conn.scope().as_slice()),
    )?;
    Ok(incarnation)
}

/// 控制链当前头：`(rev, record_hash)`；无记录时 `(0, zeros)`。
pub fn control_head(conn: &ScopeConn) -> Result<(u64, Hash32)> {
    let mut stmt = conn.prepare(
        "SELECT rev, record_hash FROM control_commits
         WHERE vault_id = ?1 ORDER BY rev DESC LIMIT 1",
    )?;
    let row = stmt
        .query_row([conn.scope().as_slice()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .optional()?;
    match row {
        None => Ok((0, [0u8; 32])),
        Some((rev, hash)) => Ok((
            u64_from_i64(rev, "control_commits.rev")?,
            hash32_from_blob(hash, "control_commits.record_hash")?,
        )),
    }
}

/// 追加一条控制转换：`record_hash = SHA256(prev_hash || signed_record)`，`prev_hash` 指向上一条。
///
/// 链首（rev=1，genesis 记录）也走同一公式：`prev_hash = 32 × 0x00`，因此从第一条起
/// 客户端只需一种哈希实现，不存在"rev=1 特例"。
///
/// 控制链**不随密文 GC 消失**（ADR §6 末段）；`rev` 单调，客户端可从头验证。
/// `rev` 只在**本 vault 内**单调（多租户后每个 vault 各自从 1 开始）。
pub fn append_control(conn: &ScopeConn, now: i64, signed_record: &[u8]) -> Result<u64> {
    let (prev_rev, prev_hash) = control_head(conn)?;
    let next_rev = prev_rev + 1;
    let record_hash = sha256_parts(&[&prev_hash, signed_record]);
    conn.execute(
        "INSERT INTO control_commits(vault_id, rev, prev_hash, record_hash, signed_record,
                                    created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            conn.scope().as_slice(),
            i64_from_u64(next_rev, "control rev")?,
            prev_hash.as_slice(),
            record_hash.as_slice(),
            signed_record,
            now,
        ),
    )?;
    conn.execute(
        "UPDATE vaults SET control_rev = ?1 WHERE vault_id = ?",
        (
            i64_from_u64(next_rev, "control rev")?,
            conn.scope().as_slice(),
        ),
    )?;
    Ok(next_rev)
}

/// 当前管理公钥（作用域内 `manager_keys` 的最大 rev）。
pub fn manager_pubkey(conn: &ScopeConn) -> Result<Option<[u8; 32]>> {
    let mut stmt = conn
        .prepare("SELECT pubkey FROM manager_keys WHERE vault_id = ?1 ORDER BY rev DESC LIMIT 1")?;
    let row = stmt
        .query_row([conn.scope().as_slice()], |row| row.get::<_, Vec<u8>>(0))
        .optional()?;
    match row {
        None => Ok(None),
        Some(bytes) => Ok(Some(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("manager pubkey"))?,
        )),
    }
}

/// 设备的活动公钥（只有作用域内 active 设备能写入）。
pub fn device_sign_pk(conn: &ScopeConn, device_id: &Id16) -> Result<Option<[u8; 32]>> {
    let mut stmt = conn.prepare(
        "SELECT sign_pk FROM devices
         WHERE device_id = ?1 AND status = 'active' AND vault_id = ?",
    )?;
    let row = stmt
        .query_row((device_id.as_slice(), conn.scope().as_slice()), |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .optional()?;
    match row {
        None => Ok(None),
        Some(bytes) => Ok(Some(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("device sign_pk"))?,
        )),
    }
}

/// 首台设备的 genesis 请求。
#[derive(Debug)]
pub struct InitRequest {
    /// vault UUID（客户端生成，同时也是本地 search.vault_uuid）。
    pub vault_id: Id16,
    /// 首台设备 id。
    pub device_id: Id16,
    /// 设备 Ed25519 公钥。
    pub sign_pk: [u8; 32],
    /// 设备 X25519 HPKE 公钥。
    pub hpke_pk: [u8; 32],
    /// 独立管理公钥（不由 vault root 派生）。
    pub manager_pub: [u8; 32],
    /// 一次性 setup secret。
    pub setup_secret: String,
    /// `CDE(DeviceCertV1)`：管理 key 签发的首台设备证书（ADR §17.1）。
    pub certificate: Vec<u8>,
    /// `MN/v1/device-cert\0 || CDE(cert)` 的签名（§17.1，不再只验原始字节）。
    pub certificate_signature: Vec<u8>,
}

/// genesis 成功后的身份信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOutcome {
    pub instance_id: Id16,
    pub log_id: Id16,
    pub epoch: u64,
    pub control_rev: u64,
    pub auth_rev: u64,
}

/// genesis 授权凭据（ADR 0014 §A）。
///
/// v1 旧路径用一次性 setup secret；v2 `/v2/vaults` 的授权在**同一事务**里已由一次性创建
/// 票据消费承担（`create_tickets`），这里不再重复检查 setup secret，也不消费它。
#[derive(Debug, Clone, Copy)]
enum GenesisAuth<'a> {
    /// v1：`SHA256(setup_secret)` 常数时间比较；legacy 默认 vault 还要检查实例级
    /// `setup.consumed`。
    SetupSecret(&'a str),
    /// v2：调用方已消费创建票据（外部授权）；本函数只做 vault 自身的结构校验。
    CreateTicket,
}

/// 初始化该作用域的新 vault（ADR 0006 §4"首台"）。
///
/// v1 语义不变：库内已有任何 vault 时返回 `ALREADY_INITIALIZED`（v2 的多 vault 创建
/// 走 S2b 的创建票据，不复用本函数）。首个 vault 会被**显式**标记
/// `legacy_default = 1`，作为 v1 旧 API 的唯一 legacy 默认 vault。
pub fn initialize(
    conn: &ScopeConn,
    clock: &dyn Clock,
    cfg: &RelayConfig,
    hook: &dyn SignatureHook,
    req: &InitRequest,
) -> Result<InitOutcome> {
    if conn.scope().vault_id() != &req.vault_id {
        return Err(Error::Invariant("initialize: scope/vault mismatch"));
    }
    let existing: i64 = conn.query_row("SELECT COUNT(*) FROM vaults", [], |row| row.get(0))?;
    if existing > 0 {
        return Err(Error::api(ErrorCode::AlreadyInitialized));
    }
    // v1 genesis 只允许建**一个** vault（上面已检查 COUNT=0）；它同时是 v1 旧 API 的
    // legacy 默认 vault。配置显式指定别的 vault 时保持 legacy_default=0（运维可控）。
    let legacy_default = match cfg.legacy_vault_id {
        Some(explicit) => explicit == req.vault_id,
        None => true,
    };
    initialize_vault(
        conn,
        clock,
        cfg,
        hook,
        req,
        GenesisAuth::SetupSecret(&req.setup_secret),
        legacy_default,
    )
}

/// 多租户创建原语（ADR 0014 §A）：**不做**"库内已有 vault"的 v1 门禁。
///
/// S2a 里只用于测试构造第二个租户；S2b 的 `/v2/vaults` 走
/// [`create_vault_with_ticket`]（授权由同一事务里的创建票据消费承担）。
/// 本函数保留 setup secret 语义（测试/内部工具），`legacy_default` 必须显式给出，
/// 绝不隐式把新 vault 变成 v1 默认租户。
#[doc(hidden)]
pub fn create_vault_in_scope(
    conn: &ScopeConn,
    clock: &dyn Clock,
    cfg: &RelayConfig,
    hook: &dyn SignatureHook,
    req: &InitRequest,
    legacy_default: bool,
) -> Result<InitOutcome> {
    create_vault_authorized(
        conn,
        clock,
        cfg,
        hook,
        req,
        GenesisAuth::SetupSecret(&req.setup_secret),
        legacy_default,
    )
}

/// `/v2/vaults` 的创建入口：调用方必须**已经在同一个写事务里**消费了创建票据
/// （`admin::consume_create_ticket`），本函数不再检查 setup secret。
///
/// 语义：
/// - 该 `vault_id` 已存在 → `ALREADY_INITIALIZED`（409；绝不覆盖已有租户），调用方事务回滚
///   后票据不会被消费；
/// - 永远不会把新 vault 标成 v1 legacy 默认（`legacy_default = 0`），v1 旧 API 仍只解析
///   显式的 legacy vault；
/// - 设备证书/签名校验与 v1 genesis 完全相同（`DeviceCertV1` 绑定 manager key）。
#[doc(hidden)]
pub fn create_vault_with_ticket(
    conn: &ScopeConn,
    clock: &dyn Clock,
    cfg: &RelayConfig,
    hook: &dyn SignatureHook,
    req: &InitRequest,
) -> Result<InitOutcome> {
    create_vault_authorized(
        conn,
        clock,
        cfg,
        hook,
        req,
        GenesisAuth::CreateTicket,
        false,
    )
}

fn create_vault_authorized(
    conn: &ScopeConn,
    clock: &dyn Clock,
    cfg: &RelayConfig,
    hook: &dyn SignatureHook,
    req: &InitRequest,
    auth: GenesisAuth<'_>,
    legacy_default: bool,
) -> Result<InitOutcome> {
    if conn.scope().vault_id() != &req.vault_id {
        return Err(Error::Invariant(
            "create_vault_authorized: scope/vault mismatch",
        ));
    }
    if load_opt(conn)?.is_some() {
        return Err(Error::api(ErrorCode::AlreadyInitialized));
    }
    initialize_vault(conn, clock, cfg, hook, req, auth, legacy_default)
}

fn initialize_vault(
    conn: &ScopeConn,
    clock: &dyn Clock,
    cfg: &RelayConfig,
    hook: &dyn SignatureHook,
    req: &InitRequest,
    auth: GenesisAuth<'_>,
    legacy_default: bool,
) -> Result<InitOutcome> {
    match auth {
        GenesisAuth::SetupSecret(secret) => {
            // setup secret：只做哈希比较（常数时间），失败不区分"没配置/不匹配"，避免 oracle。
            // 支持两种来源：部署注入的明文，或 `init` 阶段落库的 SHA256（进程重启后仍可用）。
            let expected = match (cfg.setup_secret_hash, cfg.setup_secret.as_deref()) {
                (Some(hash), _) => hash,
                (None, Some(provided)) => sha256(provided.as_bytes()),
                (None, None) => return Err(Error::api(ErrorCode::Revoked)),
            };
            if !ct_eq(sha256(secret.as_bytes()).as_slice(), expected.as_slice()) {
                return Err(Error::api(ErrorCode::Revoked));
            }
            if legacy_default
                && schema::meta_text(conn.raw(), schema::META_SETUP_CONSUMED)?.as_deref()
                    == Some("1")
            {
                // v1：setup secret 已被消费 → 拒绝第二次 genesis（与旧行为一致）。
                // 多租户创建票据路径不消费 setup secret，因此不落这条门禁。
                return Err(Error::api(ErrorCode::AlreadyInitialized));
            }
        }
        GenesisAuth::CreateTicket => {
            // v2 的授权 = 调用方在同一事务里已成功消费的一次性创建票据（ADR 0014 §A）。
            // 创建票据永远不能造出 v1 legacy 默认 vault（避免 v2 悄悄改变 v1 路由语义）。
            if legacy_default {
                return Err(Error::Invariant(
                    "create ticket cannot create the legacy default vault",
                ));
            }
        }
    }
    let instance_id = schema::instance_id(conn.raw())?;
    let now = clock.now_secs();
    let vault_id = req.vault_id;
    // §17.1：genesis 必须是管理 key 签发的 DeviceCertV1，且绑定首台设备/vault/公钥。
    let cert = menghunote_sync_proto::DeviceCertV1::decode(&req.certificate)
        .map_err(|_| Error::api(ErrorCode::InvalidRequest))?;
    if cert.vault_id != req.vault_id
        || cert.device_id != req.device_id
        || cert.sign_ed25519_pub != req.sign_pk
        || cert.hpke_x25519_pub != req.hpke_pk
        || cert.issued_manager_rev != 1
        || !cert.rights.can_read()
        || !cert.rights.can_write()
    {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    let check = SignatureCheck {
        domain: SignDomain::DeviceApprove,
        vault_id: &vault_id,
        device_id: Some(&req.device_id),
        public_key: &req.manager_pub,
        message: &[],
        signature: &req.certificate_signature,
        expect: &Expectation::DeviceCert { cert: &cert },
    };
    require(hook, &check, ErrorCode::SignatureInvalid)?;

    let log_id = random_id16()?;
    let log_incarnation = random_id16()?;
    // 部署可固定游标 HMAC key；否则 genesis 随机生成并只落库。
    let cursor_key = match cfg.cursor_key {
        Some(key) => key,
        None => random_hash32()?,
    };
    conn.execute(
        "INSERT INTO vaults(vault_id, instance_id, log_id, log_incarnation, cursor_key,
                            state, active_epoch, next_epoch, manager_rev, control_rev, auth_rev,
                            head_seq, gc_floor, normal_limit, maintenance_limit,
                            legacy_default, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'active', 1, 2, 1, 1, 1, 0, 0, ?6, ?7, ?8, ?9)",
        (
            vault_id.as_slice(),
            instance_id.as_slice(),
            log_id.as_slice(),
            log_incarnation.as_slice(),
            cursor_key.as_slice(),
            cfg.db.normal_limit,
            cfg.db.maintenance_limit,
            i64::from(legacy_default),
            now,
        ),
    )?;
    conn.execute(
        "INSERT INTO epochs(vault_id, epoch, state, created_at) VALUES (?1, 1, 'active', ?2)",
        (vault_id.as_slice(), now),
    )?;
    conn.execute(
        "INSERT INTO devices(vault_id, device_id, sign_pk, hpke_pk, certificate,
                             certificate_signature, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7)",
        (
            vault_id.as_slice(),
            req.device_id.as_slice(),
            req.sign_pk.as_slice(),
            req.hpke_pk.as_slice(),
            req.certificate.as_slice(),
            req.certificate_signature.as_slice(),
            now,
        ),
    )?;
    conn.execute(
        "INSERT INTO manager_keys(vault_id, rev, pubkey, transition, created_at)
         VALUES (?1, 1, ?2, ?3, ?4)",
        (
            vault_id.as_slice(),
            req.manager_pub.as_slice(),
            req.certificate.as_slice(),
            now,
        ),
    )?;
    // 控制链第一条：prev_hash = 32 字节全零，record_hash **仍按同一公式**
    // `SHA256(prev_hash || signed_record)` 计算（客户端/第三方只需实现一种哈希）。
    let zero = [0u8; 32];
    let genesis_hash = sha256_parts(&[zero.as_slice(), req.certificate.as_slice()]);
    conn.execute(
        "INSERT INTO control_commits(vault_id, rev, prev_hash, record_hash, signed_record,
                                    created_at)
         VALUES (?1, 1, ?2, ?3, ?4, ?5)",
        (
            vault_id.as_slice(),
            zero.as_slice(),
            genesis_hash.as_slice(),
            req.certificate.as_slice(),
            now,
        ),
    )?;
    conn.execute(
        "INSERT INTO usage(vault_id, normal_bytes, maintenance_bytes) VALUES (?1, 0, 0)",
        [vault_id.as_slice()],
    )?;
    if legacy_default {
        schema::set_meta(conn.raw(), schema::META_SETUP_CONSUMED, b"1")?;
    }
    // 首台设备同时拿到管理权：管理 token 与管理签名分离，但成员集合只有一个。
    Ok(InitOutcome {
        instance_id,
        log_id,
        epoch: 1,
        control_rev: 1,
        auth_rev: 1,
    })
}

/// 把 `vault state=rotating` 的候选 epoch 取出来（供轮换流程与测试断言）。
pub fn candidate_epoch(conn: &ScopeConn) -> Result<Option<u64>> {
    let mut stmt = conn.prepare(
        "SELECT epoch FROM epochs
         WHERE vault_id = ?1 AND state = 'staged' ORDER BY epoch DESC LIMIT 1",
    )?;
    let row = stmt
        .query_row([conn.scope().as_slice()], |row| row.get::<_, i64>(0))
        .optional()?;
    match row {
        None => Ok(None),
        Some(value) => Ok(Some(u64_from_i64(value, "epochs.epoch")?)),
    }
}

/// epoch 状态。
/// §23.1：标记某个 epoch 在其冻结期间发生过吊销（永久事实，abort 不得清）。
pub fn mark_epoch_revoked_once(conn: &ScopeConn, epoch: u64) -> Result<()> {
    conn.execute(
        "UPDATE epochs SET revoked_once = 1 WHERE epoch = ?1 AND vault_id = ?",
        (i64_from_u64(epoch, "epoch")?, conn.scope().as_slice()),
    )?;
    Ok(())
}

/// §23.1：该 epoch 是否曾在其冻结期间发生过吊销；无 epoch 行按"没有"处理。
pub fn epoch_revoked_once(conn: &ScopeConn, epoch: u64) -> Result<bool> {
    let mut stmt =
        conn.prepare("SELECT revoked_once FROM epochs WHERE epoch = ?1 AND vault_id = ?")?;
    let row = stmt
        .query_row(
            (i64_from_u64(epoch, "epoch")?, conn.scope().as_slice()),
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(row.unwrap_or(0) != 0)
}

pub fn epoch_state(conn: &ScopeConn, epoch: u64) -> Result<Option<EpochState>> {
    let mut stmt = conn.prepare("SELECT state FROM epochs WHERE epoch = ?1 AND vault_id = ?")?;
    let row = stmt
        .query_row(
            (i64_from_u64(epoch, "epoch")?, conn.scope().as_slice()),
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(row.and_then(|state| EpochState::parse(&state)))
}

/// epoch 状态枚举（ADR §6 `epochs.state`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochState {
    Staged,
    Active,
    Retired,
    Aborted,
}

impl EpochState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Active => "active",
            Self::Retired => "retired",
            Self::Aborted => "aborted",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "staged" => Some(Self::Staged),
            "active" => Some(Self::Active),
            "retired" => Some(Self::Retired),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }
}

/// 设置 epoch 状态。
pub fn set_epoch_state(conn: &ScopeConn, epoch: u64, state: EpochState) -> Result<()> {
    conn.execute(
        "UPDATE epochs SET state = ?1 WHERE epoch = ?2 AND vault_id = ?",
        (
            state.as_str(),
            i64_from_u64(epoch, "epoch")?,
            conn.scope().as_slice(),
        ),
    )?;
    Ok(())
}

/// 加 `retire_at`（旧 epoch/旧槽 14 天保留期，ADR §8 步 7）。
pub fn set_epoch_retire_at(conn: &ScopeConn, epoch: u64, at: i64) -> Result<()> {
    conn.execute(
        "UPDATE epochs SET retire_at = ?1 WHERE epoch = ?2 AND vault_id = ?",
        (at, i64_from_u64(epoch, "epoch")?, conn.scope().as_slice()),
    )?;
    Ok(())
}

/// 给 auth_rev 相关的错误补上 `current_epoch/control_rev`（ADR §5 响应字段）。
pub fn annotate(mut api: ApiError, conn: &ScopeConn) -> ApiError {
    if let Ok(Some(vault)) = load_opt(conn) {
        api.current_epoch.get_or_insert(vault.active_epoch);
        api.current_control_rev.get_or_insert(vault.control_rev);
    }
    api
}
