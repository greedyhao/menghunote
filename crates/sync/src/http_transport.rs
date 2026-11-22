//! relay 的 HTTP/1.1 传输层（TLS 与重定向门禁）。
//!
//! 评审 0002「生产阻断」要求客户端真实 TLS（校验证书链/主机名/有效期）、拒绝明文降级与
//! 跨 origin 重定向。本模块把这类**传输层安全策略**收口在一处，`http_relay` 只消费
//! [`HttpTransport`]，不自己碰 socket。
//!
//! 策略（全部默认拒绝，显式放行才可用）：
//! - `https://`：rustls（ring provider）。信任根 = 系统证书存储 + 可选注入的私有 CA
//!   （[`StdTcpTransport::with_extra_roots`]）；SNI/证书链/主机名/有效期由 rustls-webpki
//!   校验，IP 字面量按 IP SAN 校验。TLS 1.2/1.3。
//! - `http://`：只允许 loopback 目标（`127.0.0.1`、`::1`、`localhost`），且必须显式选择
//!   [`TransportPolicy::localhost_development`]（或环境变量 [`ALLOW_PLAINTEXT_HTTP_ENV`]`=1`）。
//!   非 loopback 明文在任何策略下都拒绝。**生产默认必须 https**。
//! - 3xx：只跟随**同 origin**（scheme/host/port 完全相同）的重定向，最多
//!   [`MAX_REDIRECTS`] 跳；跨 origin、协议相对 `//host/...`、`https→http` 降级一律返回
//!   [`SyncError::TransportPolicy`]（不跟随、不重试）。
//!
//! 完整性：响应必须自洽（`Content-Length` 不足即报错，超出按长度截断）；不支持
//! `Transfer-Encoding: chunked`（relay 的 wire 是 CBOR/octet-stream，不依赖它）。
//! 请求永远带 `Connection: close`，响应读到 EOF（TLS 下容忍对端不发 close_notify）。

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use crate::error::SyncError;

/// 明文 http 开发开关的环境变量名（只对 loopback 目标生效；`1`/`true`/`yes` 视为开启）。
pub const ALLOW_PLAINTEXT_HTTP_ENV: &str = "MENGHUNOTE_ALLOW_PLAINTEXT_HTTP";

/// 同 origin 重定向最多跟随的跳数（防止恶意 relay 用无限 30x 拖死客户端）。
pub const MAX_REDIRECTS: usize = 4;

/// 一个 HTTP 请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// 方法（GET/PUT/POST）。
    pub method: &'static str,
    /// 路径 + query（以 `/` 开头）。
    pub path: String,
    /// 头（小写名）。
    pub headers: Vec<(String, String)>,
    /// 请求体。
    pub body: Vec<u8>,
}

/// 一个 HTTP 响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// 状态码。
    pub status: u16,
    /// 响应体。
    pub body: Vec<u8>,
}

/// HTTP 传输抽象（测试注入点）。
pub trait HttpTransport {
    /// 发送请求并返回响应。
    ///
    /// 网络/读截断返回 [`SyncError::Transport`]；TLS 校验失败返回 [`SyncError::Tls`]；
    /// 明文/重定向策略拒绝返回 [`SyncError::TransportPolicy`]。
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError>;
}

/// 传输安全策略。
///
/// 默认（[`TransportPolicy::production`]）只允许 `https://`。明文 `http://` **只在**
/// 目标是 loopback 且显式选择本地开发策略时可用；非 loopback 明文在任何策略下都拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportPolicy {
    /// 是否允许 loopback 明文 http（仅开发）。
    pub allow_plaintext_localhost: bool,
}

impl TransportPolicy {
    /// 生产策略：只允许 https（明文一律拒绝）。
    #[must_use]
    pub const fn production() -> Self {
        Self {
            allow_plaintext_localhost: false,
        }
    }

    /// 本地开发策略：额外允许 `http://127.0.0.1|localhost`（非 loopback 仍拒绝）。
    #[must_use]
    pub const fn localhost_development() -> Self {
        Self {
            allow_plaintext_localhost: true,
        }
    }

