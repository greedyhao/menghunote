//! 口令 KDF（Argon2id）与 HKDF-SHA256 子密钥派生。
//!
//! ADR 0006 §3 的硬约束：
//!
//! - 口令是**原样 UTF-8**：不 trim、不做 Unicode 规范化，长度 1..=1024 字节；
//! - 写者固定 `argon2id, version=0x13, m=65536 KiB, t=3, p=1, salt=random16, output_len=32`；
//! - 读者允许 `m=64..256MiB, t=3..6, p=1..4`，**越界必须在调用 Argon2 之前拒绝**
//!   （否则恶意 slot 描述可以让客户端分配 1TiB 或算上几分钟）。
//!
//! 参数类型用 `menghunote-sync-proto` 的 [`Argon2idParams`]（wire 真源）；本模块只做
//! 范围策略与 KDF 计算。

use crate::error::SyncError;
use crate::keys::Secret32;
use crate::random::RandomSource;
use hkdf::Hkdf;
use menghunote_sync_proto::Argon2idParams;
use sha2::Sha256;
use zeroize::Zeroizing;

/// 口令最小字节数（ADR §3）。
pub const PASSWORD_MIN_BYTES: usize = 1;
/// 口令最大字节数（ADR §3）。
pub const PASSWORD_MAX_BYTES: usize = 1024;

/// Argon2id 版本号（ADR §3：`version=0x13`）。
pub const ARGON2_VERSION_13: u16 = 0x13;
/// Argon2id 内存下限：64 MiB（单位 KiB）。
pub const M_MIN_KIB: u32 = 65_536;
/// Argon2id 内存上限：256 MiB（单位 KiB）。
pub const M_MAX_KIB: u32 = 262_144;
/// 迭代次数下限。
pub const T_MIN: u32 = 3;
/// 迭代次数上限。
pub const T_MAX: u32 = 6;
/// 并行度下限。
pub const P_MIN: u32 = 1;
/// 并行度上限。
pub const P_MAX: u32 = 4;
/// 输出长度（v1 固定 32 字节）。
pub const OUTPUT_LEN: u32 = 32;
/// salt 长度（ADR §3：random16）。
pub const SALT_LEN: usize = 16;

/// 校验口令长度。**任何 KDF 之前**先调用。
pub fn validate_password(password: &[u8]) -> Result<(), SyncError> {
    if !(PASSWORD_MIN_BYTES..=PASSWORD_MAX_BYTES).contains(&password.len()) {
        return Err(SyncError::PasswordLength {
            actual: password.len(),
            min: PASSWORD_MIN_BYTES,
            max: PASSWORD_MAX_BYTES,
        });
    }
    Ok(())
}

fn param(field: &'static str, value: u64) -> SyncError {
    SyncError::Argon2Param { field, value }
}

/// 越界检查（ADR §3 的读者区间；必须在 Argon2 之前）。
pub fn validate_argon2id_params(params: &Argon2idParams) -> Result<(), SyncError> {
    if params.version != ARGON2_VERSION_13 {
        return Err(param("version", u64::from(params.version)));
    }
    if !(M_MIN_KIB..=M_MAX_KIB).contains(&params.m_kib) {
        return Err(param("m_kib", u64::from(params.m_kib)));
    }
    if !(T_MIN..=T_MAX).contains(&params.t) {
        return Err(param("t", u64::from(params.t)));
    }
    if !(P_MIN..=P_MAX).contains(&params.p) {
        return Err(param("p", u64::from(params.p)));
    }
    if params.output_len != OUTPUT_LEN {
        return Err(param("output_len", u64::from(params.output_len)));
    }
    Ok(())
}

/// v1 写者参数（64 MiB / t=3 / p=1 / out=32），给定 salt。
#[must_use]
pub fn writer_params(salt: [u8; SALT_LEN]) -> Argon2idParams {
    Argon2idParams {
        version: ARGON2_VERSION_13,
        m_kib: M_MIN_KIB,
        t: T_MIN,
        p: P_MIN,
        salt,
        output_len: OUTPUT_LEN,
    }
}

/// v1 写者参数 + 随机 salt。
pub fn new_writer_params(rng: &mut impl RandomSource) -> Result<Argon2idParams, SyncError> {
    let salt = crate::random::array::<SALT_LEN>(rng)?;
    Ok(writer_params(salt))
}

/// HKDF-SHA256 展开为任意长度（ADR §19 的 24B ASSET nonce 等）。
pub fn hkdf_expand(key: &[u8], salt: &[u8], info: &[u8], out: &mut [u8]) -> Result<(), SyncError> {
    let hkdf = Hkdf::<Sha256>::new(Some(salt), key);
    hkdf.expand(info, out).map_err(|_| SyncError::Hkdf)
}

/// 计算 `Argon2id(password, params)` 的 32 字节输出。
///
/// 执行顺序刻意是：**口令长度 → 参数范围 → Argon2**。参数越界时不会分配内存、
/// 不会计算哈希（测试用 1TiB 参数 + 耗时断言验证）。
pub fn argon2id_output(password: &[u8], params: &Argon2idParams) -> Result<Secret32, SyncError> {
    validate_password(password)?;
    validate_argon2id_params(params)?;
    let argon_params = argon2::Params::new(
        params.m_kib,
        params.t,
        params.p,
        Some(params.output_len as usize),
    )
    .map_err(|_| SyncError::Argon2("Argon2id 参数被底层拒绝"))?;
    let hasher = argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon_params,
    );
    let mut out = Zeroizing::new([0u8; OUTPUT_LEN as usize]);
    hasher
        .hash_password_into(password, &params.salt, out.as_mut())
        .map_err(|_| SyncError::Argon2("Argon2id 计算失败"))?;
    Ok(Secret32::from_array(*out))
}

