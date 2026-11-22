//! 同步设置面板的**状态与校验**（ADR 0011 S4）。
//!
//! ## 为什么单独一层
//!
//! 设置面板只做三件事：列出公开配置、切换 `active_instance`、修改 active 实例的
//! `relay_url`。三件事都不涉及秘密材料，也不触碰 Vault/worker：
//!
//! - 列表直接读 [`DevSyncProfile`] 的公开条目（`instance_id` / `relay_url` /
//!   `vault_path` / `vault_id` / `device_id`）；
//! - 写配置走既有的 [`ProfileStore::save`]（同目录临时文件 + fsync + rename），
//!   失败时旧配置原样保留；
//! - **不热切换**：保存后只改公开配置，当前 `AppShell`/Vault/worker 保持到重启。
//!
//! 本文件只放状态与纯函数校验；渲染/动作在 [`super::app_root`]（它持有 `AppRoot`
//! 的上下文），避免在 UI 文件里再抄一份 URL 策略。
//!
//! ## 安全边界
//!
//! 设置面板**没有** setup secret / 恢复码 / root / manager seed 字段：这些材料
//! 根本不在 [`InstanceEntry`] 里（[`super::layout`] 的类型上就写不进去）。允许
//! 复制的是 `vault_id` / `device_id` / 库路径这类公开值，并明确提示不要分享秘密。

use gpui::{Entity, Subscription};

use crate::dev_sync::gate::validate_relay_url;
use crate::dev_sync::service::ServiceConfig;

use super::input::FormInput;
use super::layout::InstanceEntry;

/// 设置面板运行期状态（**只有公开信息**）。
///
/// 不实现 `Debug`/`Clone`：`FormInput` 内部持有输入缓冲，且面板只属于 `AppRoot`
/// 一份；没有把它复制出去的需求。
pub(crate) struct SettingsState {
    /// 打开面板/切换 active 时 active 实例的快照（`None` = 配置里没有 active）。
    pub(crate) active: Option<InstanceEntry>,
    /// relay 输入框（`settings-relay`）。
    pub(crate) relay: Entity<FormInput>,
    /// 内联错误（`settings-error`；脱敏后的公开文案）。
    pub(crate) error: Option<String>,
    /// 内联提示（`settings-notice`；含「重启后生效」）。
    pub(crate) notice: Option<String>,
    /// 打开面板后是否要把焦点放到 relay 输入框（渲染时消费）。
    pub(crate) focus_pending: bool,
    /// Enter 提交的订阅（随面板一起 drop）。
    pub(crate) _subs: Vec<Subscription>,
}

/// 校验设置面板里的 relay 编辑值；成功返回**去掉首尾空白后**的地址。
///
/// 与运行时门禁（[`crate::dev_sync::gate::validate_relay_url`] + transport 策略）
/// 用同一套规则，不复制 URL 解析：
///
/// 1. 空值明确报「不能为空」；
/// 2. 必须 `http://` + loopback 主机（`127.0.0.1` / `::1` / `localhost`）；
/// 3. 再过一遍 transport 策略（端口/路径/query/userinfo 等），与
///    [`crate::dev_sync::gate::resolve`] 的兜底一致。
///
/// 错误文案**不打印原始 URL 的 userinfo**：`validate_relay` 已负责脱敏
/// （`scheme://user:pass@host` → `scheme://host`）；transport 错误只给固定说明，
/// 不回显输入。
pub(crate) fn validate_relay_edit(raw: &str) -> Result<String, String> {
    let relay = raw.trim();
    if relay.is_empty() {
        return Err(
            "relay 不能为空：当前 dev-sync 只支持明文 loopback（例如 http://127.0.0.1:18080）"
                .to_owned(),
        );
    }
    validate_relay_url(relay).map_err(|error| error.to_string())?;
    // 门禁的第二道兜底：`http://127.0.0.1:18080/api` 这类主机合法但路径非法的地址，
    // 只靠 `validate_relay` 会漏掉。
    ServiceConfig::new(relay.to_owned())
        .build_transport()
        .map_err(|_| {
            "relay 未通过传输策略校验：只允许 http://127.0.0.1 / http://localhost / \
             http://[::1] 加可选端口，不要带路径、查询或用户信息"
                .to_owned()
        })?;
    Ok(relay.to_owned())
}

/// 公开标识的短展示（10 个字符以内原样，否则前 8 个 + `…`）。
///
/// 只用于界面展示；复制按钮给的是完整值。
#[must_use]
pub(crate) fn short_public_id(value: &str) -> String {
    if value.chars().count() <= 10 {
        value.to_owned()
    } else {
        let head: String = value.chars().take(8).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合法 loopback（含 IPv6/端口边界）通过，并去掉首尾空白。
    #[test]
    fn loopback_relays_are_accepted_and_trimmed() {
        for good in [
            "http://127.0.0.1:18080",
            " http://localhost:18080 ",
            "http://[::1]:18080",
            "http://127.0.0.1:1",
        ] {
            let relay =
                validate_relay_edit(good).unwrap_or_else(|error| panic!("{good:?}：{error}"));
            assert_eq!(relay, good.trim());
        }
    }

    /// 空值/非 loopback/https/userinfo/路径都拒绝；错误不回显 userinfo。
    #[test]
    fn invalid_relays_are_rejected_without_echoing_credentials() {
        for (bad, needle) in [
            ("", "不能为空"),
            ("https://127.0.0.1:18080", "loopback"),
            ("http://relay.example:18080", "loopback"),
            ("http://192.168.1.9:18080", "loopback"),
            ("http://0.0.0.0:18080", "loopback"),
            ("http://user:pass@127.0.0.1:18080", "loopback"),
            ("http://127.0.0.1:18080/api", "传输策略"),
        ] {
            let error = validate_relay_edit(bad).expect_err("必须拒绝");
            assert!(error.contains(needle), "{bad:?} → {error}");
            if bad.contains("pass") {
                assert!(!error.contains("pass"), "不得回显 userinfo：{error}");
                assert!(!error.contains("user:"), "不得回显 userinfo：{error}");
            }
        }
    }

    #[test]
    fn short_public_ids_only_shorten_long_values() {
        assert_eq!(short_public_id("inst-a"), "inst-a");
        assert_eq!(short_public_id("0123456789"), "0123456789");
        assert_eq!(short_public_id("0123456789abcdef"), "01234567…");
        assert_eq!(short_public_id(""), "");
    }
}
