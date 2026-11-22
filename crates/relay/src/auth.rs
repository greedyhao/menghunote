//! 设备鉴权基础设施（ADR 0006 §4）。
//!
//! - **challenge**：`{realm, subject, nonce, expires_at}`，60 秒、**单用**；
//!   验证/消费 challenge + 建 token 在**一个事务**内完成（并发重放只有一个能成功）。
//! - **token**：32 字节 CSPRNG → base64url（43 字符），600 秒；DB 只存 `SHA256(raw)`，
//!   不存明文 token，也不做 JWT/永久 refresh。
//! - 每请求检查 `device.status / token.auth_rev / vault.state`：**吊销 commit 起旧 token 立即失效**
//!   （吊销同时推进 `vault.auth_rev`，所有旧 token 一起作废）。
//! - 管理写还要**额外**管理签名（`require_management`），管理 token ≠ 管理签名。

use rusqlite::{Connection, OptionalExtension};

use crate::db::schema;
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{b64url_decode, b64url_encode, random_bytes, sha256, Hash32, Id16};
use crate::sig::{require, Expectation, SignDomain, SignatureCheck, SignatureHook};
use crate::time::{secs, Clock};
use crate::vault;

/// 认证 realm。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Realm {
    /// 普通设备（读写数据面）。
    Device,
    /// 管理凭据（成员/槽/轮换等控制面）。
    Management,
}

impl Realm {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Device => "device",
            Self::Management => "management",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "device" => Some(Self::Device),
            "management" => Some(Self::Management),
            _ => None,
        }
    }

    /// 管理 realm 的 subject：当前管理公钥摘要前 16 字节（客户端不需要自己算，服务端校验）。
    pub fn management_subject(manager_pub: &[u8; 32]) -> Id16 {
        let digest = sha256(manager_pub);
        let mut subject = [0u8; 16];
        subject.copy_from_slice(&digest[..16]);
        subject
    }
}

/// 一条 challenge。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub challenge_id: Id16,
    pub realm: Realm,
    pub subject: Id16,
    pub nonce: [u8; 32],
    pub expires_at: i64,
}

/// 已签发的 token。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenOutcome {
    /// base64url(32 字节随机)；**只在响应里出现一次**，DB 只有它的 SHA256。
    pub token: String,
    pub expires_at: i64,
    pub server_time: i64,
    pub realm: Realm,
    pub auth_rev: u64,
}

/// 通过鉴权的请求上下文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthContext {
    pub realm: Realm,
    pub subject: Id16,
    /// 设备 realm 的 device_id（管理 realm 为 None）。
    pub device_id: Option<Id16>,
    pub auth_rev: u64,
}

impl AuthContext {
    /// 要求设备 realm 并返回 device_id。
    pub fn device_id(&self) -> Result<Id16> {
        match (self.realm, self.device_id) {
            (Realm::Device, Some(id)) => Ok(id),
            _ => Err(Error::api(ErrorCode::ManagementRequired)),
        }
    }

    /// 要求管理 realm。
    pub fn require_management(&self) -> Result<()> {
        if self.realm == Realm::Management {
            Ok(())
        } else {
            Err(Error::api(ErrorCode::ManagementRequired))
        }
    }
}

fn realm_from_str(value: &str) -> Result<Realm> {
    Realm::parse(value).ok_or(Error::Invariant("tokens/challenges realm"))
}

/// 创建 challenge（60s 单用）。
pub fn create_challenge(
    conn: &Connection,
    clock: &dyn Clock,
    realm: Realm,
    subject: &Id16,
) -> Result<Challenge> {
    let now = clock.now_secs();
    let vault = vault::load(conn)?;

    match realm {
        Realm::Device => {
            // 未注册/已吊销一律 403 REVOKED（不区分，避免枚举设备）。
            let status: Option<String> = conn
                .query_row(
                    "SELECT status FROM devices WHERE device_id = ?1",
                    [subject.as_slice()],
                    |row| row.get(0),
                )
                .optional()?;
            if status.as_deref() != Some("active") {
                return Err(Error::Api(
                    ApiError::new(ErrorCode::Revoked).epoch(vault.active_epoch),
                ));
            }
        }
        Realm::Management => {
            let Some(manager_pub) = vault::manager_pubkey(conn)? else {
                return Err(Error::api(ErrorCode::ManagementRequired));
            };
            let expected = Realm::management_subject(&manager_pub);
            if &expected != subject {
                return Err(Error::api(ErrorCode::ManagementRequired));
            }
        }
    }

    let challenge = Challenge {
        challenge_id: crate::ids::random_id16()?,
        realm,
        subject: *subject,
        nonce: random_bytes::<32>()?,
        expires_at: now + secs::CHALLENGE_TTL,
    };
    // 顺手清理过期 challenge（只是诊断卫生，不影响语义）。
    conn.execute(
        "DELETE FROM challenges WHERE expires_at < ?1 - 86400",
        [now],
    )?;
    conn.execute(
        "INSERT INTO challenges(challenge_id, realm, subject, nonce, expires_at, used_at)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
        (
            challenge.challenge_id.as_slice(),
            realm.as_str(),
            subject.as_slice(),
            challenge.nonce.as_slice(),
            challenge.expires_at,
        ),
    )?;
    Ok(challenge)
}

