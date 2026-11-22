//! **DEV ONLY**：进程内起一个真实 axum relay，供 sync 的 e2e example 自包含运行。
//!
//! 本 crate 是**纯测试脚手架**，不是生产组件：
//!
//! - 只允许出现在其它 crate 的 `[dev-dependencies]`，**不得**被任何生产 crate 的
//!   `[dependencies]` 引用；
//! - 内置的 `DevPermissiveHook` 对**任何签名一律放行**（验签全放行），只绑定 loopback；
//!   因此**不得用于生产，也不得把基于它的结果当作鉴权/验签验收的结论**——验签相关的
//!   验收必须走 relay 生产路径（`relay::Ed25519Hook`）。
//!
//! 背景（评审 0003 T13）："同一 durable E1 旧 B 跨两轮"必须能在**真实 HTTP + 真实 relay**
//! 上验证，并且中间要有一次 90 天 `FakeClock` 推进 + 多轮 GC。外部 `dev_server` 进程看不到
//! 客户端的假时钟、也没有 GC 入口，所以这里把 relay 直接**编进测试进程**：
//!
//! - 临时 SQLite（`target/tmp/...`，每次运行唯一目录，`Drop` 时删）；
//! - `relay::time::FakeClock` 注入 `Db::open`（relay 所有 TTL/GC 只看这个时钟）；
//! - 只用 loopback 的**放行验签 hook**（与 `relay/examples/dev_server.rs` 同一个实现；
//!   放行 hook 只属于测试脚手架，绝不进 relay lib）；
//! - `127.0.0.1:0` 绑定临时端口，独立线程 + tokio multi-thread runtime 跑 `axum::serve`；
//! - `start_tls()`（ADR 0013 S3b）用 rcgen 现场生成自签 CA + `localhost` 叶子证书，起
//!   **真实 TLS** 监听（rustls），并把 CA DER 经 `tls_ca_der()` 交给测试注入客户端信任根
//!   （`RelayContext::with_test_extra_roots`）；仍只绑 loopback、仍是放行验签 hook；
//! - 暴露 `advance_secs(secs)` / `run_gc()` / 只读 checkpoint 事实，供 e2e 断言。
//!
//! 历史：本文件的代码原先内联在 `crates/sync/examples/dev_relay.rs`，由三个 e2e example
//! 用 `#[path = "..."] mod` 引入；抽成独立 crate 后，example 只依赖
//! `menghunote-dev-relay`，不再各自编译一份 harness，也不需要 `#[path]` 黑魔法。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use menghunote_relay as relay;
use rcgen::{
    date_time_ymd, BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair,
};
use relay::config::RelayConfig;
use relay::db::{Db, DbConfig};
use relay::gc::GcOutcome;
use relay::ids::{b64url_encode, random_bytes};
pub use relay::ratelimit::RateLimitSpec;
use relay::sig::{SignatureCheck, SignatureHook};
use relay::time::{Clock, FakeClock};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// 本 harness 的错误类型（沿用 example 里统一的 `Box<dyn Error>` 口径）。
pub type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

/// 只允许开发使用的验签实现：**全部放行**（与 `relay/examples/dev_server.rs` 同一个 10 行）。
///
/// **DEV ONLY**：任何签名都接受，仅用于 loopback 的进程内 e2e；生产用
/// `relay::Ed25519Hook`。不要把它接进任何面向真实用户的路径。
#[derive(Debug, Default)]
struct DevPermissiveHook;

impl SignatureHook for DevPermissiveHook {
    fn verify(&self, _check: &SignatureCheck<'_>) -> relay::Result<()> {
        // DEV ONLY：任何签名都接受。生产用 relay::Ed25519Hook。
        Ok(())
    }
}

/// 进程内 relay 的句柄（`Drop` 时停服务器、关库、删临时目录）。
pub struct DevRelay {
    base_url: String,
    setup_secret: String,
    clock: Arc<FakeClock>,
    state: Option<Arc<relay::api::AppState>>,
    db: Option<Db>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<JoinHandle<()>>,
    dir: PathBuf,
    /// TLS 监听时给测试客户端的自签 CA（DER）；明文监听为 `None`。
    tls_ca_der: Option<Vec<u8>>,
}

impl DevRelay {
    /// 建临时库 + 起真实 **明文 HTTP** 服务（loopback）；返回时端口已在监听。
    pub fn start() -> AnyResult<Self> {
        Self::start_inner(false, None)
    }

