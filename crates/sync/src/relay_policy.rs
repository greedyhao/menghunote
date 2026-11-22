//! relay **模式与 URL 的唯一权威策略**（ADR 0013 §1/§6）。
//!
//! 生产与开发共用同一套解析/规范化：
//!
//! | 模式 | scheme | 主机 | 额外限制 |
//! | --- | --- | --- | --- |
//! | [`RelayMode::LocalDev`] | 只允许 `http` | **显式白名单** `localhost` / `127.0.0.1` / `[::1]` | 必须显式端口可选；拒绝 userinfo/query/fragment/非根路径 |
//! | [`RelayMode::RemoteTls`] | 只允许 `https` | DNS 名或 IP（含 IPv6 字面量） | 拒绝 userinfo/query/fragment/非根 base path/非法端口 |
//!
//! 关键纪律（ADR 0013「前提校正」）：
//!
//! - `local_dev` **不** 用 `IpAddr::is_loopback()` 这种宽泛判定（`::2`、127.0.0.2 之类的
//!   loopback 变体不被接受），也不读环境变量放宽生产策略；
//! - 明文只在 [`RelayMode::LocalDev`] 下合法，且词法上必须是 `http://`；
//! - 输出永远是 canonical origin（`scheme://host[:port]`，默认端口省略），
//!   这样配置、日志、pin 的显示与比较只有一种形态；
//! - 生产传输显式用 [`TransportPolicy::production`]，**绝不**调用会读环境变量的
//!   `StdTcpTransport::new`；`local_dev` 分支由调用方（app 的 `dev-sync` feature）闸控。
//!
//! 本模块还提供生产 client 的**受限构造入口** [`open_pinned_client`]：它只接受
//! `remote_tls`，并要求 vault 里已有 durable `sync.control_pin`；缺失时返回
//! [`SyncError::NeedsTrust`]，**不会**退化成 unpinned/local-dev 消费控制链。
//! S1 不把它接到 worker/onboarding（S2 统一 wiring）。

use std::fmt;
use std::net::Ipv6Addr;

use menghunote_storage::Vault;

use crate::control_pin::load_control_pin;
use crate::device::DeviceSigningKey;
use crate::error::SyncError;
use crate::http_relay::{HttpRelayClient, HttpRelayConfig};
use crate::http_transport::{StdTcpTransport, TransportPolicy, TransportTimeouts};

/// relay 接入模式（公开配置 `relay_mode` 的唯一取值集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelayMode {
    /// 本机开发：明文 `http://` + 显式 loopback 白名单。
    LocalDev,
    /// 生产：`https://` + 系统可信根（TLS 端点认证）与 durable pin 两层独立校验。
    RemoteTls,
}

impl RelayMode {
    /// 配置里的字面值（`local_dev` / `remote_tls`）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalDev => "local_dev",
            Self::RemoteTls => "remote_tls",
        }
    }

    /// 严格解析配置字面值；未知值返回 `None`（调用方必须明确拒绝，不做默认回退）。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "local_dev" => Some(Self::LocalDev),
            "remote_tls" => Some(Self::RemoteTls),
            _ => None,
        }
    }
}

impl fmt::Display for RelayMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// URL 解析/策略拒绝的原因。
///
/// 错误文案**只含公开定位信息**（scheme/host/端口），绝不回显原始 userinfo。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayUrlError {
    /// 空输入（含只有空白）。
    Empty,
    /// 缺少 `://`，无法确定 scheme。
    MissingScheme,
    /// scheme 与模式不匹配（例如 `local_dev` 收到 `https://`）。
    SchemeNotAllowed {
        /// 期望 scheme（由模式决定）。
        expected: &'static str,
        /// 实际 scheme（小写；未知 scheme 原样转小写）。
        actual: String,
    },
    /// 携带 userinfo（`user:pass@host`）。
    Userinfo,
    /// 携带 query（`?...`）。
    Query,
    /// 携带 fragment（`#...`）。
    Fragment,
    /// 非根 base path（只接受空路径或 `/`）。
    BasePath,
    /// 缺少主机名。
    MissingHost,
    /// 主机不在 `local_dev` 的显式白名单里。
    HostNotAllowed(String),
    /// 主机名/端口语法非法。
    InvalidHost(String),
    /// 端口为空/非数字/超出 u16。
    InvalidPort(String),
    /// 端口为 0。
    PortZero,
}