/// 读 challenge（诊断/测试用）。
pub fn load_challenge(conn: &Connection, id: &Id16) -> Result<Option<Challenge>> {
    let mut stmt = conn.prepare(
        "SELECT challenge_id, realm, subject, nonce, expires_at, used_at
         FROM challenges WHERE challenge_id = ?1",
    )?;
    let row = stmt
        .query_row([id.as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })
        .optional()?;
    let Some((id_bytes, realm, subject, nonce, expires_at, _used)) = row else {
        return Ok(None);
    };
    let nonce: [u8; 32] = nonce
        .as_slice()
        .try_into()
        .map_err(|_| Error::Invariant("challenge nonce"))?;
    Ok(Some(Challenge {
        challenge_id: id_bytes
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("challenge_id"))?,
        realm: realm_from_str(&realm)?,
        subject: subject
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("challenge subject"))?,
        nonce,
        expires_at,
    }))
}

/// 兑现 challenge：验签 + **原子消费** + 建 token（同一事务）。
pub fn issue_token(
    conn: &Connection,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    challenge_id: &Id16,
    message: &[u8],
    signature: &[u8],
) -> Result<TokenOutcome> {
    let now = clock.now_secs();
    let vault = vault::load(conn)?;

    let mut stmt = conn.prepare(
        "SELECT realm, subject, nonce, expires_at, used_at
         FROM challenges WHERE challenge_id = ?1",
    )?;
    let row = stmt
        .query_row([challenge_id.as_slice()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })
        .optional()?;
    let Some((realm_text, subject, nonce, expires_at, used_at)) = row else {
        return Err(Error::api(ErrorCode::ChallengeInvalid));
    };
    drop(stmt);
    if used_at.is_some() {
        return Err(Error::api(ErrorCode::ChallengeUsed));
    }
    if expires_at <= now {
        return Err(Error::api(ErrorCode::ChallengeInvalid));
    }
    let realm = realm_from_str(&realm_text)?;
    let subject: Id16 = subject
        .as_slice()
        .try_into()
        .map_err(|_| Error::Invariant("challenge subject"))?;
    let nonce: [u8; 32] = nonce
        .as_slice()
        .try_into()
        .map_err(|_| Error::Invariant("challenge nonce"))?;

    // 公钥来自 DB（客户端不能自带公钥，否则身份可伪造）。
    let (device_id, public_key) = match realm {
        Realm::Device => {
            let mut stmt = conn.prepare(
                "SELECT sign_pk FROM devices WHERE device_id = ?1 AND status = 'active'",
            )?;
            let key: Option<Vec<u8>> = stmt
                .query_row([subject.as_slice()], |row| row.get(0))
                .optional()?;
            let key: [u8; 32] = key
                .ok_or_else(|| {
                    Error::Api(ApiError::new(ErrorCode::Revoked).epoch(vault.active_epoch))
                })?
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("device sign_pk"))?;
            (Some(subject), key)
        }
        Realm::Management => {
            let manager_pub = vault::manager_pubkey(conn)?
                .ok_or_else(|| Error::api(ErrorCode::ManagementRequired))?;
            if Realm::management_subject(&manager_pub) != subject {
                return Err(Error::api(ErrorCode::ManagementRequired));
            }
            (None, manager_pub)
        }
    };

    let check = SignatureCheck {
        domain: SignDomain::Auth,
        vault_id: &vault.vault_id,
        device_id: device_id.as_ref(),
        public_key: &public_key,
        message,
        signature,
        expect: &Expectation::Challenge {
            challenge_id,
            nonce: &nonce,
        },
    };
    require(hook, &check, ErrorCode::ChallengeInvalid)?;

    // 单用：CAS 消费（并发时只有一个能成功）。
    let consumed = conn.execute(
        "UPDATE challenges SET used_at = ?1 WHERE challenge_id = ?2 AND used_at IS NULL",
        (now, challenge_id.as_slice()),
    )?;
    if consumed != 1 {
        return Err(Error::api(ErrorCode::ChallengeUsed));
    }

    let raw = random_bytes::<32>()?;
    let token = b64url_encode(&raw);
    let token_hash = sha256(&raw);
    let expires = now + secs::TOKEN_TTL;
    conn.execute(
        "INSERT INTO tokens(token_hash, realm, subject, auth_rev, expires_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            token_hash.as_slice(),
            realm.as_str(),
            subject.as_slice(),
            i64::try_from(vault.auth_rev).map_err(|_| Error::Invariant("auth_rev"))?,
            expires,
            now,
        ),
    )?;
    if let Some(device_id) = device_id {
        conn.execute(
            "UPDATE devices SET last_seen = ?1 WHERE device_id = ?2",
            (now, device_id.as_slice()),
        )?;
    }
    // 顺手清理明显过期的 token（不改变语义）。
    conn.execute("DELETE FROM tokens WHERE expires_at < ?1 - 86400", [now])?;
    Ok(TokenOutcome {
        token,
        expires_at: expires,
        server_time: now,
        realm,
        auth_rev: vault.auth_rev,
    })
}