    /// 从环境变量 [`ALLOW_PLAINTEXT_HTTP_ENV`] 读取（未设置即生产策略）。
    #[must_use]
    pub fn from_env() -> Self {
        let enabled = std::env::var(ALLOW_PLAINTEXT_HTTP_ENV)
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes"
                )
            })
            .unwrap_or(false);
        Self {
            allow_plaintext_localhost: enabled,
        }
    }
}

/// URL scheme（只支持 http/https）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Http,
    Https,
}

impl Scheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }
}

/// 解析后的 URL（绝对路径，不含 fragment）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedUrl {
    scheme: Scheme,
    /// 已规范化为 ASCII 小写（DNS 大小写不敏感；比较 origin 时直接比字符串）。
    host: String,
    port: u16,
    path: String,
}

/// 使用 `std::net::TcpStream` 的极简 HTTP/1.1 传输。
///
/// 构造时只做 URL/策略校验；TLS 信任根在第一次 https 请求时装配（系统证书存储的加载
/// 失败只在该次请求上体现为 [`SyncError::Tls`]，不阻止 http 本地开发）。
pub struct StdTcpTransport {
    scheme: Scheme,
    host: String,
    port: u16,
    timeout: Duration,
    policy: TransportPolicy,
    extra_roots: Vec<CertificateDer<'static>>,
    tls_config: Option<Arc<ClientConfig>>,
}

impl StdTcpTransport {
    /// 按环境变量策略构造（生产默认只允许 https）。
    pub fn new(base_url: &str) -> Result<Self, SyncError> {
        Self::new_with_policy(base_url, TransportPolicy::from_env())
    }

    /// 本机开发构造：显式允许 `http://127.0.0.1|localhost`。
    ///
    /// 仅用于本地联调（例如 `examples/e2e_localhost.rs` 对 `dev_server`）；生产路径必须用
    /// [`StdTcpTransport::new`] + https。
    pub fn new_local_dev(base_url: &str) -> Result<Self, SyncError> {
        Self::new_with_policy(base_url, TransportPolicy::localhost_development())
    }

    /// 按显式策略构造。
    pub fn new_with_policy(base_url: &str, policy: TransportPolicy) -> Result<Self, SyncError> {
        let parsed = parse_url(base_url)?;
        if parsed.path != "/" {
            return Err(SyncError::TransportPolicy(format!(
                "base_url 不支持路径前缀（收到 {:?}）；只接受 scheme://host:port",
                parsed.path
            )));
        }
        if parsed.scheme == Scheme::Http {
            if !policy.allow_plaintext_localhost {
                return Err(SyncError::TransportPolicy(format!(
                    "拒绝明文 http://{}:{}：生产必须 https；本机开发需显式开启（{ALLOW_PLAINTEXT_HTTP_ENV}=1 或 \
                     TransportPolicy::localhost_development）",
                    parsed.host, parsed.port
                )));
            }
            if !is_loopback_host(&parsed.host) {
                return Err(SyncError::TransportPolicy(format!(
                    "拒绝明文 http://{}:{}：明文只允许 loopback（127.0.0.1 / ::1 / localhost），\
                     非 loopback 主机必须使用 https",
                    parsed.host, parsed.port
                )));
            }
        }
        Ok(Self {
            scheme: parsed.scheme,
            host: parsed.host,
            port: parsed.port,
            timeout: Duration::from_secs(30),
            policy,
            extra_roots: Vec::new(),
            tls_config: None,
        })
    }

