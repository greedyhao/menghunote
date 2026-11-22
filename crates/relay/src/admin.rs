//! **运维创建票据**（ADR 0014 §A）：决定谁能创建新 vault 的一次性凭证。
//!
//! 这不是公开 HTTP 接口：票据只能由本机运维入口（`relay::admin::issue_create_ticket` /
//! `dev_server --issue-ticket`）发行，`POST /v2/vaults` 只负责**消费**。
//!
//! 安全语义（与 ADR 0014 §A 对齐）：
//! - 明文 = 32 字节 CSPRNG，**只返回一次**；库里只有 `SHA256(明文)`，日志/错误/DB 都不得
//!   出现明文；
//! - 票据绑定 `vault_id`（128-bit 公开路由标识，**不是**授权）、TTL 与可消费次数 `quota`；
//! - 消费在**创建 vault 的同一个写事务**里做 CAS：未过期、未消费、额度未用完、`vault_id`
//!   一致才扣额度；并发双消费只有第一个成功；创建失败整体回滚（票据不消费、不留半个 vault）；
//! - 所有票据失败（不存在/过期/已消费/额度用完/错 vault）统一映射成同一个 `403 FORBIDDEN`，
//!   不区分原因，也不泄露 vault 是否存在。

use std::time::Duration;

use rusqlite::OptionalExtension;

use crate::db::Db;
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{random_bytes, sha256, Hash32, Id16};
use crate::scope::ScopeConn;

/// 票据明文长度（32 字节 CSPRNG）。
pub const CREATE_TICKET_BYTES: usize = 32;

/// 默认 TTL：15 分钟（与邀请链接的默认 TTL 一致，ADR 0014 §B）。
pub const DEFAULT_CREATE_TICKET_TTL_SECS: i64 = 15 * 60;

/// TTL 上限：24 小时（运维忘清理也只是有限窗口；票据不是长期密码）。
pub const MAX_CREATE_TICKET_TTL_SECS: i64 = 24 * 60 * 60;

/// 单张票据允许的创建次数上限（一次性票据为 1；上限只是防止误发"无限票据"）。
pub const MAX_CREATE_TICKET_QUOTA: u32 = 16;

/// `create_tickets` 行的运维视图（诊断/测试；不含明文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTicketRow {
    /// `SHA256(明文)`。
    pub token_hash: Hash32,
    /// 票据绑定的 vault_id。
    pub vault_id: Id16,
    /// 过期时刻（Unix 秒）。
    pub expires_at: i64,
    /// 剩余创建次数。
    pub quota: i64,
    /// 额度归零（一次性票据 = 首次成功消费）的时刻；`None` = 仍可用。
    pub consumed_at: Option<i64>,
}

/// 统一票据失败：**不区分**不存在/过期/已消费/额度/错 vault（ADR 0014 §A 不泄露存在性）。
fn ticket_rejected() -> Error {
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

/// 运维发行一张创建票据：随机 32B → 只落 `SHA256`，返回**唯一一次**明文。
///
/// - `ttl` 必须在 `(0, MAX_CREATE_TICKET_TTL_SECS]`；
/// - `quota` 必须在 `[1, MAX_CREATE_TICKET_QUOTA]`；
/// - 使用 `Db` 的时钟（生产 `SystemClock`，测试 `FakeClock`），因此过期判定可精确推进；
/// - 不在同一 `vault_id` 上做唯一性限制：同一 vault 的重复票据会在创建时被
///   `ALREADY_INITIALIZED` 拒绝（票据随后回滚、不消费）。
pub fn issue_create_ticket(
    db: &Db,
    vault_id: Id16,
    ttl: Duration,
    quota: u32,
) -> Result<[u8; CREATE_TICKET_BYTES]> {
    let ttl_secs = i64::try_from(ttl.as_secs()).map_err(|_| invalid())?;
    if ttl_secs <= 0 || ttl_secs > MAX_CREATE_TICKET_TTL_SECS {
        return Err(invalid());
    }
    if quota == 0 || quota > MAX_CREATE_TICKET_QUOTA {
        return Err(invalid());
    }
    let token = random_bytes::<CREATE_TICKET_BYTES>()?;
    let token_hash = sha256(&token);
    let now = db.clock().now_secs();
    let expires_at = now
        .checked_add(ttl_secs)
        .ok_or_else(|| Error::Invariant("create ticket ttl overflow"))?;
    let quota = i64::from(quota);
    db.write_instance_blocking(move |conn| {
        conn.execute(
            "INSERT INTO create_tickets(token_hash, vault_id, expires_at, quota, consumed_at)
             VALUES (?1, ?2, ?3, ?4, NULL)",
            (
                token_hash.as_slice(),
                vault_id.as_slice(),
                expires_at,
                quota,
            ),
        )?;
        Ok(())
    })?;
    Ok(token)
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
                "SELECT vault_id, expires_at, quota, consumed_at FROM create_tickets
                 WHERE token_hash = ?1",
                [token_hash.as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((vault_id, expires_at, quota, consumed_at)) = row else {
            return Ok(None);
        };
        Ok(Some(CreateTicketRow {
            token_hash,
            vault_id: id16_from_blob(vault_id)?,
            expires_at,
            quota,
            consumed_at,
        }))
    })
}