/// 校验 `Authorization: Bearer <token>`（调用方只传 token 部分）。
pub fn authenticate(conn: &Connection, clock: &dyn Clock, token: &str) -> Result<AuthContext> {
    let now = clock.now_secs();
    let raw = b64url_decode(token).map_err(|_| Error::api(ErrorCode::Unauthorized))?;
    if raw.len() != 32 {
        return Err(Error::api(ErrorCode::Unauthorized));
    }
    let token_hash: Hash32 = sha256(&raw);
    let mut stmt = conn
        .prepare("SELECT realm, subject, auth_rev, expires_at FROM tokens WHERE token_hash = ?1")?;
    let row = stmt
        .query_row([token_hash.as_slice()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .optional()?;
    drop(stmt);
    let Some((realm_text, subject, auth_rev, expires_at)) = row else {
        return Err(Error::api(ErrorCode::Unauthorized));
    };
    let realm = realm_from_str(&realm_text)?;
    if expires_at <= now {
        return Err(Error::api(ErrorCode::Unauthorized));
    }
    let subject: Id16 = subject
        .as_slice()
        .try_into()
        .map_err(|_| Error::Invariant("token subject"))?;
    let vault = vault::load(conn)?;
    // 吊销/轮换推进 auth_rev → 旧 token 立即失效（ADR §4）。
    let auth_rev = u64::try_from(auth_rev).map_err(|_| Error::Invariant("token auth_rev"))?;
    if auth_rev < vault.auth_rev {
        return Err(Error::Api(
            ApiError::new(ErrorCode::Revoked).epoch(vault.active_epoch),
        ));
    }
    match realm {
        Realm::Device => {
            let status: Option<String> = conn
                .query_row(
                    "SELECT status FROM devices WHERE device_id = ?1",
                    [subject.as_slice()],
                    |row| row.get(0),
                )
                .optional()?;
            if status.as_deref() != Some("active") {
                return Err(Error::Api(
                    ApiError::new(ErrorCode::Revoked).epoch(vault.active_epoch),
                ));
            }
            Ok(AuthContext {
                realm,
                subject,
                device_id: Some(subject),
                auth_rev,
            })
        }
        Realm::Management => {
            let Some(manager_pub) = vault::manager_pubkey(conn)? else {
                return Err(Error::api(ErrorCode::ManagementRequired));
            };
            if Realm::management_subject(&manager_pub) != subject {
                return Err(Error::api(ErrorCode::ManagementRequired));
            }
            Ok(AuthContext {
                realm,
                subject,
                device_id: None,
                auth_rev,
            })
        }
    }
}

/// 管理写：额外要求 `b"MN/v1/manage\0" || instance || vault || request_id ||
/// expected_control_rev_BE64 || SHA256(CDE{method,path,unsigned_body})`（ADR 0006 §4）。
pub struct ManagementProof<'a> {
    /// `GET=1/POST=2/PUT=3/PATCH=4/DELETE=5`（§15）。
    pub method_tag: u8,
    /// 规范化请求路径（UTF-8）。
    pub path: &'a str,
    /// 客户端生成的一次性 request_id（幂等/防重放）。
    pub request_id: Id16,
    /// 期望的 control_rev（CAS）。
    pub expected_control_rev: u64,
    /// 管理私钥签名。
    pub signature: &'a [u8],
}

/// 只校验管理签名（**不做** `control_rev` CAS）。
///
/// §26：`unsigned_body` 必须是唯一 canonical body 的原始字节；签名输入由 verify hook
/// 用 `(method,path,body)` 重新 CDE 得到，调用方不得另行拼装业务字段。
pub fn verify_management(
    conn: &Connection,
    hook: &dyn SignatureHook,
    proof: &ManagementProof<'_>,
    unsigned_body: &[u8],
) -> Result<()> {
    let vault = vault::load(conn)?;
    let manager_pub =
        vault::manager_pubkey(conn)?.ok_or_else(|| Error::api(ErrorCode::ManagementRequired))?;
    let instance_id = vault.instance_id;
    let check = SignatureCheck {
        domain: SignDomain::Management,
        vault_id: &vault.vault_id,
        device_id: None,
        public_key: &manager_pub,
        message: unsigned_body,
        signature: proof.signature,
        expect: &Expectation::Management {
            method_tag: proof.method_tag,
            path: proof.path,
            request_id: &proof.request_id,
            expected_control_rev: proof.expected_control_rev,
            instance_id: &instance_id,
        },
    };
    require(hook, &check, ErrorCode::ManagementRequired)
}

/// 管理写的 `control_rev` CAS：期望值与 vault 当前值不一致 → 409（带当前值）。
pub fn require_control_rev(conn: &Connection, expected: u64) -> Result<u64> {
    let vault = vault::load(conn)?;
    if expected != vault.control_rev {
        return Err(Error::Api(
            ApiError::new(ErrorCode::CasConflict).control_rev(vault.control_rev),
        ));
    }
    Ok(vault.control_rev)
}

/// 校验管理签名 + CAS（保留给尚未接入 `management::run` 的调用方）。
pub fn require_management(
    conn: &Connection,
    hook: &dyn SignatureHook,
    proof: &ManagementProof<'_>,
    unsigned_body: &[u8],
) -> Result<()> {
    verify_management(conn, hook, proof, unsigned_body)?;
    require_control_rev(conn, proof.expected_control_rev)?;
    Ok(())
}

/// 吊销设备：状态置 revoked、推进 `auth_rev`（旧 token 立即失效）。
///
/// 返回 `true` 表示本次真的从 active/pending 变成 revoked（重复吊销返回 false，幂等）。
pub fn revoke_device(conn: &Connection, now: i64, device_id: &Id16) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE devices SET status = 'revoked', revoked_at = ?1
         WHERE device_id = ?2 AND status != 'revoked'",
        (now, device_id.as_slice()),
    )?;
    if changed > 0 {
        // 只推进 auth_rev：所有旧 token 立即失效（`authenticate` 会先命中 token 行、再比较
        // auth_rev，因此错误码是 REVOKED 而不是 UNAUTHORIZED——ADR §4 要求"吊销立即生效"）。
        // token 行保留到自然过期，便于诊断"吊销瞬间还有多少在线 token"。
        vault::bump_auth_rev(conn)?;
    }
    Ok(changed > 0)
}

