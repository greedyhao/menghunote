//! **DEV ONLY** 的 relay 开发服务器（真实端到端联调用）。
//!
//! 这个 example 用**完全放行**的 `SignatureHook`（生产必须是 `relay::Ed25519Hook`），
//! 只为让 `sync-core` 在本地对跑 genesis / 推送 / 拉取 / 续传 / 配对。
//!
//! ```text
//! cargo run -p menghunote-relay --example dev_server
//! cargo run -p menghunote-relay --example dev_server -- --listen 127.0.0.1:18080 --reset
//! cargo run -p menghunote-relay --example dev_server -- --setup-only   # 让客户端自己做 genesis
//! ```
//!
//! 创建票据（`mn-pair://create`）由 Web 管理面「创建领取链接」或测试夹具发行；
//! 旧的 `--issue-ticket` 开发入口已移除（ADR 0018 §4）。
//!
//! - 默认 profile 是 debug（`cfg!(debug_assertions)`），release 构建必须显式 `--dev` 才启动；
//! - `--reset` 会删掉 SQLite 文件（含 `-wal`/`-shm`）后重建；
//! - `Ctrl+C` 优雅关闭（等待 writer 线程退出）；
//! - 请求日志只打 `method path -> status (ms)`，**不含** body/header/cookie/密文。
//!
//! 参数（CLI 优先于环境变量）：
//! - `--listen <addr>` / `RELAY_DEV_LISTEN`，默认 `127.0.0.1:18080`
//! - `--db <path>` / `RELAY_DEV_DB`，默认 `target/tmp/relay-dev/relay.sqlite3`
//! - `--reset` / `RELAY_DEV_RESET=1`
//! - `--setup-only` / `RELAY_DEV_SETUP_ONLY=1`：不自动 genesis，只打印 setup secret 等客户端初始化
//! - `--dev`：release 构建下的显式解锁
//! - `--allow-plaintext` / `RELAY_DEV_ALLOW_PLAINTEXT=1`：允许非 loopback 明文监听。
//!   **默认只允许 127.0.0.1/[::1]**（与生产 `serve` 同一门禁）；开发服务器仅用于本机联调，
//!   对外暴露必须走反向代理 TLS，不能直接把放行验签的 dev server 暴露到网络。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use ed25519_dalek::SigningKey;
use tokio::net::TcpListener;

use menghunote_relay as relay;
use relay::config::RelayConfig;
use relay::db::{schema, Db};
use relay::ids::{b64url_encode, hex_lower, random_bytes, random_id16};
use relay::sig::{SignatureCheck, SignatureHook};
use relay::time::SystemClock;
use relay::vault;
use relay::verify;

const DEFAULT_LISTEN: &str = "127.0.0.1:18080";

/// 只允许开发使用的验签实现：**全部放行**。
#[derive(Debug, Default)]
struct DevPermissiveHook;

impl SignatureHook for DevPermissiveHook {
    fn verify(&self, _check: &SignatureCheck<'_>) -> relay::Result<()> {
        // DEV ONLY：任何签名都接受。生产用 relay::Ed25519Hook。
        Ok(())
    }
}

#[derive(Debug)]
struct Args {
    listen: String,
    db: PathBuf,
    reset: bool,
    dev: bool,
    setup_only: bool,
    allow_plaintext: bool,
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes")
    )
}