    /// 建临时库 + 起真实明文服务，并**显式覆盖** v2 创建入口限流（S3a 的 429 用例）。
    ///
    /// `None` = 沿用 relay 默认（6 次/分钟）；`Some(spec)` = 本实例的 `/v2/vaults` 限流。
    /// 其它面（v1 / v2 auth / v2 数据面）不受影响。
    pub fn start_with_v2_create_limit(limit: Option<RateLimitSpec>) -> AnyResult<Self> {
        Self::start_inner(false, Some(limit))
    }

    /// 建临时库 + 起真实 **TLS HTTPS** 服务（自签 CA + `localhost` 叶子证书）。
    ///
    /// 客户端必须显式注入 [`Self::tls_ca_der`] 作为信任根
    /// （`RelayContext::with_test_extra_roots`）；**不会**关闭证书链/SAN/有效期校验。
    pub fn start_tls() -> AnyResult<Self> {
        Self::start_inner(true, None)
    }

    fn start_inner(tls: bool, v2_create_limit: Option<Option<RateLimitSpec>>) -> AnyResult<Self> {
        let dir = unique_dir()?;
        std::fs::create_dir_all(&dir)?;
        let setup_secret = {
            let bytes = random_bytes::<32>()?;
            b64url_encode(&bytes)
        };
        let mut config =
            RelayConfig::new(dir.join("relay.sqlite3")).with_setup_secret(setup_secret.clone());
        if let Some(limit) = v2_create_limit {
            let auth_limit = config.v2_auth_limit;
            config = config.with_v2_rate_limits(limit, auth_limit);
        }
        let config = Arc::new(config);
        // relay 的全部 TTL/GC 只看注入的时钟；这里用可控假时钟，默认从系统时间起步。
        let clock = Arc::new(FakeClock::new(system_now_secs()));
        let db = Db::open(DbConfig::new(config.db.path.clone()), clock.clone())?;
        let hook = Arc::new(DevPermissiveHook);
        let state = relay::api::AppState::new(db.clone(), config.clone(), hook);

        // TLS 材料在起线程**之前**生成：坏证书在这里就是明确错误。
        let tls_material = if tls {
            Some(TlsMaterial::generate()?)
        } else {
            None
        };
        let tls_server = tls_material
            .as_ref()
            .map(|material| material.server.clone());
        let (addr_tx, addr_rx) = std::sync::mpsc::channel::<Result<SocketAddr, String>>();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server_state = state.clone();
        let server = std::thread::Builder::new()
            .name("e2e-inprocess-relay".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = addr_tx.send(Err(format!("tokio runtime: {error}")));
                        return;
                    }
                };
                runtime.block_on(async move {
                    // 只绑 loopback：放行验签的 relay 不得暴露到网络（生产另走 TLS 门禁）。
                    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
                        Ok(listener) => listener,
                        Err(error) => {
                            let _ = addr_tx.send(Err(format!("bind 127.0.0.1:0: {error}")));
                            return;
                        }
                    };
                    let addr = match listener.local_addr() {
                        Ok(addr) => addr,
                        Err(error) => {
                            let _ = addr_tx.send(Err(format!("local_addr: {error}")));
                            return;
                        }
                    };
                    let _ = addr_tx.send(Ok(addr));
                    // 只打 method/path/status（不打印 query/header/body：可能含密文/秘密）。
                    // 默认关闭：`E2E_RELAY_REQ_LOG=1` 时打开，便于定位某个 4xx/5xx 的出站请求。
                    let app = relay::api::router(server_state);
                    let app = if std::env::var_os("E2E_RELAY_REQ_LOG").is_some() {
                        app.layer(middleware::from_fn(log_requests))
                    } else {
                        app
                    };
                    match tls_server {
                        Some(config) => {
                            let listener = TlsListener {
                                inner: listener,
                                acceptor: TlsAcceptor::from(config),
                            };
                            let served =
                                axum::serve(listener, app).with_graceful_shutdown(async move {
                                    let _ = shutdown_rx.await;
                                });
                            if let Err(error) = served.await {
                                eprintln!("[dev-relay] tls server error: {error}");
                            }
                        }
                        None => {
                            // ADR 0017 S5a：管理端点按对端地址判定 loopback；明文 harness
                            // 必须注入 ConnectInfo。TLS 分支的自定义 Listener 暂不注入
                            // （远程 admin 后置；TLS e2e 不覆盖管理端点）。
                            let served = axum::serve(
                                listener,
                                app.into_make_service_with_connect_info::<SocketAddr>(),
                            )
                            .with_graceful_shutdown(async move {
                                let _ = shutdown_rx.await;
                            });
                            if let Err(error) = served.await {
                                eprintln!("[dev-relay] server error: {error}");
                            }
                        }
                    }
                });
            })?;
        let addr = match addr_rx.recv() {
            Ok(Ok(addr)) => addr,
            Ok(Err(message)) => {
                let _ = server.join();
                let _ = std::fs::remove_dir_all(&dir);
                return Err(message.into());
            }
            Err(error) => {
                let _ = server.join();
                let _ = std::fs::remove_dir_all(&dir);
                return Err(format!("relay 线程未回报监听地址：{error}").into());
            }
        };
        // TLS 的证书 SAN 是 `localhost`（IP SAN 也可，但 URL 固定用主机名以免与
        // `127.0.0.1`/`::1` 解析顺序耦合）；明文沿用 `http://<addr>`。
        let base_url = if tls {
            format!("https://localhost:{}", addr.port())
        } else {
            format!("http://{addr}")
        };
        println!(
            "[dev-relay] in-process {} relay: db={} listen={} (DEV ONLY 放行验签 hook)",
            if tls { "TLS" } else { "HTTP" },
            dir.display(),
            addr
        );
        Ok(Self {
            base_url,
            setup_secret,
            clock,
            state: Some(state),
            db: Some(db),
            shutdown: Some(shutdown_tx),
            server: Some(server),
            dir,
            tls_ca_der: tls_material.map(|material| material.ca_der),
        })
    }

    /// 客户端使用的基础 URL（TLS 监听为 `https://localhost:<port>`）。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// TLS 监听的自签 CA（DER）；明文监听返回 `None`。
    ///
    /// 只用于测试把该 CA 显式注入客户端信任根（`with_test_extra_roots`）；
    /// **不是**"一键信任"入口，也不关闭任何证书校验。
    pub fn tls_ca_der(&self) -> Option<&[u8]> {
        self.tls_ca_der.as_deref()
    }

    /// genesis 用的一次性 setup secret。
    pub fn setup_secret(&self) -> &str {
        &self.setup_secret
    }

    /// 为**全新** `vault_id` 建预留并发行一张 `/v2/vaults` 创建票据（ADR 0018 §2 唯一入口）。
    ///
    /// DEV ONLY：明文只返回一次，测试用它驱动客户端的票据创建；库/日志都只有哈希。
    /// 该 ID 已存在（任何 registry 状态）→ `CAS_CONFLICT`（测试必须用新 ID）。
    pub fn issue_create_ticket(
        &self,
        vault_id: [u8; 16],
        ttl: std::time::Duration,
    ) -> AnyResult<[u8; relay::admin::CREATE_TICKET_BYTES]> {
        let db = self.db.as_ref().ok_or("relay 已停止")?;
        Ok(relay::admin::create_reservation_for(db, vault_id, ttl)?.token)
    }

    /// **DEV ONLY**：给**已存在（ACTIVE）**的 vault 直接种一张新票据（不改变 lifecycle）。
    ///
    /// ADR 0018 S4b 起没有"对 ACTIVE vault 只发票据"的 HTTP/CLI/SDK 入口；这里仅供 e2e
    /// 复现 `/v2/vaults` 的 409 `ALREADY_INITIALIZED`（票据有效、不被消费）。
    pub fn seed_ticket_for_existing_vault(
        &self,
        vault_id: [u8; 16],
        ttl: std::time::Duration,
    ) -> AnyResult<[u8; relay::admin::CREATE_TICKET_BYTES]> {
        let db = self.db.as_ref().ok_or("relay 已停止")?;
        let token = relay::ids::random_bytes::<{ relay::admin::CREATE_TICKET_BYTES }>()?;
        let token_hash = relay::ids::sha256(&token);
        let now = self.clock.now_secs();
        let expires_at = now
            .checked_add(i64::try_from(ttl.as_secs())?)
            .ok_or("ticket ttl overflow")?;
        db.write_instance_blocking(move |conn| {
            conn.execute(
                "INSERT INTO create_tickets(token_hash, vault_id, created_at, expires_at, quota,
                                            consumed_at)
                 VALUES (?1, ?2, ?3, ?4, 1, NULL)",
                (token_hash.as_slice(), vault_id.as_slice(), now, expires_at),
            )?;
            Ok(())
        })?;
        Ok(token)
    }

    /// relay 假时钟当前值（Unix 秒）。
    pub fn now(&self) -> i64 {
        self.clock.now_secs()
    }

    /// 推进 relay 假时钟，返回推进后的时刻。
    pub fn advance_secs(&self, secs: i64) -> i64 {
        self.clock.advance_secs(secs);
        self.clock.now_secs()
    }

    /// 跑一次真实运维入口 `relay::api::run_gc`（与部署侧后台任务同一实现）。
    pub fn run_gc(&self) -> AnyResult<GcOutcome> {
        let state = self.state.clone().ok_or("relay 已停止")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(runtime.block_on(relay::api::run_gc(state))?)
    }

    /// 只读 checkpoint 行事实（prune 后行与签名骨架仍在，refs 已清）。
    pub fn checkpoint(
        &self,
        checkpoint_id: [u8; 16],
    ) -> AnyResult<relay::checkpoints::CheckpointRow> {
        let db = self.db.as_ref().ok_or("relay 已停止")?;
        Ok(db.read_instance(|conn| {
            let scope = relay::vault::legacy_scope_from_db(conn)?;
            let scoped = relay::ScopeConn::new(conn, scope);
            relay::checkpoints::get(&scoped, &checkpoint_id)
        })?)
    }

    /// checkpoint 当前 refs 数量（prune 后应为 0）。
    pub fn checkpoint_ref_count(&self, checkpoint_id: [u8; 16]) -> AnyResult<usize> {
        let db = self.db.as_ref().ok_or("relay 已停止")?;
        Ok(db.read_instance(|conn| {
            let scope = relay::vault::legacy_scope_from_db(conn)?;
            let scoped = relay::ScopeConn::new(conn, scope);
            relay::checkpoints::refs(&scoped, &checkpoint_id).map(|refs| refs.len())
        })?)
    }
}

