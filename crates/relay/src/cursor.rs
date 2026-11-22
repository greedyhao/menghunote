//! 不可伪造游标（ADR 0006 §5 cursor / §9 游标矩阵）。
//!
//! wire 形态由 `sync-proto` 冻结：`base64url(version || instance16 || vault16 ||
//! log_incarnation16 || last_seq_BE64 || HMAC32)` = **89 字节**。sync-proto 只做结构与
//! base64 编解码，HMAC 由本模块注入（[`CursorKey`] 实现它的 `CursorMacVerifier`）。
//!
//! 语义：
//! - `after` 排他（只返回 `relay_seq > after` 的事件）；
//! - instance/vault/incarnation 任一不匹配都**不是**"从 0 重来"，而是显式错误：
//!   instance/incarnation → `RELAY_RESET`（409），vault → `BAD_CURSOR`（400）；
//! - `last_seq < gc_floor` → `CURSOR_EXPIRED`（410），绝不静默跳水位。

use menghunote_sync_proto as proto;

use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{hmac_sha256, hmac_verify, Hash32};
use crate::vault::Vault;

/// 游标（含 MAC），由 sync-proto 定义。
pub use proto::Cursor;

/// 原始游标字节数（89）。
pub const CURSOR_LEN: usize = proto::CURSOR_RAW_LEN;
/// API 层允许的游标最大长度（base64url 形态）。
pub const CURSOR_TOKEN_MAX: usize = proto::CURSOR_BASE64_LEN;

/// 服务端游标 HMAC 密钥（只存在于 relay 的 DB，不写日志/不出 wire）。
#[derive(Debug, Clone)]
pub struct CursorKey {
    key: Hash32,
}

impl CursorKey {
    /// 以 32 字节实例密钥创建。
    pub fn new(key: Hash32) -> Self {
        Self { key }
    }

    /// 计算 `HMAC(domain || 前57B)`。
    fn mac_for(&self, cursor: &Cursor) -> Result<[u8; 32]> {
        let input = cursor
            .mac_input()
            .map_err(|_| Error::Invariant("cursor mac input"))?;
        hmac_sha256(&self.key, &[proto::CURSOR_HMAC_DOMAIN, &input])
    }

    /// 编码（补上 MAC）。
    pub fn encode(&self, cursor: &Cursor) -> Result<String> {
        let mut cursor = *cursor;
        cursor.mac = self.mac_for(&cursor)?;
        cursor
            .to_base64url()
            .map_err(|_| Error::Invariant("cursor encode"))
    }

    /// 解码 + 校验 MAC；任何结构/MAC 问题都是 `BAD_CURSOR`（不给攻击者区分 oracle）。
    pub fn decode(&self, token: &str) -> Result<Cursor> {
        if token.len() > CURSOR_TOKEN_MAX {
            return Err(Error::api(ErrorCode::BadCursor));
        }
        Cursor::from_base64url_verified(token, self).map_err(|_| Error::api(ErrorCode::BadCursor))
    }

    /// 与当前 vault 对账：分清楚"跨实例/恢复"与"跨 vault/伪造"。
    pub fn check_against(&self, cursor: &Cursor, vault: &Vault) -> Result<()> {
        if cursor.last_seq > i64::MAX as u64 {
            return Err(Error::api(ErrorCode::BadCursor));
        }
        if cursor.vault_id != vault.vault_id {
            return Err(Error::api(ErrorCode::BadCursor));
        }
        if cursor.instance_id != vault.instance_id {
            return Err(Error::Api(
                ApiError::new(ErrorCode::RelayReset).hint("instance_changed"),
            ));
        }
        if cursor.log_incarnation != vault.log_incarnation {
            return Err(Error::Api(
                ApiError::new(ErrorCode::RelayReset).hint("log_incarnation_changed"),
            ));
        }
        Ok(())
    }
}

impl proto::CursorMacVerifier for CursorKey {
    fn verify(&self, input: &[u8], mac: &[u8; proto::CURSOR_MAC_LEN]) -> bool {
        // 常数时间比较；domain 由本模块拼接（与编码端一致）。
        hmac_verify(&self.key, &[proto::CURSOR_HMAC_DOMAIN, input], mac)
    }
}

