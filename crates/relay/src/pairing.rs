//! 设备配对：一次性邀请（PENDING 注册）与设备批准（ACTIVE）
//! （ADR 0006 §4 邀请 / 管理授权）。
//!
//! 安全语义：
//! - relay 只存 `SHA256(T)`，永不落明文邀请令牌；`T` 由管理设备通过 QR fragment 给出；
//! - 兑换是**原子一次性消费**（CAS `used_at`），并发 redeem 只有一个成功；
//! - 新设备只能到 `PENDING`，必须由管理 key 签发的 `DeviceCertV1`（§17.1 域）才能 `ACTIVE`；
//! - 设备指纹 = `SHA256(vault_id || device_id || sign_pk || hpke_pk)` 前 128bit（32 hex）；
//! - relay 只验签名与引用，不解析 HPKE 明文。

use rusqlite::OptionalExtension;

use crate::auth::AuthContext;
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{ct_eq, hex_lower, random_id16, sha256, Hash32, Id16};
use crate::scope::ScopeConn;
use crate::sig::{require, Expectation, SignDomain, SignatureCheck, SignatureHook};
use crate::time::Clock;
use crate::vault;
use menghunote_sync_proto as proto;

/// 邀请有效期（ADR 0006 §4：24h）。
pub const INVITE_TTL: i64 = 24 * 60 * 60;

/// 设备指纹（32 hex）。
pub fn fingerprint(
    vault_id: &Id16,
    device_id: &Id16,
    sign_pk: &[u8; 32],
    hpke_pk: &[u8; 32],
) -> String {
    let digest = sha256(
        &[
            vault_id.as_slice(),
            device_id.as_slice(),
            sign_pk.as_slice(),
            hpke_pk.as_slice(),
        ]
        .concat(),
    );
    hex_lower(&digest[..16])
}

/// `invites` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteRow {
    pub invite_id: Id16,
    pub token_hash: Hash32,
    pub expires_at: i64,
    pub pending_device: Option<Id16>,
    pub used_at: Option<i64>,
    /// ADR 0015：V2 邀请绑定的签发 cap（v1 行为 `None`）。
    pub issuer_cap_id: Option<Id16>,
}

/// 读邀请。
pub fn invite(conn: &ScopeConn, invite_id: &Id16) -> Result<Option<InviteRow>> {
    let mut stmt = conn.prepare(
        "SELECT invite_id, token_hash, expires_at, pending_device, used_at, issuer_cap_id
         FROM invites WHERE invite_id = ?1 AND vault_id = ?",
    )?;
    let row = stmt
        .query_row((invite_id.as_slice(), conn.scope().as_slice()), |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,
            ))
        })
        .optional()?;
    let Some((id, hash, expires_at, pending, used_at, issuer_cap_id)) = row else {
        return Ok(None);
    };
    Ok(Some(InviteRow {
        invite_id: id
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("invite_id"))?,
        token_hash: hash
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("invite token_hash"))?,
        expires_at,
        pending_device: match pending {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("invite pending_device"))?,
            ),
        },
        used_at,
        issuer_cap_id: match issuer_cap_id {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("invites.issuer_cap_id"))?,
            ),
        },
    }))
}

/// 创建邀请的请求（打包避免长参数列表）。
#[derive(Debug)]
pub struct InviteRequest {
    /// `SHA256(T)`；relay 永不落明文令牌。
    pub token_hash: Hash32,
    /// 客户端请求的有效期（≤24h；缺省 now+24h）。
    pub requested_expires_at: Option<i64>,
    /// 管理签名承诺的 control_rev（CAS）。
    pub expected_control_rev: u64,
    pub request_id: Id16,
    pub path: String,
    pub unsigned_body: Vec<u8>,
    pub signature: Vec<u8>,
}

