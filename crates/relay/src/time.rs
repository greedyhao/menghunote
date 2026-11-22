//! 时钟抽象。
//!
//! ADR 0006 §9 明确测试**不允许 sleep**：所有 TTL（challenge 60s、token 600s、lease/reservation
//! 24h、checkpoint GC 7 天）都必须能被测试精确推进，因此业务代码只通过 [`Clock`] 取当前时间。
//! 生产用 [`SystemClock`]，测试用 [`FakeClock`]。

use std::sync::atomic::{AtomicI64, Ordering};

/// 当前 Unix 时间（**秒**）。全 crate 统一用秒，与 ADR 的 TTL 单位一致。
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// 当前 Unix 秒。
    fn now_secs(&self) -> i64;
}

/// 系统时钟。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_secs(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// 可推进的假时钟（测试用；也可用于受控环境）。
#[derive(Debug, Default)]
pub struct FakeClock {
    now: AtomicI64,
}

impl FakeClock {
    /// 以指定 Unix 秒创建。
    pub fn new(now_secs: i64) -> Self {
        Self {
            now: AtomicI64::new(now_secs),
        }
    }

    /// 推进（或回拨，负值）秒数。
    pub fn advance_secs(&self, delta: i64) {
        self.now.fetch_add(delta, Ordering::SeqCst);
    }

    /// 跳到指定时刻。
    pub fn set(&self, now_secs: i64) {
        self.now.store(now_secs, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_secs(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}

/// 秒级时长常量（避免魔数散落）。
pub mod secs {
    /// 认证 challenge 有效期（ADR 0006 §4）。
    pub const CHALLENGE_TTL: i64 = 60;
    /// 访问 token 有效期（ADR 0006 §4）。
    pub const TOKEN_TTL: i64 = 600;
    /// bootstrap lease 默认有效期（ADR 0006 §7）。
    pub const BOOTSTRAP_LEASE_TTL: i64 = 24 * 60 * 60;
    /// bootstrap lease 从创建起最长有效期。
    pub const BOOTSTRAP_LEASE_MAX: i64 = 7 * 24 * 60 * 60;
    /// checkpoint 维护 reservation lease 默认有效期。
    pub const CHECKPOINT_LEASE_TTL: i64 = 24 * 60 * 60;
    /// 日志 GC 宽限（ADR 0006 §7：持久计划的 7 天 deadline）。
    pub const GC_GRACE: i64 = 7 * 24 * 60 * 60;
    /// 旧 key slot 对仍授权迁移设备的保留期（ADR 0006 §8 步 7）。
    pub const OLD_SLOT_RETENTION: i64 = 14 * 24 * 60 * 60;
    /// 从未 finalize 的暂存对象可删年龄（ADR 0006 §7）。
    pub const STAGING_RETENTION: i64 = 24 * 60 * 60;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_clock_advances_without_sleeping() {
        let clock = FakeClock::new(1_000);
        assert_eq!(clock.now_secs(), 1_000);
        clock.advance_secs(secs::CHALLENGE_TTL);
        assert_eq!(clock.now_secs(), 1_060);
        clock.set(5);
        assert_eq!(clock.now_secs(), 5);
    }

    #[test]
    fn system_clock_is_plausible() {
        let now = SystemClock.now_secs();
        // 2026-01-01 之后、2100 之前即可（不依赖真实日期精度）。
        assert!(now > 1_767_225_600);
        assert!(now < 4_102_444_800);
    }
}
