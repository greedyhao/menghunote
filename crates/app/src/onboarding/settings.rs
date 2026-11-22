//! 同步设置面板的**状态与校验**（ADR 0011 S4；ADR 0013 S3 增加 relay 模式/安全指纹）。
//!
//! ## 为什么单独一层
//!
//! 设置面板只做四件事：列出公开配置、切换 `active_instance`、修改 active 实例的
//! `relay_mode`/`relay_url`、展示**公开**安全指纹。四件事都不涉及秘密材料，也不触碰
//! Vault/worker：
//!
//! - 列表直接读 [`DevSyncProfile`] 的公开条目（`instance_id` / `relay_mode` / `relay_url` /
//!   `vault_path` / `vault_id` / `device_id`）；
//! - 写配置走既有的 [`ProfileStore::save`]（同目录临时文件 + fsync + rename），
//!   失败时旧配置原样保留；
//! - **不热切换**：保存后只改公开配置，当前 `AppShell`/Vault/worker 保持到重启。
//!
//! 本文件只放状态与纯函数校验；渲染/动作在 [`super::app_root`]（它持有 `AppRoot`
//! 的上下文），避免在 UI 文件里再抄一份 URL 策略。
//!
//! ## 模式与 URL（ADR 0013 §1）
//!
//! 两者都走**唯一权威 parser**（`menghunote-sync::relay_policy::parse_relay_url`）：
//! `local_dev` 只接受明文 loopback，`remote_tls` 只接受 HTTPS + canonical origin；
//! userinfo/query/fragment/非根路径一律拒绝。`remote_tls` 是**显式选择**（需要用户
//! 勾选"远程服务器/HTTPS"确认），默认仍是 `local_dev`。
//!
//! ## 安全边界
//!
//! 设置面板**没有** setup secret / 恢复码 / root / manager seed 字段：这些材料
//! 根本不在 [`InstanceEntry`] 里（[`super::layout`] 的类型上就写不进去）。允许
//! 复制的是 `vault_id` / `device_id` / 库路径与 pin 指纹这类公开值，并明确提示不要
//! 分享秘密。安全指纹只读，**不提供**「重置 pin / 强制信任」按钮。

use gpui::{Entity, Subscription};

use super::fingerprint::FingerprintState;
use super::input::FormInput;
use super::layout::{InstanceEntry, RelayMode};

/// 设置面板运行期状态（**只有公开信息**）。
///
/// 不实现 `Debug`/`Clone`：`FormInput` 内部持有输入缓冲，且面板只属于 `AppRoot`
/// 一份；没有把它复制出去的需求。
pub(crate) struct SettingsState {
    /// 打开面板/切换 active 时 active 实例的快照（`None` = 配置里没有 active）。
    pub(crate) active: Option<InstanceEntry>,
    /// 正在编辑的 relay 模式（保存时才写回配置）。
    pub(crate) mode: RelayMode,
    /// 远程模式确认（ADR 0013 §1：`remote_tls` 必须显式同意"远程服务器/HTTPS"）。
    pub(crate) remote_consent: bool,
    /// relay 输入框（`settings-relay`）。
    pub(crate) relay: Entity<FormInput>,
    /// 安全指纹（打开面板时的只读快照；`None` = 没有打开实例，无法读取）。
    pub(crate) fingerprint: Option<FingerprintState>,
    /// 安全暂停类别（打开面板时的快照；渲染时尽量读实时值）。
    pub(crate) security: Option<crate::dev_sync::SecurityCategory>,
    /// 内联错误（`settings-error`；脱敏后的公开文案）。
    pub(crate) error: Option<String>,
    /// 内联提示（`settings-notice`；含「重启后生效」）。
    pub(crate) notice: Option<String>,
    /// 打开面板后是否要把焦点放到 relay 输入框（渲染时消费）。
    pub(crate) focus_pending: bool,
    /// Enter 提交的订阅（随面板一起 drop）。
    pub(crate) _subs: Vec<Subscription>,
}

/// 本构建是否提供 `remote_tls` 选项（ADR 0013 §1：模式能力来自编译期，不看环境变量）。
///
/// `onboarding` 只在 `dev-sync` feature 下编译（它隐含 `sync`），因此当前构建恒为
/// `true`；保留这个单一判定点，默认构建没有任何 sync UI（选项在类型上不存在）。
#[must_use]
pub(crate) const fn remote_option_available() -> bool {
    cfg!(feature = "sync") && crate::sync_gate::mode_allowed(RelayMode::RemoteTls)
}

