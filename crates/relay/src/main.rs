//! `menghunote-relay` 生产二进制（ADR 0006 §7）。
//!
//! 子命令：
//! - `serve --config <toml>`：启动 HTTP 中继；**强制真实 Ed25519 验签**（没有任何放行 hook
//!   开关），`--dev` 仅在 debug 构建里被接受（只多打一行提示，验签不变）。
//! - `init --db <path>`：建库 + schema 迁移 + 生成一次性 setup secret（只打印/落哈希，不启网络）。
//! - `status --db <path>`：打印 vault 水位/checkpoint/配额/设备数（`key=value`，机器可读）。
//! - `delete-vault --db <path> --yes`：显式删除 SQLite 库（需 `--yes` + 手动输入 `DELETE`）。
//!
//! 配置（`serve --config`）是扁平 TOML：
//! ```toml
//! listen = "127.0.0.1:18080"
//! db = "/var/lib/menghunote/relay.sqlite3"   # 必须绝对路径
//! normal_limit = 4294967296                   # Q（默认 4GiB）
//! maintenance_limit = 8589934592              # 峰值 Q+R（默认 2Q）
//! cursor_key = "…64 hex…"                     # 可选；与 cursor_key_path 二选一
//! cursor_key_path = "cursor.key"              # 可选；相对路径按配置文件目录解析
//! max_snapshot_bytes = 67108864               # 可选；默认 64MiB
//! ```
//!
//! 请求日志只打 `method path -> status`，**不含** body/query/header（可能含密文/秘密）。

use std::fs;
use std::io::{self, BufRead, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use serde::Deserialize;
use tokio::net::TcpListener;

use menghunote_relay as relay;
use relay::api::{router, AppState};
use relay::checkpoints;
use relay::config::RelayConfig;
use relay::db::{schema, Db, DbConfig};
use relay::ids::{b64url_encode, hex_lower, random_bytes, sha256, Hash32};
use relay::quota;
use relay::time::SystemClock;
use relay::vault;
use relay::verify::Ed25519Hook;

/// setup secret 的长度（32B CSPRNG → base64url 43 字符）。
const SETUP_SECRET_BYTES: usize = 32;

fn usage() -> &'static str {
    "menghunote-relay —— 自建加密中继（ADR 0006 §7）

用法:
  menghunote-relay serve --config <toml> [--dev]
      启动 HTTP 中继；强制真实 Ed25519 验签。--dev 仅 debug 构建可用（不改验签）。
  menghunote-relay init --db <path>
      建库 + 迁移 + 生成 setup secret（打印一次；不启网络）。
  menghunote-relay status --db <path>
      打印 vault state/epoch/control_rev/head_seq/gc_floor/两份 checkpoint/配额/设备数。
  menghunote-relay delete-vault --db <path> --yes
      永久删除 SQLite 库；需 --yes 且手动输入 DELETE 二次确认。
  menghunote-relay --help

配置字段（serve --config，扁平 TOML）:
  listen（必填）、db（必填，绝对路径）、normal_limit、maintenance_limit、
  cursor_key 或 cursor_key_path、max_snapshot_bytes
"
}

/// `serve` 的配置文件（严格拒绝未知字段，避免把 `permissive_hook = true` 之类误当开关）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServeConfig {
    listen: String,
    db: PathBuf,
    #[serde(default)]
    normal_limit: Option<i64>,
    #[serde(default)]
    maintenance_limit: Option<i64>,
    #[serde(default)]
    cursor_key: Option<String>,
    #[serde(default)]
    cursor_key_path: Option<PathBuf>,
    #[serde(default)]
    max_snapshot_bytes: Option<u64>,
}

/// 校验后的启动参数。
#[derive(Debug)]
struct ValidatedServe {
    listen: SocketAddr,
    config: RelayConfig,
}

