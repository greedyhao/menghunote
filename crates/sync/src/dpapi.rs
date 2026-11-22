//! DPAPI 本地身份包裹（ADR 0006 §3，仅 Windows）。
//!
//! v1 用 **`CryptProtectData` / `CryptUnprotectData`（CurrentUser 作用域）** 保护一份
//! 本地身份 BLOB（[`crate::identity::LocalIdentityV1`]）：
//!
//! - 可选 entropy = `b"MN/dpapi/v1\0" || vault_id || device_id`（见 [`entropy_for`]）；
//! - **不用** `CRYPTPROTECT_LOCAL_MACHINE`（ADR §3 明确要求 CurrentUser）；
//! - 密文存 `meta` 受保护 BLOB；本地正文/索引/备份仍是明文（ADR §10 风险项）。
//!
//! ## 平台语义（不假装）
//!
//! 非 Windows 平台本模块编译通过，但 [`is_available`] 返回 `false`，[`protect`] /
//! [`unprotect`] 一律返回 [`SyncError::DpapiUnavailable`]——**没有**降级为"明文存盘"或
//! 自造加密的路径。
//!
//! ## 跨 SID 失败（不可自动化验证）
//!
//! DPAPI CurrentUser 的密钥绑定在**用户 SID + 用户 profile** 上。把 vault/DB 复制到另一
//! 个 Windows 用户、重建 profile、或重置用户密码后（旧 DPAPI master key 没被保留），
//! `CryptUnprotectData` 会失败（典型是 `ERROR_INVALID_DATA`/`ERROR_ACCESS_DENIED`）。
//! 这是**预期行为**：本地身份 BLOB 不能跨用户解密，用户必须用口令槽/恢复码重新解锁并
//! 生成新的设备身份。该场景无法在本机自动化覆盖，属于"未验证项"。

use crate::error::SyncError;
use crate::{DeviceId, VaultId};
use zeroize::Zeroizing;

/// DPAPI entropy 前缀（ADR §3/§14：末尾是一个 0x00 字节；真源在 `sync-proto`）。
pub const ENTROPY_PREFIX: &[u8] = crate::domain::DPAPI_ENTROPY_PREFIX;

/// 计算 DPAPI 可选 entropy：`b"MN/dpapi/v1\0" || vault_id || device_id`。
#[must_use]
pub fn entropy_for(vault_id: &VaultId, device_id: &DeviceId) -> Vec<u8> {
    crate::domain::dpapi_entropy(vault_id, device_id)
}

/// 本平台是否具备 DPAPI 实现。
#[must_use]
pub const fn is_available() -> bool {
    cfg!(windows)
}

