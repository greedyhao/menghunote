//! dev-sync 的**运行时门禁**（ADR 0009「门禁」）。
//!
//! 编译期门禁是 `--features dev-sync`（`Cargo.toml` 里的 `dep:menghunote-sync`）；
//! 本文件是第二道**运行时**门禁，两道都满足才会挂上面板：
//!
//! 1. `--dev-sync` / `--dev-sync-create` / `--dev-sync-join` / `--dev-sync-info` /
//!    `--dev-sync-publish`，或 `MENGHUNOTE_DEV_SYNC=1`（`1`/`true`/`yes`）才算"请求启用"；
//!    两个初始化 flag 与两个 CLI 子命令 flag 隐含启用（切片 4c-1 / 4c-3）；
//! 2. 必须**显式**设置 `MENGHUNOTE_DEV_VAULT`——dev-sync 的两个实例各自要一个独立库，
//!    复用默认 dev vault 会让两个实例共用同一个库目录，从而共用同一份
//!    `dev-identity.bin`（slice 2 起 peer 来自身份文件）与同一条 CRDT 真源，
//!    两个实例必然撞 `peer_id`；所以这里**不静默回退**默认路径；
//! 3. relay 只允许**明文 loopback**：`http://127.0.0.1` / `http://[::1]` / `http://localhost`
//!    （端口任意）。非 loopback / https / 带 userinfo 一律明确报错退出。
//!
//! 解析逻辑全部是纯函数（args + 一个 env 读取闭包），不直接读 `std::env`：
//! 这样门禁本身可以确定性单测，也不需要在一个进程里改环境变量（`cargo test` 多线程下
//! 改 env 是不安全的）。

use std::fmt;
use std::path::PathBuf;

use menghunote_sync::error::SyncError;
use menghunote_sync::relay_policy::{parse_relay_url, RelayMode, RelayOrigin, RelayUrlError};

use super::service::ServiceConfig;

/// 启用开关（命令行）。
pub const ENABLE_FLAG: &str = "--dev-sync";
/// Create 模式开关（隐含启用 dev-sync；切片 4c-1）。
pub const CREATE_FLAG: &str = "--dev-sync-create";
/// Join 模式开关（隐含启用 dev-sync；切片 4c-1）。
pub const JOIN_FLAG: &str = "--dev-sync-join";
/// Info 子命令开关（隐含启用 dev-sync；切片 4c-3：只打印公开标识，不开窗/不联网）。
pub const INFO_FLAG: &str = "--dev-sync-info";
/// Publish 子命令开关（隐含启用 dev-sync；切片 4c-3：发布初始基线，不开窗）。
pub const PUBLISH_FLAG: &str = "--dev-sync-publish";
/// 启用开关（环境变量）。
pub const ENABLE_ENV: &str = "MENGHUNOTE_DEV_SYNC";
/// 实例库路径（启用时**必须**显式设置）。
pub const VAULT_ENV: &str = "MENGHUNOTE_DEV_VAULT";
/// relay 基地址（可选；默认 [`DEFAULT_RELAY`]）。
pub const RELAY_ENV: &str = "MENGHUNOTE_DEV_SYNC_RELAY";
/// 默认 relay：与 `crates/relay` 的 dev listen 默认值一致（`127.0.0.1:18080`）。
pub const DEFAULT_RELAY: &str = "http://127.0.0.1:18080";

/// 门禁结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// 运行时未启用：面板不渲染、**零行为变化**（即使二进制带了 feature）。
    Disabled,
    /// 已启用：面板/worker 用这份配置。
    Enabled(DevSyncConfig),
}

impl Gate {
    /// 是否启用。
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled(_))
    }

    /// 已启用时取配置。
    #[must_use]
    pub const fn config(&self) -> Option<&DevSyncConfig> {
        match self {
            Self::Disabled => None,
            Self::Enabled(config) => Some(config),
        }
    }
}