/// 设备是否 active。
pub fn device_active(conn: &Connection, device_id: &Id16) -> Result<bool> {
    let status: Option<String> = conn
        .query_row(
            "SELECT status FROM devices WHERE device_id = ?1",
            [device_id.as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(status.as_deref() == Some("active"))
}

/// 一次性 setup secret 是否已被消费（诊断/测试）。
pub fn setup_consumed(conn: &Connection) -> Result<bool> {
    Ok(schema::meta_text(conn, schema::META_SETUP_CONSUMED)?.as_deref() == Some("1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realm_roundtrips_and_management_subject_is_derived() {
        assert_eq!(Realm::parse(Realm::Device.as_str()), Some(Realm::Device));
        assert_eq!(
            Realm::parse(Realm::Management.as_str()),
            Some(Realm::Management)
        );
        assert_eq!(Realm::parse("root"), None);
        let pubkey = [7u8; 32];
        let subject = Realm::management_subject(&pubkey);
        assert_eq!(subject.as_slice(), &sha256(&pubkey)[..16]);
    }

    #[test]
    fn auth_context_realm_checks() {
        let ctx = AuthContext {
            realm: Realm::Device,
            subject: [1u8; 16],
            device_id: Some([1u8; 16]),
            auth_rev: 1,
        };
        assert!(ctx.device_id().is_ok());
        assert!(ctx.require_management().is_err());

        let mgmt = AuthContext {
            realm: Realm::Management,
            subject: [2u8; 16],
            device_id: None,
            auth_rev: 1,
        };
        assert!(mgmt.device_id().is_err());
        assert!(mgmt.require_management().is_ok());
    }
}