impl fmt::Display for RelayUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "relay 地址为空"),
            Self::MissingScheme => write!(
                f,
                "relay 缺少 scheme（生产必须 https://，本机开发必须 http://）"
            ),
            Self::SchemeNotAllowed { expected, actual } => write!(
                f,
                "relay scheme 不允许：本模式只接受 {expected}://（收到 {actual}://）"
            ),
            Self::Userinfo => write!(f, "relay 不允许携带 userinfo：凭据必须走认证协议"),
            Self::Query => write!(f, "relay 基地址不允许携带 query（?...）"),
            Self::Fragment => write!(f, "relay 基地址不允许携带 fragment（#...）"),
            Self::BasePath => write!(
                f,
                "relay 只接受 canonical origin：不能带非根 base path（/api 之类）"
            ),
            Self::MissingHost => write!(f, "relay 缺少主机名"),
            Self::HostNotAllowed(host) => write!(
                f,
                "local_dev 只允许明文 loopback 白名单 localhost / 127.0.0.1 / [::1]，\
                 收到主机 {host:?}"
            ),
            Self::InvalidHost(host) => write!(f, "relay 主机名非法：{host:?}"),
            Self::InvalidPort(port) => write!(f, "relay 端口非法：{port:?}"),
            Self::PortZero => write!(f, "relay 端口不能为 0"),
        }
    }
}

impl std::error::Error for RelayUrlError {}

/// canonical origin（`scheme://host[:port]`）。
///
/// 构造只能经 [`parse_relay_url`]：主机已小写、IPv6 已去括号（展示时补回）、
/// 默认端口已省略。`Display` 输出即 canonical origin。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayOrigin {
    mode: RelayMode,
    scheme: &'static str,
    host: String,
    port: u16,
}

impl RelayOrigin {
    /// 模式。
    #[must_use]
    pub const fn mode(&self) -> RelayMode {
        self.mode
    }

    /// 主机（DNS 小写；IPv6 不带方括号）。
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// 生效端口（显式给出或 scheme 默认值）。
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// 是否本地开发模式。
    #[must_use]
    pub const fn is_local_dev(&self) -> bool {
        matches!(self.mode, RelayMode::LocalDev)
    }

    /// canonical origin 字符串（`scheme://host[:port]`，默认端口省略）。
    #[must_use]
    pub fn as_str(&self) -> String {
        let default_port = match self.scheme {
            "https" => 443,
            _ => 80,
        };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == default_port {
            format!("{}://{host}", self.scheme)
        } else {
            format!("{}://{host}:{}", self.scheme, self.port)
        }
    }
}

impl fmt::Display for RelayOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl From<RelayUrlError> for SyncError {
    fn from(error: RelayUrlError) -> Self {
        Self::ConfigInvalid(error.to_string())
    }
}

/// 解析并校验 relay 基地址（唯一权威入口）。
///
/// 成功返回 canonical origin；失败返回 [`RelayUrlError`]（不含 userinfo）。
pub fn parse_relay_url(raw: &str, mode: RelayMode) -> Result<RelayOrigin, RelayUrlError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(RelayUrlError::Empty);
    }
    // query/fragment 在任何模式下都不允许：先按字符拒绝，避免被 authority 解析吞掉。
    if raw.contains('?') {
        return Err(RelayUrlError::Query);
    }
    if raw.contains('#') {
        return Err(RelayUrlError::Fragment);
    }
    let (scheme_text, rest) = raw.split_once("://").ok_or(RelayUrlError::MissingScheme)?;
    let scheme_text = scheme_text.to_ascii_lowercase();
    let expected = match mode {
        RelayMode::LocalDev => "http",
        RelayMode::RemoteTls => "https",
    };
    if scheme_text != expected {
        return Err(RelayUrlError::SchemeNotAllowed {
            expected,
            actual: scheme_text,
        });
    }
    // authority 到第一个 '/' 为止；之后只能是根路径。
    let cut = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..cut];
    let path = &rest[cut..];
    if !path.is_empty() && path != "/" {
        return Err(RelayUrlError::BasePath);
    }
    if authority.contains('@') {
        return Err(RelayUrlError::Userinfo);
    }
    if authority.is_empty() {
        return Err(RelayUrlError::MissingHost);
    }
    let (host, port_text) = split_authority(authority)?;
    if host.is_empty() {
        return Err(RelayUrlError::MissingHost);
    }
    if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(RelayUrlError::InvalidHost(host));
    }
    let port = match port_text {
        Some(text) => {
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(RelayUrlError::InvalidPort(text.to_owned()));
            }
            let port = text
                .parse::<u16>()
                .map_err(|_| RelayUrlError::InvalidPort(text.to_owned()))?;
            if port == 0 {
                return Err(RelayUrlError::PortZero);
            }
            port
        }
        None => match mode {
            RelayMode::LocalDev => 80,
            RelayMode::RemoteTls => 443,
        },
    };
    let host = host.to_ascii_lowercase();
    match mode {
        RelayMode::LocalDev => {
            // 显式白名单：**不用** is_loopback()。
            if !matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") {
                return Err(RelayUrlError::HostNotAllowed(host));
            }
        }
        RelayMode::RemoteTls => {
            if !is_remote_host(&host) {
                return Err(RelayUrlError::InvalidHost(host));
            }
        }
    }
    Ok(RelayOrigin {
        mode,
        scheme: expected,
        host,
        port,
    })
}