/// 门禁产出的配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevSyncConfig {
    /// 本实例的独立 vault 文件路径（来自 `MENGHUNOTE_DEV_VAULT`，必须显式）。
    pub vault: PathBuf,
    /// relay 接入模式（运行时门禁只产出 `local_dev`；`remote_tls` 由公开配置/设置给出）。
    pub relay_mode: RelayMode,
    /// worker 配置（模式 + relay + 操作 deadline + 传输超时）。
    pub service: ServiceConfig,
}

impl DevSyncConfig {
    /// 面板显示的 relay 地址（公开地址，不含 secret）。
    #[must_use]
    pub fn relay(&self) -> &str {
        &self.service.relay
    }

    /// relay 模式（`local_dev` / `remote_tls`；公开配置）。
    #[must_use]
    pub const fn relay_mode(&self) -> RelayMode {
        self.relay_mode
    }

    /// 本配置的 canonical origin（唯一权威 parser 校验）。
    pub fn origin(&self) -> Result<RelayOrigin, SyncError> {
        parse_relay_url(&self.service.relay, self.relay_mode).map_err(Into::into)
    }
}

/// 门禁拒绝的原因（每一条都对应"明确报错退出、不静默回退"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateError {
    /// 启用了 dev-sync 但没显式设置 `MENGHUNOTE_DEV_VAULT`（或为空）。
    MissingVault,
    /// relay 不是允许的 loopback 明文。
    RelayNotLoopback(String),
    /// relay 语法非法（端口/路径/userinfo 等；transport 的策略兜底）。
    RelayInvalid(String),
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingVault => write!(
                f,
                "启用 dev-sync 必须显式设置 {VAULT_ENV}=<独立实例库路径>：\
                 两个实例不能共用同一个 dev vault（会撞 peer_id），因此不回退到默认路径"
            ),
            Self::RelayNotLoopback(url) => write!(
                f,
                "dev-sync 的 relay 只允许明文 loopback（http://127.0.0.1 / http://[::1] / \
                 http://localhost，端口任意），收到 {url:?}；非 loopback 或 https 一律拒绝"
            ),
            Self::RelayInvalid(detail) => {
                write!(f, "dev-sync 的 relay 地址非法：{detail}")
            }
        }
    }
}

impl std::error::Error for GateError {}

/// 是否"请求启用"：`--dev-sync` / `--dev-sync-create` / `--dev-sync-join` /
/// `--dev-sync-info` / `--dev-sync-publish` / `MENGHUNOTE_DEV_SYNC=1`。
///
/// 两个初始化 flag 与两个 CLI 子命令 flag 也隐含启用（否则用户只给 `--dev-sync-create`
/// 会被当成"未启用"、静默回退默认 vault，那正是切片 4c-1 要消灭的隐式路径）。
///
/// 只看开关，不做配置校验（配置校验在 [`resolve`]）。
#[must_use]
pub fn requested(
    args: impl IntoIterator<Item = String>,
    env: impl Fn(&str) -> Option<String>,
) -> bool {
    let by_flag = args.into_iter().any(|arg| {
        arg == ENABLE_FLAG
            || arg == CREATE_FLAG
            || arg == JOIN_FLAG
            || arg == INFO_FLAG
            || arg == PUBLISH_FLAG
    });
    if by_flag {
        return true;
    }
    env(ENABLE_ENV).is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
}

/// 解析运行时门禁（纯函数：args + env 读取闭包）。
///
/// 未请求启用 → `Ok(Gate::Disabled)`（**不是错误**：带 feature 的普通启动也走这条路）。
/// 请求启用但配置不合法 → `Err(..)`，调用方必须明确报错退出。
pub fn resolve(
    args: impl IntoIterator<Item = String>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Gate, GateError> {
    if !requested(args, &env) {
        return Ok(Gate::Disabled);
    }
    // 1. 实例库：必须显式且非空（不静默回退默认 dev vault）。
    let vault = env(VAULT_ENV)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or(GateError::MissingVault)?;
    // 2. relay：默认 loopback；显式给了就严格校验。
    let relay = env(RELAY_ENV)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_RELAY.to_owned());
    validate_relay(&relay)?;
    let service = ServiceConfig::new(relay.clone());
    // 3. 再让 transport 的策略兜一遍（端口/路径/userinfo/策略），错误里不回显口令。
    service.build_transport().map_err(|error| {
        GateError::RelayInvalid(format!("{}（{error}）", redact_userinfo(&relay)))
    })?;
    Ok(Gate::Enabled(DevSyncConfig {
        vault: PathBuf::from(vault),
        relay_mode: service.mode,
        service,
    }))
}

