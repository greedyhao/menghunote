//! relay 模式/URL 的**构建期能力闸**（ADR 0013 §1）。
//!
//! 权威解析在 `menghunote-sync::relay_policy`（生产/开发共用一份）；本模块只回答
//! "**这次构建**允许哪些模式"：
//!
//! - `remote_tls`：所有含 `sync` feature 的构建都允许（生产入口）；
//! - `local_dev`：**只有** `dev-sync` 构建允许。默认构建根本没有本模块（`sync`
//!   feature 关闭时连 sync 依赖都不进图），`sync`（非 dev）构建里显式拒绝
//!   `local_dev`，不会因为环境/配置被手改而放宽。
//!
//! 设置/向导在 S1 仍只使用 `local_dev`（不开放 remote UI），但 parser 已有 remote
//! 分支与负例，S2 的 wiring 可直接复用 [`parse_for_build`]。

use menghunote_sync::error::SyncError;
use menghunote_sync::relay_policy::{parse_relay_url, RelayMode, RelayOrigin};

/// 本次构建允许的 relay 模式。
///
/// `local_dev` 的许可来自编译期 feature，而不是运行时环境变量。
#[must_use]
pub const fn mode_allowed(mode: RelayMode) -> bool {
    match mode {
        RelayMode::LocalDev => cfg!(feature = "dev-sync"),
        RelayMode::RemoteTls => true,
    }
}

/// 按构建能力解析 relay URL；未开放的模式是明确错误（不回退、不默认）。
pub fn parse_for_build(raw: &str, mode: RelayMode) -> Result<RelayOrigin, SyncError> {
    if !mode_allowed(mode) {
        return Err(SyncError::ConfigInvalid(format!(
            "本次构建不开放 relay 模式 {}：local_dev 只在 `dev-sync` 构建中允许",
            mode.as_str()
        )));
    }
    Ok(parse_relay_url(raw, mode)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_capability_is_compile_time_not_runtime() {
        // 生产模式在所有含 sync 的构建里都允许。
        parse_for_build("https://relay.example", RelayMode::RemoteTls).expect("remote_tls 允许");
        assert!(mode_allowed(RelayMode::RemoteTls));

        #[cfg(feature = "dev-sync")]
        {
            assert!(
                mode_allowed(RelayMode::LocalDev),
                "dev-sync 构建允许 local_dev"
            );
            parse_for_build("http://127.0.0.1:18080", RelayMode::LocalDev)
                .expect("dev-sync 构建的 local_dev 合法");
        }
        #[cfg(not(feature = "dev-sync"))]
        {
            assert!(
                !mode_allowed(RelayMode::LocalDev),
                "非 dev-sync 构建必须关闭 local_dev"
            );
            let error = parse_for_build("http://127.0.0.1:18080", RelayMode::LocalDev)
                .expect_err("非 dev-sync 构建必须拒绝 local_dev");
            assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
        }
    }
}