/// 拆 authority 为 `(host, Option<port_text>)`；IPv6 必须写成 `[::1][:port]`。
fn split_authority(authority: &str) -> Result<(String, Option<&str>), RelayUrlError> {
    if let Some(after_bracket) = authority.strip_prefix('[') {
        let (host, rest) = after_bracket
            .split_once(']')
            .ok_or_else(|| RelayUrlError::InvalidHost(authority.to_owned()))?;
        // 括号内必须是合法 IPv6 字面量（拒绝 `[localhost]` 之类）。
        host.parse::<Ipv6Addr>()
            .map_err(|_| RelayUrlError::InvalidHost(host.to_owned()))?;
        match rest {
            "" => Ok((host.to_owned(), None)),
            _ => {
                let port = rest
                    .strip_prefix(':')
                    .ok_or_else(|| RelayUrlError::InvalidHost(authority.to_owned()))?;
                Ok((host.to_owned(), Some(port)))
            }
        }
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        // 裸 IPv6（含多个冒号）必须加方括号；这里拒绝以免把 `::1` 当 host:port。
        if host.contains(':') {
            return Err(RelayUrlError::InvalidHost(authority.to_owned()));
        }
        Ok((host.to_owned(), Some(port)))
    } else {
        Ok((authority.to_owned(), None))
    }
}

/// `remote_tls` 的主机：合法 IPv4/IPv6 字面量，或语法合法的 DNS 名。
fn is_remote_host(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    // DNS：总长 ≤ 253，各 label 非空且 ≤ 63，只允许 ASCII 字母/数字/`-`/`_`，
    // label 不以 `-` 开头/结尾（`_` 常见于内网主机记录，这里不额外放宽其它字符）。
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    })
}

/// 生产传输：**显式** [`TransportPolicy::production`]；不接受 `local_dev`。
///
/// 不读 `MENGHUNOTE_ALLOW_PLAINTEXT_HTTP` 之类的环境放宽，也不调用 `StdTcpTransport::new`。
pub fn production_transport(
    origin: &RelayOrigin,
    timeouts: Option<TransportTimeouts>,
) -> Result<StdTcpTransport, SyncError> {
    if origin.mode() != RelayMode::RemoteTls {
        return Err(SyncError::ConfigInvalid(
            "生产 relay 必须是 remote_tls（https）；local_dev 只在显式开发构建中可用".to_owned(),
        ));
    }
    let transport =
        StdTcpTransport::new_with_policy(&origin.as_str(), TransportPolicy::production())?;
    Ok(apply_timeouts(transport, timeouts))
}

/// 本地开发传输：只接受 `local_dev`，用显式的 loopback 开发策略。
///
/// 调用方必须是显式的开发入口（app 的 `dev-sync` feature）；生产 client 不得走到这里。
pub fn local_dev_transport(
    origin: &RelayOrigin,
    timeouts: Option<TransportTimeouts>,
) -> Result<StdTcpTransport, SyncError> {
    if origin.mode() != RelayMode::LocalDev {
        return Err(SyncError::ConfigInvalid(
            "本地开发 transport 只接受 local_dev（http://loopback）".to_owned(),
        ));
    }
    let transport = StdTcpTransport::new_with_policy(
        &origin.as_str(),
        TransportPolicy::localhost_development(),
    )?;
    Ok(apply_timeouts(transport, timeouts))
}

