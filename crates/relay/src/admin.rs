//! **运维创建票据 / 预留**（ADR 0014 §A / ADR 0017 S5a / ADR 0018 §2）：决定谁能创建
//! 新 vault 的一次性凭证。
//!
//! 发行入口（ADR 0018 S4b：唯一的 HTTP/SDK 发行入口是 Web 管理面）：
//! - [`create_reservation`]：在 writer 事务内生成 CSPRNG `vault_id`、registry `RESERVED`
//!   行与票据（`created_at/expires_at` 同事务生成、`quota=1`），**不建** `vaults` 行；
//!   Web 管理页「创建领取链接」与测试夹具走它。
//! - [`create_reservation_for`]：显式 `vault_id` 的预留（只允许全新 ID；同 ID 终态禁止复用）。
//!
//! 旧的运维兼容入口（`issue_create_ticket*` / `consume_create_ticket`）与
//! `POST /v2/admin/create-tickets` 已在 ADR 0018 S4b 一并移除；不再有 loopback setup
//! secret 发票路径，也不再接受"对已存在 ACTIVE vault 只发票据"的兼容分支。
//!
//! 安全语义（ADR 0014 §A + ADR 0018 §2）：
//! - 明文 = 32 字节 CSPRNG，**只返回一次**；库里只有 `SHA256(明文)`，日志/错误/DB 都不得
//!   出现明文；
//! - 票据绑定 `vault_id`（128-bit 公开路由标识，**不是**授权）、TTL 与 `quota`（新发放只 1）；
//! - 消费/claim 在**创建 vault 的同一个写事务**里做 CAS：未过期、未消费、额度未用完、
//!   `vault_id` 一致、registry 为 `RESERVED` 才成功；并发双消费只有第一个成功；创建失败
//!   整体回滚（票据不消费、不留半个 vault）；
//! - 所有票据失败（不存在/过期/已消费/额度用完/错 vault）统一映射成同一个 `403 FORBIDDEN`，
//!   不区分原因，也不泄露 vault 是否存在。

use std::fmt;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

use crate::db::Db;
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{random_bytes, random_id16, sha256, Hash32, Id16};
use crate::registry;
use crate::scope::ScopeConn;

/// 票据明文长度（32 字节 CSPRNG）。
pub const CREATE_TICKET_BYTES: usize = 32;

/// 默认 TTL：15 分钟（与邀请链接的默认 TTL 一致，ADR 0014 §B / ADR 0017 S5a）。
pub const DEFAULT_CREATE_TICKET_TTL_SECS: i64 = 15 * 60;

/// TTL 上限：24 小时（运维忘清理也只是有限窗口；票据不是长期密码）。
pub const MAX_CREATE_TICKET_TTL_SECS: i64 = 24 * 60 * 60;

/// 新发放票据的唯一额度（ADR 0018 §2：quota 只 0/1）。
pub const CREATE_TICKET_QUOTA: u32 = 1;

/// 一次预留/票据发行的完整结果：**明文 `token` 只活在这里/调用方内存**。
///
/// `created_at`/`expires_at`/`quota` 直接来自签发事务写入的值（不重算）。
pub struct IssuedCreateTicket {
    /// 32B 明文票据（只返回一次；不落库/日志）。
    pub token: [u8; CREATE_TICKET_BYTES],
    /// 票据绑定的 vault_id（`create_reservation` 的 CSPRNG 值）。
    pub vault_id: Id16,
    /// 票据/预留的创建时刻（Unix 秒，写事务内采样）。
    pub created_at: i64,
    /// 过期时刻（Unix 秒，写事务内由 `created_at + ttl` 得出）。
    pub expires_at: i64,
    /// 允许的创建次数（新发放固定 1）。
    pub quota: u32,
}

impl fmt::Debug for IssuedCreateTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedCreateTicket")
            .field("token", &"<redacted: one-time create ticket>")
            .field("vault_id", &crate::ids::hex_lower(&self.vault_id))
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .field("quota", &self.quota)
            .finish()
    }
}

/// `create_tickets` 行的运维视图（诊断/测试；不含明文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTicketRow {
    /// `SHA256(明文)`。
    pub token_hash: Hash32,
    /// 票据绑定的 vault_id。
    pub vault_id: Id16,
    /// 创建时刻（迁移出来的旧票据 = 迁移时刻，不是原始创建时间）。
    pub created_at: i64,
    /// 过期时刻（Unix 秒）。
    pub expires_at: i64,
    /// 剩余创建次数（0 = 已用完/作废）。
    pub quota: i64,
    /// 消费/作废时刻；`None` = 仍可用。
    pub consumed_at: Option<i64>,
}