fn default_db_path() -> PathBuf {
    if let Some(dir) = option_env!("CARGO_TARGET_TMPDIR") {
        return PathBuf::from(dir).join("relay-dev").join("relay.sqlite3");
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("tmp")
        .join("relay-dev")
        .join("relay.sqlite3")
}

fn usage() -> String {
    format!(
        "relay dev server (DEV ONLY)\n\
         \n\
         usage: dev_server [options]\n\
         \n\
         options:\n\
           --listen <addr>   listen address (default {DEFAULT_LISTEN}, env RELAY_DEV_LISTEN)\n\
           --db <path>       sqlite path (default target/tmp/relay-dev/relay.sqlite3, env RELAY_DEV_DB)\n\
           --reset           delete the sqlite file (and -wal/-shm) before starting\n\
           --setup-only      do not auto-genesis; print a setup secret for the client\n\
           --allow-plaintext allow non-loopback plaintext listen (env RELAY_DEV_ALLOW_PLAINTEXT)\n\
           --dev             allow running in release builds (DEV ONLY)\n\
           -h, --help        print this help"
    )
}

fn parse_args() -> std::result::Result<Args, String> {
    let mut listen =
        std::env::var("RELAY_DEV_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
    let mut db = std::env::var("RELAY_DEV_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_db_path());
    let mut reset = env_flag("RELAY_DEV_RESET");
    let mut dev = false;
    let mut setup_only = env_flag("RELAY_DEV_SETUP_ONLY");
    let mut allow_plaintext = env_flag("RELAY_DEV_ALLOW_PLAINTEXT");

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" | "-l" => {
                listen = args.next().ok_or_else(|| "--listen 需要参数".to_string())?;
            }
            "--db" => {
                db = PathBuf::from(args.next().ok_or_else(|| "--db 需要参数".to_string())?);
            }
            "--reset" => reset = true,
            "--dev" => dev = true,
            "--setup-only" => setup_only = true,
            "--allow-plaintext" => allow_plaintext = true,
            "-h" | "--help" => return Err(usage()),
            other => return Err(format!("未知参数 {other}\n\n{}", usage())),
        }
    }
    Ok(Args {
        listen,
        db,
        reset,
        dev,
        setup_only,
        allow_plaintext,
    })
}

fn remove_sqlite_files(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let target = PathBuf::from(format!("{}{suffix}", path.display()));
        match std::fs::remove_file(&target) {
            Ok(()) => println!("[dev] removed {}", target.display()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => eprintln!("[dev] warning: cannot remove {}: {err}", target.display()),
        }
    }
}

/// 自动 genesis 时使用的开发身份（私钥**打印**出来，供联调脚本使用）。
struct DevIdentity {
    device_id: relay::ids::Id16,
    sign_seed: [u8; 32],
    manager_seed: [u8; 32],
    hpke_seed: [u8; 32],
}

fn auto_genesis(
    db: &Db,
    hook: Arc<dyn SignatureHook>,
    config: Arc<RelayConfig>,
) -> std::result::Result<DevIdentity, String> {
    let sign_seed = random_bytes::<32>().map_err(|err| format!("csprng: {err}"))?;
    let manager_seed = random_bytes::<32>().map_err(|err| format!("csprng: {err}"))?;
    let hpke_seed = random_bytes::<32>().map_err(|err| format!("csprng: {err}"))?;
    let vault_id = random_id16().map_err(|err| format!("csprng: {err}"))?;
    let device_id = random_id16().map_err(|err| format!("csprng: {err}"))?;

    let sign_pk = SigningKey::from_bytes(&sign_seed)
        .verifying_key()
        .to_bytes();
    let manager_pub = SigningKey::from_bytes(&manager_seed)
        .verifying_key()
        .to_bytes();
    // relay 不引入 x25519 crate：dev identity 把 32B 随机数同时当作"HPKE 私钥/公钥"占位，
    // 真实 HPKE grant 解密由客户端自己造钥（--setup-only 流程）。
    let hpke_placeholder = hpke_seed;
    let certificate =
        verify::new_certificate(&vault_id, &device_id, &sign_pk, &hpke_placeholder, 1)
            .encode()
            .map_err(|err| format!("certificate cde: {err}"))?;

    let request = vault::InitRequest {
        vault_id,
        device_id,
        sign_pk,
        hpke_pk: hpke_placeholder,
        manager_pub,
        setup_secret: config
            .setup_secret
            .clone()
            .ok_or_else(|| "missing setup secret".to_string())?,
        certificate,
        certificate_signature: vec![0u8; 64],
    };
    let db_handle = db.clone();
    let clock = db.clock_handle();
    let scope = relay::VaultScope::new(vault_id);
    db_handle
        .write_blocking(scope, move |conn| {
            vault::initialize(conn, clock.as_ref(), &config, hook.as_ref(), &request)
        })
        .map_err(|err| format!("genesis: {err}"))?;
    Ok(DevIdentity {
        device_id,
        sign_seed,
        manager_seed,
        hpke_seed,
    })
}

