//! relay 的运行配置。

use std::net::{Ipv6Addr, SocketAddr};
use std::path::PathBuf;

use crate::db::DbConfig;
use crate::ids::{Hash32, Id16};
use crate::ratelimit::{
    RateLimitSpec, DEFAULT_V2_AUTH_LIMIT, DEFAULT_V2_CREATE_LIMIT, DEFAULT_V2_DATA_LIMIT,
};

/// 明文监听门禁（评审 0002「生产阻断」）。
///
/// relay 自身只讲明文 HTTP（ADR 0006 §7）：生产部署必须让 relay 监听 loopback，由同机
/// 反向代理终止 TLS。因此启动时：
/// - `listen` 是 loopback（`127.0.0.0/8`、`::1`）→ 允许；
/// - 非 loopback（`0.0.0.0`、内网/公网 IP）→ 必须显式 `allow_plaintext = true`，否则拒绝启动。
///
/// 返回 `Err` 时调用方必须在 bind/打开数据库之前退出，错误文案面向运维（说明怎么改）。
pub fn ensure_plaintext_listen_allowed(
    listen: SocketAddr,
    allow_plaintext: bool,
) -> Result<(), String> {
    if listen.ip().is_loopback() || allow_plaintext {
        return Ok(());
    }
    Err(format!(
        "拒绝在非 loopback 地址 {listen} 上明文监听：relay 只讲明文 HTTP，对外入口必须由 TLS 反向代理终止。\n\
         做法一（生产推荐）：listen = \"127.0.0.1:18080\"，用 nginx/Caddy 在本机监听 443 并代理到 127.0.0.1:18080，例如 nginx：\n\
         \x20 server {{\n\
         \x20   listen 443 ssl;\n\
         \x20   ssl_certificate     /etc/menghunote/fullchain.pem;\n\
         \x20   ssl_certificate_key /etc/menghunote/privkey.pem;\n\
         \x20   location / {{ proxy_pass http://127.0.0.1:18080; proxy_http_version 1.1; }}\n\
         \x20 }}\n\
         做法二：确认运行在受信任的私网/隧道内，显式设置 allow_plaintext = true\n\
         （风险：bearer token、设备与控制面元数据会明文传输，可被窃听/篡改）。"
    ))
}

/// 内嵌 Web 管理面（ADR 0018 S1）默认监听地址：**只绑定 loopback**。
///
/// 运维可改 LAN 地址（如 `0.0.0.0:18081`），此时 `serve` 会打印"明文可被截获"的警告；
/// 显式配置 `admin_listen = "off"` 才完全关闭。
pub const DEFAULT_ADMIN_LISTEN: &str = "127.0.0.1:18081";

/// 数据面 **canonical public origin**（ADR 0018 S4a）：只用于生成 claim 领取链接。
///
/// 来源只有部署配置 `public_url`，**绝不**从 `Host`/`listen`/请求头推导；它必须与客户端
/// 唯一 URL parser（`sync::relay_policy::parse_relay_url`）的 canonical 输出逐字节一致，
/// 否则客户端 `mn-pair://create` 解析会拒绝该链接（= 生成了不可用链接）。
///
/// 允许的形状（与客户端 parser 的白名单一致）：
/// - `https://<DNS/IP>[:port]`：TLS 入口（推荐由同机反向代理终止 TLS）；
/// - `http://localhost` / `http://127.0.0.1` / `http://[::1]`（可带端口）：本机开发。
///
/// 非 loopback 的明文 HTTP 一律**拒绝**（客户端 `local_dev` 只接受 loopback 白名单，
/// `remote_tls` 只接受 https）；规范化为小写 scheme/host、默认端口（80/443）省略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicUrl {
    text: String,
    https: bool,
}

