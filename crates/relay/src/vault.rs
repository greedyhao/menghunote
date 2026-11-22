//! vault 行、初始化、控制链（ADR 0006 §6）。
//!
//! 单实例只允许一个 vault：`vault.singleton = 1` 是 PK，第二行直接写不进去。

use rusqlite::{Connection, OptionalExtension};

use crate::config::RelayConfig;
use crate::db::schema;
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{ct_eq, random_hash32, random_id16, sha256, sha256_parts, Hash32, Id16};
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

/// 单行 vault。
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

/// 读已落库的 setup secret 哈希（`init` 写入）。
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

/// 写入 setup secret 哈希（`init` 调用；只落哈希，不存明文）。
pub fn store_setup_secret_hash(conn: &Connection, hash: &Hash32) -> Result<()> {
    conn.execute(
        "INSERT INTO relay_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (schema::META_SETUP_SECRET_HASH, hash.as_slice()),
    )?;
    Ok(())
}

/// 读取 vault 行；不存在返回 404。
pub fn load(conn: &Connection) -> Result<Vault> {
    load_opt(conn)?.ok_or_else(|| Error::api(ErrorCode::NotFound))
}

/// 读取 vault 行（可不存在）。
pub fn load_opt(conn: &Connection) -> Result<Option<Vault>> {
    let mut stmt = conn.prepare(
        "SELECT vault_id, instance_id, log_id, log_incarnation, cursor_key, state,
                active_epoch, next_epoch, manager_rev, control_rev, auth_rev,
                head_seq, gc_floor, normal_limit, maintenance_limit, created_at
         FROM vault WHERE singleton = 1",
    )?;
    let row = stmt
        .query_row([], |row| {
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
        vault_id: id16_from_blob(vault_id, "vault.vault_id")?,
        instance_id: id16_from_blob(instance_id, "vault.instance_id")?,
        log_id: id16_from_blob(log_id, "vault.log_id")?,
        log_incarnation: id16_from_blob(log_incarnation, "vault.log_incarnation")?,
        cursor_key: hash32_from_blob(cursor_key, "vault.cursor_key")?,
        state: VaultState::parse(&state).ok_or(Error::Invariant("vault.state"))?,
        active_epoch: u64_from_i64(active_epoch, "vault.active_epoch")?,
        next_epoch: u64_from_i64(next_epoch, "vault.next_epoch")?,
        manager_rev: u64_from_i64(manager_rev, "vault.manager_rev")?,
        control_rev: u64_from_i64(control_rev, "vault.control_rev")?,
        auth_rev: u64_from_i64(auth_rev, "vault.auth_rev")?,
        head_seq: u64_from_i64(head_seq, "vault.head_seq")?,
        gc_floor: u64_from_i64(gc_floor, "vault.gc_floor")?,
        normal_limit,
        maintenance_limit,
        created_at,
    }))
}

/// 改 vault 状态。
pub fn set_state(conn: &Connection, state: VaultState) -> Result<()> {
    let changed = conn.execute(
        "UPDATE vault SET state = ?1 WHERE singleton = 1",
        [state.as_str()],
    )?;
    if changed != 1 {
        return Err(Error::Invariant("vault row missing"));
    }
    Ok(())
}