fn print_identity(
    vault: &vault::Vault,
    identity: Option<&DevIdentity>,
    setup_secret: &str,
    args: &Args,
) {
    println!("============================================================");
    println!("  DEV ONLY — relay 使用放行签名的 hook（生产必须 Ed25519Hook）");
    println!("  NEVER EXPOSE THIS SERVER OUTSIDE localhost / your dev box");
    println!("============================================================");
    println!("listen          : {}", args.listen);
    println!("db              : {}", args.db.display());
    println!("vault_id        : {}", hex_lower(&vault.vault_id));
    println!(
        "relay_instance_id: {}（服务器标识，不是 vault_id）",
        hex_lower(&vault.instance_id)
    );
    println!("log_id          : {}", hex_lower(&vault.log_id));
    println!("epoch           : {}", vault.active_epoch);
    println!("control_rev     : {}", vault.control_rev);
    println!("setup secret    : {setup_secret}");
    match identity {
        Some(identity) => {
            println!("dev_device_id   : {}", hex_lower(&identity.device_id));
            println!("dev_ed25519_seed: {}", hex_lower(&identity.sign_seed));
            println!("dev_manager_seed: {}", hex_lower(&identity.manager_seed));
            println!(
                "dev_hpke_seed   : {} (占位：dev genesis 未做真实 X25519)",
                hex_lower(&identity.hpke_seed)
            );
            println!(
                "auth hint       : POST /v1/auth/challenges {{0:1,1:device_id}} → /v1/auth/tokens"
            );
        }
        None => {
            println!("state           : 未初始化（--setup-only），请用下方的 setup secret 调");
            println!("                  POST /v1/vaults 完成客户端 genesis（CBOR 控制面）");
        }
    }
    println!("content-type    : application/cbor（对象 part 除外：application/octet-stream）");
    println!("============================================================");
}

async fn log_requests(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    // 只打 method/path/status/耗时：绝不打印 query、header、body（可能含密文/秘密）。
    println!(
        "[req] {method} {path} -> {} ({} ms)",
        response.status().as_u16(),
        started.elapsed().as_millis()
    );
    response
}

async fn shutdown_signal() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => println!("[dev] Ctrl+C received, shutting down…"),
        Err(err) => eprintln!("[dev] failed to listen for Ctrl+C: {err}"),
    }
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            println!("{message}");
            std::process::exit(if message.starts_with("未知参数") {
                2
            } else {
                0
            });
        }
    };

    // DEV ONLY 闸门：release 构建必须显式 --dev。
    if !args.dev && !cfg!(debug_assertions) {
        eprintln!(
            "refusing to start: release build without --dev.\n\
             这个 example 用完全放行的签名 hook（DEV ONLY），release 下必须显式 --dev。"
        );
        std::process::exit(2);
    }

    // 明文门禁（与生产 `serve` 同一策略）：非 loopback 需要显式 --allow-plaintext，
    // 且必须在建库/开端口之前拒绝（dev server 放行验签，更不能误暴露）。
    let listen: SocketAddr = match args.listen.parse() {
        Ok(addr) => addr,
        Err(err) => {
            eprintln!("invalid --listen {}: {err}", args.listen);
            std::process::exit(2);
        }
    };
    if let Err(message) =
        relay::config::ensure_plaintext_listen_allowed(listen, args.allow_plaintext)
    {
        eprintln!("refusing to start: {message}");
        std::process::exit(2);
    }

    if let Some(parent) = args.db.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                eprintln!("cannot create {}: {err}", parent.display());
                std::process::exit(1);
            }
        }
    }
    if args.reset {
        remove_sqlite_files(&args.db);
    }

    let setup_secret = match random_bytes::<32>() {
        Ok(bytes) => b64url_encode(&bytes),
        Err(err) => {
            eprintln!("csprng unavailable: {err}");
            std::process::exit(1);
        }
    };
    let config =
        Arc::new(RelayConfig::new(args.db.clone()).with_setup_secret(setup_secret.clone()));
    let db = match Db::open(config.db.clone(), Arc::new(SystemClock)) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("cannot open {}: {err}", args.db.display());
            std::process::exit(1);
        }
    };
    let hook = Arc::new(DevPermissiveHook);

    // 多租户（ADR 0014）：先显式列出库里的 vault，再按 scope 读；不按"第一行"猜租户。
    let existing_ids = match db.read_instance(schema::list_vault_ids) {
        Ok(ids) => ids,
        Err(err) => {
            eprintln!("cannot list vaults: {err}");
            std::process::exit(1);
        }
    };
    let existing_scope = existing_ids.first().map(|id| relay::VaultScope::new(*id));
    let existing = match existing_scope {
        Some(scope) => match db.read(scope, vault::load) {
            Ok(vault) => Some(vault),
            Err(err) => {
                eprintln!("cannot read vault: {err}");
                std::process::exit(1);
            }
        },
        None => None,
    };

    let mut started_identity = None;
    let vault = match existing {
        Some(vault) => {
            println!(
                "[dev] 已存在 vault（{}）；如需重建请加 --reset",
                hex_lower(&vault.vault_id)
            );
            vault
        }
        None if args.setup_only => {
            println!("[dev] --setup-only：不自动 genesis，等待客户端 POST /v1/vaults");
            let instance = match db.read_instance(schema::instance_id) {
                Ok(instance) => instance,
                Err(err) => {
                    eprintln!("cannot read instance_id: {err}");
                    std::process::exit(1);
                }
            };
            print_setup_only_banner(&args, &setup_secret, &instance);
            let state = relay::api::AppState::new(db.clone(), config.clone(), hook.clone());
            run_server(state, &args);
            drop(db);
            return;
        }
        None => {
            let identity = match auto_genesis(&db, hook.clone(), config.clone()) {
                Ok(identity) => identity,
                Err(err) => {
                    eprintln!("genesis failed: {err}");
                    std::process::exit(1);
                }
            };
            started_identity = Some(identity);
            let scope = match db.read_instance(schema::list_vault_ids) {
                Ok(ids) => match ids.first() {
                    Some(id) => relay::VaultScope::new(*id),
                    None => {
                        eprintln!("genesis produced no vault row");
                        std::process::exit(1);
                    }
                },
                Err(err) => {
                    eprintln!("cannot list vaults: {err}");
                    std::process::exit(1);
                }
            };
            match db.read(scope, vault::load) {
                Ok(vault) => vault,
                Err(err) => {
                    eprintln!("cannot reload vault: {err}");
                    std::process::exit(1);
                }
            }
        }
    };

    print_identity(&vault, started_identity.as_ref(), &setup_secret, &args);

    let state = relay::api::AppState::new(db.clone(), config.clone(), hook.clone());
    run_server(state, &args);
    drop(db);
    println!("[dev] stopped cleanly");
}