impl PublicUrl {
    /// 解析并校验 canonical origin；错误文案面向运维（说明允许的形状）。
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let example = "示例 https://relay.example.com 或 http://127.0.0.1:18080";
        if raw.is_empty() {
            return Err(format!("public_url 不能为空（{example}）"));
        }
        if raw.contains('?') {
            return Err(
                "public_url 不允许携带 query（只接受 canonical origin scheme://host[:port]）"
                    .to_string(),
            );
        }
        if raw.contains('#') {
            return Err(
                "public_url 不允许携带 fragment（只接受 canonical origin scheme://host[:port]）"
                    .to_string(),
            );
        }
        let (scheme_text, rest) = raw
            .split_once("://")
            .ok_or_else(|| format!("public_url 缺少 scheme（{example}）"))?;
        let scheme = scheme_text.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(format!(
                "public_url 只接受 http:// 或 https://（收到 {scheme_text}://）"
            ));
        }
        // authority 到第一个 '/' 为止；之后只允许根路径 '/'（规范化时省略）。
        let cut = rest.find('/').unwrap_or(rest.len());
        let authority = &rest[..cut];
        let path = &rest[cut..];
        if !path.is_empty() && path != "/" {
            return Err(format!(
                "public_url 必须是 canonical origin：不允许 base path（收到 {path:?}）"
            ));
        }
        if authority.contains('@') {
            return Err("public_url 不允许携带 userinfo（凭据必须走认证协议）".to_string());
        }
        if authority.is_empty() {
            return Err(format!("public_url 缺少主机名（{example}）"));
        }
        let (host, port_text) = split_public_url_authority(authority)?;
        if host.is_empty() {
            return Err("public_url 缺少主机名".to_string());
        }
        if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!("public_url 主机名含空白/控制字符：{host:?}"));
        }
        let port = match port_text {
            Some(text) => {
                if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(format!("public_url 端口非法：{text:?}"));
                }
                let port = text
                    .parse::<u16>()
                    .map_err(|_| format!("public_url 端口超出 1..=65535：{text:?}"))?;
                if port == 0 {
                    return Err("public_url 端口不能为 0".to_string());
                }
                port
            }
            None => {
                if scheme == "https" {
                    443
                } else {
                    80
                }
            }
        };
        let host = host.to_ascii_lowercase();
        let https = scheme == "https";
        if https {
            if !is_public_dns_or_ip_host(&host) {
                return Err(format!(
                    "public_url 主机名不合法（需要 IP 字面量或合法 DNS 名）：{host:?}"
                ));
            }
        } else if !matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") {
            return Err(format!(
                "public_url 明文 HTTP 只允许 loopback（localhost / 127.0.0.1 / [::1]）；\
                 收到主机 {host:?}。claim 领取链接的 relay 必须是 https（TLS 反向代理）\
                 或 loopback http —— 客户端 local_dev 不接受其它明文主机，\
                 生成这种链接只会得到不可用地址。"
            ));
        }
        let host_text = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        let default_port = if https { 443 } else { 80 };
        let text = if port == default_port {
            format!("{scheme}://{host_text}")
        } else {
            format!("{scheme}://{host_text}:{port}")
        };
        Ok(Self { text, https })
    }

    /// canonical origin 文本（链接里 `relay=` 的百分号编码源、管理页只读展示值）。
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// 是否 `https://`（TLS；否 = loopback 明文开发）。
    pub fn is_https(&self) -> bool {
        self.https
    }
}

impl std::fmt::Display for PublicUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// 拆 `authority` 为 `(host, Option<port_text>)`；IPv6 必须写成 `[addr][:port]`。
fn split_public_url_authority(authority: &str) -> Result<(String, Option<&str>), String> {
    if let Some(after_bracket) = authority.strip_prefix('[') {
        let (host, rest) = after_bracket
            .split_once(']')
            .ok_or_else(|| format!("public_url 的 IPv6 缺少 `]`：{authority:?}"))?;
        host.parse::<Ipv6Addr>()
            .map_err(|_| format!("public_url 方括号内不是合法 IPv6：{host:?}"))?;
        match rest {
            "" => Ok((host.to_string(), None)),
            _ => {
                let port = rest
                    .strip_prefix(':')
                    .ok_or_else(|| format!("public_url 的 IPv6 后只能是 `:port`：{authority:?}"))?;
                Ok((host.to_string(), Some(port)))
            }
        }
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        // 裸 IPv6（多个冒号）必须加方括号，避免把 `::1` 当 host:port。
        if host.contains(':') {
            return Err(format!("public_url 的 IPv6 必须加方括号：[{authority}]"));
        }
        Ok((host.to_string(), Some(port)))
    } else {
        Ok((authority.to_string(), None))
    }
}