    /// 注入额外信任根（DER 编码的 CA 证书）。系统根证书仍然生效。
    ///
    /// 用于私有 CA / 自签部署；证书非法时立即返回 [`SyncError::Tls`]。
    pub fn with_extra_roots(
        mut self,
        roots: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> Result<Self, SyncError> {
        let mut probe = RootCertStore::empty();
        for root in roots {
            let der = CertificateDer::from(root.as_ref().to_vec());
            probe
                .add(der.clone())
                .map_err(|err| SyncError::Tls(format!("注入的信任根不是合法 CA 证书：{err}")))?;
            self.extra_roots.push(der);
        }
        // 信任集合变了，丢弃旧配置（下一次请求重建）。
        self.tls_config = None;
        Ok(self)
    }

    /// 当前策略（诊断/测试用）。
    #[must_use]
    pub fn policy(&self) -> TransportPolicy {
        self.policy
    }

    /// 单独完成一次 HTTP 交换（不处理重定向）。
    fn exchange(&mut self, request: &HttpRequest) -> Result<RawResponse, SyncError> {
        let stream = TcpStream::connect((self.host.as_str(), self.port))
            .map_err(|_| SyncError::Transport("连接 relay 失败"))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(|_| SyncError::Transport("设置读超时失败"))?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(|_| SyncError::Transport("设置写超时失败"))?;
        let host_header = self.host_header();
        let raw = match self.scheme {
            Scheme::Http => {
                let mut stream = stream;
                write_request(&mut stream, request, &host_header)
                    .map_err(|_| SyncError::Transport("写请求失败"))?;
                read_plain_response(&mut stream)
                    .map_err(|_| SyncError::Transport("读响应失败/被截断"))?
            }
            Scheme::Https => {
                let config = self.tls_config()?;
                let server_name = ServerName::try_from(self.host.clone()).map_err(|_| {
                    SyncError::TransportPolicy(format!("主机名不能用于 TLS 校验：{:?}", self.host))
                })?;
                let connection = ClientConnection::new(config, server_name)
                    .map_err(|err| SyncError::Tls(format!("构建 TLS 连接失败：{err}")))?;
                let mut tls = StreamOwned::new(connection, stream);
                // 先单独完成握手：证书链/主机名/有效期错误在这里变成 Tls，不混进后续 IO。
                if let Err(err) = tls.conn.complete_io(&mut tls.sock) {
                    return Err(map_tls_io(err));
                }
                write_request(&mut tls, request, &host_header).map_err(map_tls_io)?;
                read_tls_response(&mut tls)?
            }
        };
        parse_raw_response(&raw)
    }

    /// 延迟装配 TLS 客户端配置（系统根 + 注入根）。
    fn tls_config(&mut self) -> Result<Arc<ClientConfig>, SyncError> {
        if let Some(config) = &self.tls_config {
            return Ok(Arc::clone(config));
        }
        let mut roots = RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        let (native_added, _) = roots.add_parsable_certificates(native.certs);
        for der in &self.extra_roots {
            roots
                .add(der.clone())
                .map_err(|err| SyncError::Tls(format!("注入的信任根不是合法 CA 证书：{err}")))?;
        }
        if roots.is_empty() {
            return Err(SyncError::Tls(format!(
                "没有可用的信任根（系统证书存储可用 {} 张，注入 {} 张）；无法校验 relay 证书",
                native_added,
                self.extra_roots.len()
            )));
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|err| SyncError::Tls(format!("rustls 协议版本配置失败：{err}")))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let config = Arc::new(config);
        self.tls_config = Some(Arc::clone(&config));
        Ok(config)
    }

    /// `Host` 头：IPv6 加方括号；默认端口（80/443）省略端口（RFC 7230 §5.4）。
    fn host_header(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == self.scheme.default_port() {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    /// 解析 3xx 的 `Location`，只接受同 origin 目标，返回下一跳的 path(+query)。
    fn same_origin_target(&self, current_path: &str, location: &str) -> Result<String, SyncError> {
        let location = location.trim();
        if location.is_empty() {
            return Err(SyncError::TransportPolicy(
                "重定向 Location 为空".to_owned(),
            ));
        }
        let mut path = if location.contains("://") {
            let parsed = parse_url(location)?;
            if parsed.scheme == Scheme::Http && self.scheme == Scheme::Https {
                return Err(SyncError::TransportPolicy(format!(
                    "拒绝 https→http 降级重定向：{location}"
                )));
            }
            if parsed.scheme != self.scheme || parsed.host != self.host || parsed.port != self.port
            {
                return Err(SyncError::TransportPolicy(format!(
                    "拒绝跨 origin 重定向：{}://{}:{} → {}://{}:{}",
                    self.scheme.as_str(),
                    self.host,
                    self.port,
                    parsed.scheme.as_str(),
                    parsed.host,
                    parsed.port
                )));
            }
            parsed.path
        } else if location.starts_with("//") {
            return Err(SyncError::TransportPolicy(
                "拒绝协议相对重定向（//host/path）：无法保证同 origin".to_owned(),
            ));
        } else if location.starts_with('/') {
            location.to_owned()
        } else if let Some(query) = location.strip_prefix('?') {
            let base = current_path.split('#').next().unwrap_or(current_path);
            let base_path = base.split('?').next().unwrap_or(base);
            format!("{base_path}?{query}")
        } else {
            // 相对路径：拼到当前 path 的目录下（只用于转发，不做路径规范化）。
            let base = current_path.split('#').next().unwrap_or(current_path);
            let base = base.split('?').next().unwrap_or(base);
            match base.rsplit_once('/') {
                Some((dir, _)) => format!("{dir}/{location}"),
                None => format!("/{location}"),
            }
        };
        if let Some((before_fragment, _)) = path.split_once('#') {
            path = before_fragment.to_owned();
        }
        if !path.starts_with('/') {
            return Err(SyncError::TransportPolicy(format!(
                "重定向目标不是绝对路径：{location:?}"
            )));
        }
        if path.chars().any(|c| c.is_control() || c == ' ') {
            return Err(SyncError::TransportPolicy(
                "重定向目标含控制字符/空格，拒绝拼进请求行".to_owned(),
            ));
        }
        Ok(path)
    }
}

impl HttpTransport for StdTcpTransport {
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
        let mut current = request.clone();
        let mut hops = 0usize;
        loop {
            let raw = self.exchange(&current)?;
            if !matches!(raw.status, 301..=303 | 307 | 308) {
                return Ok(HttpResponse {
                    status: raw.status,
                    body: raw.body,
                });
            }
            let location = raw.location.ok_or_else(|| {
                SyncError::TransportPolicy(format!("HTTP {} 重定向缺少 Location", raw.status))
            })?;
            let next_path = self.same_origin_target(&current.path, &location)?;
            hops += 1;
            if hops > MAX_REDIRECTS {
                return Err(SyncError::TransportPolicy(format!(
                    "重定向超过 {MAX_REDIRECTS} 跳，拒绝继续跟随"
                )));
            }
            rewrite_for_redirect(&mut current, raw.status);
            current.path = next_path;
        }
    }
}

/// 一条未经业务解析的响应（保留 `Location` 供重定向策略判断）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawResponse {
    status: u16,
    location: Option<String>,
    body: Vec<u8>,
}

fn write_request<W: Write>(
    stream: &mut W,
    request: &HttpRequest,
    host_header: &str,
) -> std::io::Result<()> {
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\nContent-Length: {}\r\n",
        request.method,
        request.path,
        request.body.len()
    );
    for (name, value) in &request.headers {
        // 防御 header 注入：名字/值里不允许 CR/LF（调用方在本 crate 内，仍显式拒绝）。
        if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "header 含 CR/LF",
            ));
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&request.body)?;
    stream.flush()
}

