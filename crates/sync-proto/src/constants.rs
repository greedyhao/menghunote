//! ADR 0006 §2/§3/§4/§14 的 HKDF info、签名域与待哈希输入。
//!
//! 域分隔符是**一个真实的 0x00 字节**（源码中的 `\0`），不是两个 ASCII 字符。

use crate::error::ProtoError;
use crate::records::{Challenge, CheckpointCommit, HttpMethod, ObjectCommit, ObjectType};

/// 数据对象 HKDF：`MN/v1/data\0`。
pub const HKDF_DATA_INFO_PREFIX: &[u8] = b"MN/v1/data\0";
/// 资产对象名 HKDF：`MN/v1/asset-id\0`。
pub const HKDF_ASSET_ID_INFO: &[u8] = b"MN/v1/asset-id\0";
/// 口令槽包装 HKDF 前缀。
pub const HKDF_PASSWORD_WRAP_PREFIX: &[u8] = b"MN/v1/password-wrap\0";
/// 恢复码槽包装 HKDF 前缀。
pub const HKDF_RECOVERY_WRAP_PREFIX: &[u8] = b"MN/v1/recovery-wrap\0";
/// 管理 Ed25519 种子 HKDF info。
pub const HKDF_MANAGEMENT_ED25519_INFO: &[u8] = b"MN/v1/management-ed25519\0";
/// HPKE info。
pub const HPKE_INFO: &[u8] = b"MN/v1/epoch-grant\0";
/// DPAPI entropy 前缀。
pub const DPAPI_ENTROPY_PREFIX: &[u8] = b"MN/dpapi/v1\0";
/// 设备 challenge 签名域。
pub const SIGN_AUTH_DOMAIN: &[u8] = b"MN/v1/auth\0";
/// 管理写签名域。
pub const SIGN_MANAGE_DOMAIN: &[u8] = b"MN/v1/manage\0";
/// ObjectCommit 签名域。
pub const SIGN_OBJECT_COMMIT_DOMAIN: &[u8] = b"MN/v1/object-commit\0";
/// DeviceCertV1 管理签名域（§17.1）。
pub const SIGN_DEVICE_CERT_DOMAIN: &[u8] = b"MN/v1/device-cert\0";
/// CheckpointCommit 签名域。
pub const SIGN_CHECKPOINT_DOMAIN: &[u8] = b"MN/v1/checkpoint\0";
/// 验证修订 2 的证据摘要域（`validation_digest` 的输入前缀；不是签名域）。
pub const CHECKPOINT_VALIDATION_DOMAIN: &[u8] = b"MN/v1/checkpoint-validation\0";
/// 当前生产 `CheckpointCommit.validation_version`（task-122 起）。
///
/// 旧修订仍能**解码**（便于识别并显式拒绝），但不得作为基线/GC 依据。
pub const CHECKPOINT_COMMIT_VALIDATION_VERSION: u16 = 2;
/// 完整 activation T 的双签域（评审 0002 P1-1.4）：只用于
/// `activation_t_signing_input`，不得与其它签名域复用。
pub const SIGN_ACTIVATION_T_DOMAIN: &[u8] = b"MN/v1/activation-t\0";
/// cursor HMAC 域（relay 使用）。
pub const CURSOR_HMAC_DOMAIN: &[u8] = b"MN/v1/cursor\0";

// —— ADR 0015：管理控制链 V2 的域（与 v1 域互不复用）——

/// V2 设备控制记录签名域。
pub const SIGN_V2_CONTROL_DOMAIN: &[u8] = b"MN/v2/control\0";
/// V2 旧 creator 升级签名域。
pub const SIGN_V2_UPGRADE_DOMAIN: &[u8] = b"MN/v2/upgrade\0";
/// V2 离线恢复根签名域。
pub const SIGN_V2_RECOVER_DOMAIN: &[u8] = b"MN/v2/recover\0";
/// V2 目标设备 PoP 签名域（`MN/v2/pop\0 || h || pk32`）。
pub const SIGN_V2_POP_DOMAIN: &[u8] = b"MN/v2/pop\0";
/// V2 鉴权签名域（token 请求签 `MN/v2/auth\0 || challenge`）。
pub const SIGN_AUTH_V2_DOMAIN: &[u8] = b"MN/v2/auth\0";
/// V2 epoch grant 的 HPKE info（ADR 0015：明文仅 epoch root）。
pub const HPKE_INFO_V2: &[u8] = b"MN/v2/epoch-grant\0";

/// `K_object` 的 HKDF info：`MN/v1/data\0 || type_u8 || epoch_BE64 || object_id`。
pub fn hkdf_data_info(
    object_type: ObjectType,
    key_epoch: u64,
    object_id: &[u8; 32],
) -> Result<Vec<u8>, ProtoError> {
    if key_epoch > i64::MAX as u64 {
        return Err(ProtoError::Invalid("key_epoch 超过 i64::MAX".to_owned()));
    }
    let mut out = Vec::with_capacity(HKDF_DATA_INFO_PREFIX.len() + 1 + 8 + 32);
    out.extend_from_slice(HKDF_DATA_INFO_PREFIX);
    out.push(object_type.as_u8());
    out.extend_from_slice(&key_epoch.to_be_bytes());
    out.extend_from_slice(object_id);
    Ok(out)
}