/// `https` 的主机：IP 字面量，或语法合法的 DNS 名（与客户端 parser 同一口径）。
fn is_public_dns_or_ip_host(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
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

/// relay 进程配置（单进程、同库多 vault；配额/密钥由部署时配置，ADR 0006 §7）。
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// 数据库参数（路径 + 预算）。
    pub db: DbConfig,
    /// 一次性 setup secret（首台设备初始化用；哈希比较，成功后关闭该端点）。
    ///
    /// 生产由服务器控制台/权限受限文件交给本人（ADR 0006 §4）；`None` 表示禁止初始化。
    /// **只**用于 v1 `POST /v1/vaults` genesis（旧实例/CLI）；App 的 v2 创建一律用
    /// Web 管理面发行的 claim 票据，不再接受 setup secret 作为管理凭证（ADR 0018 S4b）。
    pub setup_secret: Option<String>,
    /// 单页日志上限（ADR §5 `limit`）。
    pub max_log_page: u32,
    /// 单次 refs 请求上限（ADR §5：≤1024 条）。
    pub max_refs_per_request: usize,
    /// 控制面请求体上限（ADR §5：≤1MiB）。
    pub max_control_body: usize,
    /// setup secret 的 `SHA256`（`init` 只落哈希，进程重启后仍能接受 genesis）。
    pub setup_secret_hash: Option<Hash32>,
    /// 固定的游标 HMAC key（缺省在 genesis 时随机生成并落库）。
    pub cursor_key: Option<Hash32>,
    /// SNAPSHOT 对象明文上限（部署配置；默认 64MiB）。
    pub max_snapshot_bytes: u64,
    /// v1 旧 API 显式绑定的 legacy 默认 vault（ADR 0014 §A）。
    ///
    /// 缺省时从库里 `vaults.legacy_default = 1` 解析（迁移把旧单 vault 标成它）；
    /// 两者都没有就是 404，**绝不**按 token subject 或"第一个 vault"猜租户。
    pub legacy_vault_id: Option<Id16>,
    /// v2 `POST /v2/vaults`（创建票据消费）入口限流；`None` = 关闭（ADR 0014 §A）。
    pub v2_create_limit: Option<RateLimitSpec>,
    /// v2 `/v2/vaults/{id}/auth/{challenges,tokens}` 入口限流；`None` = 关闭。
    pub v2_auth_limit: Option<RateLimitSpec>,
    /// v2 租户**业务**路由（数据面/控制面）入口限流；`None` = 关闭。
    ///
    /// v1 旧路由永不受此限流影响（ADR 0014 §A：v1 行为不变）。
    pub v2_data_limit: Option<RateLimitSpec>,
    /// 内嵌 Web 管理面（ADR 0018 S1）的监听地址；`None` = 显式关闭（只跑数据面）。
    ///
    /// 默认 [`DEFAULT_ADMIN_LISTEN`]。这里只存**地址字符串**：与数据面 `listen` 一样由
    /// `serve` 解析成 `SocketAddr` 并真正绑定（端口 0 表示由内核分配，启动横幅打印实际地址）。
    pub admin_listen: Option<String>,
    /// admin Host 白名单的**额外**精确条目（`host:port`，可带域名）。
    ///
    /// 生效白名单 = 监听地址的 `host:port` + `localhost`/`127.0.0.1`/`[::1]` 同端口
    /// + 这里的配置条目。用于"LAN 监听 + 浏览器用域名/网卡 IP 访问"的部署；
    ///   不做通配、不做 CORS。ADR 0018 §1「精确 Host/Origin 白名单」。
    pub admin_allowed_hosts: Vec<String>,
    /// 部署注入的 `SHA256(admin token 文本)`；`serve` **优先用库内 `relay_meta`** 的哈希。
    ///
    /// 只用于"没有 `init` 生成流程"的部署/测试；配置里的哈希永远不覆盖库内已有值。
    pub admin_token_hash: Option<Hash32>,
    /// 数据面 canonical public origin（claim 链接用；ADR 0018 S4a）。
    ///
    /// **只**来自配置（[`PublicUrl::parse`] 校验后的 canonical 文本），绝不从
    /// `Host`/`listen`/请求头推导；`None` 时创建页面明确报错、不生成链接。
    pub public_url: Option<PublicUrl>,
}

