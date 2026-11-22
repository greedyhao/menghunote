//! ADR 0015 **S4b1**：`/v2/vaults/{id}/authz/{challenges,tokens}`。
//!
//! V2 challenge-response（与 v1 `/auth/*` 并存但语义不同）：
//!
//! - challenge 请求 `{0: realm, 1: subject16, 2: cap_id(0/16)}`；
//!   realm 1 = 普通设备；realm 3 = 管理设备（**必须**带一个此刻有效的 cap）；
//!   realm 4 = 离线恢复根（`subject = SHA256(recovery_pub)[..16]`，不带 cap，TTL 60s，
//!   只能读恢复证据/提交 kind 13）。realm 2（旧管理钥）由 v1 入口处理，这里拒绝。
//! - 响应 `{0: bytes(ChallengeV2)}`；客户端对**整份 ChallengeV2 字节**签
//!   `MN/v2/auth\0 || challenge`。
//! - token 请求 `{0: challenge bytes, 1: sig64}`；challenge 60s 单用，
//!   token = 32B CSPRNG（DB 只存 `SHA256`），TTL = Device 600s / 管理
//!   `min(600, cap 剩余)`（ADR 0015 §鉴权）。
//!
//! # 校验
//!
//! challenge 行投影签发瞬间的 `authz_version/authz_rev/authority_rev/cap_id`；
//! 兑现时逐字段与客户端回传的 ChallengeV2 比对，再与 vault 当前值比对（任何授权/鉴权
//! 变化都会让未兑现 challenge 失效）。管理 token 在**每个请求**上还要重验 cap
//! （`auth::authenticate`），所以 cap 撤销/到期立即切断 bearer 与 WS 帧。
//!
//! realm 3 的 bearer 只用于"选定一个 cap"，**不代替**管理签名：所有管理写仍由
//! `control_v2` 的控制记录签名门限授权。realm 4 的 bearer 只读恢复证据，**不能**代替
//! 离线恢复根的签名（kind 13/14 仍是自证的控制记录）。
//!
//! `recovery_only` 下：realm 1（设备）与 realm 4（恢复）仍可签发，realm 3 因没有任何
//! 有效 cap 一律 403（管理面关闭，只许离线 Reset）。

use rusqlite::OptionalExtension;

use crate::auth::Realm;
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{b64url_encode, random_bytes, random_id16, sha256, Id16};
use crate::scope::ScopeConn;
use crate::time::{secs, Clock};
use crate::vault::{self, AuthzState};
use menghunote_sync_proto as proto;

/// challenge 有效期（秒，单用）。
pub const AUTHZ_CHALLENGE_TTL: i64 = 60;
/// 普通设备 token TTL（秒）。
pub const AUTHZ_TOKEN_TTL: i64 = 600;
/// 恢复 token TTL（秒）：ADR 0015「恢复 60s」（与 challenge 同窗，短寿命高敏）。
pub const AUTHZ_RECOVERY_TOKEN_TTL: i64 = 60;

/// 签发成功后的 token 事实（明文 token 只在这里出现一次）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthzToken {
    pub token: String,
    pub expires_at: i64,
    pub server_time: i64,
    pub realm: Realm,
    pub auth_rev: u64,
    pub authz_rev: u64,
    pub authority_rev: u64,
    pub cap_id: Option<Id16>,
}

fn invalid() -> Error {
    Error::api(ErrorCode::InvalidRequest)
}

fn forbidden() -> Error {
    Error::api(ErrorCode::Forbidden)
}

fn revoked(vault: &vault::Vault) -> Error {
    Error::Api(ApiError::new(ErrorCode::Revoked).epoch(vault.active_epoch))
}

fn i64_of(value: u64, field: &'static str) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Invariant(field))
}

fn u64_of(value: i64, field: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Invariant(field))
}

/// V2 challenge 的 origin 绑定摘要：`SHA256(instance_id || vault_id)`。
///
/// ADR 0015 只固定了字段名；实现把它定义为 relay origin（实例 + 租户）的确定性绑定，
/// 客户端必须原样回传（签名覆盖它）。跨 relay/跨 vault 的 challenge 因此不可混用。
pub fn origin_hash(instance_id: &Id16, vault_id: &Id16) -> [u8; 32] {
    sha256(&[instance_id.as_slice(), vault_id.as_slice()].concat())
}

