//! 真实 TLS 传输门禁（评审 0002「生产阻断」，task-112）。
//!
//! 用 rustls 起**本地真实 TLS 服务器**（rcgen 现场生成自签 CA + 叶子证书），验证客户端：
//!
//! - 把自签 CA 注入信任根后，`https://localhost` 能完成握手并收发 HTTP；
//! - 过期证书 / hostname 不匹配 / 未信任的自签链 → 拒绝（[`SyncError::Tls`]）；
//! - 非 loopback 明文、跨 origin 重定向、`https→http` 降级 → 拒绝（[`SyncError::TransportPolicy`]）；
//! - 同 origin 302 可跟随（带路径重写）；
//! - loopback 明文在显式开发策略下仍可用（现有 `http://127.0.0.1` 开发脚本路径）。
//!
//! 这是**真实 socket + 真实握手**的回归，不是 mock transport。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use menghunote_sync::http_relay::{HttpRequest, HttpTransport, StdTcpTransport};
use menghunote_sync::SyncError;
use rcgen::{
    date_time_ymd, BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

/// 测试用自签 CA。
struct TestCa {
    cert: CertificateDer<'static>,
    params: CertificateParams,
    key: KeyPair,
}

impl TestCa {
    fn new() -> Self {
        let key = KeyPair::generate().expect("生成 CA 私钥");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA 参数");
        params
            .distinguished_name
            .push(DnType::CommonName, "menghunote tls test CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        let cert = params.self_signed(&key).expect("自签 CA");
        Self {
            cert: cert.der().clone(),
            params,
            key,
        }
    }

    fn der(&self) -> CertificateDer<'static> {
        self.cert.clone()
    }

    fn issue(
        &self,
        sans: &[&str],
        not_before: (i32, u8, u8),
        not_after: (i32, u8, u8),
    ) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
        let key = KeyPair::generate().expect("生成叶子私钥");
        let mut params =
            CertificateParams::new(sans.iter().map(|san| (*san).to_owned()).collect::<Vec<_>>())
                .expect("叶子参数");
        params.distinguished_name.push(
            DnType::CommonName,
            sans.first().copied().unwrap_or("localhost"),
        );
        params.is_ca = IsCa::NoCa;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.not_before = date_time_ymd(not_before.0, not_before.1, not_before.2);
        params.not_after = date_time_ymd(not_after.0, not_after.1, not_after.2);
        let issuer = Issuer::from_params(&self.params, &self.key);
        let cert = params.signed_by(&key, &issuer).expect("CA 签发叶子证书");
        (
            cert.der().clone(),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
    }
}

/// 有效证书的起止日期（跨当前时间，避免测试随时间失效）。
const VALID_START: (i32, u8, u8) = (2024, 1, 1);
const VALID_END: (i32, u8, u8) = (2124, 1, 1);

/// 已过期证书的起止日期（恒定过期，测试不依赖系统时钟的具体值）。
const EXPIRED_START: (i32, u8, u8) = (2020, 1, 1);
const EXPIRED_END: (i32, u8, u8) = (2021, 1, 1);

type Handler = Arc<dyn Fn(&str, &str) -> Vec<u8> + Send + Sync>;

/// 本地 TLS 服务器（记录收到的 `(method, path)` 供断言）。
struct TlsServer {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

impl TlsServer {
    fn start(
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
        handler: Handler,
    ) -> Self {
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("TLS 协议版本")
                .with_no_client_auth()
                .with_single_cert(chain, key)
                .expect("TLS 服务器证书");
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定 TLS 测试端口");
        let addr = listener.local_addr().expect("TLS 测试端口");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_for_thread = Arc::clone(&seen);
        thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(tcp) = incoming else { continue };
                let config = Arc::clone(&config);
                let handler = Arc::clone(&handler);
                let seen = Arc::clone(&seen_for_thread);
                thread::spawn(move || {
                    let _ = serve_tls(tcp, config, handler, seen);
                });
            }
        });
        Self { addr, seen }
    }

    fn url(&self) -> String {
        format!("https://localhost:{}", self.addr.port())
    }

    fn paths(&self) -> Vec<String> {
        self.seen
            .lock()
            .expect("seen")
            .iter()
            .map(|(_, path)| path.clone())
            .collect()
    }
}

fn serve_tls(
    tcp: TcpStream,
    config: Arc<ServerConfig>,
    handler: Handler,
    seen: Arc<Mutex<Vec<(String, String)>>>,
) -> std::io::Result<()> {
    let connection = ServerConnection::new(config).map_err(std::io::Error::other)?;
    let mut tls = StreamOwned::new(connection, tcp);
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = tls.read(&mut chunk)?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(end) = header_end(&buffer) else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
        if buffer.len() < end + 4 + content_length(&head) {
            continue;
        }
        let (method, path) = request_line(&head)?;
        seen.lock()
            .expect("seen")
            .push((method.clone(), path.clone()));
        tls.write_all(&handler(&method, &path))?;
        tls.flush()?;
        tls.conn.send_close_notify();
        tls.flush()?;
        return Ok(());
    }
}