impl RelayConfig {
    /// 以数据库路径构造默认配置。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            db: DbConfig::new(path),
            setup_secret: None,
            max_log_page: 512,
            max_refs_per_request: 1024,
            max_control_body: 1024 * 1024,
            setup_secret_hash: None,
            cursor_key: None,
            max_snapshot_bytes: menghunote_sync_proto::limits::MAX_SNAPSHOT_CDE_BYTES as u64,
            legacy_vault_id: None,
            v2_create_limit: Some(DEFAULT_V2_CREATE_LIMIT),
            v2_auth_limit: Some(DEFAULT_V2_AUTH_LIMIT),
            v2_data_limit: Some(DEFAULT_V2_DATA_LIMIT),
            admin_listen: Some(DEFAULT_ADMIN_LISTEN.to_string()),
            admin_allowed_hosts: Vec::new(),
            admin_token_hash: None,
            public_url: None,
        }
    }

    /// 设置 setup secret（部署时从控制台注入）。
    pub fn with_setup_secret(mut self, secret: impl Into<String>) -> Self {
        self.setup_secret = Some(secret.into());
        self
    }

    /// 覆盖配额。
    pub fn with_limits(mut self, normal_limit: i64, maintenance_limit: i64) -> Self {
        self.db.normal_limit = normal_limit;
        self.db.maintenance_limit = maintenance_limit;
        self
    }

    /// 用 setup secret 的哈希代替明文（生产 `init` → `serve` 两段式启动）。
    pub fn with_setup_secret_hash(mut self, hash: Hash32) -> Self {
        self.setup_secret_hash = Some(hash);
        self
    }

    /// 固定游标 HMAC key（部署级配置；缺省随机生成）。
    pub fn with_cursor_key(mut self, key: Hash32) -> Self {
        self.cursor_key = Some(key);
        self
    }

    /// 显式指定 v1 legacy 默认 vault（运维在配置文件里覆盖 DB 标记）。
    pub fn with_legacy_vault_id(mut self, vault_id: Id16) -> Self {
        self.legacy_vault_id = Some(vault_id);
        self
    }

    /// 覆盖 v2 入口限流（`None` = 关闭；测试用低阈值证明 429，不影响 v1）。
    pub fn with_v2_rate_limits(
        mut self,
        create: Option<RateLimitSpec>,
        auth: Option<RateLimitSpec>,
    ) -> Self {
        self.v2_create_limit = create;
        self.v2_auth_limit = auth;
        self
    }

    /// 覆盖 v2 **业务**路由入口限流（`None` = 关闭；只影响 v2，v1 从不受影响）。
    pub fn with_v2_data_limit(mut self, data: Option<RateLimitSpec>) -> Self {
        self.v2_data_limit = data;
        self
    }

    /// 关闭 v2 入口限流（测试/受控部署专用；create/auth/data 三面全关）。
    pub fn without_v2_rate_limits(mut self) -> Self {
        self.v2_create_limit = None;
        self.v2_auth_limit = None;
        self.v2_data_limit = None;
        self
    }

    /// SNAPSHOT 对象明文上限。
    pub fn with_max_snapshot_bytes(mut self, bytes: u64) -> Self {
        self.max_snapshot_bytes = bytes;
        self
    }

    /// 设置内嵌 Web 管理面的监听地址（`None` = 显式关闭；ADR 0018 S1）。
    pub fn with_admin_listen(mut self, listen: Option<String>) -> Self {
        self.admin_listen = listen;
        self
    }

    /// 显式关闭 admin listener（只跑数据面；等价于配置 `admin_listen = "off"`）。
    pub fn without_admin_listener(mut self) -> Self {
        self.admin_listen = None;
        self
    }

    /// 追加 admin Host 白名单的精确条目（`host:port`）。
    pub fn with_admin_allowed_hosts(mut self, hosts: Vec<String>) -> Self {
        self.admin_allowed_hosts = hosts;
        self
    }

    /// 部署注入 admin token 哈希（库内已有哈希时以库为准）。
    pub fn with_admin_token_hash(mut self, hash: Hash32) -> Self {
        self.admin_token_hash = Some(hash);
        self
    }

    /// 设置 claim 链接使用的数据面 canonical origin（**只能**来自配置；ADR 0018 S4a）。
    pub fn with_public_url(mut self, public_url: PublicUrl) -> Self {
        self.public_url = Some(public_url);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn addr(ip: IpAddr) -> SocketAddr {
        SocketAddr::new(ip, 18080)
    }

    #[test]
    fn loopback_plaintext_listen_is_allowed_by_default() {
        for ip in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ] {
            assert!(
                ensure_plaintext_listen_allowed(addr(ip), false).is_ok(),
                "loopback 应默认允许：{ip}"
            );
        }
    }

    #[test]
    fn non_loopback_plaintext_listen_requires_explicit_allow() {
        for ip in [
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ] {
            let error = ensure_plaintext_listen_allowed(addr(ip), false)
                .expect_err("非 loopback 明文必须默认拒绝");
            assert!(
                error.contains("allow_plaintext") && error.contains("反向代理"),
                "错误文案要给出可执行的做法：{error}"
            );
            // 显式允许后放行（风险由部署方承担，CLI 会打警告）。
            assert!(
                ensure_plaintext_listen_allowed(addr(ip), true).is_ok(),
                "显式 allow_plaintext 后应放行：{ip}"
            );
        }
    }

    #[test]
    fn public_url_is_canonicalized_for_claim_links() {
        for (raw, expected, https) in [
            ("http://127.0.0.1:18080", "http://127.0.0.1:18080", false),
            ("  HTTP://LOCALHOST:80/  ", "http://localhost", false),
            ("http://[::1]:18080", "http://[::1]:18080", false),
            ("http://[::1]:80", "http://[::1]", false),
            (
                "https://relay.example.com",
                "https://relay.example.com",
                true,
            ),
            (
                "https://Relay.Example.COM:443",
                "https://relay.example.com",
                true,
            ),
            (
                "https://[2001:DB8::1]:8443",
                "https://[2001:db8::1]:8443",
                true,
            ),
            (
                "https://192.168.1.10:18080",
                "https://192.168.1.10:18080",
                true,
            ),
        ] {
            let parsed = PublicUrl::parse(raw).unwrap_or_else(|err| panic!("{raw}: {err}"));
            assert_eq!(parsed.as_str(), expected, "raw={raw}");
            assert_eq!(parsed.is_https(), https, "raw={raw}");
            assert_eq!(parsed.to_string(), expected);
        }
    }

    #[test]
    fn public_url_rejects_non_canonical_or_unusable_origins() {
        for raw in [
            "",
            "   ",
            "127.0.0.1:18080",
            "ftp://127.0.0.1:18080",
            "http://",
            "http://127.0.0.1:18080/api",
            "http://127.0.0.1:18080?x=1",
            "http://127.0.0.1:18080#frag",
            "http://user@127.0.0.1:18080",
            "http://127.0.0.1:0",
            "http://127.0.0.1:",
            "http://127.0.0.1:port",
            "http://127.0.0.1:65536",
            "http://::1:18080",
            "http://[::1",
            "http://[localhost]",
            // 客户端 local_dev 白名单之外（会生成不可用领取链接）。
            "http://192.168.1.10:18080",
            "http://127.0.0.2:18080",
            "http://[::2]:18080",
            "http://relay.example.com:18080",
            // https 的主机名语法。
            "https://-bad.example.com",
            "https://bad-.example.com",
            "https://bad host.example.com",
        ] {
            assert!(
                PublicUrl::parse(raw).is_err(),
                "必须拒绝 public_url={raw:?}"
            );
        }
        // 错误文案要说明允许的形状/为什么拒绝，不能只抛内部路径。
        let err = PublicUrl::parse("http://192.168.1.10:18080").expect_err("LAN http");
        assert!(err.contains("loopback") && err.contains("https"), "{err}");
    }
}