/// 创建 challenge（事务内；60s 单用）。
pub fn create_challenge(
    conn: &ScopeConn,
    clock: &dyn Clock,
    realm_tag: u64,
    subject: &Id16,
    cap_id: Option<Id16>,
) -> Result<proto::ChallengeV2> {
    let now = clock.now_secs();
    let now_u64 = u64_of(now, "now")?;
    let vault = vault::load(conn)?;
    require_v2(&vault)?;

    let (realm, cap_id) = match realm_tag {
        1 => {
            if cap_id.is_some() {
                return Err(invalid());
            }
            if !crate::auth::device_active(conn, subject)? {
                return Err(revoked(&vault));
            }
            (proto::CHALLENGE_V2_REALM_DEVICE, None)
        }
        3 => {
            let Some(cap_id) = cap_id else {
                return Err(invalid());
            };
            // cap 有效 = 未撤销 + authority 当前 + 时间窗内 + 主体 ACTIVE 且 key 匹配。
            let cap = crate::control_v2::effective_cap(conn, now, &cap_id)?;
            if cap.subject_device_id != *subject {
                return Err(forbidden());
            }
            (proto::CHALLENGE_V2_REALM_MANAGEMENT_DEVICE, Some(cap_id))
        }
        // realm 4：离线恢复根。subject 必须等于 SHA256(recovery_pub)[..16]；
        // 只有 legacy 之外的 vault 才有恢复根（legacy 由 require_v2 拦在前面）。
        4 => {
            if cap_id.is_some() {
                return Err(invalid());
            }
            let Some(recovery_pub) = vault.recovery_pub else {
                return Err(revoked(&vault));
            };
            if Realm::management_subject(&recovery_pub) != *subject {
                return Err(forbidden());
            }
            (proto::CHALLENGE_V2_REALM_RECOVERY, None)
        }
        // realm 2（旧管理钥）不走这里。
        2 => return Err(invalid()),
        _ => return Err(invalid()),
    };

    let challenge = proto::ChallengeV2 {
        version: proto::V2_WIRE_VERSION,
        instance_id: vault.instance_id,
        origin_hash: origin_hash(&vault.instance_id, &vault.vault_id),
        vault_id: vault.vault_id,
        realm,
        subject: *subject,
        challenge_id: random_id16()?,
        nonce: random_bytes::<32>()?,
        expires_at: now_u64.saturating_add(AUTHZ_CHALLENGE_TTL as u64),
        auth_rev: vault.auth_rev,
        authz_rev: vault.authz_rev,
        cap_id,
        authority_rev: vault.authority_rev,
    };
    challenge
        .encode()
        .map_err(|_| Error::Invariant("challenge v2"))?;

    // 顺手清理过期 challenge（诊断卫生，不影响语义）。
    conn.execute(
        "DELETE FROM challenges WHERE expires_at < ?1 - 86400 AND vault_id = ?",
        (now, conn.scope().as_slice()),
    )?;
    conn.execute(
        "INSERT INTO challenges(vault_id, challenge_id, realm, subject, nonce, expires_at, used_at,
                                authz_version, authz_rev, authority_rev, cap_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 2, ?7, ?8, ?9)",
        (
            conn.scope().as_slice(),
            challenge.challenge_id.as_slice(),
            // 上面 match 已把 realm_tag 限定为 1/3，直接落库为整数 tag。
            i64::try_from(realm_tag).map_err(|_| invalid())?,
            subject.as_slice(),
            challenge.nonce.as_slice(),
            i64_of(challenge.expires_at, "challenges.expires_at")?,
            i64_of(vault.authz_rev, "challenges.authz_rev")?,
            i64_of(vault.authority_rev, "challenges.authority_rev")?,
            cap_id.as_ref().map(|id| id.to_vec()),
        ),
    )?;
    Ok(challenge)
}