fn apply_timeouts(
    transport: StdTcpTransport,
    timeouts: Option<TransportTimeouts>,
) -> StdTcpTransport {
    let Some(timeouts) = timeouts else {
        return transport;
    };
    let transport = transport
        .with_connect_timeout(timeouts.connect)
        .with_io_timeout(timeouts.io);
    match timeouts.request_deadline {
        Some(deadline) => transport.with_request_deadline(deadline),
        None => transport,
    }
}

/// 生产 client 的构造参数（S2 的 `RelayContext` 收敛点；S1 只提供受限入口）。
#[derive(Debug, Clone)]
pub struct PinnedClientSpec {
    /// 已解析的 `remote_tls` origin。
    pub origin: RelayOrigin,
    /// vault 身份（与 `/state.vault_id` 比对由 client 完成）。
    pub vault_id: [u8; 16],
    /// 本机设备身份。
    pub device_id: [u8; 16],
    /// 管理公钥信任锚（ADR §35.2；绝不取 relay genesis key）。
    pub manager_pub: [u8; 32],
    /// 本地身份里 pin 住的 relay instance（若已有）；用于在发请求前 fail-stop。
    pub expected_instance: Option<[u8; 16]>,
    /// 传输预算；`None` = 历史默认。
    pub timeouts: Option<TransportTimeouts>,
}