/// 创建邀请：管理签名 + `SHA256(T)` + 24h 有效期。
pub fn create_invite(
    conn: &ScopeConn,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    actor: &AuthContext,
    request: &InviteRequest,
) -> Result<InviteRow> {
    actor.require_management()?;
    let _vault = vault::load(conn)?;
    let now = clock.now_secs();
    crate::auth::require_management(
        conn,
        hook,
        &crate::auth::ManagementProof {
            method_tag: 2, // POST
            path: &request.path,
            request_id: request.request_id,
            expected_control_rev: request.expected_control_rev,
            signature: &request.signature,
        },
        &request.unsigned_body,
    )?;
    let expires_at = request.requested_expires_at.unwrap_or(now + INVITE_TTL);
    // 邀请不得超过 24h（ADR §4）；过期时间必须仍在未来。
    if expires_at <= now || expires_at > now + INVITE_TTL {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    let invite_id = random_id16()?;
    // 顺手清理一年前的过期邀请（只是诊断卫生）。
    conn.execute(
        "DELETE FROM invites WHERE expires_at < ?1 - 31536000 AND vault_id = ?",
        (now, conn.scope().as_slice()),
    )?;
    conn.execute(
        "INSERT INTO invites(vault_id, invite_id, token_hash, expires_at, pending_device, used_at)
         VALUES (?1, ?2, ?3, ?4, NULL, NULL)",
        (
            conn.scope().as_slice(),
            invite_id.as_slice(),
            request.token_hash.as_slice(),
            expires_at,
        ),
    )
    .map_err(|err| match err {
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Error::api(ErrorCode::IdConflict)
        }
        other => other.into(),
    })?;
    Ok(InviteRow {
        invite_id,
        token_hash: request.token_hash,
        expires_at,
        pending_device: None,
        used_at: None,
        // v1 管理写创建的邀请没有 cap 绑定（V2 走控制记录 kind 11）。
        issuer_cap_id: None,
    })
}

/// 新设备兑换邀请时提交的材料。
#[derive(Debug)]
pub struct NewDevice {
    pub device_id: Id16,
    pub sign_pk: [u8; 32],
    pub hpke_pk: [u8; 32],
    /// 新设备本地私钥对 `proof` 的签名（自证明持有新公钥）。
    pub proof: Vec<u8>,
    pub proof_signature: Vec<u8>,
}

/// 兑换结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedeemOutcome {
    pub pending_device: Id16,
    pub fingerprint: String,
}

/// 兑换邀请：验 T、原子消费、创建 PENDING 设备。
pub fn redeem_invite(
    conn: &ScopeConn,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    invite_id: &Id16,
    token: &[u8; 32],
    device: &NewDevice,
) -> Result<RedeemOutcome> {
    let vault = vault::load(conn)?;
    let now = clock.now_secs();
    let invite = invite(conn, invite_id)?.ok_or_else(|| Error::api(ErrorCode::InviteUsed))?;
    if invite.used_at.is_some() {
        return Err(Error::api(ErrorCode::InviteUsed));
    }
    if invite.expires_at <= now {
        return Err(Error::api(ErrorCode::Expired));
    }
    // T 的哈希必须匹配（常数时间比较；错 token 与错 id 都同一错误码，不泄露邀请存在性）。
    if !ct_eq(sha256(token).as_slice(), invite.token_hash.as_slice()) {
        return Err(Error::api(ErrorCode::Forbidden));
    }
    // ADR 0015 kind 11：V2 邀请绑定签发 cap；cap 失效（撤销/到期/authority 变更）则
    // 未兑换邀请一并失效。v1 邀请（issuer_cap_id = NULL）行为不变。
    if let Some(cap_id) = invite.issuer_cap_id {
        crate::control_v2::require_effective_cap(conn, clock, &cap_id)
            .map_err(|_| Error::api(ErrorCode::Forbidden))?;
    }
    // 设备 id 永不复用。
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM devices WHERE device_id = ?1 AND vault_id = ?",
        (device.device_id.as_slice(), conn.scope().as_slice()),
        |row| row.get(0),
    )?;
    if exists != 0 {
        return Err(Error::api(ErrorCode::IdConflict));
    }
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM devices WHERE status != 'revoked' AND vault_id = ?1",
        [conn.scope().as_slice()],
        |row| row.get(0),
    )?;
    proto::check_device_count(usize::try_from(total).unwrap_or(usize::MAX))
        .map_err(|_| Error::api(ErrorCode::InvalidRequest))?;

    // 自证明：新设备的 Ed25519 私钥对它提交的 proof 签名（relay 用提交的公钥验）。
    let check = SignatureCheck {
        domain: SignDomain::InviteRedeem,
        vault_id: &vault.vault_id,
        device_id: Some(&device.device_id),
        public_key: &device.sign_pk,
        message: &device.proof,
        signature: &device.proof_signature,
        expect: &Expectation::None,
    };
    require(hook, &check, ErrorCode::Forbidden)?;

    // 原子一次性消费。
    let consumed = conn.execute(
        "UPDATE invites SET used_at = ?1, pending_device = ?2
         WHERE invite_id = ?3 AND used_at IS NULL AND expires_at > ?1 AND vault_id = ?",
        (
            now,
            device.device_id.as_slice(),
            invite_id.as_slice(),
            conn.scope().as_slice(),
        ),
    )?;
    if consumed != 1 {
        return Err(Error::api(ErrorCode::InviteUsed));
    }
    conn.execute(
        "INSERT INTO devices(vault_id, device_id, sign_pk, hpke_pk, certificate, status,
                             created_at)
         VALUES (?1, ?2, ?3, ?4, X'', 'pending', ?5)",
        (
            conn.scope().as_slice(),
            device.device_id.as_slice(),
            device.sign_pk.as_slice(),
            device.hpke_pk.as_slice(),
            now,
        ),
    )?;
    Ok(RedeemOutcome {
        pending_device: device.device_id,
        fingerprint: fingerprint(
            &vault.vault_id,
            &device.device_id,
            &device.sign_pk,
            &device.hpke_pk,
        ),
    })
}

