//! 极简**有界**入口限流（ADR 0014 §A：「入口先限流」）。
//!
//! 约束：
//! - **不引入新依赖**：只用 `std::time::Instant` + `Mutex`，固定窗口（`max_requests` 次 /
//!   `window_secs` 秒）即可满足"有界速率"的最低要求，不做令牌桶/分布式限流；
//! - 只挂在 **v2** `/v2/vaults`（创建票据消费）、`/v2/vaults/{id}/auth/*`（挑战/换 token）
//!   与 v2 数据面/控制面上；**v1 旧 API 完全不受影响**（ADR 0014 §A：v1 行为保留）；
//! - 限流是**实例级**的：relay 是单进程单库，攻击者先耗尽实例预算/写队列比"按租户分摊"
//!   更值得防；这里不声称提供 per-IP/per-vault 公平性（见 ADR 0014 §A 的限制）；
//! - 拒绝返回 `429 RATE_LIMITED`（`retryable = true`），错误体与其它协议错误同构，
//!   不含 vault/ticket 信息。

use std::sync::Mutex;
use std::time::Instant;

use crate::error::{Error, ErrorCode, Result};

/// 固定窗口限流参数：`window_secs` 秒内最多 `max_requests` 次。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitSpec {
    /// 窗口内允许的请求数（`>= 1`）。
    pub max_requests: u32,
    /// 窗口长度（秒，`>= 1`）。
    pub window_secs: u64,
}

impl RateLimitSpec {
    /// 构造并校验参数（非法值返回 `None`，由调用方决定是默认值还是配置错误）。
    pub const fn new(max_requests: u32, window_secs: u64) -> Option<Self> {
        if max_requests == 0 || window_secs == 0 {
            return None;
        }
        Some(Self {
            max_requests,
            window_secs,
        })
    }
}

/// 默认 v2 创建限流：**6 次/分钟**（票据本身就是受限运维物资，正常创建不会有此频率）。
pub const DEFAULT_V2_CREATE_LIMIT: RateLimitSpec = RateLimitSpec {
    max_requests: 6,
    window_secs: 60,
};

/// 默认 v2 鉴权限流：**60 次/分钟**（challenge + token 各算一次；多设备正常登录远低于此）。
pub const DEFAULT_V2_AUTH_LIMIT: RateLimitSpec = RateLimitSpec {
    max_requests: 60,
    window_secs: 60,
};

/// 默认 v2 **业务**路由限流：**3000 次/分钟**。
///
/// 业务面既有读轮询（`/log`、`/state`）也有分片上传（大资产 = 多个 part），正常单客户端
/// 突发可到数百次/分钟，因此阈值取"有界但不误伤"的 50 req/s；真正的大规模初始同步应
/// 由部署方显式调高或关闭（[`crate::config::RelayConfig::with_v2_data_limit`]）。
/// 与 auth/create 一样是**实例级**固定窗口，不声称 per-IP/per-vault 公平性。
pub const DEFAULT_V2_DATA_LIMIT: RateLimitSpec = RateLimitSpec {
    max_requests: 3000,
    window_secs: 60,
};

#[derive(Debug)]
struct Window {
    started: Instant,
    used: u32,
}

/// 一个固定窗口限流器；`spec = None` 表示关闭（测试/受控部署）。
#[derive(Debug)]
pub struct RateLimiter {
    spec: Option<RateLimitSpec>,
    window: Mutex<Window>,
}

impl RateLimiter {
    /// 关闭限流。
    pub fn disabled() -> Self {
        Self {
            spec: None,
            window: Mutex::new(Window {
                started: Instant::now(),
                used: 0,
            }),
        }
    }

    /// 按给定参数创建（`None` = 关闭）。
    pub fn new(spec: Option<RateLimitSpec>) -> Self {
        Self {
            spec,
            window: Mutex::new(Window {
                started: Instant::now(),
                used: 0,
            }),
        }
    }

    /// 当前是否启用。
    pub const fn enabled(&self) -> bool {
        self.spec.is_some()
    }

    /// 取一个配额；超限返回 `429 RATE_LIMITED`。
    ///
    /// 锁中毒时**降级为拒绝**（限流器状态已不可信，宁可 429 也不静默放行）。
    pub fn acquire(&self) -> Result<()> {
        let Some(spec) = self.spec else {
            return Ok(());
        };
        let now = Instant::now();
        let mut window = match self.window.lock() {
            Ok(window) => window,
            Err(poisoned) => poisoned.into_inner(),
        };
        if now.saturating_duration_since(window.started).as_secs() >= spec.window_secs {
            *window = Window {
                started: now,
                used: 0,
            };
        }
        if window.used >= spec.max_requests {
            return Err(Error::Api(
                crate::error::ApiError::new(ErrorCode::RateLimited).retryable(true),
            ));
        }
        window.used += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_limiter_never_rejects() {
        let limiter = RateLimiter::disabled();
        assert!(!limiter.enabled());
        for _ in 0..10_000 {
            limiter.acquire().expect("disabled limiter must pass");
        }
    }

    #[test]
    fn limiter_rejects_after_window_budget_is_used() {
        let limiter = RateLimiter::new(RateLimitSpec::new(2, 60));
        assert!(limiter.enabled());
        limiter.acquire().expect("first");
        limiter.acquire().expect("second");
        let err = limiter.acquire().expect_err("third must be limited");
        assert_eq!(err.code(), Some(ErrorCode::RateLimited));
        let api = match err {
            Error::Api(api) => api,
            _ => panic!("expected Api error"),
        };
        assert_eq!(api.code.status(), 429);
        assert!(api.retryable);
    }

    #[test]
    fn spec_rejects_zero_limits() {
        assert!(RateLimitSpec::new(0, 60).is_none());
        assert!(RateLimitSpec::new(1, 0).is_none());
        assert!(RateLimitSpec::new(1, 1).is_some());
    }
}
