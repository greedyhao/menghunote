//! 域分隔符（ADR 0006 §14）。
//!
//! **真源是 `menghunote-sync-proto::constants`**：wire 契约只有一份，本 crate 只做
//! re-export + 一个 ADR 缺口的补充域。每个域 = `UTF8("MN/v1/<domain>")` 后接**一个 0x00
//! 字节**（源码里写作 `\0`，编译后是一个 NUL）。测试用十六进制断言这一点，任何把它改成
//! 反斜杠+`0` 两个字符的改动都会失败。

pub use menghunote_sync_proto::{
    dpapi_entropy, hkdf_data_info, password_wrap_info, recovery_wrap_info, signature_input,
    DPAPI_ENTROPY_PREFIX, HKDF_ASSET_ID_INFO, HKDF_DATA_INFO_PREFIX, HKDF_MANAGEMENT_ED25519_INFO,
    HKDF_PASSWORD_WRAP_PREFIX, HKDF_RECOVERY_WRAP_PREFIX, HPKE_INFO, SIGN_AUTH_DOMAIN,
    SIGN_CHECKPOINT_DOMAIN, SIGN_MANAGE_DOMAIN, SIGN_OBJECT_COMMIT_DOMAIN,
};

/// MNE1 信封 AAD 前缀（ADR §2：`b"MN/e2e/v1\0" || header[0..196] || ...`）。
pub use menghunote_sync_proto::AAD_DOMAIN as ENVELOPE_AAD;

/// 设备证书签名域（ADR §17 冻结：`MN/v1/device-cert\0`）。
pub use menghunote_sync_proto::SIGN_DEVICE_CERT_DOMAIN as DEVICE_CERT;

#[cfg(test)]
mod tests {
    use super::*;
    use menghunote_sync_proto::domain_ends_with_nul;

    /// ADR §14：域分隔符必须是真正的 NUL（0x00），不是反斜杠+`0`，也不是两个 ASCII 字节。
    #[test]
    fn domains_end_with_nul() {
        let all: &[(&str, &[u8])] = &[
            ("envelope_aad", ENVELOPE_AAD),
            ("password_wrap", HKDF_PASSWORD_WRAP_PREFIX),
            ("recovery_wrap", HKDF_RECOVERY_WRAP_PREFIX),
            ("management_ed25519", HKDF_MANAGEMENT_ED25519_INFO),
            ("data_key", HKDF_DATA_INFO_PREFIX),
            ("asset_id", HKDF_ASSET_ID_INFO),
            ("epoch_grant", HPKE_INFO),
            ("object_commit", SIGN_OBJECT_COMMIT_DOMAIN),
            ("checkpoint", SIGN_CHECKPOINT_DOMAIN),
            ("auth", SIGN_AUTH_DOMAIN),
            ("manage", SIGN_MANAGE_DOMAIN),
            ("dpapi_entropy", DPAPI_ENTROPY_PREFIX),
            ("device_cert", DEVICE_CERT),
        ];
        for (name, domain) in all {
            assert_eq!(
                domain.last().copied(),
                Some(0x00),
                "{name} 必须以单个 0x00 结尾（ADR 0006 §14）"
            );
            assert!(
                domain_ends_with_nul(domain),
                "{name} 必须恰好一个结尾 NUL（proto 的口径）"
            );
            // 末尾不能是 "\\0" 这两个 ASCII 字符（反斜杠 0x5c + '0' 0x30）
            assert!(!domain.ends_with(b"\\0"), "{name} 不能以反斜杠+0 结尾");
            // 域体是 ASCII，且不含除结尾 NUL 之外的 NUL
            let body = &domain[..domain.len() - 1];
            assert!(body.iter().all(|b| b.is_ascii_graphic() || *b == b'/'));
            assert_eq!(body.iter().filter(|b| **b == 0x00).count(), 0);
        }
    }

    /// ADR §14 的 golden hex：`MN/e2e/v1\0` = 4d4e 2f65 3265 2f76 3100。
    #[test]
    fn aad_prefix_golden_hex() {
        assert_eq!(ENVELOPE_AAD, b"MN/e2e/v1\0");
        assert_eq!(
            ENVELOPE_AAD,
            &[0x4d, 0x4e, 0x2f, 0x65, 0x32, 0x65, 0x2f, 0x76, 0x31, 0x00]
        );
        assert_eq!(ENVELOPE_AAD.len(), 10);
        assert_eq!(HPKE_INFO, b"MN/v1/epoch-grant\0");
        assert_eq!(HPKE_INFO.len(), 18);
        assert_eq!(HPKE_INFO[17], 0x00);
        assert_eq!(DPAPI_ENTROPY_PREFIX, b"MN/dpapi/v1\0");
        assert_eq!(DPAPI_ENTROPY_PREFIX.len(), 12);
    }

    #[test]
    fn slot_info_is_domain_then_slot_id() {
        let slot_id = [0xab; 32];
        let info = password_wrap_info(&slot_id);
        assert_eq!(
            &info[..HKDF_PASSWORD_WRAP_PREFIX.len()],
            HKDF_PASSWORD_WRAP_PREFIX
        );
        assert_eq!(info[HKDF_PASSWORD_WRAP_PREFIX.len() - 1], 0x00);
        assert_eq!(&info[HKDF_PASSWORD_WRAP_PREFIX.len()..], &slot_id);
        assert_eq!(recovery_wrap_info(&slot_id).len(), info.len());
        assert_ne!(
            recovery_wrap_info(&slot_id)[..HKDF_RECOVERY_WRAP_PREFIX.len()],
            password_wrap_info(&slot_id)[..HKDF_PASSWORD_WRAP_PREFIX.len()]
        );
    }

    #[test]
    fn dpapi_entropy_layout() {
        let entropy = dpapi_entropy(&[1u8; 16], &[2u8; 16]);
        assert_eq!(&entropy[..DPAPI_ENTROPY_PREFIX.len()], DPAPI_ENTROPY_PREFIX);
        assert_eq!(
            &entropy[DPAPI_ENTROPY_PREFIX.len()..DPAPI_ENTROPY_PREFIX.len() + 16],
            &[1u8; 16]
        );
        assert_eq!(&entropy[DPAPI_ENTROPY_PREFIX.len() + 16..], &[2u8; 16]);
    }
}