fn read_plain_response<S: Read>(stream: &mut S) -> std::io::Result<Vec<u8>> {
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    Ok(raw)
}

/// 读 TLS 响应体。
///
/// 部分 TLS 端点/反向代理不发 `close_notify` 就关闭 TCP；只要已经收到字节就按 EOF 处理，
/// 响应完整性由 [`parse_raw_response`] 的 `Content-Length` 校验兜底，避免把合法响应误判成截断。
fn read_tls_response<S: Read>(stream: &mut S) -> Result<Vec<u8>, SyncError> {
    let mut raw = Vec::new();
    match stream.read_to_end(&mut raw) {
        Ok(_) => Ok(raw),
        Err(err) if err.kind() == ErrorKind::UnexpectedEof && !raw.is_empty() => Ok(raw),
        Err(err) => Err(map_tls_io(err)),
    }
}

/// 把 rustls 的 IO 错误拆成"证书/协议失败（不可重试）"与"网络失败（可重试）"。
fn map_tls_io(error: std::io::Error) -> SyncError {
    if let Some(cause) = rustls_cause(&error) {
        return SyncError::Tls(cause);
    }
    match error.kind() {
        ErrorKind::UnexpectedEof => SyncError::Transport("relay 连接被对端关闭（响应不完整）"),
        ErrorKind::WouldBlock | ErrorKind::TimedOut => SyncError::Transport("relay TLS 读写超时"),
        _ => SyncError::Transport("relay TLS 读写失败"),
    }
}