/// 校验设置面板里的 relay 编辑值；成功返回**canonical origin**。
///
/// 与运行时门禁/worker 共用唯一权威 parser（ADR 0013 §1），不复制 URL 解析：
///
/// 1. 空值明确报「不能为空」；
/// 2. 模式与 scheme 必须匹配（local_dev → `http://` + loopback 白名单；
///    remote_tls → `https://` + DNS/IP）；
/// 3. 路径/query/fragment/userinfo/端口一并由 parser 拒绝；
/// 4. 解析成功返回 canonical origin（默认端口省略、无尾斜杠）。
///
/// 错误文案**不打印原始 URL 的 userinfo**：parser 只含 scheme/host 等公开定位信息。
pub(crate) fn validate_relay_edit(raw: &str, mode: RelayMode) -> Result<String, String> {
    let relay = raw.trim();
    if relay.is_empty() {
        return Err(match mode {
            RelayMode::LocalDev => {
                "relay 不能为空：local_dev 只支持明文 loopback（例如 http://127.0.0.1:18080）"
                    .to_owned()
            }
            RelayMode::RemoteTls => {
                "relay 不能为空：remote_tls 需要 HTTPS 服务器（例如 https://relay.example）"
                    .to_owned()
            }
        });
    }
    let origin =
        crate::sync_gate::parse_for_build(relay, mode).map_err(|error| error.to_string())?;
    Ok(origin.as_str())
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
            let relay = validate_relay_edit(good, RelayMode::LocalDev)
                .unwrap_or_else(|error| panic!("{good:?}：{error}"));
            assert_eq!(relay, good.trim());
        }
    }

    /// 合法 remote：HTTPS + DNS/IP，canonical 化（默认端口/尾斜杠）。
    #[test]
    fn remote_relays_require_https_and_canonical_origin() {
        for (good, canonical) in [
            ("https://relay.example", "https://relay.example"),
            ("https://relay.example:8443/", "https://relay.example:8443"),
            ("https://203.0.113.7", "https://203.0.113.7"),
            ("https://[2001:db8::1]:443", "https://[2001:db8::1]"),
        ] {
            assert_eq!(
                validate_relay_edit(good, RelayMode::RemoteTls).expect(good),
                canonical
            );
        }
    }

    /// 空值/模式不符/userinfo/路径都拒绝；错误不回显 userinfo。
    #[test]
    fn invalid_relays_are_rejected_without_echoing_credentials() {
        for (bad, mode, needle) in [
            ("", RelayMode::LocalDev, "不能为空"),
            ("https://127.0.0.1:18080", RelayMode::LocalDev, "http://"),
            (
                "http://relay.example:18080",
                RelayMode::LocalDev,
                "loopback",
            ),
            ("http://192.168.1.9:18080", RelayMode::LocalDev, "loopback"),
            ("http://0.0.0.0:18080", RelayMode::LocalDev, "loopback"),
            ("http://127.0.0.2:18080", RelayMode::LocalDev, "loopback"),
            (
                "http://user:pass@127.0.0.1:18080",
                RelayMode::LocalDev,
                "userinfo",
            ),
            (
                "http://127.0.0.1:18080/api",
                RelayMode::LocalDev,
                "canonical origin",
            ),
            ("http://relay.example", RelayMode::RemoteTls, "https"),
            (
                "https://user:pass@relay.example",
                RelayMode::RemoteTls,
                "userinfo",
            ),
            (
                "https://relay.example/api",
                RelayMode::RemoteTls,
                "canonical origin",
            ),
            ("https://relay.example?x=1", RelayMode::RemoteTls, "query"),
            (
                "https://relay.example#frag",
                RelayMode::RemoteTls,
                "fragment",
            ),
        ] {
            let error = validate_relay_edit(bad, mode).expect_err("必须拒绝");
            assert!(error.contains(needle), "{bad:?} → {error}");
            if bad.contains("pass") {
                assert!(!error.contains("pass"), "不得回显 userinfo：{error}");
                assert!(!error.contains("user:"), "不得回显 userinfo：{error}");
            }
        }
    }

    #[test]
    fn remote_option_is_compile_time_capability() {
        // dev-sync 构建隐含 sync：remote_tls 一定可用；local_dev 只在 dev-sync 构建可用。
        assert!(remote_option_available());
        assert!(crate::sync_gate::mode_allowed(RelayMode::RemoteTls));
    }

    #[test]
    fn short_public_ids_only_shorten_long_values() {
        assert_eq!(short_public_id("inst-a"), "inst-a");
        assert_eq!(short_public_id("0123456789"), "0123456789");
        assert_eq!(short_public_id("0123456789abcdef"), "01234567…");
        assert_eq!(short_public_id(""), "");
    }
}