/// 批准请求：管理签名的设备证书 + §17.1 域签名 + HPKE grant。
#[derive(Debug)]
pub struct ApproveRequest {
    /// `CDE(DeviceCertV1)`（管理 key 签发）。
    pub certificate: Vec<u8>,
    /// `MN/v1/device-cert\0 || CDE(DeviceCertV1)` 的管理签名。
    pub certificate_signature: Vec<u8>,
    pub grant_id: Id16,
    pub hpke_enc: [u8; 32],
    pub hpke_ciphertext: Vec<u8>,
    /// `AAD || enc32 || SHA256(ciphertext)` 的管理签名（§17.2）。
    pub grant_signature: Vec<u8>,
    /// 管理签名承诺的 control_rev（CAS）。
    pub expected_control_rev: u64,
    /// 管理请求自身的一次性 id（控制链/防重放）。
    pub request_id: Id16,
    pub path: String,
    pub unsigned_body: Vec<u8>,
    pub management_signature: Vec<u8>,
}

/// 批准结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproveOutcome {
    pub device_id: Id16,
    pub fingerprint: String,
    pub epoch: u64,
    pub control_rev: u64,
    /// §17.1 证书签名（控制链持久化；客户端验别的设备证书要用）。
    pub certificate_signature: Vec<u8>,
}