/// 生产 client 的**唯一**通用构造入口：`remote_tls` + durable pin + 生产传输策略。
///
/// 失败语义（fail-closed，均可分类到 ADR 0013 §4）：
/// - origin 不是 `remote_tls` → [`SyncError::ConfigInvalid`]；
/// - 没有 durable `sync.control_pin` → [`SyncError::NeedsTrust`]（**不**退回 unpinned）；
/// - pin 损坏/绑定不符 → [`SyncError::PinCorrupt`]；
/// - pin 的实例与本地身份不一致 → [`SyncError::InstanceMismatch`]。
pub fn open_pinned_client(
    vault: &Vault,
    spec: PinnedClientSpec,
    signing_key: DeviceSigningKey,
) -> Result<HttpRelayClient<StdTcpTransport>, SyncError> {
    let PinnedClientSpec {
        origin,
        vault_id,
        device_id,
        manager_pub,
        expected_instance,
        timeouts,
    } = spec;
    let pin = load_control_pin(vault, vault_id)?.ok_or(SyncError::NeedsTrust(
        "缺少 durable sync.control_pin：生产路径拒绝以 unpinned 方式消费 /control",
    ))?;
    if let Some(expected) = expected_instance {
        if pin.instance_id != expected {
            return Err(SyncError::InstanceMismatch);
        }
    }
    let transport = production_transport(&origin, timeouts)?;
    let config = HttpRelayConfig {
        base_url: origin.as_str(),
        // 生产受限入口是历史 v1 语义；v2 只由票据创建/显式配置进入。
        api_version: crate::api_version::RelayApiVersion::V1,
        vault_id,
        device_id,
        manager_pub: Some(manager_pub),
    };
    Ok(HttpRelayClient::new(transport, config, signing_key).with_control_pin(pin.to_control_pin()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(raw: &str) -> Result<RelayOrigin, RelayUrlError> {
        parse_relay_url(raw, RelayMode::LocalDev)
    }

    fn remote(raw: &str) -> Result<RelayOrigin, RelayUrlError> {
        parse_relay_url(raw, RelayMode::RemoteTls)
    }

    #[test]
    fn local_dev_accepts_only_explicit_loopback_allowlist() {
        for good in [
            ("http://localhost:18080", "http://localhost:18080"),
            ("http://127.0.0.1:18080", "http://127.0.0.1:18080"),
            ("http://[::1]:18080", "http://[::1]:18080"),
            // 默认端口规范化：显式 `:80` 被省略，尾斜杠也归一。
            ("http://localhost:80", "http://localhost"),
            ("http://127.0.0.1", "http://127.0.0.1"),
            ("http://[::1]/", "http://[::1]"),
            ("  http://LOCALHOST:1234  ", "http://localhost:1234"),
        ] {
            let origin = local(good.0).unwrap_or_else(|error| panic!("{good:?}：{error}"));
            assert_eq!(origin.as_str(), good.1);
            assert_eq!(origin.mode(), RelayMode::LocalDev);
        }
        // 不是白名单里的 loopback 变体也不行（显式白名单，不用 is_loopback()）。
        for bad in [
            "http://127.0.0.2:18080",
            "http://[::2]:18080",
            "http://relay.example:18080",
            "http://192.168.1.9:18080",
            "http://0.0.0.0:18080",
            "http://localhost.evil.example",
        ] {
            assert!(
                matches!(local(bad), Err(RelayUrlError::HostNotAllowed(_))),
                "{bad:?} 必须被主机白名单拒绝"
            );
        }
    }

    #[test]
    fn local_dev_rejects_scheme_userinfo_query_fragment_path_and_bad_ports() {
        assert!(matches!(
            local("https://localhost:18080"),
            Err(RelayUrlError::SchemeNotAllowed { .. })
        ));
        assert!(matches!(
            local("http://user:pass@127.0.0.1:18080"),
            Err(RelayUrlError::Userinfo)
        ));
        assert!(matches!(
            local("http://127.0.0.1:18080/?x=1"),
            Err(RelayUrlError::Query)
        ));
        assert!(matches!(
            local("http://127.0.0.1:18080/#frag"),
            Err(RelayUrlError::Fragment)
        ));
        assert!(matches!(
            local("http://127.0.0.1:18080/api"),
            Err(RelayUrlError::BasePath)
        ));
        assert!(matches!(
            local("http://127.0.0.1:0"),
            Err(RelayUrlError::PortZero)
        ));
        assert!(matches!(
            local("http://127.0.0.1:notaport"),
            Err(RelayUrlError::InvalidPort(_))
        ));
        assert!(matches!(
            local("http://127.0.0.1:99999"),
            Err(RelayUrlError::InvalidPort(_))
        ));
        assert!(matches!(
            local("127.0.0.1:18080"),
            Err(RelayUrlError::MissingScheme)
        ));
        assert!(matches!(local("   "), Err(RelayUrlError::Empty)));
        assert!(matches!(
            local("http://[::1:18080"),
            Err(RelayUrlError::InvalidHost(_))
        ));
    }

    #[test]
    fn remote_tls_only_accepts_https_and_canonicalizes_origin() {
        for good in [
            ("https://relay.example", "https://relay.example"),
            ("https://relay.example:8443", "https://relay.example:8443"),
            ("https://Relay.Example:443/", "https://relay.example"),
            ("https://192.0.2.10:9443", "https://192.0.2.10:9443"),
            ("https://[2001:db8::1]:8443", "https://[2001:db8::1]:8443"),
            ("https://[2001:db8::1]", "https://[2001:db8::1]"),
        ] {
            let origin = remote(good.0).unwrap_or_else(|error| panic!("{good:?}：{error}"));
            assert_eq!(origin.as_str(), good.1);
            assert_eq!(origin.mode(), RelayMode::RemoteTls);
        }
        for bad in [
            "http://relay.example",
            "https://user:pass@relay.example",
            "https://relay.example/api",
            "https://relay.example/?x=1",
            "https://relay.example/#frag",
            "https://relay.example:0",
            "https://relay.example:70000",
            "https://",
            "https://[::1",
            "https://exa mple.com",
            "https://-bad.example.com",
        ] {
            assert!(remote(bad).is_err(), "{bad:?} 必须被拒绝");
        }
    }

    #[test]
    fn canonical_origin_roundtrips_through_the_parser() {
        for (raw, mode) in [
            ("http://127.0.0.1:1", RelayMode::LocalDev),
            ("http://localhost", RelayMode::LocalDev),
            ("https://relay.example:8443", RelayMode::RemoteTls),
        ] {
            let origin = parse_relay_url(raw, mode).expect("合法");
            let again = parse_relay_url(&origin.as_str(), mode).expect("canonical 必须可再解析");
            assert_eq!(origin, again);
        }
    }

    #[test]
    fn modes_are_strict_and_never_default() {
        assert_eq!(RelayMode::parse("local_dev"), Some(RelayMode::LocalDev));
        assert_eq!(RelayMode::parse("remote_tls"), Some(RelayMode::RemoteTls));
        for bad in ["", "LOCAL_DEV", "remote", "https", "local-dev"] {
            assert_eq!(RelayMode::parse(bad), None, "{bad:?} 不能被默认接受");
        }
        assert_eq!(RelayMode::LocalDev.as_str(), "local_dev");
        assert_eq!(RelayMode::RemoteTls.to_string(), "remote_tls");
    }

    #[test]
    fn production_and_dev_transports_are_mode_locked() {
        let remote_origin = remote("https://relay.example").expect("remote");
        let transport = production_transport(&remote_origin, None).expect("生产策略可构造");
        assert_eq!(
            transport.policy(),
            TransportPolicy::production(),
            "生产入口必须显式 production policy"
        );
        let local_origin = local("http://127.0.0.1:18080").expect("local");
        assert!(
            matches!(
                production_transport(&local_origin, None),
                Err(SyncError::ConfigInvalid(_))
            ),
            "生产入口绝不能接受 local_dev"
        );
        let transport = local_dev_transport(&local_origin, None).expect("开发策略可构造");
        assert_eq!(transport.policy(), TransportPolicy::localhost_development());
        assert!(matches!(
            local_dev_transport(&remote_origin, None),
            Err(SyncError::ConfigInvalid(_))
        ));
    }

    fn fresh_vault() -> (tempfile::TempDir, Vault) {
        let base = std::env::var("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/tmp"));
        std::fs::create_dir_all(&base).expect("target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("relay-policy-")
            .tempdir_in(&base)
            .expect("临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
        (dir, vault)
    }

    fn spec(origin: RelayOrigin, instance: Option<[u8; 16]>) -> PinnedClientSpec {
        PinnedClientSpec {
            origin,
            vault_id: [0x5a; 16],
            device_id: [0x04; 16],
            manager_pub: [0x07; 32],
            expected_instance: instance,
            timeouts: None,
        }
    }

    fn signing_key() -> DeviceSigningKey {
        DeviceSigningKey::from_seed(crate::keys::Secret32::from_array([0x09; 32]))
    }

    /// 生产 client：没有 durable pin 时必须 NeedsTrust（绝不 unpinned、也不建空 pin）。
    #[test]
    fn production_client_requires_a_durable_pin() {
        let (_dir, vault) = fresh_vault();
        let error = match open_pinned_client(
            &vault,
            spec(remote("https://relay.example").expect("remote"), None),
            signing_key(),
        ) {
            Ok(_) => panic!("缺 durable pin 必须 NeedsTrust"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");
        assert!(
            vault
                .get_meta(crate::control_pin::META_CONTROL_PIN)
                .expect("meta")
                .is_none(),
            "拒绝路径不得写入 pin"
        );
    }

    /// 生产 client：有 pin 才能打开；instance 绑定不符与 local_dev origin 都拒绝。
    #[test]
    fn production_client_uses_the_pinned_head_and_rejects_bad_bindings() {
        let (_dir, vault) = fresh_vault();
        crate::control_pin::commit_verified_head(
            &vault,
            None,
            crate::control_pin::VerifiedHead::from_verified_control(
                [0x5a; 16], [0x22; 16], 1, [0x11; 32],
            ),
        )
        .expect("写 durable pin");

        // instance 一致 → 可构造（不联网；TLS 根在首次请求时才装配）。
        let origin = remote("https://relay.example:8443").expect("remote");
        open_pinned_client(&vault, spec(origin, Some([0x22; 16])), signing_key())
            .expect("有 pin 且 instance 一致必须可构造");

        let origin = remote("https://relay.example").expect("remote");
        let error = match open_pinned_client(&vault, spec(origin, Some([0x33; 16])), signing_key())
        {
            Ok(_) => panic!("instance 不符必须拒绝"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::InstanceMismatch), "{error:?}");

        let origin = local("http://127.0.0.1:18080").expect("local");
        let error = match open_pinned_client(&vault, spec(origin, Some([0x22; 16])), signing_key())
        {
            Ok(_) => panic!("生产 client 不得接受 local_dev"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
    }

    #[test]
    fn production_never_accepts_plaintext_even_with_dev_origin() {
        // 生产工厂入口按模式拒绝；即使调用方手里是 loopback origin 也不行。
        let origin = local("http://127.0.0.1:18080").expect("local");
        let error = match production_transport(&origin, None) {
            Ok(_) => panic!("明文必须拒绝"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
    }
}