/// 本地明文 HTTP 服务器（只用于 loopback 开发策略回归）。
struct PlainServer {
    addr: SocketAddr,
}

impl PlainServer {
    fn start(handler: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定明文测试端口");
        let addr = listener.local_addr().expect("明文测试端口");
        thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut tcp) = incoming else { continue };
                let handler = Arc::clone(&handler);
                thread::spawn(move || {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 8192];
                    loop {
                        let Ok(read) = tcp.read(&mut chunk) else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        let Some(end) = header_end(&buffer) else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
                        if buffer.len() < end + 4 + content_length(&head) {
                            continue;
                        }
                        let Ok((method, path)) = request_line(&head) else {
                            return;
                        };
                        let _ = tcp.write_all(&handler(&method, &path));
                        let _ = tcp.flush();
                        return;
                    }
                });
            }
        });
        Self { addr }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }
}

fn header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length(head: &str) -> usize {
    head.split("\r\n")
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}

fn request_line(head: &str) -> std::io::Result<(String, String)> {
    let mut parts = head.split("\r\n").next().unwrap_or("").split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| std::io::Error::other("缺方法"))?
        .to_owned();
    let path = parts
        .next()
        .ok_or_else(|| std::io::Error::other("缺路径"))?
        .to_owned();
    Ok((method, path))
}

fn ok_response(body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn redirect_response(location: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

fn get(path: &str) -> HttpRequest {
    HttpRequest {
        method: "GET",
        path: path.to_owned(),
        headers: Vec::new(),
        body: Vec::new(),
    }
}

fn https_with_ca(server: &TlsServer, ca: &TestCa) -> StdTcpTransport {
    StdTcpTransport::new(&server.url())
        .expect("https base_url")
        .with_extra_roots(vec![ca.der()])
        .expect("注入自签 CA")
}

#[test]
fn trusted_local_tls_server_accepts_request() {
    let ca = TestCa::new();
    let (chain, key) = ca.issue(&["localhost"], VALID_START, VALID_END);
    let server = TlsServer::start(
        vec![chain],
        key,
        Arc::new(|_method, path| {
            if path == "/v1/ping" {
                ok_response(b"pong")
            } else {
                ok_response(path.as_bytes())
            }
        }),
    );
    let mut transport = https_with_ca(&server, &ca);
    let response = transport
        .send(&get("/v1/ping"))
        .expect("注入信任根后 TLS 必须连通");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"pong");
    assert_eq!(server.paths(), vec!["/v1/ping".to_owned()]);
}

#[test]
fn expired_certificate_is_rejected() {
    let ca = TestCa::new();
    let (chain, key) = ca.issue(&["localhost"], EXPIRED_START, EXPIRED_END);
    let server = TlsServer::start(vec![chain], key, Arc::new(|_, _| ok_response(b"leak")));
    let mut transport = https_with_ca(&server, &ca);
    match transport.send(&get("/v1/ping")) {
        Err(SyncError::Tls(message)) => {
            assert!(
                message.contains("Expired") || message.contains("expired"),
                "错误信息应指出过期：{message}"
            );
        }
        other => panic!("过期证书必须拒绝，实际 {other:?}"),
    }
    assert!(server.paths().is_empty(), "失败握手不应到达 HTTP 层");
}

#[test]
fn wrong_hostname_certificate_is_rejected() {
    let ca = TestCa::new();
    let (chain, key) = ca.issue(&["other.example"], VALID_START, VALID_END);
    let server = TlsServer::start(vec![chain], key, Arc::new(|_, _| ok_response(b"leak")));
    let mut transport = https_with_ca(&server, &ca);
    match transport.send(&get("/v1/ping")) {
        Err(SyncError::Tls(message)) => {
            assert!(
                message.contains("not valid for name")
                    || message.contains("NotValidForName")
                    || message.contains("InvalidCertificate"),
                "错误信息应指出 hostname 不匹配：{message}"
            );
        }
        other => panic!("hostname 不匹配必须拒绝，实际 {other:?}"),
    }
}

#[test]
fn untrusted_self_signed_certificate_is_rejected() {
    let server_ca = TestCa::new();
    let (chain, key) = server_ca.issue(&["localhost"], VALID_START, VALID_END);
    let server = TlsServer::start(vec![chain], key, Arc::new(|_, _| ok_response(b"leak")));
    // 客户端只信任另一个（无关的）CA：必须报 UnknownIssuer，而不是连上。
    let other_ca = TestCa::new();
    let mut transport = StdTcpTransport::new(&server.url())
        .expect("https base_url")
        .with_extra_roots(vec![other_ca.der()])
        .expect("注入无关 CA");
    match transport.send(&get("/v1/ping")) {
        Err(SyncError::Tls(message)) => {
            assert!(
                message.contains("UnknownIssuer") || message.contains("invalid peer certificate"),
                "错误信息应指出证书链不受信：{message}"
            );
        }
        other => panic!("未信任自签链必须拒绝，实际 {other:?}"),
    }
}

#[test]
fn cross_origin_redirect_is_rejected() {
    let ca = TestCa::new();
    let (chain, key) = ca.issue(&["localhost"], VALID_START, VALID_END);
    let server = TlsServer::start(
        vec![chain],
        key,
        Arc::new(|_method, path| match path {
            "/cross-host" => redirect_response("https://evil.example/v1/ping"),
            "/cross-port" => redirect_response("https://localhost:9443/v1/ping"),
            // 协议相对 URL 也可能是另一个 host。
            "/protocol-relative" => redirect_response("//evil.example/v1/ping"),
            _ => ok_response(b"unexpected"),
        }),
    );
    let mut transport = https_with_ca(&server, &ca);
    for path in ["/cross-host", "/cross-port", "/protocol-relative"] {
        match transport.send(&get(path)) {
            Err(SyncError::TransportPolicy(message)) => {
                assert!(
                    message.contains("origin") || message.contains("协议相对"),
                    "错误信息应说明 origin：{message}"
                );
            }
            other => panic!("跨 origin 重定向必须拒绝（{path}），实际 {other:?}"),
        }
    }
    // 拒绝发生在跟随之前：服务器只看到三个初始请求，没有任何重定向目标。
    let paths = server.paths();
    assert_eq!(
        paths,
        vec![
            "/cross-host".to_owned(),
            "/cross-port".to_owned(),
            "/protocol-relative".to_owned()
        ]
    );
    assert!(
        !paths.iter().any(|path| path == "/v1/ping"),
        "不得跟随到重定向目标"
    );
}

#[test]
fn https_downgrade_redirect_is_rejected() {
    let ca = TestCa::new();
    let (chain, key) = ca.issue(&["localhost"], VALID_START, VALID_END);
    let server = TlsServer::start(
        vec![chain],
        key,
        Arc::new(|_method, _path| redirect_response("http://localhost:8080/v1/ping")),
    );
    let mut transport = https_with_ca(&server, &ca);
    match transport.send(&get("/v1/ping")) {
        Err(SyncError::TransportPolicy(message)) => {
            assert!(
                message.contains("降级") && message.contains("http"),
                "错误信息应指出 https→http 降级：{message}"
            );
        }
        other => panic!("降级到明文必须拒绝，实际 {other:?}"),
    }
}

#[test]
fn same_origin_redirect_is_followed() {
    let ca = TestCa::new();
    let (chain, key) = ca.issue(&["localhost"], VALID_START, VALID_END);
    let server = TlsServer::start(
        vec![chain],
        key,
        Arc::new(|method, path| match path {
            "/v1/old" => redirect_response("/v1/new"),
            "/v1/new" => {
                assert_eq!(method, "GET");
                ok_response(b"moved")
            }
            _ => ok_response(b"unexpected"),
        }),
    );
    let mut transport = https_with_ca(&server, &ca);
    let response = transport
        .send(&get("/v1/old"))
        .expect("同 origin 302 应跟随");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"moved");
    assert_eq!(
        server.paths(),
        vec!["/v1/old".to_owned(), "/v1/new".to_owned()]
    );
}