impl ServeConfig {
    /// 校验并构造 `RelayConfig`；错误文案面向运维（说明怎么改）。
    fn validate(self, base_dir: &Path) -> Result<ValidatedServe, String> {
        let listen: SocketAddr = self.listen.parse().map_err(|_| {
            format!(
                "listen 不是合法地址: {:?}（示例 127.0.0.1:18080 或 0.0.0.0:8080）",
                self.listen
            )
        })?;
        if self.db.as_os_str().is_empty() {
            return Err("db 不能为空".to_string());
        }
        if self.db.is_relative() {
            return Err(format!(
                "db 必须是绝对路径（收到 {:?}）；相对路径会随进程 cwd 漂移，无法保证重启后指向同一库",
                self.db
            ));
        }

        let defaults = DbConfig::new(self.db.clone());
        let normal_limit = self.normal_limit.unwrap_or(defaults.normal_limit);
        let maintenance_limit = self.maintenance_limit.unwrap_or(defaults.maintenance_limit);
        if normal_limit <= 0 {
            return Err(format!("normal_limit 必须 > 0（收到 {normal_limit}）"));
        }
        if maintenance_limit <= normal_limit {
            return Err(format!(
                "maintenance_limit({maintenance_limit}) 必须 > normal_limit({normal_limit})；\
                 维护预算 R = 峰值 - Q 必须为正"
            ));
        }

        let cursor_key = match (self.cursor_key, self.cursor_key_path) {
            (Some(_), Some(_)) => {
                return Err("cursor_key 与 cursor_key_path 只能给一个".to_string())
            }
            (Some(text), None) => Some(parse_cursor_key(&text, "cursor_key")?),
            (None, Some(path)) => {
                let resolved = if path.is_relative() {
                    base_dir.join(&path)
                } else {
                    path.clone()
                };
                let text = fs::read_to_string(&resolved)
                    .map_err(|err| format!("读 cursor_key_path {:?} 失败: {err}", resolved))?;
                Some(parse_cursor_key(text.trim(), "cursor_key_path")?)
            }
            (None, None) => None,
        };

        let proto_max = relay::proto::limits::MAX_LORO_SNAPSHOT_BYTES as u64;
        let max_snapshot_bytes = self
            .max_snapshot_bytes
            .unwrap_or(relay::proto::limits::MAX_SNAPSHOT_CDE_BYTES as u64);
        if max_snapshot_bytes == 0 {
            return Err("max_snapshot_bytes 必须 > 0".to_string());
        }
        if max_snapshot_bytes > proto_max {
            return Err(format!(
                "max_snapshot_bytes({max_snapshot_bytes}) 超过协议上限 {proto_max}"
            ));
        }

        let mut config = RelayConfig::new(self.db)
            .with_limits(normal_limit, maintenance_limit)
            .with_max_snapshot_bytes(max_snapshot_bytes);
        if let Some(key) = cursor_key {
            config = config.with_cursor_key(key);
        }
        Ok(ValidatedServe { listen, config })
    }
}