impl Drop for DevRelay {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
        let _ = self.state.take();
        let _ = self.db.take();
        // 调试用：E2E_KEEP_RELAY_DB=1 保留临时目录（默认删）。
        if std::env::var_os("E2E_KEEP_RELAY_DB").is_some() {
            println!(
                "[dev-relay] keep relay db dir {} (E2E_KEEP_RELAY_DB)",
                self.dir.display()
            );
            return;
        }
        // 临时目录只含本 harness 的 SQLite（无私钥/身份），best-effort 清理。
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 进程内 TLS relay 的证书材料（自签 CA + `localhost` 叶子证书 + rustls 服务器配置）。
struct TlsMaterial {
    /// 自签 CA（DER）；测试端用它作为唯一显式信任根。
    ca_der: Vec<u8>,
    /// rustls 服务器配置（叶子证书 + 私钥）。
    server: Arc<ServerConfig>,
}

impl TlsMaterial {
    /// 现场生成 CA + 叶子证书（有效期跨当前时间，避免测试随时间失效）。
    fn generate() -> AnyResult<Self> {
        let ca_key = KeyPair::generate()?;
        let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "menghunote dev-relay test CA");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        let ca_cert = ca_params.self_signed(&ca_key)?;
        let ca_der: CertificateDer<'static> = ca_cert.der().clone();