#[test]
fn redirect_without_location_is_rejected() {
    let ca = TestCa::new();
    let (chain, key) = ca.issue(&["localhost"], VALID_START, VALID_END);
    let server = TlsServer::start(
        vec![chain],
        key,
        Arc::new(|_method, _path| {
            b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        }),
    );
    let mut transport = https_with_ca(&server, &ca);
    assert!(matches!(
        transport.send(&get("/v1/ping")),
        Err(SyncError::TransportPolicy(_))
    ));
}

#[test]
fn loopback_plaintext_requires_explicit_dev_policy_then_works() {
    let server = PlainServer::start(Arc::new(|_method, path| {
        if path == "/v1/ping" {
            ok_response(b"pong")
        } else {
            ok_response(b"unexpected")
        }
    }));
    let url = server.url();
    // 生产策略（默认）：loopback 明文也拒绝。
    assert!(matches!(
        StdTcpTransport::new(&url),
        Err(SyncError::TransportPolicy(_))
    ));
    // 显式开发策略：现有 `http://127.0.0.1` 开发脚本仍可跑。
    let mut dev = StdTcpTransport::new_local_dev(&url).expect("loopback 开发传输");
    let response = dev.send(&get("/v1/ping")).expect("loopback 明文必须可用");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"pong");
}

#[test]
fn non_loopback_plaintext_is_rejected_even_in_dev_mode() {
    assert!(matches!(
        StdTcpTransport::new_local_dev("http://192.0.2.10:18080"),
        Err(SyncError::TransportPolicy(_))
    ));
    assert!(matches!(
        StdTcpTransport::new("http://relay.example:18080"),
        Err(SyncError::TransportPolicy(_))
    ));
}