/// 去掉 URL 里的 userinfo（`scheme://user:pass@host` → `scheme://host`）。
///
/// 错误文案/日志**不得**出现口令（ADR 0009「不记录 secret/恢复码」）：即使调用方把
/// `http://user:secret@127.0.0.1:18080` 写进 `MENGHUNOTE_DEV_SYNC_RELAY`，我们拒绝它时
/// 也只回显脱敏后的地址。
fn redact_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let cut = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(cut);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_userinfo, host)| host);
    format!("{scheme}://{host}{tail}")
}

/// 公开的 relay 校验入口（向导/配置加载复用同一套策略）。
///
/// 为什么公开：`onboarding` 的 Create 表单与已保存实例加载都必须拒绝非 loopback 的
/// relay，但两处都不应该复制一份 URL 解析（策略只能有一份）。
pub fn validate_relay_url(url: &str) -> Result<(), GateError> {
    validate_relay(url)
}

/// relay 必须是 `http://` + **显式 loopback 白名单**（`localhost` / `127.0.0.1` / `[::1]`）。
///
/// 解析逻辑唯一权威在 `menghunote-sync::relay_policy`（ADR 0013 §1）：这里不再复制
/// URL 语法，只把错误映射成门禁的两类文案（非 loopback / 地址非法）。白名单不是
/// `is_loopback()`：`127.0.0.2`、`[::2]` 之类也不会被接受。
fn validate_relay(url: &str) -> Result<(), GateError> {
    let shown = redact_userinfo(url);
    match parse_relay_url(url, RelayMode::LocalDev) {
        Ok(_) => Ok(()),
        Err(error) => Err(match error {
            RelayUrlError::SchemeNotAllowed { .. } | RelayUrlError::HostNotAllowed(_) => {
                GateError::RelayNotLoopback(shown)
            }
            other => GateError::RelayInvalid(format!("{shown}（{other}）")),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 用固定表构造 env 闭包（不改进程环境，测试可并行）。
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn nothing_requested_means_disabled() {
        let gate = resolve(Vec::<String>::new(), env_of(&[])).expect("未启用不是错误");
        assert_eq!(gate, Gate::Disabled);
        // 只要没请求启用，缺 vault/坏 relay 都不影响默认行为（零行为变化）。
        let gate = resolve(
            Vec::<String>::new(),
            env_of(&[(RELAY_ENV, "http://relay.example:1")]),
        )
        .expect("未启用时不该校验配置");
        assert_eq!(gate, Gate::Disabled);
    }

    #[test]
    fn flag_or_env_requests_enable() {
        assert!(requested(["--dev-sync".to_owned()], env_of(&[])));
        // 切片 4c-1：两个初始化 flag 本身也隐含启用 dev-sync。
        assert!(requested([CREATE_FLAG.to_owned()], env_of(&[])));
        assert!(requested([JOIN_FLAG.to_owned()], env_of(&[])));
        // 切片 4c-3：两个不开窗的 CLI 子命令同样隐含启用。
        assert!(requested([INFO_FLAG.to_owned()], env_of(&[])));
        assert!(requested([PUBLISH_FLAG.to_owned()], env_of(&[])));
        assert!(requested(
            Vec::<String>::new(),
            env_of(&[(ENABLE_ENV, "1")])
        ));
        assert!(requested(
            Vec::<String>::new(),
            env_of(&[(ENABLE_ENV, "TRUE")])
        ));
        assert!(!requested(
            Vec::<String>::new(),
            env_of(&[(ENABLE_ENV, "0")])
        ));
        assert!(!requested(["app".to_owned()], env_of(&[])));
    }

    #[test]
    fn enabling_requires_an_explicit_vault() {
        let error = resolve(["--dev-sync".to_owned()], env_of(&[])).expect_err("缺 vault 必须拒绝");
        assert_eq!(error, GateError::MissingVault);
        // 空字符串等同于没设置（不能靠 `MENGHUNOTE_DEV_VAULT=` 混过去）。
        let error = resolve(["--dev-sync".to_owned()], env_of(&[(VAULT_ENV, "   ")]))
            .expect_err("空 vault 必须拒绝");
        assert_eq!(error, GateError::MissingVault);
    }

    #[test]
    fn relay_must_be_plaintext_loopback() {
        let vault = [(VAULT_ENV, "/tmp/menghunote-dev-sync-a/vault.sqlite3")];
        for good in [
            "http://127.0.0.1:18080",
            "http://localhost:18080",
            "http://[::1]:18080",
            "http://127.0.0.1:1",
        ] {
            let mut pairs = vault.to_vec();
            pairs.push((RELAY_ENV, good));
            let gate = resolve(["--dev-sync".to_owned()], env_of(&pairs))
                .unwrap_or_else(|error| panic!("{good} 应被接受：{error}"));
            assert!(gate.is_enabled(), "{good} 必须启用");
            assert_eq!(gate.config().expect("配置").relay(), good);
        }
        for bad in [
            "http://relay.example:18080",
            "http://192.168.1.9:18080",
            "https://localhost:18080",
            "http://0.0.0.0:18080",
            "http://user:pass@127.0.0.1:18080",
            "relay.example:18080",
            "http://127.0.0.1:18080/api",
        ] {
            let mut pairs = vault.to_vec();
            pairs.push((RELAY_ENV, bad));
            let error = match resolve(["--dev-sync".to_owned()], env_of(&pairs)) {
                Ok(gate) => panic!("{bad} 必须被拒绝，实际启用：{gate:?}"),
                Err(error) => error,
            };
            assert!(
                matches!(
                    error,
                    GateError::RelayNotLoopback(_) | GateError::RelayInvalid(_)
                ),
                "{bad} 的错误类型不对：{error:?}"
            );
            // 错误文案不得回显口令。
            assert!(
                !error.to_string().contains("pass"),
                "错误不得回显 userinfo：{error}"
            );
        }
    }

    #[test]
    fn userinfo_is_redacted_from_error_messages() {
        assert_eq!(
            redact_userinfo("http://user:pass@127.0.0.1:18080"),
            "http://127.0.0.1:18080"
        );
        assert_eq!(
            redact_userinfo("https://alice@relay.example/v1?x=1"),
            "https://relay.example/v1?x=1"
        );
        // 没有 userinfo 时原样返回。
        assert_eq!(
            redact_userinfo("http://127.0.0.1:18080"),
            "http://127.0.0.1:18080"
        );
        assert_eq!(redact_userinfo("not-a-url"), "not-a-url");
    }

    #[test]
    fn default_relay_is_loopback_and_missing_relay_uses_it() {
        let gate = resolve(
            ["--dev-sync".to_owned()],
            env_of(&[(VAULT_ENV, "/tmp/menghunote-dev-sync-b/vault.sqlite3")]),
        )
        .expect("默认 relay 必须可用");
        assert_eq!(gate.config().expect("配置").relay(), DEFAULT_RELAY);
        // 环境变量开关同样生效。
        let gate = resolve(
            Vec::<String>::new(),
            env_of(&[
                (ENABLE_ENV, "1"),
                (VAULT_ENV, "/tmp/menghunote-dev-sync-b/vault.sqlite3"),
            ]),
        )
        .expect("env 开关必须生效");
        assert!(gate.is_enabled());
    }
}