fn print_setup_only_banner(args: &Args, setup_secret: &str, instance: &relay::ids::Id16) {
    println!("============================================================");
    println!("  DEV ONLY — relay 使用放行签名的 hook（生产必须 Ed25519Hook）");
    println!("  NEVER EXPOSE THIS SERVER OUTSIDE localhost / your dev box");
    println!("============================================================");
    println!("listen          : {}", args.listen);
    println!("db              : {}", args.db.display());
    println!(
        "relay_instance_id: {}（服务器标识，不是 vault_id；vault_id 由客户端 POST /v1/vaults 创建）",
        hex_lower(instance)
    );
    println!("setup secret    : {setup_secret}");
    println!("next step       : POST /v1/vaults（CBOR, application/cbor）");
    println!("============================================================");
}

/// 建 runtime 再跑服务：开库/genesis 都在同步上下文完成（`write_blocking` 不能在 runtime 里阻塞）。
fn run_server(state: Arc<relay::api::AppState>, args: &Args) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("cannot build tokio runtime: {err}");
            std::process::exit(1);
        }
    };
    runtime.block_on(serve(state, args));
}

async fn serve(state: Arc<relay::api::AppState>, args: &Args) {
    let listen: SocketAddr = match args.listen.parse() {
        Ok(addr) => addr,
        Err(err) => {
            eprintln!("invalid --listen {}: {err}", args.listen);
            std::process::exit(2);
        }
    };
    let listener = match TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("cannot bind {listen}: {err}");
            std::process::exit(1);
        }
    };
    let addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(err) => {
            eprintln!("cannot read local addr: {err}");
            std::process::exit(1);
        }
    };
    println!("[dev] listening on http://{addr}  (Ctrl+C to stop)");
    let app = relay::api::router(state).layer(middleware::from_fn(log_requests));
    // ADR 0017 S5a：管理端点按对端地址判定 loopback，必须把连接信息注入扩展。
    if let Err(err) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    {
        eprintln!("server error: {err}");
    }
}