/// 吊销/轮换后推进 auth_rev：**所有旧 token 立即失效**（ADR 0006 §4）。
pub fn bump_auth_rev(conn: &Connection) -> Result<u64> {
    conn.execute("UPDATE vault SET auth_rev = auth_rev + 1", [])?;
    let rev: i64 = conn.query_row(
        "SELECT auth_rev FROM vault WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    u64_from_i64(rev, "vault.auth_rev")
}

/// 分配下一个 epoch（单调递增，废弃 epoch 永不复用；ADR §8 步 3）。
pub fn allocate_epoch(conn: &Connection, clock_now: i64) -> Result<u64> {
    let vault = load(conn)?;
    let epoch = vault.next_epoch;
    conn.execute(
        "UPDATE vault SET next_epoch = next_epoch + 1 WHERE singleton = 1",
        [],
    )?;
    conn.execute(
        "INSERT INTO epochs(epoch, state, created_at) VALUES (?1, 'staged', ?2)",
        (i64_from_u64(epoch, "epoch")?, clock_now),
    )?;
    Ok(epoch)
}

/// 换 log incarnation（备份 restore 时显式调用；ADR §7：不偷偷回滚游标）。
///
/// 换完后所有旧 cursor 都会得到 `RELAY_RESET`，客户端必须显式 reconcile。
pub fn reincarnate_log(conn: &Connection) -> Result<Id16> {
    let incarnation = random_id16()?;
    conn.execute(
        "UPDATE vault SET log_incarnation = ?1 WHERE singleton = 1",
        [incarnation.as_slice()],
    )?;
    Ok(incarnation)
}

/// 控制链当前头：`(rev, record_hash)`；无记录时 `(0, zeros)`。
pub fn control_head(conn: &Connection) -> Result<(u64, Hash32)> {
    let mut stmt =
        conn.prepare("SELECT rev, record_hash FROM control_commits ORDER BY rev DESC LIMIT 1")?;
    let row = stmt
        .query_row([], |row| {
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
pub fn append_control(conn: &Connection, now: i64, signed_record: &[u8]) -> Result<u64> {
    let (prev_rev, prev_hash) = control_head(conn)?;
    let next_rev = prev_rev + 1;
    let record_hash = sha256_parts(&[&prev_hash, signed_record]);
    conn.execute(
        "INSERT INTO control_commits(rev, prev_hash, record_hash, signed_record, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            i64_from_u64(next_rev, "control rev")?,
            prev_hash.as_slice(),
            record_hash.as_slice(),
            signed_record,
            now,
        ),
    )?;
    conn.execute(
        "UPDATE vault SET control_rev = ?1 WHERE singleton = 1",
        [i64_from_u64(next_rev, "control rev")?],
    )?;
    Ok(next_rev)
}

/// 当前管理公钥（`manager_keys` 的最大 rev）。
pub fn manager_pubkey(conn: &Connection) -> Result<Option<[u8; 32]>> {
    let mut stmt = conn.prepare("SELECT pubkey FROM manager_keys ORDER BY rev DESC LIMIT 1")?;
    let row = stmt
        .query_row([], |row| row.get::<_, Vec<u8>>(0))
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

/// 设备的活动公钥（只有 active 设备能写入）。
pub fn device_sign_pk(conn: &Connection, device_id: &Id16) -> Result<Option<[u8; 32]>> {
    let mut stmt =
        conn.prepare("SELECT sign_pk FROM devices WHERE device_id = ?1 AND status = 'active'")?;
    let row = stmt
        .query_row([device_id.as_slice()], |row| row.get::<_, Vec<u8>>(0))
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

/// 初始化唯一 vault（ADR 0006 §4"首台"）。
pub fn initialize(
    conn: &Connection,
    clock: &dyn Clock,
    cfg: &RelayConfig,
    hook: &dyn SignatureHook,
    req: &InitRequest,
) -> Result<InitOutcome> {
    if load_opt(conn)?.is_some() {
        return Err(Error::api(ErrorCode::AlreadyInitialized));
    }
    // setup secret：只做哈希比较（常数时间），失败不区分"没配置/不匹配"，避免 oracle。
    // 支持两种来源：部署注入的明文，或 `init` 阶段落库的 SHA256（进程重启后仍可用）。
    let expected = match (cfg.setup_secret_hash, cfg.setup_secret.as_deref()) {
        (Some(hash), _) => hash,
        (None, Some(secret)) => sha256(secret.as_bytes()),
        (None, None) => return Err(Error::api(ErrorCode::Revoked)),
    };
    if !ct_eq(
        sha256(req.setup_secret.as_bytes()).as_slice(),
        expected.as_slice(),
    ) {
        return Err(Error::api(ErrorCode::Revoked));
    }
    if schema::meta_text(conn, schema::META_SETUP_CONSUMED)?.as_deref() == Some("1") {
        return Err(Error::api(ErrorCode::AlreadyInitialized));
    }

    let instance_id = schema::instance_id(conn)?;
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
        "INSERT INTO vault(singleton, vault_id, instance_id, log_id, log_incarnation, cursor_key,
                           state, active_epoch, next_epoch, manager_rev, control_rev, auth_rev,
                           head_seq, gc_floor, normal_limit, maintenance_limit, created_at)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, 'active', 1, 2, 1, 1, 1, 0, 0, ?6, ?7, ?8)",
        (
            vault_id.as_slice(),
            instance_id.as_slice(),
            log_id.as_slice(),
            log_incarnation.as_slice(),
            cursor_key.as_slice(),
            cfg.db.normal_limit,
            cfg.db.maintenance_limit,
            now,
        ),
    )?;
    conn.execute(
        "INSERT INTO epochs(epoch, state, created_at) VALUES (1, 'active', ?1)",
        [now],
    )?;
    conn.execute(
        "INSERT INTO devices(device_id, sign_pk, hpke_pk, certificate, certificate_signature,
                             status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6)",
        (
            req.device_id.as_slice(),
            req.sign_pk.as_slice(),
            req.hpke_pk.as_slice(),
            req.certificate.as_slice(),
            req.certificate_signature.as_slice(),
            now,
        ),
    )?;
    conn.execute(
        "INSERT INTO manager_keys(rev, pubkey, transition, created_at)
         VALUES (1, ?1, ?2, ?3)",
        (req.manager_pub.as_slice(), req.certificate.as_slice(), now),
    )?;
    // 控制链第一条：prev_hash = 32 字节全零，record_hash **仍按同一公式**
    // `SHA256(prev_hash || signed_record)` 计算（客户端/第三方只需实现一种哈希）。
    let zero = [0u8; 32];
    let genesis_hash = sha256_parts(&[zero.as_slice(), req.certificate.as_slice()]);
    conn.execute(
        "INSERT INTO control_commits(rev, prev_hash, record_hash, signed_record, created_at)
         VALUES (1, ?1, ?2, ?3, ?4)",
        (
            zero.as_slice(),
            genesis_hash.as_slice(),
            req.certificate.as_slice(),
            now,
        ),
    )?;
    conn.execute(
        "INSERT INTO usage(singleton, normal_bytes, maintenance_bytes) VALUES (1, 0, 0)",
        [],
    )?;
    schema::set_meta(conn, schema::META_SETUP_CONSUMED, b"1")?;
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
pub fn candidate_epoch(conn: &Connection) -> Result<Option<u64>> {
    let mut stmt = conn
        .prepare("SELECT epoch FROM epochs WHERE state = 'staged' ORDER BY epoch DESC LIMIT 1")?;
    let row = stmt.query_row([], |row| row.get::<_, i64>(0)).optional()?;
    match row {
        None => Ok(None),
        Some(value) => Ok(Some(u64_from_i64(value, "epochs.epoch")?)),
    }
}

/// epoch 状态。
/// §23.1：标记某个 epoch 在其冻结期间发生过吊销（永久事实，abort 不得清）。
pub fn mark_epoch_revoked_once(conn: &Connection, epoch: u64) -> Result<()> {
    conn.execute(
        "UPDATE epochs SET revoked_once = 1 WHERE epoch = ?1",
        [i64_from_u64(epoch, "epoch")?],
    )?;
    Ok(())
}

/// §23.1：该 epoch 是否曾在其冻结期间发生过吊销；无 epoch 行按"没有"处理。
pub fn epoch_revoked_once(conn: &Connection, epoch: u64) -> Result<bool> {
    let mut stmt = conn.prepare("SELECT revoked_once FROM epochs WHERE epoch = ?1")?;
    let row = stmt
        .query_row([i64_from_u64(epoch, "epoch")?], |row| row.get::<_, i64>(0))
        .optional()?;
    Ok(row.unwrap_or(0) != 0)
}

pub fn epoch_state(conn: &Connection, epoch: u64) -> Result<Option<EpochState>> {
    let mut stmt = conn.prepare("SELECT state FROM epochs WHERE epoch = ?1")?;
    let row = stmt
        .query_row([i64_from_u64(epoch, "epoch")?], |row| {
            row.get::<_, String>(0)
        })
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
pub fn set_epoch_state(conn: &Connection, epoch: u64, state: EpochState) -> Result<()> {
    conn.execute(
        "UPDATE epochs SET state = ?1 WHERE epoch = ?2",
        (state.as_str(), i64_from_u64(epoch, "epoch")?),
    )?;
    Ok(())
}

/// 加 `retire_at`（旧 epoch/旧槽 14 天保留期，ADR §8 步 7）。
pub fn set_epoch_retire_at(conn: &Connection, epoch: u64, at: i64) -> Result<()> {
    conn.execute(
        "UPDATE epochs SET retire_at = ?1 WHERE epoch = ?2",
        (at, i64_from_u64(epoch, "epoch")?),
    )?;
    Ok(())
}

/// 给 auth_rev 相关的错误补上 `current_epoch/control_rev`（ADR §5 响应字段）。
pub fn annotate(mut api: ApiError, conn: &Connection) -> ApiError {
    if let Ok(Some(vault)) = load_opt(conn) {
        api.current_epoch.get_or_insert(vault.active_epoch);
        api.current_control_rev.get_or_insert(vault.control_rev);
    }
    api
}