/// 批准设备：验证书域签名 + grant，PENDING → ACTIVE。
pub fn approve_device(
    conn: &ScopeConn,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    actor: &AuthContext,
    device_id: &Id16,
    request: &ApproveRequest,
) -> Result<ApproveOutcome> {
    actor.require_management()?;
    let vault = vault::load(conn)?;
    let now = clock.now_secs();
    crate::auth::require_management(
        conn,
        hook,
        &crate::auth::ManagementProof {
            method_tag: 2, // POST
            path: &request.path,
            request_id: request.request_id,
            expected_control_rev: request.expected_control_rev,
            signature: &request.management_signature,
        },
        &request.unsigned_body,
    )?;

    let (status, sign_pk, hpke_pk): (String, Vec<u8>, Vec<u8>) = conn
        .query_row(
            "SELECT status, sign_pk, hpke_pk FROM devices
             WHERE device_id = ?1 AND vault_id = ?",
            (device_id.as_slice(), conn.scope().as_slice()),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| Error::api(ErrorCode::NotFound))?;
    if status == "revoked" {
        return Err(Error::api(ErrorCode::Revoked));
    }

    let cert = proto::DeviceCertV1::decode(&request.certificate)
        .map_err(|_| Error::api(ErrorCode::InvalidRequest))?;
    // 证书必须绑定这台设备和这个 vault，且公钥与兑换时提交的一致（防止 approve 换钥匙）。
    if cert.vault_id != vault.vault_id || cert.device_id != *device_id {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    if cert.sign_ed25519_pub.as_slice() != sign_pk.as_slice()
        || cert.hpke_x25519_pub.as_slice() != hpke_pk.as_slice()
    {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    if cert.issued_manager_rev != vault.manager_rev {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    if !cert.rights.can_read() || !cert.rights.can_write() {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }

    let manager_pub =
        vault::manager_pubkey(conn)?.ok_or_else(|| Error::api(ErrorCode::ManagementRequired))?;
    // §17.1：设备证书签名域。
    let cert_check = SignatureCheck {
        domain: SignDomain::DeviceApprove,
        vault_id: &vault.vault_id,
        device_id: Some(device_id),
        public_key: &manager_pub,
        message: &[],
        signature: &request.certificate_signature,
        expect: &Expectation::DeviceCert { cert: &cert },
    };
    require(hook, &cert_check, ErrorCode::ManagementRequired)?;

    // §17.2：HPKE grant 的外层管理签名（收件公钥来自证书）。
    let ciphertext_sha256 = sha256(&request.hpke_ciphertext);
    let grant_expectation = crate::sig::EpochGrantInput {
        epoch: vault.active_epoch,
        recipient_device_id: device_id,
        recipient_x25519_pub: &cert.hpke_x25519_pub,
        manager_rev: vault.manager_rev,
        grant_id: &request.grant_id,
        hpke_enc: &request.hpke_enc,
        ciphertext_sha256: &ciphertext_sha256,
    };
    let grant_check = SignatureCheck {
        domain: SignDomain::DeviceApprove,
        vault_id: &vault.vault_id,
        device_id: Some(device_id),
        public_key: &manager_pub,
        message: &[],
        signature: &request.grant_signature,
        expect: &Expectation::EpochGrant {
            grant: &grant_expectation,
        },
    };
    require(hook, &grant_check, ErrorCode::ManagementRequired)?;

    // 允许的 grant 待签字节：重建后存库（relay 不进 DB 存客户端任意 bytes）。
    let grant_aad = proto::GrantAadV1 {
        pv: 1,
        vault_id: vault.vault_id,
        epoch: vault.active_epoch,
        recipient_device_id: *device_id,
        recipient_x25519_pub: cert.hpke_x25519_pub,
        manager_rev: vault.manager_rev,
        grant_id: request.grant_id,
    };
    let signed_header = grant_aad
        .grant_signed_bytes(&request.hpke_enc, &ciphertext_sha256)
        .map_err(|_| Error::api(ErrorCode::InvalidRequest))?;

    conn.execute(
        "UPDATE devices SET status = 'active', certificate = ?1, certificate_signature = ?2
         WHERE device_id = ?3 AND vault_id = ?",
        (
            request.certificate.as_slice(),
            request.certificate_signature.as_slice(),
            device_id.as_slice(),
            conn.scope().as_slice(),
        ),
    )?;
    conn.execute(
        // §24.2/评审 §三 4：外层管理签名（`AAD || enc || SHA256(ciphertext)` 的签名）与
        // grant_id 必须一并落库，否则 `GET devices/self/grants/latest` 拿不到 signature64，
        // 客户端 `verify_latest_grant` 无法验签（真实恢复/自批路径会失败）。
        "INSERT INTO device_grants(vault_id, epoch, device_id, manager_rev, hpke_enc,
                                   hpke_ciphertext, signed_header, signature, grant_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(vault_id, epoch, device_id) DO UPDATE SET
           manager_rev = excluded.manager_rev,
           hpke_enc = excluded.hpke_enc,
           hpke_ciphertext = excluded.hpke_ciphertext,
           signed_header = excluded.signed_header,
           signature = excluded.signature,
           grant_id = excluded.grant_id",
        (
            conn.scope().as_slice(),
            i64::try_from(vault.active_epoch).map_err(|_| Error::Invariant("epoch"))?,
            device_id.as_slice(),
            i64::try_from(vault.manager_rev).map_err(|_| Error::Invariant("manager_rev"))?,
            request.hpke_enc.as_slice(),
            request.hpke_ciphertext.as_slice(),
            signed_header.as_slice(),
            request.grant_signature.as_slice(),
            request.grant_id.as_slice(),
        ),
    )?;
    // 成员/证书变化进控制链（永久保留，不随密文 GC）。
    let control_rev = vault::append_control(conn, now, &request.certificate)?;
    Ok(ApproveOutcome {
        device_id: *device_id,
        fingerprint: fingerprint(
            &vault.vault_id,
            device_id,
            &cert.sign_ed25519_pub,
            &cert.hpke_x25519_pub,
        ),
        epoch: vault.active_epoch,
        control_rev,
        certificate_signature: request.certificate_signature.clone(),
    })
}

/// PENDING 设备列表（诊断/测试）。
pub fn pending_devices(conn: &ScopeConn) -> Result<Vec<Id16>> {
    let mut stmt =
        conn.prepare("SELECT device_id FROM devices WHERE status = 'pending' AND vault_id = ?1")?;
    let rows: Vec<Vec<u8>> = stmt
        .query_map([conn.scope().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<std::result::Result<_, _>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for bytes in rows {
        out.push(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("pending device_id"))?,
        );
    }
    Ok(out)
}

/// 拒绝未授权请求的辅助（供 API 层统一错误码）。
pub fn management_required() -> ApiError {
    ApiError::new(ErrorCode::ManagementRequired)
}