/// 统一票据失败：**不区分**不存在/过期/已消费/额度/错 vault（ADR 0014 §A 不泄露存在性）。
pub(crate) fn ticket_rejected() -> Error {
    Error::Api(ApiError::new(ErrorCode::Forbidden).retryable(false))
}

fn invalid() -> Error {
    Error::api(ErrorCode::InvalidRequest)
}

fn id16_from_blob(bytes: Vec<u8>) -> Result<Id16> {
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Invariant("create_tickets.vault_id"))
}

/// 校验 TTL 并把 `Duration` 收成秒。
fn ttl_secs(ttl: Duration) -> Result<i64> {
    let secs = i64::try_from(ttl.as_secs()).map_err(|_| invalid())?;
    if secs <= 0 || secs > MAX_CREATE_TICKET_TTL_SECS {
        return Err(invalid());
    }
    Ok(secs)
}

/// 写事务内的一次票据发行（registry + 票据两行同事务；调用方负责事务边界）。
struct NewTicket {
    token: [u8; CREATE_TICKET_BYTES],
    created_at: i64,
    expires_at: i64,
}

/// 在**调用方事务**里发行票据（ADR 0018 §2：`created_at/expires_at` 在事务内生成）。
///
/// - registry 无行 → 新建 `RESERVED` 预留（**不建** `vaults` 行）；
/// - registry `ACTIVE`/`RESERVED`/终态 → `CasConflict`（同 vault 重复票拒绝；终态 ID
///   禁止复用；`ACTIVE` 不能再用新票据"探测"或覆盖）。
fn issue_ticket_in_tx(
    conn: &Connection,
    vault_id: Id16,
    ttl_secs: i64,
    now: i64,
) -> Result<NewTicket> {
    match registry::state_raw(conn, &vault_id)? {
        None => {
            if registry::vault_row_exists(conn, &vault_id)? {
                // `vaults` 行存在却没有 registry 行 = 损坏（ACTIVE 必须对应 registry）：
                // 不在这里"顺手修复"，fail closed 让运维处理。
                return Err(Error::Invariant("vaults row without vault_registry row"));
            }
            conn.execute(
                "INSERT INTO vault_registry(vault_id, state, created_at)
                 VALUES (?1, 'RESERVED', ?2)",
                (vault_id.as_slice(), now),
            )?;
        }
        Some(_) => {
            // 同一 vault 的重复预留 / 终态 ID 复用：明确拒绝（不是幂等重发）。
            return Err(Error::api(ErrorCode::CasConflict));
        }
    }
    let token = random_bytes::<CREATE_TICKET_BYTES>()?;
    let token_hash = sha256(&token);
    let expires_at = now
        .checked_add(ttl_secs)
        .ok_or_else(|| Error::Invariant("create ticket ttl overflow"))?;
    conn.execute(
        "INSERT INTO create_tickets(token_hash, vault_id, created_at, expires_at, quota, consumed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
        (
            token_hash.as_slice(),
            vault_id.as_slice(),
            now,
            expires_at,
            i64::from(CREATE_TICKET_QUOTA),
        ),
    )?;
    Ok(NewTicket {
        token,
        created_at: now,
        expires_at,
    })
}

/// 新语义预留（ADR 0018 §2）：`vault_id` 由写事务内的 CSPRNG 生成，registry `RESERVED`
/// + 票据同事务落库；`created_at/expires_at` 也在事务内生成（不再用进队列前的采样点）。
///
/// **不建** `vaults` 行；预留只有在 `/v2/vaults` claim 成功后才变成 `ACTIVE`。
pub fn create_reservation(db: &Db, ttl: Duration) -> Result<IssuedCreateTicket> {
    let ttl_secs = ttl_secs(ttl)?;
    let clock = db.clock_handle();
    db.write_instance_blocking(move |conn| {
        let now = clock.now_secs();
        for _ in 0..8 {
            let vault_id = random_id16()?;
            if registry::id_taken(conn, &vault_id)? || registry::vault_row_exists(conn, &vault_id)?
            {
                continue;
            }
            let ticket = issue_ticket_in_tx(conn, vault_id, ttl_secs, now)?;
            return Ok(IssuedCreateTicket {
                token: ticket.token,
                vault_id,
                created_at: ticket.created_at,
                expires_at: ticket.expires_at,
                quota: CREATE_TICKET_QUOTA,
            });
        }
        Err(Error::Invariant("reservation vault_id collision"))
    })
}