/// 解析 64 位十六进制（32B）的游标 HMAC key。
fn parse_cursor_key(text: &str, field: &str) -> Result<Hash32, String> {
    let trimmed = text.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() != 64 {
        return Err(format!(
            "{field} 必须是 64 个十六进制字符（32B HMAC key；收到 {} 个字符）",
            bytes.len()
        ));
    }
    let mut out = [0u8; 32];
    for (index, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        let high = hex_nibble(pair[0])
            .ok_or_else(|| format!("{field} 含非十六进制字符（第 {index} 字节）"))?;
        let low = hex_nibble(pair[1])
            .ok_or_else(|| format!("{field} 含非十六进制字符（第 {index} 字节）"))?;
        out[index] = (high << 4) | low;
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 解析后的命令行。
#[derive(Debug)]
enum Command {
    Serve { config: PathBuf, dev: bool },
    Init { db: PathBuf },
    Status { db: PathBuf },
    DeleteVault { db: PathBuf, yes: bool },
    Help,
}

fn parse_args(raw: &[String]) -> Result<Command, String> {
    let Some(sub) = raw.first().map(String::as_str) else {
        return Ok(Command::Help);
    };
    match sub {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "serve" => {
            let mut config = None;
            let mut dev = false;
            let mut iter = raw.iter().skip(1);
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--config" => {
                        config = Some(PathBuf::from(
                            iter.next()
                                .ok_or_else(|| "--config 需要 <toml 路径>".to_string())?,
                        ));
                    }
                    "--dev" => dev = true,
                    "-h" | "--help" => return Ok(Command::Help),
                    other => return Err(format!("serve 不认识参数 {other:?}\n\n{}", usage())),
                }
            }
            let config = config.ok_or_else(|| "serve 缺少 --config <toml>".to_string())?;
            Ok(Command::Serve { config, dev })
        }
        "init" | "status" => {
            let mut db = None;
            let mut iter = raw.iter().skip(1);
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--db" => {
                        db = Some(PathBuf::from(
                            iter.next()
                                .ok_or_else(|| "--db 需要 <sqlite 路径>".to_string())?,
                        ));
                    }
                    "-h" | "--help" => return Ok(Command::Help),
                    other => return Err(format!("{sub} 不认识参数 {other:?}\n\n{}", usage())),
                }
            }
            let db = db.ok_or_else(|| format!("{sub} 缺少 --db <path>"))?;
            Ok(if sub == "init" {
                Command::Init { db }
            } else {
                Command::Status { db }
            })
        }
        "delete-vault" => {
            let mut db = None;
            let mut yes = false;
            let mut iter = raw.iter().skip(1);
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--db" => {
                        db = Some(PathBuf::from(
                            iter.next()
                                .ok_or_else(|| "--db 需要 <sqlite 路径>".to_string())?,
                        ));
                    }
                    "--yes" => yes = true,
                    "-h" | "--help" => return Ok(Command::Help),
                    other => {
                        return Err(format!("delete-vault 不认识参数 {other:?}\n\n{}", usage()))
                    }
                }
            }
            let db = db.ok_or_else(|| "delete-vault 缺少 --db <path>".to_string())?;
            Ok(Command::DeleteVault { db, yes })
        }
        other => Err(format!("未知子命令 {other:?}\n\n{}", usage())),
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&raw) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    let result = match command {
        Command::Help => {
            println!("{}", usage());
            Ok(())
        }
        Command::Serve { config, dev } => serve(&config, dev),
        Command::Init { db } => init(&db),
        Command::Status { db } => status(&db),
        Command::DeleteVault { db, yes } => delete_vault(&db, yes),
    };
    if let Err(message) = result {
        eprintln!("error: {message}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------- serve

fn serve(config_path: &Path, dev: bool) -> Result<(), String> {
    if dev && !cfg!(debug_assertions) {
        return Err(
            "--dev 仅 debug 构建可用；release 生产二进制始终使用真实 Ed25519Hook".to_string(),
        );
    }
    let text = fs::read_to_string(config_path)
        .map_err(|err| format!("读配置文件 {} 失败: {err}", config_path.display()))?;
    let raw: ServeConfig = toml::from_str(&text)
        .map_err(|err| format!("配置文件 {} 解析失败: {err}", config_path.display()))?;
    let base_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let validated = raw.validate(base_dir)?;

    let db_path = validated.config.db.path.clone();
    let db = Db::open(validated.config.db.clone(), Arc::new(SystemClock))
        .map_err(|err| format!("打开数据库 {} 失败: {err}", db_path.display()))?;

    let vault_row = db
        .read(vault::load_opt)
        .map_err(|err| format!("读 vault 失败: {err}"))?;
    let mut config = validated.config;
    if vault_row.is_none() {
        let stored_hash = db
            .read(vault::setup_secret_hash)
            .map_err(|err| format!("读 setup secret 哈希失败: {err}"))?;
        let Some(hash) = stored_hash else {
            return Err(format!(
                "数据库 {} 尚未初始化：请先执行 `menghunote-relay init --db {}` 生成 setup secret",
                db_path.display(),
                db_path.display()
            ));
        };
        config.setup_secret_hash = Some(hash);
    }
    if dev {
        eprintln!("[relay] --dev 已启用（debug 构建）：仅多打提示，验签仍是真实 Ed25519Hook");
    }

    // 生产强制真实验签：这里只构造 `Ed25519Hook`，没有任何放行/注入开关。
    let state = AppState::new(db.clone(), Arc::new(config), Arc::new(Ed25519Hook));
    println!(
        "[relay] dev={} listen={} db={}",
        cfg!(debug_assertions),
        validated.listen,
        db_path.display()
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("构建 tokio runtime 失败: {err}"))?;
    let served = runtime.block_on(serve_http(state, validated.listen));
    drop(db);
    served?;
    println!("[relay] stopped");
    Ok(())
}

async fn serve_http(state: Arc<AppState>, listen: SocketAddr) -> Result<(), String> {
    let listener = TcpListener::bind(listen)
        .await
        .map_err(|err| format!("监听 {listen} 失败: {err}"))?;
    let addr = listener
        .local_addr()
        .map_err(|err| format!("读取监听地址失败: {err}"))?;
    println!("[relay] listening on http://{addr}（Ctrl+C / SIGTERM 优雅退出）");
    let app = router(state).layer(middleware::from_fn(log_requests));
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|err| format!("server error: {err}"))
}

/// 只打 method/path/status/耗时：绝不打印 body/query/header（可能含密文/秘密/令牌）。
async fn log_requests(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    println!(
        "[req] {method} {path} -> {} ({} ms)",
        response.status().as_u16(),
        started.elapsed().as_millis()
    );
    response
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => ctrl_c.await,
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await;
    }
}