/// 以 vault 的身份字段与水位构造（MAC 由 [`CursorKey::encode`] 填）。
pub fn for_vault(vault: &Vault, last_seq: u64) -> Result<Cursor> {
    Cursor::new(
        vault.instance_id,
        vault.vault_id,
        vault.log_incarnation,
        last_seq,
        [0u8; proto::CURSOR_MAC_LEN],
    )
    .map_err(|_| Error::Invariant("cursor fields"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> Vault {
        Vault {
            vault_id: [1u8; 16],
            instance_id: [2u8; 16],
            log_id: [3u8; 16],
            log_incarnation: [4u8; 16],
            cursor_key: [5u8; 32],
            state: crate::vault::VaultState::Active,
            active_epoch: 1,
            next_epoch: 2,
            manager_rev: 1,
            control_rev: 1,
            auth_rev: 1,
            head_seq: 10,
            gc_floor: 0,
            normal_limit: 1024,
            maintenance_limit: 2048,
            created_at: 0,
        }
    }

    #[test]
    fn cursor_is_89_bytes_and_roundtrips() {
        let v = vault();
        let key = CursorKey::new(v.cursor_key);
        let cursor = for_vault(&v, 42).expect("cursor");
        let token = key.encode(&cursor).expect("encode");
        let raw = crate::ids::b64url_decode(&token).expect("b64");
        assert_eq!(raw.len(), CURSOR_LEN);
        assert_eq!(CURSOR_LEN, 89);
        assert_eq!(raw[0], proto::CURSOR_VERSION);
        assert_eq!(&raw[1..17], &v.instance_id);
        assert_eq!(&raw[17..33], &v.vault_id);
        assert_eq!(&raw[33..49], &v.log_incarnation);
        assert_eq!(u64::from_be_bytes(raw[49..57].try_into().expect("seq")), 42);
        let decoded = key.decode(&token).expect("decode");
        assert_eq!(decoded.last_seq, 42);
        key.check_against(&decoded, &v).expect("matches vault");
    }

    #[test]
    fn tampered_mac_is_rejected() {
        let v = vault();
        let key = CursorKey::new(v.cursor_key);
        let token = key
            .encode(&for_vault(&v, 7).expect("cursor"))
            .expect("encode");
        let mut raw = crate::ids::b64url_decode(&token).expect("b64");
        raw[CURSOR_LEN - 1] ^= 1;
        assert!(matches!(
            key.decode(&crate::ids::b64url_encode(&raw)),
            Err(Error::Api(e)) if e.code == ErrorCode::BadCursor
        ));
        // 换 key 也失败（跨 relay 重放）。
        assert!(CursorKey::new([9u8; 32]).decode(&token).is_err());
    }

    #[test]
    fn tampered_seq_breaks_mac() {
        let v = vault();
        let key = CursorKey::new(v.cursor_key);
        let token = key
            .encode(&for_vault(&v, 7).expect("cursor"))
            .expect("encode");
        let mut raw = crate::ids::b64url_decode(&token).expect("b64");
        raw[56] ^= 0xff; // last_seq 最低位
        assert!(key.decode(&crate::ids::b64url_encode(&raw)).is_err());
    }

    #[test]
    fn wrong_incarnation_or_instance_is_relay_reset() {
        let v = vault();
        let key = CursorKey::new(v.cursor_key);
        let mut cursor = for_vault(&v, 7).expect("cursor");
        cursor.log_incarnation = [9u8; 16];
        assert!(matches!(
            key.check_against(&cursor, &v),
            Err(Error::Api(e)) if e.code == ErrorCode::RelayReset
        ));
        let mut cursor = for_vault(&v, 7).expect("cursor");
        cursor.instance_id = [9u8; 16];
        assert!(matches!(
            key.check_against(&cursor, &v),
            Err(Error::Api(e)) if e.code == ErrorCode::RelayReset
        ));
        let mut cursor = for_vault(&v, 7).expect("cursor");
        cursor.vault_id = [9u8; 16];
        assert!(matches!(
            key.check_against(&cursor, &v),
            Err(Error::Api(e)) if e.code == ErrorCode::BadCursor
        ));
    }

    #[test]
    fn garbage_tokens_are_bad_cursor() {
        let key = CursorKey::new([1u8; 32]);
        for token in ["", "AAAA", "!!!!", &"A".repeat(200)] {
            assert!(matches!(
                key.decode(token),
                Err(Error::Api(e)) if e.code == ErrorCode::BadCursor
            ));
        }
    }
}