/// 显式为某个**全新** `vault_id` 建预留（ADR 0018 §2）。
///
/// 该 ID 已存在（`RESERVED`/`ACTIVE`/任何终态）→ `CasConflict`：同 vault 重复票拒绝，
/// 终态 ID 禁止复用（需要重发就先 `cancel_reservation` 换新 ID）。
pub fn create_reservation_for(
    db: &Db,
    vault_id: Id16,
    ttl: Duration,
) -> Result<IssuedCreateTicket> {
    let ttl_secs = ttl_secs(ttl)?;
    let clock = db.clock_handle();
    db.write_instance_blocking(move |conn| {
        let now = clock.now_secs();
        let ticket = issue_ticket_in_tx(conn, vault_id, ttl_secs, now)?;
        Ok(IssuedCreateTicket {
            token: ticket.token,
            vault_id,
            created_at: ticket.created_at,
            expires_at: ticket.expires_at,
            quota: CREATE_TICKET_QUOTA,
        })
    })
}

/// 读票据行（运维/测试诊断；按明文哈希查，不返回明文）。
pub fn load_create_ticket(db: &Db, token: &[u8]) -> Result<Option<CreateTicketRow>> {
    if token.len() != CREATE_TICKET_BYTES {
        return Ok(None);
    }
    let token_hash = sha256(token);
    db.read_instance(move |conn| {
        let row = conn
            .query_row(
                "SELECT vault_id, created_at, expires_at, quota, consumed_at FROM create_tickets
                 WHERE token_hash = ?1",
                [token_hash.as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((vault_id, created_at, expires_at, quota, consumed_at)) = row else {
            return Ok(None);
        };
        Ok(Some(CreateTicketRow {
            token_hash,
            vault_id: id16_from_blob(vault_id)?,
            created_at,
            expires_at,
            quota,
            consumed_at,
        }))
    })
}

/// **只读**校验一张票据（claim 的第一步；ADR 0018 §2"票据先验"）。
///
/// 通过 = 未过期、未消费、额度 > 0、绑定 `scope.vault_id()`。任何失败都映射成统一的
/// `403 FORBIDDEN`（不区分原因、不泄露 vault 是否存在）；校验本身不写库。
pub fn validate_create_ticket(conn: &ScopeConn, now: i64, token: &[u8]) -> Result<CreateTicketRow> {
    if token.len() != CREATE_TICKET_BYTES {
        return Err(ticket_rejected());
    }
    let token_hash = sha256(token);
    let row = conn
        .raw()
        .query_row(
            "SELECT vault_id, created_at, expires_at, quota, consumed_at FROM create_tickets
             WHERE token_hash = ?1",
            [token_hash.as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((vault_id, created_at, expires_at, quota, consumed_at)) = row else {
        return Err(ticket_rejected());
    };
    // 错 vault：与"票据不存在"同一个错误（不泄露票据/租户关系）。
    if vault_id.as_slice() != conn.scope().as_slice() {
        return Err(ticket_rejected());
    }
    if consumed_at.is_some() || expires_at <= now || quota <= 0 {
        return Err(ticket_rejected());
    }
    Ok(CreateTicketRow {
        token_hash,
        vault_id: id16_from_blob(vault_id)?,
        created_at,
        expires_at,
        quota,
        consumed_at,
    })
}

/// claim 时的票据消费：**一次用完**（`quota=0` + `consumed_at=now`），CAS 条件与只读
/// 校验一致（同一写事务里再验一次，防 TOCTOU）。
///
pub fn claim_create_ticket(conn: &ScopeConn, now: i64, token: &[u8]) -> Result<()> {
    let row = validate_create_ticket(conn, now, token)?;
    let changed = conn.raw().execute(
        "UPDATE create_tickets
            SET quota = 0, consumed_at = ?1
          WHERE token_hash = ?2 AND vault_id = ?3 AND consumed_at IS NULL
            AND quota > 0 AND expires_at > ?1",
        (now, row.token_hash.as_slice(), conn.scope().as_slice()),
    )?;
    if changed != 1 {
        return Err(ticket_rejected());
    }
    Ok(())
}

/// claim 成功后作废同 vault 的**兄弟票据**（同事务；ADR 0018 §2）。
///
/// `keep` 是被消费的那张票据的 hash；其它未消费票据一律 `quota=0` + `consumed_at=now`
/// （已过期/未消费都作废；已消费的行按事实保留）。
pub fn invalidate_sibling_tickets(conn: &ScopeConn, now: i64, keep: &Hash32) -> Result<u64> {
    let changed = conn.raw().execute(
        "UPDATE create_tickets SET quota = 0, consumed_at = ?1
          WHERE vault_id = ?2 AND token_hash <> ?3 AND consumed_at IS NULL",
        (now, conn.scope().as_slice(), keep.as_slice()),
    )?;
    Ok(u64::try_from(changed).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;
    use crate::registry::{self, CancelOutcome, LifecycleState};
    use crate::scope::VaultScope;
    use crate::time::FakeClock;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn target_tmp() -> PathBuf {
        option_env!("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("..")
                    .join("..")
                    .join("target")
                    .join("tmp")
            })
            .join("relay-unit")
            .join("admin")
    }

    fn test_db_with_clock(label: &str) -> (Db, Arc<FakeClock>) {
        let dir = target_tmp();
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(format!("{label}-{}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        let clock = Arc::new(FakeClock::new(1_000));
        let db = Db::open(DbConfig::new(path), clock.clone()).expect("open");
        (db, clock)
    }

    fn test_db(label: &str) -> Db {
        test_db_with_clock(label).0
    }

    fn state(db: &Db, vault_id: Id16) -> Option<LifecycleState> {
        db.read_instance(move |conn| registry::state_raw(conn, &vault_id))
            .expect("state")
    }

    #[test]
    fn reservation_is_random_and_writes_registry_plus_ticket_only() {
        let db = test_db("reserve");
        // 预留：随机 vault_id + registry RESERVED + 票据（created_at/expires_at 同事务）。
        let first = create_reservation(
            &db,
            Duration::from_secs(
                u64::try_from(DEFAULT_CREATE_TICKET_TTL_SECS).expect("default ttl"),
            ),
        )
        .expect("reserve");
        assert_eq!(first.token.len(), 32);
        assert_ne!(first.vault_id, [0u8; 16]);
        assert_eq!(first.created_at, 1_000);
        assert_eq!(first.expires_at, 1_000 + DEFAULT_CREATE_TICKET_TTL_SECS);
        assert_eq!(first.quota, 1);
        let row = load_create_ticket(&db, &first.token)
            .expect("load")
            .expect("row");
        assert_eq!(row.token_hash, sha256(&first.token));
        assert_eq!(row.vault_id, first.vault_id);
        assert_eq!(row.created_at, 1_000);
        assert_eq!(row.expires_at, first.expires_at);
        assert_eq!(row.quota, 1);
        assert_eq!(row.consumed_at, None);
        // registry：RESERVED；**不建** vaults 行。
        let registry_row = db
            .read_instance(|conn| registry::load_raw(conn, &first.vault_id))
            .expect("registry")
            .expect("registry row");
        assert_eq!(registry_row.state, LifecycleState::Reserved);
        assert_eq!(registry_row.created_at, 1_000);
        assert_eq!(state(&db, first.vault_id), Some(LifecycleState::Reserved));
        let vaults: i64 = db
            .read_instance(|conn| {
                Ok(conn.query_row("SELECT COUNT(*) FROM vaults", [], |row| row.get(0))?)
            })
            .expect("vaults count");
        assert_eq!(vaults, 0, "预留不得创建 vaults 行");
        // 明文不得出现在库里：只存 SHA256。
        let plaintext_rows: i64 = db
            .read_instance(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM create_tickets WHERE token_hash = ?1",
                    [first.token.as_slice()],
                    |row| row.get(0),
                )?)
            })
            .expect("plaintext probe");
        assert_eq!(plaintext_rows, 0, "库里只能有 SHA256(明文)");

        // 第二次预留：CSPRNG 产生不同 vault_id/token，互不覆盖。
        let second = create_reservation(&db, Duration::from_secs(60)).expect("reserve");
        assert_ne!(second.vault_id, first.vault_id);
        assert_ne!(second.token, first.token);
        assert_eq!(second.created_at, 1_000);
        assert_eq!(second.expires_at, 1_060);
        assert_eq!(
            state(&db, second.vault_id),
            Some(LifecycleState::Reserved),
            "每个预留各自一行 registry"
        );
        db.close();
    }

    #[test]
    fn duplicate_and_terminal_ids_are_rejected_and_params_validated() {
        let db = test_db("reserve-dup");
        let vault_id = [0x51u8; 16];
        let first =
            create_reservation_for(&db, vault_id, Duration::from_secs(900)).expect("reserve");
        assert_eq!(first.vault_id, vault_id);
        assert_eq!(
            state(&db, vault_id),
            Some(LifecycleState::Reserved),
            "显式 id 的预留同样落 RESERVED"
        );
        // 同 vault 重复票 → 拒绝（不是幂等重发）。
        let duplicate = create_reservation_for(&db, vault_id, Duration::from_secs(900))
            .expect_err("duplicate reservation");
        assert_eq!(duplicate.code(), Some(ErrorCode::CasConflict));
        assert_eq!(
            create_reservation_for(&db, vault_id, Duration::from_secs(900))
                .err()
                .and_then(|err| err.code()),
            Some(ErrorCode::CasConflict),
            "旧 API 的同 vault 重复发票同样拒绝"
        );
        // 取消后终态 ID 禁止复用。
        assert_eq!(
            registry::cancel_reservation(&db, vault_id).expect("cancel"),
            CancelOutcome::Cancelled {
                vault_id,
                cancelled_at: 1_000,
                invalidated_tickets: 1,
            }
        );
        assert_eq!(
            state(&db, vault_id),
            Some(LifecycleState::Cancelled),
            "取消后 registry 落 CANCELLED"
        );
        assert_eq!(
            create_reservation_for(&db, vault_id, Duration::from_secs(900))
                .expect_err("reuse terminal id")
                .code(),
            Some(ErrorCode::CasConflict)
        );
        // 重复取消幂等。
        assert_eq!(
            registry::cancel_reservation(&db, vault_id).expect("cancel again"),
            CancelOutcome::AlreadyCancelled
        );

        // TTL / quota 参数校验（新发放固定 quota=1）。
        assert!(create_reservation(&db, Duration::from_secs(0)).is_err());
        assert!(create_reservation(
            &db,
            Duration::from_secs(u64::try_from(MAX_CREATE_TICKET_TTL_SECS + 1).expect("ttl"))
        )
        .is_err());
        db.close();
    }

    #[test]
    fn claim_consumes_once_and_cancel_expiry_is_observed_not_faked() {
        let (db, clock) = test_db_with_clock("claim");
        let vault_id = [0x71u8; 16];
        let issued =
            create_reservation_for(&db, vault_id, Duration::from_secs(60)).expect("reserve");
        // claim 用一次 CAS 用完票据。
        {
            let scope = VaultScope::new(vault_id);
            let token = issued.token.to_vec();
            db.write_blocking(scope, move |conn| claim_create_ticket(conn, 1_000, &token))
                .expect("claim ticket");
        }
        let row = load_create_ticket(&db, &issued.token)
            .expect("load")
            .expect("row");
        assert_eq!(row.quota, 0);
        assert_eq!(row.consumed_at, Some(1_000));
        // 再次 claim → 403（不会重复消费）。
        {
            let scope = VaultScope::new(vault_id);
            let token = issued.token.to_vec();
            let err = db
                .write_blocking(scope, move |conn| claim_create_ticket(conn, 1_000, &token))
                .expect_err("repeat claim");
            assert_eq!(err.code(), Some(ErrorCode::Forbidden));
        }
        // 已过期预留：cancel 不伪造"取消成功"，而是显式落 EXPIRED 并作废票据。
        let vault_expired = [0x72u8; 16];
        let expired =
            create_reservation_for(&db, vault_expired, Duration::from_secs(60)).expect("reserve");
        clock.set(1_100);
        assert_eq!(
            registry::cancel_reservation(&db, vault_expired).expect("cancel"),
            CancelOutcome::Expired
        );
        assert_eq!(
            state(&db, vault_expired),
            Some(LifecycleState::Expired),
            "过期预留必须显式标 EXPIRED"
        );
        assert_eq!(
            load_create_ticket(&db, &expired.token)
                .expect("load")
                .expect("row")
                .consumed_at,
            Some(1_100),
            "过期票据在取消路径被作废"
        );
        db.close();
    }
}