        let leaf_key = KeyPair::generate()?;
        let mut leaf_params = CertificateParams::new(vec!["localhost".to_owned()])?;
        leaf_params
            .distinguished_name
            .push(DnType::CommonName, "localhost");
        leaf_params.is_ca = IsCa::NoCa;
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        leaf_params.not_before = date_time_ymd(2024, 1, 1);
        leaf_params.not_after = date_time_ymd(2124, 1, 1);
        let issuer = Issuer::from_params(&ca_params, &ca_key);
        let leaf_cert = leaf_params.signed_by(&leaf_key, &issuer)?;
        let leaf_der = leaf_cert.der().clone();
        let leaf_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));

        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_no_client_auth()
                .with_single_cert(vec![leaf_der], leaf_key)?;
        Ok(Self {
            ca_der: ca_der.as_ref().to_vec(),
            server: Arc::new(config),
        })
    }
}

/// axum 的 TLS 监听适配器：每个连接先完成 rustls 握手再交给 hyper。
struct TlsListener {
    inner: tokio::net::TcpListener,
    acceptor: TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = match self.inner.accept().await {
                Ok(accepted) => accepted,
                Err(_) => continue,
            };
            match self.acceptor.accept(stream).await {
                Ok(tls) => return (tls, addr),
                Err(error) => {
                    // 握手失败（例如客户端不信任自签 CA）只影响该连接；继续 accept。
                    eprintln!("[dev-relay] TLS handshake failed: {error}");
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// `target/tmp` 下唯一的临时目录（不用 %TEMP%；Windows 上 cargo 子进程不可写）。
fn unique_dir() -> AnyResult<PathBuf> {
    let root = match option_env!("CARGO_TARGET_TMPDIR") {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("tmp"),
    };
    let nonce: [u8; 8] = random_bytes::<8>()?;
    Ok(root.join(format!(
        "e2e-dev-relay-{}-{}",
        std::process::id(),
        relay::ids::hex_lower(&nonce)
    )))
}

async fn log_requests(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;
    println!(
        "[dev-relay] {method} {path} -> {}",
        response.status().as_u16()
    );
    response
}

fn system_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