/// 兑现 challenge：逐字段比对 → 验签 → 原子消费 → 签发 token（同一事务）。
pub fn issue_token(
    conn: &ScopeConn,
    clock: &dyn Clock,
    challenge: &proto::ChallengeV2,
    challenge_bytes: &[u8],
    signature: &[u8],
) -> Result<AuthzToken> {
    let now = clock.now_secs();
    let vault = vault::load(conn)?;
    require_v2(&vault)?;
    if challenge.instance_id != vault.instance_id || challenge.vault_id != vault.vault_id {
        return Err(Error::api(ErrorCode::ChallengeInvalid));
    }
    if signature.len() != 64 {
        return Err(Error::api(ErrorCode::ChallengeInvalid));
    }
    if challenge.encode().map_err(|_| invalid())? != challenge_bytes {
        return Err(Error::api(ErrorCode::ChallengeInvalid));
    }
    if challenge.expires_at <= u64_of(now, "now")? {
        return Err(Error::api(ErrorCode::ChallengeInvalid));
    }

    let mut stmt = conn.prepare(
        "SELECT realm, subject, nonce, expires_at, used_at, authz_version, authz_rev,
                authority_rev, cap_id
         FROM challenges WHERE vault_id = ?1 AND challenge_id = ?2",
    )?;
    let row = stmt
        .query_row(
            (conn.scope().as_slice(), challenge.challenge_id.as_slice()),
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, Option<Vec<u8>>>(8)?,
                ))
            },
        )
        .optional()?;
    drop(stmt);
    let Some((
        realm_tag,
        subject,
        nonce,
        expires_at,
        used_at,
        authz_version,
        authz_rev,
        authority_rev,
        cap_id,
    )) = row
    else {
        return Err(Error::api(ErrorCode::ChallengeInvalid));
    };
    if used_at.is_some() {
        return Err(Error::api(ErrorCode::ChallengeUsed));
    }
    if expires_at <= now {
        return Err(Error::api(ErrorCode::ChallengeInvalid));
    }
    let realm = Realm::parse_tag(realm_tag).ok_or_else(invalid)?;
    let db_cap_id: Option<Id16> = match cap_id {
        None => None,
        Some(bytes) => Some(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("challenges.cap_id"))?,
        ),
    };
    let matches = realm.as_tag() == i64::from(challenge.realm)
        && subject.as_slice() == challenge.subject
        && nonce.as_slice() == challenge.nonce
        && expires_at == i64_of(challenge.expires_at, "challenge.expires_at")?
        && authz_version == 2
        && authz_rev == i64_of(vault.authz_rev, "challenge.authz_rev")?
        && authority_rev == i64_of(vault.authority_rev, "challenge.authority_rev")?
        && db_cap_id == challenge.cap_id
        && challenge.auth_rev == vault.auth_rev;
    if !matches {
        return Err(Error::api(ErrorCode::ChallengeInvalid));
    }

    // 公钥来自 DB（客户端不能自带公钥）；realm 3 先重验 cap（撤销/到期立即失败）。
    let (device_id, public_key, cap_expires_at, realm) = match challenge.realm {
        proto::CHALLENGE_V2_REALM_DEVICE => {
            let Some(key) = vault::device_sign_pk(conn, &challenge.subject)? else {
                return Err(revoked(&vault));
            };
            (challenge.subject, key, None, Realm::Device)
        }
        proto::CHALLENGE_V2_REALM_MANAGEMENT_DEVICE => {
            let Some(cap_id) = challenge.cap_id else {
                return Err(Error::api(ErrorCode::ChallengeInvalid));
            };
            let cap = crate::control_v2::effective_cap(conn, now, &cap_id)?;
            if cap.subject_device_id != challenge.subject {
                return Err(forbidden());
            }
            let Some(key) = vault::device_sign_pk(conn, &challenge.subject)? else {
                return Err(revoked(&vault));
            };
            (
                challenge.subject,
                key,
                Some(cap.expires_at),
                Realm::ManagementDevice,
            )
        }
        proto::CHALLENGE_V2_REALM_RECOVERY => {
            if challenge.cap_id.is_some() {
                return Err(Error::api(ErrorCode::ChallengeInvalid));
            }
            let Some(recovery_pub) = vault.recovery_pub else {
                return Err(revoked(&vault));
            };
            if Realm::management_subject(&recovery_pub) != challenge.subject {
                return Err(forbidden());
            }
            (challenge.subject, recovery_pub, None, Realm::Recovery)
        }
        _ => return Err(Error::api(ErrorCode::ChallengeInvalid)),
    };

    let input = proto::auth_signing_input_v2(challenge_bytes);
    crate::sig::verify_ed25519(&public_key, &input, signature)
        .map_err(|_| Error::api(ErrorCode::ChallengeInvalid))?;

    // 单用：CAS 消费（并发时只有一个能成功）。
    let consumed = conn.execute(
        "UPDATE challenges SET used_at = ?1
         WHERE vault_id = ?2 AND challenge_id = ?3 AND used_at IS NULL",
        (
            now,
            conn.scope().as_slice(),
            challenge.challenge_id.as_slice(),
        ),
    )?;
    if consumed != 1 {
        return Err(Error::api(ErrorCode::ChallengeUsed));
    }

    // TTL = min(600, cap 剩余)；管理 token 不允许活过 cap。恢复 token 固定 60s。
    let mut expires = now.saturating_add(match realm {
        Realm::Recovery => AUTHZ_RECOVERY_TOKEN_TTL,
        _ => AUTHZ_TOKEN_TTL,
    });
    if let Some(cap_expires_at) = cap_expires_at {
        let cap_expires_at = i64_of(cap_expires_at, "cap.expires_at")?;
        if cap_expires_at <= now {
            return Err(forbidden());
        }
        expires = expires.min(cap_expires_at);
    }

    let raw = random_bytes::<32>()?;
    let token = b64url_encode(&raw);
    let token_hash = sha256(&raw);
    conn.execute(
        "INSERT INTO tokens(vault_id, token_hash, realm, subject, auth_rev, expires_at,
                            created_at, authz_version, authz_rev, authority_rev, cap_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 2, ?8, ?9, ?10)",
        (
            conn.scope().as_slice(),
            token_hash.as_slice(),
            realm.as_tag(),
            challenge.subject.as_slice(),
            i64_of(vault.auth_rev, "tokens.auth_rev")?,
            expires,
            now,
            i64_of(vault.authz_rev, "tokens.authz_rev")?,
            i64_of(vault.authority_rev, "tokens.authority_rev")?,
            challenge.cap_id.as_ref().map(|id| id.to_vec()),
        ),
    )?;
    conn.execute(
        "UPDATE devices SET last_seen = ?1 WHERE device_id = ?2 AND vault_id = ?",
        (now, device_id.as_slice(), conn.scope().as_slice()),
    )?;
    // 顺手清理明显过期的 token（不改变语义）。
    conn.execute(
        "DELETE FROM tokens WHERE expires_at < ?1 - 86400 AND vault_id = ?",
        (now, conn.scope().as_slice()),
    )?;
    Ok(AuthzToken {
        token,
        expires_at: expires,
        server_time: now,
        realm,
        auth_rev: vault.auth_rev,
        authz_rev: vault.authz_rev,
        authority_rev: vault.authority_rev,
        cap_id: challenge.cap_id,
    })
}