/// 从 `io::Error` 的 cause 链里找 `rustls::Error`（握手失败时 rustls 用它包装证书错误）。
fn rustls_cause(error: &std::io::Error) -> Option<String> {
    let inner = error.get_ref()?;
    if let Some(cause) = inner.downcast_ref::<rustls::Error>() {
        return Some(cause.to_string());
    }
    let mut source = inner.source();
    while let Some(current) = source {
        if let Some(cause) = current.downcast_ref::<rustls::Error>() {
            return Some(cause.to_string());
        }
        source = current.source();
    }
    None
}

fn parse_raw_response(raw: &[u8]) -> Result<RawResponse, SyncError> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(SyncError::Transport("响应头不完整"))?;
    let head =
        std::str::from_utf8(&raw[..split]).map_err(|_| SyncError::Transport("响应头不是 ASCII"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or(SyncError::Transport("缺少状态行"))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or(SyncError::Transport("状态行非法"))?
        .parse::<u16>()
        .map_err(|_| SyncError::Transport("状态码非法"))?;
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    let mut location = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "content-length" {
                content_length = value.parse().ok();
            } else if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            } else if name == "location" {
                location = Some(value.to_owned());
            }
        }
    }
    if chunked {
        return Err(SyncError::Transport("不支持 chunked 响应"));
    }
    let body = &raw[split + 4..];
    let body = match content_length {
        Some(length) => body
            .get(..length)
            .ok_or(SyncError::Transport("响应体被截断"))?
            .to_vec(),
        None => body.to_vec(),
    };
    Ok(RawResponse {
        status,
        location,
        body,
    })
}

/// 3xx 之后的方法/请求体改写：303 一律 GET；301/302 遵循浏览器惯例把 POST 降为 GET；
/// 307/308 保留方法与原始 body（重放同一份 CBOR 请求）。
fn rewrite_for_redirect(request: &mut HttpRequest, status: u16) {
    let downgrade_to_get =
        status == 303 || ((status == 301 || status == 302) && request.method == "POST");
    if downgrade_to_get {
        request.method = "GET";
        request.body.clear();
        request.headers.retain(|(name, _)| {
            !matches!(
                name.as_str(),
                "content-type" | "content-length" | "transfer-encoding"
            )
        });
    }
}

/// 解析 URL（`scheme://[host]:port[/path][?query][#fragment]`）。
fn parse_url(url: &str) -> Result<ParsedUrl, SyncError> {
    let url = url.trim();
    let (scheme_text, rest) = url.split_once("://").ok_or_else(|| {
        SyncError::TransportPolicy(format!(
            "URL 缺少 scheme（需要 http:// 或 https://）：{url:?}"
        ))
    })?;
    let scheme = match scheme_text.to_ascii_lowercase().as_str() {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        other => {
            return Err(SyncError::TransportPolicy(format!(
                "不支持的 URL scheme {other:?}（只支持 http/https）"
            )))
        }
    };
    let cut = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..cut];
    let mut path = rest[cut..].to_owned();
    if path.is_empty() {
        path.push('/');
    } else if path.starts_with('?') || path.starts_with('#') {
        path.insert(0, '/');
    }
    let (host, port) = parse_authority(authority, scheme)?;
    Ok(ParsedUrl {
        scheme,
        host,
        port,
        path,
    })
}

