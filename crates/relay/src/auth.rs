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
use crate::scope::{ScopeConn, VaultScope};
use crate::sig::{require, Expectation, SignDomain, SignatureCheck, SignatureHook};
use crate::time::{secs, Clock};
use crate::vault;

/// 认证 realm。
///
/// schema 12 起 DB 存**整数**（ADR 0015 §鉴权）：1 = Device、2 = Management（旧管理钥，
/// 仅 legacy）、3 = ManagementDevice（绑定 `cap_id`，S4b 引入）、4 = Recovery（S4c：
/// 离线恢复根，只能读恢复证据/提交 kind 13）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Realm {
    /// 普通设备（读写数据面）。
    Device,
    /// 旧管理凭据（v1 管理钥；`authz_state = active` 后一律拒绝）。
    Management,
    /// 管理设备（V2：`/authz/*` 签发，必须绑定一个有效 cap）。
    ManagementDevice,
    /// 离线恢复根（V2：`subject = SHA256(recovery_pub)[..16]`，TTL 60s，只读恢复证据）。
    Recovery,
}

impl Realm {
    /// DB 与 wire 共用的整数 tag。
    pub const fn as_tag(self) -> i64 {
        match self {
            Self::Device => 1,
            Self::Management => 2,
            Self::ManagementDevice => 3,
            Self::Recovery => 4,
        }
    }

