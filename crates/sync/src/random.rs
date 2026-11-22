//! 随机源抽象。
//!
//! 生产路径统一走 [`OsRandom`]（`getrandom`，失败返回错误）；单元测试注入确定性实现，
//! 这样"随机"本身可复现，测试不依赖运气。
//!
//! 注意：HPKE 的临时 X25519 密钥与 nonce **不经过这里**——RFC 9180 的 nonce 由 HPKE
//! 内部的 seq 派生（ADR 0006 §4 明确要求"不要自造 nonce"）。

use crate::error::SyncError;

/// 可注入的随机源。
pub trait RandomSource {
    /// 用随机字节填满 `dst`。
    fn fill(&mut self, dst: &mut [u8]) -> Result<(), SyncError>;
}

/// 系统随机源（Windows：`BCryptGenRandom`，由 `getrandom` 封装）。
#[derive(Debug, Default, Clone, Copy)]
pub struct OsRandom;

impl RandomSource for OsRandom {
    fn fill(&mut self, dst: &mut [u8]) -> Result<(), SyncError> {
        getrandom::getrandom(dst).map_err(|_| SyncError::Random)
    }
}

/// 生成 `N` 字节随机数组。
pub fn array<const N: usize>(rng: &mut impl RandomSource) -> Result<[u8; N], SyncError> {
    let mut out = [0u8; N];
    rng.fill(&mut out)?;
    Ok(out)
}

/// 生成 `len` 字节随机向量。
pub fn vec(rng: &mut impl RandomSource, len: usize) -> Result<Vec<u8>, SyncError> {
    let mut out = vec![0u8; len];
    rng.fill(&mut out)?;
    Ok(out)
}