fn parse_authority(authority: &str, scheme: Scheme) -> Result<(String, u16), SyncError> {
    if authority.is_empty() {
        return Err(SyncError::TransportPolicy("URL 缺少主机名".to_owned()));
    }
    if authority.contains('@') {
        return Err(SyncError::TransportPolicy(
            "URL 不允许携带 userinfo（user:pass@host）；凭据必须走认证协议".to_owned(),
        ));
    }
    let (host, port_text) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let (host, rest) = after_bracket.split_once(']').ok_or_else(|| {
            SyncError::TransportPolicy(format!("IPv6 地址缺少右括号 ]：{authority:?}"))
        })?;
        match rest {
            "" => (host, None),
            _ => {
                let port = rest.strip_prefix(':').ok_or_else(|| {
                    SyncError::TransportPolicy(format!("IPv6 地址格式非法：{authority:?}"))
                })?;
                (host, Some(port))
            }
        }
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if host.contains(':') {
            return Err(SyncError::TransportPolicy(format!(
                "裸 IPv6 地址必须写成 [::1]：{authority:?}"
            )));
        }
        if port.is_empty() {
            return Err(SyncError::TransportPolicy(format!(
                "端口为空：{authority:?}"
            )));
        }
        (host, Some(port))
    } else {
        (authority, None)
    };
    if host.is_empty() {
        return Err(SyncError::TransportPolicy("URL 缺少主机名".to_owned()));
    }
    if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(SyncError::TransportPolicy(format!(
            "主机名含空白/控制字符：{host:?}"
        )));
    }
    let port = match port_text {
        Some(text) => text
            .parse::<u16>()
            .map_err(|_| SyncError::TransportPolicy(format!("端口不是合法 u16：{text:?}")))?,
        None => scheme.default_port(),
    };
    if port == 0 {
        return Err(SyncError::TransportPolicy("端口不能为 0".to_owned()));
    }
    Ok((host.to_ascii_lowercase(), port))
}