/// 用 DPAPI（CurrentUser）保护明文，返回密文 BLOB。
///
/// 非 Windows 返回 [`SyncError::DpapiUnavailable`]。
#[cfg(windows)]
pub fn protect(plaintext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, SyncError> {
    imp::protect(plaintext, entropy)
}

/// 用 DPAPI（CurrentUser）解封 BLOB，返回**零化**的明文。
///
/// 非 Windows 返回 [`SyncError::DpapiUnavailable`]。
#[cfg(windows)]
pub fn unprotect(blob: &[u8], entropy: &[u8]) -> Result<Zeroizing<Vec<u8>>, SyncError> {
    imp::unprotect(blob, entropy)
}

/// 非 Windows：明确不可用（不假装成功、不降级明文）。
#[cfg(not(windows))]
pub fn protect(_plaintext: &[u8], _entropy: &[u8]) -> Result<Vec<u8>, SyncError> {
    Err(SyncError::DpapiUnavailable)
}

/// 非 Windows：明确不可用。
#[cfg(not(windows))]
pub fn unprotect(_blob: &[u8], _entropy: &[u8]) -> Result<Zeroizing<Vec<u8>>, SyncError> {
    Err(SyncError::DpapiUnavailable)
}

/// Windows 上的 Win32 FFI 实现（唯一允许 unsafe 的地方）。
#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use super::SyncError;
    use core::ffi::c_void;
    use core::ptr;
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    use zeroize::Zeroizing;

    fn blob(data: &[u8]) -> Result<CRYPT_INTEGER_BLOB, SyncError> {
        let len = u32::try_from(data.len()).map_err(|_| SyncError::DpapiUnavailable)?;
        Ok(CRYPT_INTEGER_BLOB {
            cbData: len,
            pbData: data.as_ptr() as *mut u8,
        })
    }

    /// 把 DPAPI 输出拷出，并**先擦除再释放**系统缓冲区（解封时里面是明文）。
    fn take_output(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let data = unsafe {
            // SAFETY: CryptProtectData/CryptUnprotectData 成功时保证 pbData/cbData 有效，
            // 缓冲区由 LocalAlloc 分配，必须用 LocalFree 释放。
            ptr::slice_from_raw_parts(out.pbData, out.cbData as usize).as_ref()
        };
        let copied = match data {
            Some(slice) => slice.to_vec(),
            None => Vec::new(),
        };
        if !out.pbData.is_null() {
            unsafe {
                // 先清零（解封出来的明文不留在系统堆里），再释放。
                ptr::write_bytes(out.pbData, 0, out.cbData as usize);
                LocalFree(out.pbData.cast::<c_void>());
            }
        }
        copied
    }

    pub fn protect(plaintext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, SyncError> {
        let input = blob(plaintext)?;
        let entropy = blob(entropy)?;
        let mut out = CRYPT_INTEGER_BLOB::default();
        let ok = unsafe {
            CryptProtectData(
                &input,
                ptr::null(),
                &entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(SyncError::Dpapi(unsafe { GetLastError() }));
        }
        Ok(take_output(out))
    }

    pub fn unprotect(blob_bytes: &[u8], entropy: &[u8]) -> Result<Zeroizing<Vec<u8>>, SyncError> {
        let input = blob(blob_bytes)?;
        let entropy = blob(entropy)?;
        let mut out = CRYPT_INTEGER_BLOB::default();
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                &entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(SyncError::Dpapi(unsafe { GetLastError() }));
        }
        Ok(Zeroizing::new(take_output(out)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entropy_layout() {
        let entropy = entropy_for(&[1u8; 16], &[2u8; 16]);
        assert_eq!(&entropy[..ENTROPY_PREFIX.len()], ENTROPY_PREFIX);
        assert_eq!(entropy[ENTROPY_PREFIX.len() - 1], 0x00);
        assert_eq!(entropy.len(), ENTROPY_PREFIX.len() + 32);
        assert_eq!(
            &entropy[ENTROPY_PREFIX.len()..ENTROPY_PREFIX.len() + 16],
            &[1u8; 16]
        );
        assert_eq!(&entropy[ENTROPY_PREFIX.len() + 16..], &[2u8; 16]);
    }

    #[test]
    fn platform_flag_matches_cfg() {
        assert_eq!(is_available(), cfg!(windows));
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_explicitly_unavailable() {
        assert!(!is_available());
        assert!(matches!(
            protect(b"secret", b"entropy"),
            Err(SyncError::DpapiUnavailable)
        ));
        assert!(matches!(
            unprotect(b"blob", b"entropy"),
            Err(SyncError::DpapiUnavailable)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_roundtrip_and_entropy_binding() {
        let plaintext = b"local identity canary 0001";
        let entropy = entropy_for(&[7u8; 16], &[8u8; 16]);
        let blob = protect(plaintext, &entropy).unwrap();
        assert_ne!(blob, plaintext, "DPAPI 密文不能等于明文");
        assert!(!blob.windows(plaintext.len()).any(|w| w == plaintext));

        let opened = unprotect(&blob, &entropy).unwrap();
        assert_eq!(&**opened, plaintext);

        // 换 entropy（模拟另一个 vault/device）必须解不开
        let other = entropy_for(&[7u8; 16], &[9u8; 16]);
        assert!(unprotect(&blob, &other).is_err());

        // 篡改密文必须解不开
        let mut tampered = blob.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(unprotect(&tampered, &entropy).is_err());

        // 截断必须解不开（且不 panic）
        assert!(unprotect(&blob[..blob.len() / 2], &entropy).is_err());
        assert!(unprotect(&[], &entropy).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_protect_same_bytes_twice_produces_different_blobs() {
        // DPAPI 每次加密带随机 salt；同明文两次密文不同（不构成确定性承诺，只是健全性检查）。
        let entropy = entropy_for(&[1u8; 16], &[1u8; 16]);
        let a = protect(b"same", &entropy).unwrap();
        let b = protect(b"same", &entropy).unwrap();
        assert_ne!(a, b);
    }
}
