//! 仅测试用的共享辅助：确定性随机源、内存槽存储。

use crate::error::SyncError;
use crate::random::RandomSource;

/// 确定性随机源（SplitMix64）。
///
/// **只用于测试**：让"随机"可复现，断言能稳定命中。不要在生产路径使用。
pub struct TestRng {
    state: u64,
}

impl TestRng {
    /// 用种子构造（不同种子产生不同流）。
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1),
        }
    }
}

impl RandomSource for TestRng {
    fn fill(&mut self, dst: &mut [u8]) -> Result<(), SyncError> {
        for chunk in dst.chunks_mut(8) {
            self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            let bytes = z.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        Ok(())
    }
}