pub fn password_wrap_info(slot_id: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HKDF_PASSWORD_WRAP_PREFIX.len() + 32);
    out.extend_from_slice(HKDF_PASSWORD_WRAP_PREFIX);
    out.extend_from_slice(slot_id);
    out
}

pub fn recovery_wrap_info(slot_id: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HKDF_RECOVERY_WRAP_PREFIX.len() + 32);
    out.extend_from_slice(HKDF_RECOVERY_WRAP_PREFIX);
    out.extend_from_slice(slot_id);
    out
}

pub fn dpapi_entropy(vault_id: &[u8; 16], device_id: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(DPAPI_ENTROPY_PREFIX.len() + 32);
    out.extend_from_slice(DPAPI_ENTROPY_PREFIX);
    out.extend_from_slice(vault_id);
    out.extend_from_slice(device_id);
    out
}

fn with_domain(domain: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(domain.len() + payload.len());
    out.extend_from_slice(domain);
    out.extend_from_slice(payload);
    out
}

/// 设备 challenge 的签名输入：`MN/v1/auth\0 || CDE(challenge)`。
pub fn auth_signing_input(challenge: &Challenge) -> Result<Vec<u8>, ProtoError> {
    Ok(with_domain(SIGN_AUTH_DOMAIN, &challenge.encode()?))
}

/// ObjectCommit 的签名输入：`MN/v1/object-commit\0 || CDE(ObjectCommit)`。
pub fn object_commit_signing_input(commit: &ObjectCommit) -> Result<Vec<u8>, ProtoError> {
    Ok(with_domain(SIGN_OBJECT_COMMIT_DOMAIN, &commit.encode()?))
}

/// CheckpointCommit 的签名输入：`MN/v1/checkpoint\0 || CDE(CheckpointCommit)`。
pub fn checkpoint_signing_input(commit: &CheckpointCommit) -> Result<Vec<u8>, ProtoError> {
    Ok(with_domain(SIGN_CHECKPOINT_DOMAIN, &commit.encode()?))
}

/// 验证修订 2 证据摘要的待哈希输入：
/// `MN/v1/checkpoint-validation\0 || CDE(CheckpointValidationV2)`。
///
/// 调用方做 `SHA256(本函数输出)` 后写入 `CheckpointCommit.validation_digest`。
pub fn checkpoint_validation_input(
    evidence: &crate::records::CheckpointValidationV2,
) -> Result<Vec<u8>, ProtoError> {
    Ok(with_domain(
        CHECKPOINT_VALIDATION_DOMAIN,
        &evidence.encode()?,
    ))
}

/// 管理写签名输入：`MN/v1/manage\0 || instance || vault || request_id ||
/// expected_control_rev_BE64 || SHA256(CDE{method,path,unsigned_body})`。
///
/// `body_digest` 由调用方（relay/sync）计算；本 crate 不做 SHA256。
pub fn management_signing_input(
    instance_id: &[u8; 16],
    vault_id: &[u8; 16],
    request_id: &[u8; 16],
    expected_control_rev: u64,
    body_digest: &[u8; 32],
) -> Result<Vec<u8>, ProtoError> {
    if expected_control_rev > i64::MAX as u64 {
        return Err(ProtoError::Invalid("control_rev 超过 i64::MAX".to_owned()));
    }
    let mut out = Vec::with_capacity(SIGN_MANAGE_DOMAIN.len() + 16 + 16 + 16 + 8 + 32);
    out.extend_from_slice(SIGN_MANAGE_DOMAIN);
    out.extend_from_slice(instance_id);
    out.extend_from_slice(vault_id);
    out.extend_from_slice(request_id);
    out.extend_from_slice(&expected_control_rev.to_be_bytes());
    out.extend_from_slice(body_digest);
    Ok(out)
}

/// 管理签名 CDE 的待哈希输入（method/path/unsigned_body 的 typed CDE map）。
pub fn management_request_cde(
    method: HttpMethod,
    path: &str,
    unsigned_body: &[u8],
) -> Result<Vec<u8>, ProtoError> {
    if !path.starts_with('/') {
        return Err(ProtoError::Invalid("management path 必须绝对".to_owned()));
    }
    crate::cde::encode(&crate::cde::CdeValue::Map(vec![
        (0, crate::cde::CdeValue::UInt(u64::from(u8::from(method)))),
        (1, crate::cde::CdeValue::Text(path.to_owned())),
        (2, crate::cde::CdeValue::Bytes(unsigned_body.to_vec())),
    ]))
}

/// 通用 `domain || payload` 签名输入。
pub fn signature_input(domain: &[u8], payload: &[u8]) -> Vec<u8> {
    with_domain(domain, payload)
}

/// 域常量必须各自恰好以一个 0x00 结尾（§14）；测试和调用方都可复用。
pub fn domain_ends_with_nul(domain: &[u8]) -> bool {
    domain.last() == Some(&0)
        && domain[..domain.len().saturating_sub(1)]
            .iter()
            .all(|b| *b != 0)
}