/// V2 授权状态（`active` 或 `recovery_only`）：legacy 没有 V2 鉴权。
///
/// `recovery_only` 下管理 realm 仍会因"没有任何有效 cap"而 403；设备/恢复 realm 可用
/// （撤权不等于删数据，恢复证据必须可读）。
fn require_v2(vault: &vault::Vault) -> Result<()> {
    if vault.authz_version != 2 || vault.authz_state == AuthzState::Legacy {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    Ok(())
}

/// challenge 默认 TTL（供测试/文档引用；与 `secs::CHALLENGE_TTL` 同值但独立域）。
pub const fn challenge_ttl() -> i64 {
    secs::CHALLENGE_TTL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_hash_binds_instance_and_vault() {
        let a = origin_hash(&[1u8; 16], &[2u8; 16]);
        assert_eq!(a, sha256(&[[1u8; 16], [2u8; 16]].concat()));
        assert_ne!(a, origin_hash(&[1u8; 16], &[3u8; 16]));
        assert_ne!(a, origin_hash(&[9u8; 16], &[2u8; 16]));
    }

    #[test]
    fn challenge_ttl_matches_v1_window() {
        assert_eq!(challenge_ttl(), 60);
        assert_eq!(AUTHZ_CHALLENGE_TTL, 60);
        assert_eq!(AUTHZ_TOKEN_TTL, 600);
        assert_eq!(AUTHZ_RECOVERY_TOKEN_TTL, 60);
    }
}