/// HKDF-SHA256：`KM = HKDF(ikm, salt, info, 32B)`。
///
/// ADR §3 的各 KEK 都用这个原语；info 由 `sync-proto` 的 `password_wrap_info` /
/// `recovery_wrap_info` 等生成（域分隔符末尾是真正的 0x00）。
pub fn hkdf32(ikm: &[u8], salt: &[u8], info: &[u8]) -> Result<Secret32, SyncError> {
    let hkdf = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut okm = Zeroizing::new([0u8; 32]);
    hkdf.expand(info, okm.as_mut())
        .map_err(|_| SyncError::Hkdf)?;
    Ok(Secret32::from_array(*okm))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::random::RandomSource;
    use std::time::Instant;

    fn params(m_kib: u32, t: u32, p: u32, output_len: u32) -> Argon2idParams {
        Argon2idParams {
            version: ARGON2_VERSION_13,
            m_kib,
            t,
            p,
            salt: [7u8; SALT_LEN],
            output_len,
        }
    }

    #[test]
    fn password_length_checked_before_anything() {
        assert!(matches!(
            validate_password(&[]),
            Err(SyncError::PasswordLength { actual: 0, .. })
        ));
        let too_long = vec![b'x'; PASSWORD_MAX_BYTES + 1];
        assert!(matches!(
            validate_password(&too_long),
            Err(SyncError::PasswordLength { actual: 1025, .. })
        ));
        assert!(validate_password(b"x").is_ok());
        assert!(validate_password(&vec![b'x'; PASSWORD_MAX_BYTES]).is_ok());
        // 口令原样 UTF-8：不 trim，前后空格是有效字节
        assert!(validate_password(b"  x  ").is_ok());
    }

    /// 越界参数必须在 KDF 之前拒绝：用 1TiB 参数 + 耗时上界证明没有真的跑 Argon2。
    #[test]
    fn out_of_range_params_rejected_before_kdf() {
        let cases = [
            Argon2idParams {
                version: 0x12,
                ..params(M_MIN_KIB, T_MIN, P_MIN, OUTPUT_LEN)
            },
            params(1 << 20, T_MIN, P_MIN, OUTPUT_LEN),
            params(M_MIN_KIB - 1, T_MIN, P_MIN, OUTPUT_LEN),
            params(M_MAX_KIB + 1, T_MIN, P_MIN, OUTPUT_LEN),
            params(M_MIN_KIB, T_MIN - 1, P_MIN, OUTPUT_LEN),
            params(M_MIN_KIB, T_MAX + 1, P_MIN, OUTPUT_LEN),
            params(M_MIN_KIB, T_MIN, 0, OUTPUT_LEN),
            params(M_MIN_KIB, T_MIN, P_MAX + 1, OUTPUT_LEN),
            params(M_MIN_KIB, T_MIN, P_MIN, 16),
        ];
        for params in cases {
            let started = Instant::now();
            let err = match argon2id_output(b"correct horse battery staple", &params) {
                Ok(_) => panic!("越界参数必须拒绝"),
                Err(err) => err,
            };
            let elapsed = started.elapsed();
            assert!(
                matches!(err, SyncError::Argon2Param { .. }),
                "必须是参数错误：{err:?}"
            );
            assert!(
                elapsed.as_millis() < 50,
                "越界参数不能真的跑 KDF（耗时 {elapsed:?}）"
            );
        }
    }

    #[test]
    fn writer_params_hit_expected_bounds() {
        let params = writer_params([1u8; SALT_LEN]);
        assert_eq!(
            (params.version, params.m_kib, params.t, params.p),
            (0x13, 65_536, 3, 1)
        );
        assert_eq!(params.output_len, 32);
        validate_argon2id_params(&params).unwrap();
    }

    /// 真实 Argon2id（64 MiB）一次，验证确定性 + 输出长度 + 边界可满足。
    #[test]
    fn argon2id_is_deterministic_and_meets_writer_params() {
        let params = writer_params([9u8; SALT_LEN]);
        let a = argon2id_output(b"pass phrase", &params).unwrap();
        let b = argon2id_output(b"pass phrase", &params).unwrap();
        assert!(a.ct_eq(&b));
        let c = argon2id_output(b"pass phrasf", &params).unwrap();
        assert!(!a.ct_eq(&c));
        // salt 不同 → 输出不同
        let other_salt = argon2id_output(b"pass phrase", &writer_params([8u8; SALT_LEN])).unwrap();
        assert!(!a.ct_eq(&other_salt));
    }

    #[test]
    fn hkdf_is_deterministic_and_domain_separated() {
        let a = hkdf32(b"ikm", b"salt", b"info-a\0").unwrap();
        let a2 = hkdf32(b"ikm", b"salt", b"info-a\0").unwrap();
        let b = hkdf32(b"ikm", b"salt", b"info-b\0").unwrap();
        let c = hkdf32(b"ikm", b"other-salt", b"info-a\0").unwrap();
        assert!(a.ct_eq(&a2));
        assert!(!a.ct_eq(&b));
        assert!(!a.ct_eq(&c));
        let other_ikm = hkdf32(b"other-ikm", b"salt", b"info-a\0").unwrap();
        assert!(!a.ct_eq(&other_ikm));
    }

    #[test]
    fn test_rng_is_deterministic() {
        let mut rng = crate::test_support::TestRng::new(42);
        let mut a = [0u8; 8];
        rng.fill(&mut a).unwrap();
        let mut rng = crate::test_support::TestRng::new(42);
        let mut b = [0u8; 8];
        rng.fill(&mut b).unwrap();
        assert_eq!(a, b);
    }
}