/// 明文目标是否 loopback（`localhost` 或 loopback IP 字面量）。
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport(base_url: &str) -> StdTcpTransport {
        StdTcpTransport::new_with_policy(base_url, TransportPolicy::production())
            .expect("transport")
    }

    #[test]
    fn plaintext_is_rejected_by_default_but_allowed_for_explicit_localhost_dev() {
        // 生产策略：loopback 明文也拒绝（默认必须 https）。
        assert!(matches!(
            StdTcpTransport::new_with_policy(
                "http://127.0.0.1:8080",
                TransportPolicy::production()
            ),
            Err(SyncError::TransportPolicy(_))
        ));
        // 显式开发策略：loopback 明文可以构造。
        let dev = StdTcpTransport::new_with_policy(
            "http://127.0.0.1:8080",
            TransportPolicy::localhost_development(),
        )
        .expect("loopback dev");
        assert_eq!(dev.host, "127.0.0.1");
        assert_eq!(dev.port, 8080);
        assert_eq!(dev.scheme, Scheme::Http);
        assert!(dev.policy().allow_plaintext_localhost);
        assert!(StdTcpTransport::new_local_dev("http://localhost:18080").is_ok());
        assert!(StdTcpTransport::new_local_dev("http://[::1]:18080").is_ok());
        // 非 loopback 明文即使显式开启也拒绝。
        assert!(matches!(
            StdTcpTransport::new_local_dev("http://relay.example:8080"),
            Err(SyncError::TransportPolicy(_))
        ));
        assert!(matches!(
            StdTcpTransport::new_local_dev("http://192.168.1.10:8080"),
            Err(SyncError::TransportPolicy(_))
        ));
        // 0.0.0.0 不是 loopback。
        assert!(StdTcpTransport::new_local_dev("http://0.0.0.0:8080").is_err());
    }

    #[test]
    fn https_defaults_and_url_validation() {
        let tls = transport("https://Relay.Example");
        assert_eq!(tls.host, "relay.example");
        assert_eq!(tls.port, 443);
        assert_eq!(tls.scheme, Scheme::Https);
        let tls = transport("https://relay.example:8443");
        assert_eq!(tls.port, 8443);
        assert_eq!(tls.host_header(), "relay.example:8443");
        assert_eq!(
            transport("https://relay.example").host_header(),
            "relay.example"
        );
        assert_eq!(transport("https://[::1]:8443").host_header(), "[::1]:8443");

        for bad in [
            "relay.example",
            "ftp://relay.example",
            "https://relay.example/api",
            "https://relay.example:notaport",
            "https://:8443",
            "https://user:pass@relay.example",
            "https://relay.example:0",
            "https://::1:8443",
        ] {
            assert!(
                StdTcpTransport::new(bad).is_err(),
                "非法 URL 必须拒绝：{bad}"
            );
        }
    }

    #[test]
    fn env_switch_enables_only_loopback_plaintext() {
        std::env::set_var(ALLOW_PLAINTEXT_HTTP_ENV, "1");
        let transport =
            StdTcpTransport::new("http://127.0.0.1:8080").expect("env 开关只放行 loopback 明文");
        assert!(transport.policy().allow_plaintext_localhost);
        assert!(
            StdTcpTransport::new("http://relay.example:8080").is_err(),
            "env 开关不能放行非 loopback 明文"
        );
        std::env::remove_var(ALLOW_PLAINTEXT_HTTP_ENV);
        assert!(!TransportPolicy::from_env().allow_plaintext_localhost);
        assert!(StdTcpTransport::new("http://127.0.0.1:8080").is_err());
    }

    #[test]
    fn redirects_must_stay_on_the_same_origin() {
        let tls = transport("https://relay.example:8443");
        // 同 origin：绝对 URL / 绝对路径 / 相对路径 / 仅 query 都可以跟随。
        assert_eq!(
            tls.same_origin_target("/v1/x", "https://relay.example:8443/v2?q=1")
                .expect("同 origin 绝对 URL"),
            "/v2?q=1"
        );
        assert_eq!(
            tls.same_origin_target("/v1/x", "/v2").expect("绝对路径"),
            "/v2"
        );
        assert_eq!(
            tls.same_origin_target("/v1/x", "sibling")
                .expect("相对路径"),
            "/v1/sibling"
        );
        assert_eq!(
            tls.same_origin_target("/v1/x?a=1", "?b=2")
                .expect("仅 query"),
            "/v1/x?b=2"
        );
        assert_eq!(
            tls.same_origin_target("/v1/x#frag", "/v2#frag")
                .expect("fragment 丢弃"),
            "/v2"
        );
        // 跨 origin / 降级 / 协议相对：一律拒绝。
        for bad in [
            "https://evil.example/v2",
            "https://127.0.0.1:8443/v2",
            "https://relay.example:9443/v2",
            "http://relay.example:8443/v2",
            "//evil.example/v2",
            "/v2 with space",
            "",
        ] {
            assert!(
                matches!(
                    tls.same_origin_target("/v1/x", bad),
                    Err(SyncError::TransportPolicy(_))
                ),
                "必须拒绝的重定向：{bad}"
            );
        }
    }

    #[test]
    fn redirect_method_rewrite_keeps_signed_body_only_for_307_308() {
        let mut post = HttpRequest {
            method: "POST",
            path: "/v1/x".to_owned(),
            headers: vec![("content-type".to_owned(), "application/cbor".to_owned())],
            body: vec![1, 2, 3],
        };
        rewrite_for_redirect(&mut post, 307);
        assert_eq!(post.method, "POST");
        assert_eq!(post.body, vec![1, 2, 3]);
        rewrite_for_redirect(&mut post, 303);
        assert_eq!(post.method, "GET");
        assert!(post.body.is_empty());
        assert!(post.headers.is_empty());

        let mut put = HttpRequest {
            method: "PUT",
            path: "/v1/x".to_owned(),
            headers: Vec::new(),
            body: vec![9],
        };
        rewrite_for_redirect(&mut put, 301);
        assert_eq!(put.method, "PUT", "301 不降级非 POST 方法");
        assert_eq!(put.body, vec![9]);
    }

    #[test]
    fn parses_response_with_content_length_and_location() {
        let raw = b"HTTP/1.1 201 Created\r\nContent-Length: 3\r\n\r\nabc";
        let response = parse_raw_response(raw).expect("parse");
        assert_eq!(response.status, 201);
        assert_eq!(response.body, b"abc");
        assert_eq!(response.location, None);

        let raw = b"HTTP/1.1 302 Found\r\nLocation: /v2\r\nContent-Length: 0\r\n\r\n";
        let response = parse_raw_response(raw).expect("parse");
        assert_eq!(response.status, 302);
        assert_eq!(response.location.as_deref(), Some("/v2"));

        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc";
        assert!(parse_raw_response(raw).is_err(), "截断必须报错");
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
        assert!(parse_raw_response(raw).is_err(), "chunked 不支持");
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabcdef";
        assert_eq!(parse_raw_response(raw).expect("按长度截断").body, b"abcde");
    }
}