/// 在**调用方的写事务**里消费一次票据额度。
///
/// 由 `POST /v2/vaults` 在 `db.write(scope, ...)` 内**先**调用，再调用
/// [`crate::vault::create_vault_with_ticket`]；两者同事务：创建失败会整体回滚，
/// 因此票据不会被"白消费"。
///
/// `create_tickets` 是全局表（票据发行时目标 vault 还不存在），所以这里显式用
/// [`ScopeConn::raw`] 访问；**绑定关系**由 `vault_id = scope.vault_id()` 这一条硬校验保证。
pub fn consume_create_ticket(conn: &ScopeConn, now: i64, token: &[u8]) -> Result<()> {
    if token.len() != CREATE_TICKET_BYTES {
        return Err(ticket_rejected());
    }
    let token_hash = sha256(token);
    let row = conn
        .raw()
        .query_row(
            "SELECT vault_id, expires_at, quota, consumed_at FROM create_tickets
             WHERE token_hash = ?1",
            [token_hash.as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((vault_id, expires_at, quota, consumed_at)) = row else {
        return Err(ticket_rejected());
    };
    // 错 vault：与"票据不存在"同一个错误（不泄露票据/租户关系）。
    if vault_id.as_slice() != conn.scope().as_slice() {
        return Err(ticket_rejected());
    }
    if consumed_at.is_some() || expires_at <= now || quota <= 0 {
        return Err(ticket_rejected());
    }
    // CAS：同一事务里扣一次额度；`consumed_at` 只在额度归零时写入（一次性票据=首次消费）。
    // 并发/重放只有第一个能把 `changed` 拿到 1（writer 串行 + 条件更新）。
    let changed = conn.raw().execute(
        "UPDATE create_tickets
            SET quota = quota - 1,
                consumed_at = CASE WHEN quota - 1 = 0 THEN ?1 ELSE consumed_at END
          WHERE token_hash = ?2 AND vault_id = ?3 AND consumed_at IS NULL
            AND quota > 0 AND expires_at > ?1",
        (now, token_hash.as_slice(), conn.scope().as_slice()),
    )?;
    if changed != 1 {
        return Err(ticket_rejected());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;
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

    fn test_db(label: &str) -> Db {
        let dir = target_tmp();
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(format!("{label}-{}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        Db::open(DbConfig::new(path), Arc::new(FakeClock::new(1_000))).expect("open")
    }

    fn consume(db: &Db, vault_id: Id16, token: &[u8], now: i64) -> Result<()> {
        let scope = VaultScope::new(vault_id);
        let token = token.to_vec();
        db.write_blocking(scope, move |conn| consume_create_ticket(conn, now, &token))
    }

    #[test]
    fn issue_stores_only_the_hash_and_validates_parameters() {
        let db = test_db("issue");
        let vault_id = [0x51u8; 16];
        let token = issue_create_ticket(
            &db,
            vault_id,
            Duration::from_secs(
                u64::try_from(DEFAULT_CREATE_TICKET_TTL_SECS).expect("default ttl"),
            ),
            1,
        )
        .expect("issue");
        assert_eq!(token.len(), 32);
        let row = load_create_ticket(&db, &token).expect("load").expect("row");
        assert_eq!(row.token_hash, sha256(&token));
        assert_eq!(row.vault_id, vault_id);
        assert_eq!(row.expires_at, 1_000 + DEFAULT_CREATE_TICKET_TTL_SECS);
        assert_eq!(row.quota, 1);
        assert_eq!(row.consumed_at, None);
        // 明文不得出现在库里（任何一列/任何表）：只在返回值和调用方内存中存在。
        let plaintext_in_db: i64 = db
            .read_instance(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM create_tickets WHERE token_hash = ?1",
                    [token.as_slice()],
                    |row| row.get(0),
                )?)
            })
            .expect("query");
        assert_eq!(plaintext_in_db, 0, "库里只能有 SHA256(明文)");

        // 重复发行是两张不同票据（CSPRNG）。
        let second = issue_create_ticket(&db, vault_id, Duration::from_secs(60), 2).expect("issue");
        assert_ne!(second, token);

        // 参数校验。
        assert!(issue_create_ticket(&db, vault_id, Duration::from_secs(0), 1).is_err());
        assert!(issue_create_ticket(&db, vault_id, Duration::from_secs(1), 0).is_err());
        assert!(issue_create_ticket(
            &db,
            vault_id,
            Duration::from_secs(u64::try_from(MAX_CREATE_TICKET_TTL_SECS + 1).expect("ttl")),
            1
        )
        .is_err());
        assert!(issue_create_ticket(
            &db,
            vault_id,
            Duration::from_secs(60),
            MAX_CREATE_TICKET_QUOTA + 1
        )
        .is_err());
        db.close();
    }

    #[test]
    fn consume_is_single_use_expiring_and_bound_to_one_vault() {
        let db = test_db("consume");
        let vault_a = [0x61u8; 16];
        let vault_b = [0x62u8; 16];
        let token = issue_create_ticket(&db, vault_a, Duration::from_secs(60), 1).expect("issue");

        // 错 vault → 403，且不消费。
        let wrong_vault = consume(&db, vault_b, &token, 1_000).expect_err("wrong vault");
        assert_eq!(wrong_vault.code(), Some(ErrorCode::Forbidden));
        assert_eq!(
            load_create_ticket(&db, &token)
                .expect("load")
                .expect("row")
                .consumed_at,
            None
        );

        // 正确 vault → 成功，`quota` 归零且写 `consumed_at`。
        consume(&db, vault_a, &token, 1_000).expect("consume");
        let row = load_create_ticket(&db, &token).expect("load").expect("row");
        assert_eq!(row.quota, 0);
        assert_eq!(row.consumed_at, Some(1_000));

        // 重复消费 → 403。
        let again = consume(&db, vault_a, &token, 1_000).expect_err("repeat");
        assert_eq!(again.code(), Some(ErrorCode::Forbidden));

        // 过期 → 403（新票据，时钟推进 61s）。
        let expiring =
            issue_create_ticket(&db, vault_a, Duration::from_secs(60), 1).expect("issue");
        let expired = consume(&db, vault_a, &expiring, 1_061).expect_err("expired");
        assert_eq!(expired.code(), Some(ErrorCode::Forbidden));
        assert_eq!(
            load_create_ticket(&db, &expiring)
                .expect("load")
                .expect("row")
                .consumed_at,
            None,
            "过期拒绝不得消费票据"
        );

        // 未知票据（含错误长度）→ 同一个 403。
        assert_eq!(
            consume(&db, vault_a, &[0u8; 32], 1_000)
                .expect_err("unknown")
                .code(),
            Some(ErrorCode::Forbidden)
        );
        assert_eq!(
            consume(&db, vault_a, &[0u8; 31], 1_000)
                .expect_err("bad length")
                .code(),
            Some(ErrorCode::Forbidden)
        );
        db.close();
    }

    #[test]
    fn quota_greater_than_one_needs_repeated_consumption() {
        let db = test_db("quota");
        let vault = [0x71u8; 16];
        let token = issue_create_ticket(&db, vault, Duration::from_secs(600), 2).expect("issue");
        consume(&db, vault, &token, 1_000).expect("first");
        let row = load_create_ticket(&db, &token).expect("load").expect("row");
        assert_eq!(row.quota, 1);
        assert_eq!(row.consumed_at, None, "额度未归零不写 consumed_at");
        consume(&db, vault, &token, 1_000).expect("second");
        let row = load_create_ticket(&db, &token).expect("load").expect("row");
        assert_eq!(row.quota, 0);
        assert_eq!(row.consumed_at, Some(1_000));
        assert_eq!(
            consume(&db, vault, &token, 1_000)
                .expect_err("exhausted")
                .code(),
            Some(ErrorCode::Forbidden)
        );
        db.close();
    }
}