// ---------------------------------------------------------------- init

fn init(db_path: &Path) -> Result<(), String> {
    let db = Db::open(DbConfig::new(db_path), Arc::new(SystemClock))
        .map_err(|err| format!("打开数据库 {} 失败: {err}", db_path.display()))?;
    if db
        .read(vault::load_opt)
        .map_err(|err| format!("读 vault 失败: {err}"))?
        .is_some()
    {
        return Err(format!(
            "数据库 {} 已有 vault；如需重建请先 `delete-vault --db {} --yes`",
            db_path.display(),
            db_path.display()
        ));
    }
    if db
        .read(vault::setup_secret_hash)
        .map_err(|err| format!("读 setup secret 哈希失败: {err}"))?
        .is_some()
    {
        return Err(format!(
            "数据库 {} 已经 init 过（setup secret 已生成且只打印一次）；如需重置请先 delete-vault",
            db_path.display()
        ));
    }

    let secret_bytes = random_bytes::<SETUP_SECRET_BYTES>()
        .map_err(|err| format!("CSPRNG 不可用，无法生成 setup secret: {err}"))?;
    let secret = b64url_encode(&secret_bytes);
    let hash = sha256(secret.as_bytes());
    db.write_blocking(move |conn| vault::store_setup_secret_hash(conn, &hash))
        .map_err(|err| format!("写 setup secret 哈希失败: {err}"))?;
    let instance = db
        .read(schema::instance_id)
        .map_err(|err| format!("读 instance_id 失败: {err}"))?;

    println!("db={}", db_path.display());
    println!("instance_id={}", hex_lower(&instance));
    println!("setup_secret={secret}");
    println!("setup_secret_sha256={}", hex_lower(&hash));
    println!("# 下一步：serve --config <toml> 后，用 setup_secret 调 POST /v1/vaults 完成 genesis");
    println!("# genesis 响应键 5 是初始游标（seq=0），客户端应写入 sync.relay_cursor");
    Ok(())
}

// ---------------------------------------------------------------- status

fn status(db_path: &Path) -> Result<(), String> {
    if !db_path.exists() {
        return Err(format!("数据库不存在: {}", db_path.display()));
    }
    let db = Db::open(DbConfig::new(db_path), Arc::new(SystemClock))
        .map_err(|err| format!("打开数据库 {} 失败: {err}", db_path.display()))?;
    println!("db={}", db_path.display());
    let version = db
        .read(|conn| schema::meta_text(conn, schema::META_SCHEMA_VERSION))
        .map_err(|err| format!("读 schema_version 失败: {err}"))?
        .unwrap_or_else(|| "unknown".to_string());
    println!("schema_version={version}");
    let instance = db
        .read(schema::instance_id)
        .map_err(|err| format!("读 instance_id 失败: {err}"))?;
    println!("instance_id={}", hex_lower(&instance));

    let Some(vault) = db
        .read(vault::load_opt)
        .map_err(|err| format!("读 vault 失败: {err}"))?
    else {
        println!("state=uninitialized");
        let devices = db
            .read(count_devices)
            .map_err(|err| format!("统计设备失败: {err}"))?;
        println!("devices={devices}");
        return Ok(());
    };

    println!("state={}", vault.state.as_str());
    println!("vault_id={}", hex_lower(&vault.vault_id));
    println!("epoch={}", vault.active_epoch);
    println!("control_rev={}", vault.control_rev);
    println!("head_seq={}", vault.head_seq);
    println!("gc_floor={}", vault.gc_floor);

    let checkpoints = db
        .read(|conn| checkpoints::published_newest_first(conn, 2))
        .map_err(|err| format!("读 checkpoint 失败: {err}"))?;
    for (label, checkpoint) in [
        ("primary", checkpoints.first()),
        ("fallback", checkpoints.get(1)),
    ] {
        match checkpoint {
            Some(checkpoint) => {
                println!(
                    "{label}_checkpoint={}",
                    hex_lower(&checkpoint.checkpoint_id)
                );
                println!("{label}_checkpoint_epoch={}", checkpoint.epoch);
                println!("{label}_checkpoint_covered_seq={}", checkpoint.covered_seq);
            }
            None => println!("{label}_checkpoint=none"),
        }
    }

    let now = db.clock_handle().now_secs();
    let usage = db
        .read(quota::usage)
        .map_err(|err| format!("读配额用量失败: {err}"))?;
    let available = db
        .read(move |conn| quota::maintenance_available(conn, now))
        .map_err(|err| format!("读维护预算失败: {err}"))?;
    println!("normal_bytes={}", usage.normal_bytes);
    println!("maintenance_bytes={}", usage.maintenance_bytes);
    println!("normal_limit={}", vault.normal_limit);
    println!("maintenance_limit={}", vault.maintenance_limit);
    println!("maintenance_available={available}");

    let devices = db
        .read(count_devices)
        .map_err(|err| format!("统计设备失败: {err}"))?;
    println!("devices={devices}");
    let statuses = db
        .read(|conn| {
            let mut stmt = conn.prepare("SELECT status, COUNT(*) FROM devices GROUP BY status")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            let mut counts = Vec::new();
            for row in rows {
                counts.push(row?);
            }
            Ok(counts)
        })
        .map_err(|err| format!("统计设备状态失败: {err}"))?;
    for (status, count) in statuses {
        println!("devices_{status}={count}");
    }
    Ok(())
}