    /// 从 DB 的整数 tag 解析（未知 tag 是不变量错误）。
    pub fn parse_tag(value: i64) -> Option<Self> {
        match value {
            1 => Some(Self::Device),
            2 => Some(Self::Management),
            3 => Some(Self::ManagementDevice),
            4 => Some(Self::Recovery),
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
///
/// `vault` 是**鉴权发生的作用域**（ADR 0014 §A）：token 只在它被签发的 vault 内有效，
/// v2 路由必须再断言 `route.vault == auth.vault == signed.vault`（见 `api::authenticate_v2`），
/// 不允许跨租户复用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthContext {
    pub realm: Realm,
    pub subject: Id16,
    /// 设备 realm 的 device_id（管理 realm 为 None）。
    pub device_id: Option<Id16>,
    pub auth_rev: u64,
    /// token 绑定的授权版本（V2 token 必须等于 vault 当前值；legacy 为 0）。
    pub authz_rev: u64,
    /// token 绑定的离线根代（同上）。
    pub authority_rev: u64,
    /// realm 3 绑定的 cap（其余 realm 为 None）。
    pub cap_id: Option<Id16>,
    /// 鉴权作用域（token 所属租户；绝不跨 scope 复用）。
    pub vault: VaultScope,
}

impl AuthContext {
    /// 要求设备 realm 并返回 device_id（realm 3 是管理身份，**不**授予数据面）。
    pub fn device_id(&self) -> Result<Id16> {
        match (self.realm, self.device_id) {
            (Realm::Device, Some(id)) => Ok(id),
            _ => Err(Error::api(ErrorCode::ManagementRequired)),
        }
    }

    /// 要求**旧**管理 realm（v1 语义；`authz_state = active` 后不复存在）。
    pub fn require_management(&self) -> Result<()> {
        if self.realm == Realm::Management {
            Ok(())
        } else {
            Err(Error::api(ErrorCode::ManagementRequired))
        }
    }

    /// 要求 V2 管理设备 realm 并返回绑定的 cap。
    pub fn require_management_device(&self) -> Result<Id16> {
        match (self.realm, self.cap_id) {
            (Realm::ManagementDevice, Some(cap_id)) => Ok(cap_id),
            _ => Err(Error::api(ErrorCode::ManagementRequired)),
        }
    }

    /// 要求离线恢复 realm（只能读恢复证据；**不**授予数据面或管理面）。
    pub fn require_recovery(&self) -> Result<()> {
        if self.realm == Realm::Recovery {
            Ok(())
        } else {
            Err(Error::api(ErrorCode::Forbidden))
        }
    }
}

fn realm_from_tag(value: i64) -> Result<Realm> {
    Realm::parse_tag(value).ok_or(Error::Invariant("tokens/challenges realm"))
}

/// 创建 challenge（60s 单用）。
pub fn create_challenge(
    conn: &ScopeConn,
    clock: &dyn Clock,
    realm: Realm,
    subject: &Id16,
) -> Result<Challenge> {
    let now = clock.now_secs();
    let vault = vault::load(conn)?;
    // ADR 0015：V2 启用后关闭**所有** v1 鉴权入口（含 device realm）；
    // 客户端必须改走 `/v2/vaults/{id}/authz/{challenges,tokens}`。
    require_legacy_auth(&vault)?;

    match realm {
        Realm::Device => {
            // 未注册/已吊销一律 403 REVOKED（不区分，避免枚举设备）。
            let status: Option<String> = conn
                .query_row(
                    "SELECT status FROM devices WHERE device_id = ?1 AND vault_id = ?",
                    (subject.as_slice(), conn.scope().as_slice()),
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
        // realm 3（管理设备）/ 4（恢复）只能走 `/authz/*`（必须绑定 cap / 恢复钥）。
        Realm::ManagementDevice | Realm::Recovery => {
            return Err(Error::api(ErrorCode::InvalidRequest))
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
        "DELETE FROM challenges WHERE expires_at < ?1 - 86400 AND vault_id = ?",
        (now, conn.scope().as_slice()),
    )?;
    // schema 12：challenge 行投影当时的两 rev（S4b 用它校验 token；v1 只写不读）。
    conn.execute(
        "INSERT INTO challenges(vault_id, challenge_id, realm, subject, nonce, expires_at, used_at,
                                authz_version, authz_rev, authority_rev, cap_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?9, NULL)",
        (
            conn.scope().as_slice(),
            challenge.challenge_id.as_slice(),
            realm.as_tag(),
            subject.as_slice(),
            challenge.nonce.as_slice(),
            challenge.expires_at,
            i64::from(vault.authz_version),
            i64::try_from(vault.authz_rev).map_err(|_| Error::Invariant("authz_rev"))?,
            i64::try_from(vault.authority_rev).map_err(|_| Error::Invariant("authority_rev"))?,
        ),
    )?;
    Ok(challenge)
}

/// 读 challenge（诊断/测试用）。
pub fn load_challenge(conn: &ScopeConn, id: &Id16) -> Result<Option<Challenge>> {
    let mut stmt = conn.prepare(
        "SELECT challenge_id, realm, subject, nonce, expires_at, used_at
         FROM challenges WHERE challenge_id = ?1 AND vault_id = ?",
    )?;
    let row = stmt
        .query_row((id.as_slice(), conn.scope().as_slice()), |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
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
        realm: realm_from_tag(realm)?,
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
    conn: &ScopeConn,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    challenge_id: &Id16,
    message: &[u8],
    signature: &[u8],
) -> Result<TokenOutcome> {
    let now = clock.now_secs();
    let vault = vault::load(conn)?;
    // ADR 0015：V2 启用后 v1 challenge/token 全部关闭（含 realm 1）。
    require_legacy_auth(&vault)?;

    let mut stmt = conn.prepare(
        "SELECT realm, subject, nonce, expires_at, used_at
         FROM challenges WHERE challenge_id = ?1 AND vault_id = ?",
    )?;
    let row = stmt
        .query_row((challenge_id.as_slice(), conn.scope().as_slice()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
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
    let realm = realm_from_tag(realm_text)?;
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
                "SELECT sign_pk FROM devices
                 WHERE device_id = ?1 AND status = 'active' AND vault_id = ?",
            )?;
            let key: Option<Vec<u8>> = stmt
                .query_row((subject.as_slice(), conn.scope().as_slice()), |row| {
                    row.get(0)
                })
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
        // v1 入口从不签出 realm 3/4 的 challenge；出现即拒绝。
        Realm::ManagementDevice | Realm::Recovery => {
            return Err(Error::api(ErrorCode::ManagementRequired))
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
        "UPDATE challenges SET used_at = ?1
         WHERE challenge_id = ?2 AND used_at IS NULL AND vault_id = ?",
        (now, challenge_id.as_slice(), conn.scope().as_slice()),
    )?;
    if consumed != 1 {
        return Err(Error::api(ErrorCode::ChallengeUsed));
    }

    let raw = random_bytes::<32>()?;
    let token = b64url_encode(&raw);
    let token_hash = sha256(&raw);
    let expires = now + secs::TOKEN_TTL;
    conn.execute(
        "INSERT INTO tokens(vault_id, token_hash, realm, subject, auth_rev, expires_at, created_at,
                            authz_version, authz_rev, authority_rev, cap_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL)",
        (
            conn.scope().as_slice(),
            token_hash.as_slice(),
            realm.as_tag(),
            subject.as_slice(),
            i64::try_from(vault.auth_rev).map_err(|_| Error::Invariant("auth_rev"))?,
            expires,
            now,
            i64::from(vault.authz_version),
            i64::try_from(vault.authz_rev).map_err(|_| Error::Invariant("authz_rev"))?,
            i64::try_from(vault.authority_rev).map_err(|_| Error::Invariant("authority_rev"))?,
        ),
    )?;
    if let Some(device_id) = device_id {
        conn.execute(
            "UPDATE devices SET last_seen = ?1 WHERE device_id = ?2 AND vault_id = ?",
            (now, device_id.as_slice(), conn.scope().as_slice()),
        )?;
    }
    // 顺手清理明显过期的 token（不改变语义）。
    conn.execute(
        "DELETE FROM tokens WHERE expires_at < ?1 - 86400 AND vault_id = ?",
        (now, conn.scope().as_slice()),
    )?;
    Ok(TokenOutcome {
        token,
        expires_at: expires,
        server_time: now,
        realm,
        auth_rev: vault.auth_rev,
    })
}

/// 校验 `Authorization: Bearer <token>`（调用方只传 token 部分）。
pub fn authenticate(conn: &ScopeConn, clock: &dyn Clock, token: &str) -> Result<AuthContext> {
    let now = clock.now_secs();
    let raw = b64url_decode(token).map_err(|_| Error::api(ErrorCode::Unauthorized))?;
    if raw.len() != 32 {
        return Err(Error::api(ErrorCode::Unauthorized));
    }
    let token_hash: Hash32 = sha256(&raw);
    let mut stmt = conn.prepare(
        "SELECT realm, subject, auth_rev, expires_at, authz_version, authz_rev,
                authority_rev, cap_id
         FROM tokens WHERE token_hash = ?1 AND vault_id = ?",
    )?;
    let row = stmt
        .query_row((token_hash.as_slice(), conn.scope().as_slice()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<Vec<u8>>>(7)?,
            ))
        })
        .optional()?;
    drop(stmt);
    let Some((
        realm_tag,
        subject,
        auth_rev,
        expires_at,
        authz_version,
        authz_rev,
        authority_rev,
        cap_id,
    )) = row
    else {
        return Err(Error::api(ErrorCode::Unauthorized));
    };
    let realm = realm_from_tag(realm_tag)?;
    if expires_at <= now {
        return Err(Error::api(ErrorCode::Unauthorized));
    }
    let subject: Id16 = subject
        .as_slice()
        .try_into()
        .map_err(|_| Error::Invariant("token subject"))?;
    let cap_id: Option<Id16> = match cap_id {
        None => None,
        Some(bytes) => Some(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("tokens.cap_id"))?,
        ),
    };
    let vault = vault::load(conn)?;
    // 吊销/轮换推进 auth_rev → 旧 token 立即失效（ADR §4）。
    let auth_rev = u64::try_from(auth_rev).map_err(|_| Error::Invariant("token auth_rev"))?;
    if auth_rev < vault.auth_rev {
        return Err(revoked(&vault));
    }
    let token_authz_rev =
        u64::try_from(authz_rev).map_err(|_| Error::Invariant("token authz_rev"))?;
    let token_authority_rev =
        u64::try_from(authority_rev).map_err(|_| Error::Invariant("token authority_rev"))?;
    match u8::try_from(authz_version).map_err(|_| Error::Invariant("token authz_version"))? {
        // v1 token：只对 legacy vault 有效；升级后所有旧 token 立即失效（含 device realm）。
        1 => {
            if vault.authz_state != vault::AuthzState::Legacy {
                return Err(revoked(&vault));
            }
            match realm {
                Realm::Device => {
                    if cap_id.is_some() {
                        return Err(Error::Invariant("v1 device token with cap"));
                    }
                    if !device_active(conn, &subject)? {
                        return Err(revoked(&vault));
                    }
                    Ok(AuthContext {
                        realm,
                        subject,
                        device_id: Some(subject),
                        auth_rev,
                        authz_rev: token_authz_rev,
                        authority_rev: token_authority_rev,
                        cap_id: None,
                        vault: conn.scope(),
                    })
                }
                Realm::Management => {
                    let Some(manager_pub) = vault::manager_pubkey(conn)? else {
                        return Err(Error::api(ErrorCode::ManagementRequired));
                    };
                    if Realm::management_subject(&manager_pub) != subject || cap_id.is_some() {
                        return Err(Error::api(ErrorCode::ManagementRequired));
                    }
                    Ok(AuthContext {
                        realm,
                        subject,
                        device_id: None,
                        auth_rev,
                        authz_rev: token_authz_rev,
                        authority_rev: token_authority_rev,
                        cap_id: None,
                        vault: conn.scope(),
                    })
                }
                // v1 从不会签出 realm 3/4（只有 `/authz/*` 会），出现即拒绝。
                Realm::ManagementDevice | Realm::Recovery => {
                    Err(Error::api(ErrorCode::Unauthorized))
                }
            }
        }
        // V2 token：两个 rev 必须**相等**（ADR 0015：不能用旧 `<` 判断），
        // realm 3 还要逐请求重验 cap（撤销/到期/authority 变更立即生效）。
        //
        // `recovery_only` 下：设备 realm 与恢复 realm 仍可用（读数据/恢复证据），
        // 管理设备 realm 不再有任何有效 cap（`effective_cap` 直接失败）。
        2 => {
            if vault.authz_state == vault::AuthzState::Legacy {
                return Err(revoked(&vault));
            }
            if token_authz_rev != vault.authz_rev || token_authority_rev != vault.authority_rev {
                return Err(revoked(&vault));
            }
            match realm {
                Realm::Device => {
                    if cap_id.is_some() {
                        return Err(Error::Invariant("v2 device token with cap"));
                    }
                    if !device_active(conn, &subject)? {
                        return Err(revoked(&vault));
                    }
                    Ok(AuthContext {
                        realm,
                        subject,
                        device_id: Some(subject),
                        auth_rev,
                        authz_rev: token_authz_rev,
                        authority_rev: token_authority_rev,
                        cap_id: None,
                        vault: conn.scope(),
                    })
                }
                Realm::ManagementDevice => {
                    if vault.authz_state != vault::AuthzState::Active {
                        return Err(revoked(&vault));
                    }
                    let Some(cap_id) = cap_id else {
                        return Err(Error::api(ErrorCode::Unauthorized));
                    };
                    let cap = match crate::control_v2::effective_cap(conn, now, &cap_id) {
                        Ok(cap) => cap,
                        Err(_) => return Err(revoked(&vault)),
                    };
                    if cap.subject_device_id != subject {
                        return Err(revoked(&vault));
                    }
                    Ok(AuthContext {
                        realm,
                        subject,
                        device_id: Some(subject),
                        auth_rev,
                        authz_rev: token_authz_rev,
                        authority_rev: token_authority_rev,
                        cap_id: Some(cap_id),
                        vault: conn.scope(),
                    })
                }
                // 恢复 realm：subject = SHA256(recovery_pub)[..16]，每请求重验当前恢复根。
                Realm::Recovery => {
                    if cap_id.is_some() {
                        return Err(Error::Invariant("v2 recovery token with cap"));
                    }
                    let Some(recovery_pub) = vault.recovery_pub else {
                        return Err(revoked(&vault));
                    };
                    if Realm::management_subject(&recovery_pub) != subject {
                        return Err(revoked(&vault));
                    }
                    Ok(AuthContext {
                        realm,
                        subject,
                        device_id: None,
                        auth_rev,
                        authz_rev: token_authz_rev,
                        authority_rev: token_authority_rev,
                        cap_id: None,
                        vault: conn.scope(),
                    })
                }
                // v1 管理钥在 V2 里没有 bearer 语义。
                Realm::Management => Err(revoked(&vault)),
            }
        }
        _ => Err(Error::Invariant("tokens.authz_version")),
    }
}

fn revoked(vault: &vault::Vault) -> Error {
    Error::Api(ApiError::new(ErrorCode::Revoked).epoch(vault.active_epoch))
}

/// ADR 0015：`authz_state != legacy` 后 v1 鉴权入口一律关闭。
fn require_legacy_auth(vault: &vault::Vault) -> Result<()> {
    if vault.authz_state == vault::AuthzState::Legacy {
        Ok(())
    } else {
        Err(Error::api(ErrorCode::ManagementRequired))
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
    conn: &ScopeConn,
    hook: &dyn SignatureHook,
    proof: &ManagementProof<'_>,
    unsigned_body: &[u8],
) -> Result<()> {
    let vault = vault::load(conn)?;
    // ADR 0015：V2 启用后关闭所有 v1 管理写（v1 与 v2 路径共用同一实现）。
    require_legacy_auth(&vault)?;
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
pub fn require_control_rev(conn: &ScopeConn, expected: u64) -> Result<u64> {
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
    conn: &ScopeConn,
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
pub fn revoke_device(conn: &ScopeConn, now: i64, device_id: &Id16) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE devices SET status = 'revoked', revoked_at = ?1
         WHERE device_id = ?2 AND status != 'revoked' AND vault_id = ?",
        (now, device_id.as_slice(), conn.scope().as_slice()),
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
pub fn device_active(conn: &ScopeConn, device_id: &Id16) -> Result<bool> {
    let status: Option<String> = conn
        .query_row(
            "SELECT status FROM devices WHERE device_id = ?1 AND vault_id = ?",
            (device_id.as_slice(), conn.scope().as_slice()),
            |row| row.get(0),
        )
        .optional()?;
    Ok(status.as_deref() == Some("active"))
}

/// 旧 v1 兼容探测：该 token 是否是**另一个 vault** 的有效凭据。
///
/// 语义只用于把"跨租户的旧 token"映射回 v1 的 404（而不是 401）：新作用域查询失败后，
/// 用它显式复查 token 在别处是否有效；**不返回**其它 vault 的任何数据或 id。
pub fn token_valid_in_other_vault(
    conn: &rusqlite::Connection,
    clock: &dyn Clock,
    token: &str,
    scope: crate::scope::VaultScope,
) -> Result<bool> {
    let raw = b64url_decode(token).map_err(|_| Error::api(ErrorCode::Unauthorized))?;
    if raw.len() != 32 {
        return Err(Error::api(ErrorCode::Unauthorized));
    }
    let token_hash = sha256(&raw);
    let mut stmt = conn
        .prepare("SELECT DISTINCT vault_id FROM tokens WHERE token_hash = ?1 AND vault_id != ?2")?;
    let rows = stmt.query_map((token_hash.as_slice(), scope.as_slice()), |row| {
        row.get::<_, Vec<u8>>(0)
    })?;
    let mut candidates = Vec::new();
    for row in rows {
        let bytes = row?;
        let vault_id: Id16 = bytes
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("tokens.vault_id"))?;
        candidates.push(vault_id);
    }
    for vault_id in candidates {
        let scoped = crate::scope::ScopeConn::new(conn, crate::scope::VaultScope::new(vault_id));
        if authenticate(&scoped, clock, token).is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
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
        assert_eq!(
            Realm::parse_tag(Realm::Device.as_tag()),
            Some(Realm::Device)
        );
        assert_eq!(
            Realm::parse_tag(Realm::Management.as_tag()),
            Some(Realm::Management)
        );
        // 3 = ManagementDevice（S4b）；4 = Recovery（S4c）。
        assert_eq!(Realm::parse_tag(3), Some(Realm::ManagementDevice));
        assert_eq!(Realm::parse_tag(4), Some(Realm::Recovery));
        assert_eq!(Realm::parse_tag(5), None);
        assert_eq!(Realm::parse_tag(0), None);
        let pubkey = [7u8; 32];
        let subject = Realm::management_subject(&pubkey);
        assert_eq!(subject.as_slice(), &sha256(&pubkey)[..16]);
    }

    #[test]
    fn auth_context_realm_checks() {
        let scope = VaultScope::new([9u8; 16]);
        let ctx = AuthContext {
            realm: Realm::Device,
            subject: [1u8; 16],
            device_id: Some([1u8; 16]),
            auth_rev: 1,
            authz_rev: 0,
            authority_rev: 0,
            cap_id: None,
            vault: scope,
        };
        assert_eq!(ctx.vault, scope);
        assert!(ctx.device_id().is_ok());
        assert!(ctx.require_management().is_err());

        let mgmt = AuthContext {
            realm: Realm::Management,
            subject: [2u8; 16],
            device_id: None,
            auth_rev: 1,
            authz_rev: 0,
            authority_rev: 0,
            cap_id: None,
            vault: scope,
        };
        assert!(mgmt.device_id().is_err());
        assert!(mgmt.require_management().is_ok());
        assert!(mgmt.require_management_device().is_err());

        let mgmt_device = AuthContext {
            realm: Realm::ManagementDevice,
            subject: [3u8; 16],
            device_id: Some([3u8; 16]),
            auth_rev: 1,
            authz_rev: 1,
            authority_rev: 1,
            cap_id: Some([9u8; 16]),
            vault: scope,
        };
        // realm 3 是管理身份，不授予数据面 device 语义。
        assert!(mgmt_device.device_id().is_err());
        assert!(mgmt_device.require_management().is_err());
        assert_eq!(
            mgmt_device.require_management_device().expect("cap"),
            [9u8; 16]
        );
    }
}
