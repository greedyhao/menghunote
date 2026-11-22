//! **DEV ONLY**：进程内起一个真实 axum relay，供 `e2e_rotation_two_rounds` 自包含运行。
//!
//! 评审 0003 T13 要求"同一 durable E1 旧 B 跨两轮"必须能在**真实 HTTP + 真实 relay**
//! 上验证，并且中间要有一次 90 天 `FakeClock` 推进 + 多轮 GC。外部 `dev_server` 进程
//! 看不到客户端的假时钟、也没有 GC 入口，所以本文件把 relay 直接**编进 example**：
//!
//! - 临时 SQLite（`target/tmp/...`，每次运行唯一目录，`Drop` 时删）；
//! - `relay::time::FakeClock` 注入 `Db::open`（relay 所有 TTL/GC 只看这个时钟）；
//! - 只用 loopback 的**放行验签 hook**（复制 `relay/examples/dev_server.rs` 的
//!   `DevPermissiveHook`；放行 hook 只属于 example，绝不进 relay lib）；
//! - `127.0.0.1:0` 绑定临时端口，独立线程 + tokio multi-thread runtime 跑 `axum::serve`；
//! - 暴露 `advance(secs)` / `run_gc()` / 只读 checkpoint 事实，供 e2e 断言。
//!
//! 本文件同时被 cargo 当作独立 example 编译（`examples/*.rs` 都会），因此保留一个
//! 只打印提示的 `main`；真正的 harness 由 `e2e_rotation_two_rounds` 通过
//! `#[path = "dev_relay.rs"] mod dev_relay;` 引入。

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use menghunote_relay as relay;
use relay::config::RelayConfig;
use relay::db::{Db, DbConfig};
use relay::gc::GcOutcome;
use relay::ids::{b64url_encode, random_bytes};
use relay::sig::{SignatureCheck, SignatureHook};
use relay::time::{Clock, FakeClock};

/// 本 harness 的错误类型（example 里统一用 `Box<dyn Error>`）。
pub type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

/// 只允许开发使用的验签实现：**全部放行**（与 `relay/examples/dev_server.rs` 同一个 10 行）。
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
}

impl DevRelay {
    /// 建临时库 + 起真实 HTTP 服务；返回时端口已在监听（客户端可立刻 genesis）。
    pub fn start() -> AnyResult<Self> {
        let dir = unique_dir()?;
        std::fs::create_dir_all(&dir)?;
        let setup_secret = {
            let bytes = random_bytes::<32>()?;
            b64url_encode(&bytes)
        };
        let config = Arc::new(
            RelayConfig::new(dir.join("relay.sqlite3")).with_setup_secret(setup_secret.clone()),
        );
        // relay 的全部 TTL/GC 只看注入的时钟；这里用可控假时钟，默认从系统时间起步。
        let clock = Arc::new(FakeClock::new(system_now_secs()));
        let db = Db::open(DbConfig::new(config.db.path.clone()), clock.clone())?;
        let hook = Arc::new(DevPermissiveHook);
        let state = relay::api::AppState::new(db.clone(), config.clone(), hook);

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
                    let served = axum::serve(listener, app).with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    });
                    if let Err(error) = served.await {
                        eprintln!("[dev-relay] server error: {error}");
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
        println!(
            "[dev-relay] in-process relay: db={} listen={} (DEV ONLY 放行验签 hook)",
            dir.display(),
            addr
        );
        Ok(Self {
            base_url: format!("http://{addr}"),
            setup_secret,
            clock,
            state: Some(state),
            db: Some(db),
            shutdown: Some(shutdown_tx),
            server: Some(server),
            dir,
        })
    }

    /// 客户端使用的基础 URL。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// genesis 用的一次性 setup secret。
    pub fn setup_secret(&self) -> &str {
        &self.setup_secret
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
        Ok(db.read(move |conn| relay::checkpoints::get(conn, &checkpoint_id))?)
    }

    /// checkpoint 当前 refs 数量（prune 后应为 0）。
    pub fn checkpoint_ref_count(&self, checkpoint_id: [u8; 16]) -> AnyResult<usize> {
        let db = self.db.as_ref().ok_or("relay 已停止")?;
        Ok(db.read(move |conn| {
            relay::checkpoints::refs(conn, &checkpoint_id).map(|refs| refs.len())
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

fn main() {
    println!("dev_relay 是 e2e_rotation_two_rounds 的进程内 relay 脚手架，请直接运行该示例。");
}