fn count_devices(conn: &rusqlite::Connection) -> relay::Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM devices", [], |row| row.get(0))?)
}

// ---------------------------------------------------------------- delete-vault

fn delete_vault(db_path: &Path, yes: bool) -> Result<(), String> {
    if !yes {
        return Err(
            "delete-vault 必须显式加 --yes（relay 不提供 HTTP 删除接口；需人工确认）".to_string(),
        );
    }
    if !db_path.exists() {
        return Err(format!("数据库不存在: {}", db_path.display()));
    }
    println!(
        "将永久删除 relay 数据库（含 -wal/-shm）：{}",
        db_path.display()
    );
    print!("输入 DELETE 确认（其它输入取消）: ");
    io::stdout()
        .flush()
        .map_err(|err| format!("写 stdout 失败: {err}"))?;
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|err| format!("读 stdin 失败: {err}"))?;
    if line.trim() != "DELETE" {
        return Err("确认失败，已取消（未删除任何文件）".to_string());
    }

    for suffix in ["", "-wal", "-shm"] {
        let target = PathBuf::from(format!("{}{suffix}", db_path.display()));
        match fs::remove_file(&target) {
            Ok(()) => println!("deleted={}", target.display()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("删除 {} 失败: {err}", target.display())),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> String {
        std::env::temp_dir()
            .join("relay-cli-test.sqlite3")
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn parse(text: &str) -> Result<ValidatedServe, String> {
        let config: ServeConfig = toml::from_str(text).map_err(|err| err.to_string())?;
        config.validate(Path::new("."))
    }

    fn minimal() -> String {
        format!("listen = \"127.0.0.1:18080\"\ndb = \"{}\"\n", temp_db())
    }

    #[test]
    fn minimal_config_uses_default_limits() {
        let parsed = parse(&minimal()).expect("valid config");
        assert_eq!(parsed.listen.to_string(), "127.0.0.1:18080");
        assert_eq!(parsed.config.db.normal_limit, 4 * 1024 * 1024 * 1024);
        assert_eq!(parsed.config.db.maintenance_limit, 8 * 1024 * 1024 * 1024);
        assert!(parsed.config.cursor_key.is_none());
        assert_eq!(
            parsed.config.max_snapshot_bytes,
            relay::proto::limits::MAX_SNAPSHOT_CDE_BYTES as u64
        );
    }

    #[test]
    fn relative_db_is_rejected_with_actionable_message() {
        let error = parse("listen = \"127.0.0.1:18080\"\ndb = \"relay.sqlite3\"\n")
            .expect_err("relative db");
        assert!(error.contains("绝对路径"), "{error}");
    }

    #[test]
    fn bad_listen_is_rejected() {
        let error = parse(&format!(
            "listen = \"not-an-addr\"\ndb = \"{}\"\n",
            temp_db()
        ))
        .expect_err("bad listen");
        assert!(error.contains("listen"), "{error}");
    }

    #[test]
    fn missing_required_fields_are_rejected() {
        let error = parse("listen = \"127.0.0.1:18080\"\n").expect_err("missing db");
        assert!(
            error.contains("db") || error.contains("missing field"),
            "{error}"
        );
        let error = parse(&format!("db = \"{}\"\n", temp_db())).expect_err("missing listen");
        assert!(
            error.contains("listen") || error.contains("missing field"),
            "{error}"
        );
    }

    #[test]
    fn unknown_config_field_is_rejected() {
        let text = format!("{}\npermissive_hook = true\n", minimal());
        let error = parse(&text).expect_err("unknown field");
        assert!(
            error.contains("permissive_hook") || error.contains("unknown field"),
            "{error}"
        );
    }

    #[test]
    fn maintenance_must_exceed_normal() {
        let text = format!(
            "listen = \"127.0.0.1:18080\"\ndb = \"{}\"\nnormal_limit = 100\nmaintenance_limit = 100\n",
            temp_db()
        );
        let error = parse(&text).expect_err("limits");
        assert!(error.contains("maintenance_limit"), "{error}");
    }

    #[test]
    fn cursor_key_must_be_64_hex_chars() {
        let good = "0123456789abcdef".repeat(4);
        let text = format!("{}\ncursor_key = \"{good}\"\n", minimal());
        let parsed = parse(&text).expect("valid cursor key");
        assert!(parsed.config.cursor_key.is_some());

        let text = format!("{}\ncursor_key = \"abcd\"\n", minimal());
        let error = parse(&text).expect_err("short cursor key");
        assert!(error.contains("cursor_key"), "{error}");
    }

    #[test]
    fn cursor_key_and_path_are_exclusive() {
        let good = "0123456789abcdef".repeat(4);
        let text = format!(
            "{}\ncursor_key = \"{good}\"\ncursor_key_path = \"/tmp/key.hex\"\n",
            minimal()
        );
        let error = parse(&text).expect_err("both cursor sources");
        assert!(error.contains("只能给一个"), "{error}");
    }

    #[test]
    fn cursor_key_path_resolves_against_config_dir() {
        // 本机 cargo test 子进程不能写 %TEMP%（AGENTS 记录的坑），统一放 workspace target/tmp。
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("tmp")
            .join(format!("relay-cli-unit-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        let key = "0123456789abcdef".repeat(4);
        fs::write(dir.join("cursor.key"), &key).expect("write key");
        let text = format!(
            "listen = \"127.0.0.1:18080\"\ndb = \"{}\"\ncursor_key_path = \"cursor.key\"\n",
            temp_db()
        );
        let config: ServeConfig = toml::from_str(&text).expect("toml");
        let parsed = config.validate(&dir).expect("resolved cursor key");
        assert!(parsed.config.cursor_key.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn max_snapshot_bytes_must_be_positive_and_bounded() {
        let text = format!("{}\nmax_snapshot_bytes = 0\n", minimal());
        let error = parse(&text).expect_err("zero");
        assert!(error.contains("max_snapshot_bytes"), "{error}");
        let text = format!(
            "{}\nmax_snapshot_bytes = {}\n",
            minimal(),
            relay::proto::limits::MAX_LORO_SNAPSHOT_BYTES as u64 + 1
        );
        let error = parse(&text).expect_err("too large");
        assert!(error.contains("max_snapshot_bytes"), "{error}");
    }

    #[test]
    fn cli_requires_subcommand_arguments() {
        let raw: Vec<String> = vec!["serve".into()];
        assert!(parse_args(&raw).unwrap_err().contains("--config"));
        let raw: Vec<String> = vec!["status".into()];
        assert!(parse_args(&raw).unwrap_err().contains("--db"));
        let raw: Vec<String> = vec!["nonsense".into()];
        assert!(parse_args(&raw).unwrap_err().contains("未知子命令"));
    }

    #[test]
    fn help_and_dev_flags_parse() {
        let raw: Vec<String> = vec!["--help".into()];
        assert!(matches!(parse_args(&raw), Ok(Command::Help)));
        let raw: Vec<String> = vec![
            "serve".into(),
            "--config".into(),
            "relay.toml".into(),
            "--dev".into(),
        ];
        match parse_args(&raw).expect("serve args") {
            Command::Serve { dev, .. } => assert!(dev),
            other => panic!("unexpected {other:?}"),
        }
    }
}
