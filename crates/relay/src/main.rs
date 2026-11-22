//! `menghunote-relay` 生产二进制（ADR 0006 §7）。
//!
//! 子命令：
//! - `serve --config <toml>`：启动 HTTP 中继；**强制真实 Ed25519 验签**（没有任何放行 hook
//!   开关），`--dev` 仅在 debug 构建里被接受（只多打一行提示，验签不变）。
//!   同时启动**独立的内嵌 Web 管理 listener**（ADR 0018 S1；默认 `127.0.0.1:18081`，
//!   配置 `admin_listen` 可改 LAN、写 `"off"` 关闭）；两个 listener 绑定成功后才开始服务。
//! - `init --db <path>`：建库 + schema 迁移 + 生成一次性 setup secret 与独立 admin token
//!   （各只打印一次，只落哈希，不启网络）。
//! - `status --db <path>`：**只读**诊断（existing-only、不迁移、不建库）：打印 vault
//!   水位/checkpoint/配额/设备数（`key=value`，机器可读）；schema 版本不匹配时明确报错。
//!
//! 本二进制**不提供**删库/重置子命令（ADR 0018 §4）：实例重置只能手工停服 + 备份 +
//! 删除库文件，或在 Web 管理面按 vault 逐个删除（在线、fenced、不可逆）。
//!
//! 配置（`serve --config`）是扁平 TOML：
//! ```toml
//! listen = "127.0.0.1:18080"                  # 默认只允许 loopback；对外入口走 TLS 反向代理
//! db = "/var/lib/menghunote/relay.sqlite3"   # 必须绝对路径
//! allow_plaintext = false                     # 非 loopback 明文监听必须显式置 true（有风险）
//! admin_listen = "127.0.0.1:18081"            # 可选；默认 loopback:18081，写 "off" 关闭
//! admin_allowed_hosts = ["relay.example.com"] # 可选；admin Host/Origin 精确白名单
//! admin_token_sha256 = "…64 hex…"             # 可选；库内 init 哈希优先
//! public_url = "http://127.0.0.1:18080"       # 可选；claim 领取链接的数据面 canonical origin
//! normal_limit = 4294967296                   # Q（默认 4GiB）
//! maintenance_limit = 8589934592              # 峰值 Q+R（默认 2Q）
//! cursor_key = "…64 hex…"                     # 可选；与 cursor_key_path 二选一
//! cursor_key_path = "cursor.key"              # 可选；相对路径按配置文件目录解析
//! max_snapshot_bytes = 67108864               # 可选；默认 64MiB
//! ```
//!
//! **明文门禁**：`listen` 非 loopback 且未显式 `allow_plaintext = true` 时拒绝启动并打印
//! 反向代理/TLS 部署示例（见 [`relay::config::ensure_plaintext_listen_allowed`]）。
//! admin listener **不强制 TLS**（ADR 0018 §1）：默认绑定 loopback；改 LAN 时会打印
//! "token/会话可被截获"的警告，生产应放在 TLS 反向代理之后。
//!
//! 请求日志只打 `method path -> status`，**不含** body/query/header/cookie/token。

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;
use serde::Deserialize;
use tokio::net::TcpListener;

use menghunote_relay as relay;
use relay::admin_web::{self, AdminHostPolicy, AdminWeb};
use relay::api::{router, AppState};
use relay::checkpoints;
use relay::config::RelayConfig;
use relay::db::{schema, Db, DbConfig};
use relay::deletion::Deletion;
use relay::ids::{b64url_encode, hex_lower, random_bytes, sha256, Hash32};
use relay::quota;
use relay::scope::VaultScope;
use relay::time::{Clock as _, SystemClock};
use relay::vault;
use relay::verify::Ed25519Hook;

/// setup secret 的长度（32B CSPRNG → base64url 43 字符）。
const SETUP_SECRET_BYTES: usize = 32;

fn usage() -> &'static str {
    "menghunote-relay —— 自建加密中继（ADR 0006 §7）

用法:
  menghunote-relay serve --config <toml> [--dev]
      启动 HTTP 中继；强制真实 Ed25519 验签。--dev 仅 debug 构建可用（不改验签）。
      同时启动内嵌 Web 管理 listener（默认 127.0.0.1:18081；admin_listen 可改/关闭）。
  menghunote-relay init --db <path>
      建库 + 迁移 + 生成 setup secret 与独立 admin token（各打印一次；不启网络）。
      <path> 的相对路径按当前工作目录解析；建议在 serve.toml 同目录执行，或直接写绝对路径。
  menghunote-relay status --db <path>
      只读诊断（不迁移、不建库）：schema 版本必须匹配；打印 vault state/epoch/
      control_rev/head_seq/gc_floor/两份 checkpoint/配额/设备数。
  menghunote-relay --help

重置说明（ADR 0018 §4）:
  没有删库 CLI。实例重置只能：停掉 relay → 备份库文件 → 手工删除 <db>（含 -wal/-shm），
  或在 Web 管理面按 vault 执行不可逆删除（在线、fenced；其它租户继续服务）。
  旧版本 `delete-vault` / `--issue-ticket` 入口已移除。

配置字段（serve --config，扁平 TOML）:
  listen（必填）、db（必填；相对路径按 serve.toml 所在目录解析，建议与配置同目录）、allow_plaintext（默认 false）、
  admin_listen（默认 \"127.0.0.1:18081\"；\"off\" 关闭）、admin_allowed_hosts（可选，精确 Host 白名单）、
  admin_token_sha256（可选，64 hex；库内 init 生成的哈希优先）、
  public_url（可选，claim 领取链接的 canonical 数据面 origin；如 http://127.0.0.1:18080
    或 https://relay.example.com；未配置时创建领取链接会报错，绝不从 Host/listen 推导）、
  normal_limit、maintenance_limit、cursor_key 或 cursor_key_path、max_snapshot_bytes

TLS 部署（relay 自身只讲明文 HTTP；默认只允许 loopback）:
  默认 listen = \"127.0.0.1:18080\" 或 \"[::1]:18080\"；对外入口由同机反向代理终止 TLS，例如 nginx：
    server {
      listen 443 ssl;
      ssl_certificate     /etc/menghunote/fullchain.pem;
      ssl_certificate_key /etc/menghunote/privkey.pem;
      location / { proxy_pass http://127.0.0.1:18080; proxy_http_version 1.1; }
    }
  客户端配置 https://your.domain（证书必须匹配 hostname 且未过期）。
  非 loopback（0.0.0.0 / 内网 IP）明文监听会被拒绝；确认在受信任私网/隧道内才可显式
  allow_plaintext = true（bearer token 与元数据将明文传输）。

Web 管理面（ADR 0018 S1）:
  默认 admin_listen = \"127.0.0.1:18081\"，浏览器打开 http://127.0.0.1:18081/admin/login，
  粘贴 init 只打印一次的 admin token。改 LAN 监听（如 0.0.0.0:18081）时会打印警告：
  **明文下 token/会话 Cookie 可被截获，不要把 admin 端口映射到公网**；建议同机 TLS
  反向代理，并把浏览器使用的 host:port 写进 admin_allowed_hosts（Host/Origin 精确白名单）。
  写 admin_listen = \"off\" 可只运行数据面（明确关闭，不会静默降级）。

Web 管理页面（ADR 0018 S4a）:
  登录后 /admin 是 vault 列表（状态/数据面 URL/设备数/票据过期/删除进度）；可创建
  一次性领取链接（只在创建后显示一次）、取消预留、按精确 DELETE <id> 确认不可逆删除；
  DELETING 页面用 meta refresh 显示进度。claim 链接 origin 只来自 public_url。
"
}

/// `serve` 的配置文件（严格拒绝未知字段，避免把 `permissive_hook = true` 之类误当开关）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServeConfig {
    listen: String,
    db: PathBuf,
    /// 非 loopback 地址上的明文监听必须显式允许（默认 false：只允许 loopback 明文）。
    #[serde(default)]
    allow_plaintext: bool,
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
    /// 内嵌 Web 管理 listener（ADR 0018 S1）。缺省 = `127.0.0.1:18081`；
    /// `"off"`/`"none"`/`"disabled"` = 显式关闭（只跑数据面）。
    #[serde(default)]
    admin_listen: Option<String>,
    /// admin Host/Origin 精确白名单的额外条目（`host:port`；LAN/域名访问时必填）。
    #[serde(default)]
    admin_allowed_hosts: Vec<String>,
    /// 部署注入的 `SHA256(admin token 文本)`（64 hex）；库内 `relay_meta` 优先。
    #[serde(default)]
    admin_token_sha256: Option<String>,
    /// claim 领取链接使用的数据面 canonical origin（ADR 0018 S4a）。
    ///
    /// **只**用于生成 `mn-pair://create` 的 `relay=`；绝不从 `Host`/`listen`/请求头推导。
    /// 未配置时管理页仍可浏览/删除，但创建领取链接会明确报错（不生成不可用链接）。
    /// 启动校验：https 任意合法主机，或 http + loopback 白名单（localhost/127.0.0.1/[::1]）。
    #[serde(default)]
    public_url: Option<String>,
}

/// 校验后的启动参数。
#[derive(Debug)]
struct ValidatedServe {
    listen: SocketAddr,
    allow_plaintext: bool,
    admin_listen: Option<SocketAddr>,
    admin_allowed_hosts: Vec<String>,
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
        // 明文门禁先于任何 IO：非 loopback 且未显式允许时直接拒绝启动。
        relay::config::ensure_plaintext_listen_allowed(listen, self.allow_plaintext)?;
        if self.db.as_os_str().is_empty() {
            return Err("db 不能为空".to_string());
        }
        // 相对路径按**配置文件目录**解析（与 cursor_key_path 一致），避免随 cwd 漂移；
        // `serve` 会保证 base_dir 是绝对路径。`init` 没有配置文件，按 cwd 解析。
        let db_path = if self.db.is_relative() {
            base_dir.join(&self.db)
        } else {
            self.db.clone()
        };

        let defaults = DbConfig::new(db_path.clone());
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

        // ADR 0018 S1：admin listener 缺省 = 默认 loopback 地址；只有显式关闭才是 None。
        let admin_listen_text = match self.admin_listen.as_deref().map(str::trim) {
            None => Some(relay::config::DEFAULT_ADMIN_LISTEN.to_string()),
            Some(text)
                if text.is_empty()
                    || matches!(
                        text.to_ascii_lowercase().as_str(),
                        "off" | "none" | "disabled"
                    ) =>
            {
                None
            }
            Some(text) => Some(text.to_string()),
        };
        let admin_listen = match admin_listen_text.as_deref() {
            None => None,
            Some(text) => Some(text.parse::<SocketAddr>().map_err(|_| {
                format!(
                    "admin_listen 不是合法地址: {text:?}（示例 127.0.0.1:18081；关闭写 admin_listen = \"off\"）"
                )
            })?),
        };
        // 端口 0 由内核分配，两个 0 不构成冲突；其余情况地址必须不同。
        if let Some(admin) = admin_listen {
            if listen.port() != 0 && admin == listen {
                return Err(format!(
                    "admin_listen({admin}) 不能与数据面 listen({listen}) 相同：两个 listener 必须分开绑定"
                ));
            }
        }
        let mut admin_allowed_hosts = Vec::with_capacity(self.admin_allowed_hosts.len());
        for host in self.admin_allowed_hosts {
            let host = host.trim().to_ascii_lowercase();
            if host.is_empty() {
                return Err("admin_allowed_hosts 不能包含空条目".to_string());
            }
            if host.contains("://") || host.contains('/') || host.contains(char::is_whitespace) {
                return Err(format!(
                    "admin_allowed_hosts 条目必须是 host:port（不含 scheme/路径/空白）: {host:?}"
                ));
            }
            admin_allowed_hosts.push(host);
        }
        let admin_token_hash = self
            .admin_token_sha256
            .as_deref()
            .map(|text| parse_hex32(text, "admin_token_sha256"))
            .transpose()?;

        // ADR 0018 S4a：claim 链接的 canonical origin 只来自显式配置；未配置时创建页会报错。
        let public_url = self
            .public_url
            .as_deref()
            .map(relay::config::PublicUrl::parse)
            .transpose()?;

        let mut config = RelayConfig::new(db_path)
            .with_limits(normal_limit, maintenance_limit)
            .with_max_snapshot_bytes(max_snapshot_bytes)
            .with_admin_listen(admin_listen_text)
            .with_admin_allowed_hosts(admin_allowed_hosts.clone());
        if let Some(key) = cursor_key {
            config = config.with_cursor_key(key);
        }
        if let Some(hash) = admin_token_hash {
            config = config.with_admin_token_hash(hash);
        }
        if let Some(public_url) = public_url {
            config = config.with_public_url(public_url);
        }
        Ok(ValidatedServe {
            listen,
            allow_plaintext: self.allow_plaintext,
            admin_listen,
            admin_allowed_hosts,
            config,
        })
    }
}

/// 解析 64 位十六进制（32B）的字段值（游标 HMAC key / admin token 哈希）。
fn parse_hex32(text: &str, field: &str) -> Result<Hash32, String> {
    let trimmed = text.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() != 64 {
        return Err(format!(
            "{field} 必须是 64 个十六进制字符（32B；收到 {} 个字符）",
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

/// 兼容旧字段名：游标 HMAC key 的 64 位十六进制解析。
fn parse_cursor_key(text: &str, field: &str) -> Result<Hash32, String> {
    parse_hex32(text, field)
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
    // 先把配置文件路径规范化成绝对路径，后续相对 db/cursor_key_path 都以它的目录为基准。
    let config_path = fs::canonicalize(config_path)
        .map_err(|err| format!("配置文件 {} 不存在或不可访问: {err}", config_path.display()))?;
    let text = fs::read_to_string(&config_path)
        .map_err(|err| format!("读配置文件 {} 失败: {err}", config_path.display()))?;
    let raw: ServeConfig = toml::from_str(&text)
        .map_err(|err| format!("配置文件 {} 解析失败: {err}", config_path.display()))?;
    let base_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let validated = raw.validate(base_dir)?;
    if validated.allow_plaintext && !validated.listen.ip().is_loopback() {
        eprintln!(
            "[relay] 警告：allow_plaintext=true 且监听非 loopback 地址 {}：明文入口只应存在于受信任私网/本机反向代理之后",
            validated.listen
        );
    }

    let db_path = validated.config.db.path.clone();
    let db = Db::open(validated.config.db.clone(), Arc::new(SystemClock))
        .map_err(|err| format!("打开数据库 {} 失败: {err}", db_path.display()))?;

    let vault_ids = db
        .read_instance(schema::list_vault_ids)
        .map_err(|err| format!("读 vault 列表失败: {err}"))?;
    let mut config = validated.config;
    if vault_ids.is_empty() {
        let stored_hash = db
            .read_instance(vault::setup_secret_hash)
            .map_err(|err| format!("读 setup secret 哈希失败: {err}"))?;
        let Some(hash) = stored_hash else {
            return Err(format!(
                "数据库 {} 尚未初始化：请先执行 `menghunote-relay init --db {}` 生成 setup secret 与 admin token",
                db_path.display(),
                db_path.display()
            ));
        };
        config.setup_secret_hash = Some(hash);
    }
    if dev {
        eprintln!("[relay] --dev 已启用（debug 构建）：仅多打提示，验签仍是真实 Ed25519Hook");
    }

    // ADR 0018 S1：admin 凭证只从 `relay_meta` 读；配置哈希仅作兜底，绝不覆盖库内值。
    let resolved = admin_web::resolve_token_hash(&db, config.admin_token_hash)
        .map_err(|err| format!("读 admin token 哈希失败: {err}"))?;
    if resolved.config_conflict {
        eprintln!(
            "[relay] 警告：配置 admin_token_sha256 与库内哈希不一致；以库内（init 生成）为准"
        );
    }
    let admin_token_hash = resolved.hash;
    if validated.admin_listen.is_some() && admin_token_hash.is_none() {
        eprintln!(
            "[relay] admin token 未初始化：admin listener 照常启动，但登录会被拒绝；\
             请执行 `menghunote-relay init --db {}`（relay 不会自动生成/打印）",
            db_path.display()
        );
    }

    // ADR 0018 S4a：claim 链接 origin 只来自配置。未配置不是启动错误（管理页仍可浏览/删除），
    // 但创建领取链接必须明确报错，绝不退回 Host/listen 推导。
    let public_url = config.public_url.clone();
    if validated.admin_listen.is_some() {
        match &public_url {
            Some(url) => println!(
                "[relay] public_url={url}（claim 领取链接的 canonical origin；只来自配置）"
            ),
            None => eprintln!(
                "[relay] 警告：未配置 public_url：管理页可浏览/删除，但创建领取链接会报错\
                 （relay 不会从 Host/listen 推导链接 origin）"
            ),
        }
    }

    // 生产强制真实验签：这里只构造 `Ed25519Hook`，没有任何放行/注入开关。
    // ADR 0018 S3：删除作业运行时必须与 admin listener **共享同一个实例**（同一份 fence）。
    let deletion = Arc::new(Deletion::new(db.clone()));
    let state = AppState::with_deletion(
        db.clone(),
        Arc::new(config),
        Arc::new(Ed25519Hook),
        deletion,
    );
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
    let plan = AdminServePlan {
        listen: validated.admin_listen,
        token_hash: admin_token_hash,
        allowed_hosts: validated.admin_allowed_hosts,
        public_url,
    };
    let served = runtime.block_on(serve_all(state, validated.listen, plan));
    drop(db);
    served?;
    println!("[relay] stopped");
    Ok(())
}

/// admin listener 的启动计划（只在内嵌 Web 管理面启用时存在）。
struct AdminServePlan {
    listen: Option<SocketAddr>,
    token_hash: Option<Hash32>,
    allowed_hosts: Vec<String>,
    /// claim 链接的 canonical origin（只来自 `ServeConfig::public_url`；`None` = 未配置）。
    public_url: Option<relay::config::PublicUrl>,
}

/// **先绑定两个 listener，再开始服务**（ADR 0018 §1 / 验收矩阵"持久标记前"之外的另一条：
/// 端口冲突必须明确失败，不能静默只启动数据面）。
async fn serve_all(
    state: Arc<AppState>,
    data_listen: SocketAddr,
    admin: AdminServePlan,
) -> Result<(), String> {
    // ADR 0018 S3：**绑定 listener 之前**加载 admission fence 并恢复幂等删除作业；
    // 状态矛盾/缺失直接中止启动（fail closed，不能对外服务一个"说删了但其实还在"的实例）。
    let recovery = state.deletion().recover_startup().await.map_err(|err| {
        format!("加载删除 fence/恢复删除作业失败（启动中止，fail closed）：{err}")
    })?;
    if !recovery.is_empty() {
        println!(
            "[relay] 删除 fence：{} 个 vault 已关闭（恢复 {} 个 DELETING 作业、{} 个 DELETE_FAILED 等显式重试、{} 个 DELETED 墓碑）",
            recovery.fenced.len(),
            recovery.resumed.len(),
            recovery.failed,
            recovery.deleted
        );
    }
    let data_listener = TcpListener::bind(data_listen).await.map_err(|err| {
        format!("监听数据面 {data_listen} 失败: {err}；启动中止（不会只启动其中一个 listener）")
    })?;
    let data_addr = data_listener
        .local_addr()
        .map_err(|err| format!("读取数据面监听地址失败: {err}"))?;

    let admin_listener = match admin.listen {
        None => None,
        Some(listen) => Some(TcpListener::bind(listen).await.map_err(|err| {
            format!(
                "监听 admin listener {listen} 失败: {err}；启动中止（数据面 listener 已关闭，\
                 不会静默只启动数据面）。请改 admin_listen 或停掉占用该端口的进程。"
            )
        })?),
    };

    let admin_web = match (&admin_listener, admin.listen) {
        (Some(listener), Some(_)) => {
            let addr = listener
                .local_addr()
                .map_err(|err| format!("读取 admin 监听地址失败: {err}"))?;
            let policy = AdminHostPolicy::for_listen(addr, &admin.allowed_hosts);
            Some((
                addr,
                AdminWeb::new(
                    state.db.clone(),
                    state.db.clock_handle(),
                    admin.token_hash,
                    admin.public_url,
                    policy,
                    state.deletion().clone(),
                ),
            ))
        }
        _ => None,
    };

    // 绑定全部成功后才打印横幅（运维/测试据此判定"服务已就绪"）。
    println!("[relay] listening on http://{data_addr}（Ctrl+C / SIGTERM 优雅退出）");
    match (&admin_listener, &admin_web) {
        (Some(_), Some((addr, web))) => {
            println!(
                "[relay] admin listening on http://{addr}（token 登录；明文下 token/会话可被截获，勿直接暴露公网）"
            );
            if !addr.ip().is_loopback() {
                eprintln!(
                    "[relay] 警告：admin listener 监听非 loopback 地址 {addr}：HTTP 明文下 admin token 与会话 Cookie 可被截获；\
                     请确认在受信任私网/隧道内，或放在 TLS 反向代理之后，并把浏览器使用的 host:port 写进 admin_allowed_hosts"
                );
            }
            if !web.has_token() {
                eprintln!(
                    "[relay] 警告：admin token 尚未初始化，登录会被拒绝；请先 `menghunote-relay init`（不会自动生成）"
                );
            }
        }
        _ => println!("[relay] admin listener disabled（admin_listen = \"off\"）；仅数据面运行"),
    }

    let data_app = router(state).layer(middleware::from_fn(log_requests));
    let Some((_, web)) = admin_web else {
        return serve_data(data_listener, data_app).await;
    };
    let admin_app = admin_web::router(web).layer(middleware::from_fn(log_requests));
    let admin_listener = admin_listener.expect("admin_web 只会在 listener 就绪时构造");
    tokio::try_join!(
        serve_data(data_listener, data_app),
        serve_admin(admin_listener, admin_app)
    )
    .map(|_| ())
}

async fn serve_data(listener: TcpListener, app: Router) -> Result<(), String> {
    // 数据面 handler 不读对端地址（admission 只看路径/权威 registry），无需 ConnectInfo。
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|err| format!("data server error: {err}"))
}

async fn serve_admin(listener: TcpListener, app: Router) -> Result<(), String> {
    // admin 登录失败限流按对端 IP 分桶，同样需要连接信息（不读可伪造的 Host/转发头）。
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .map_err(|err| format!("admin server error: {err}"))
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
    if !db
        .read_instance(schema::list_vault_ids)
        .map_err(|err| format!("读 vault 列表失败: {err}"))?
        .is_empty()
    {
        return Err(format!(
            "数据库 {} 已有 vault；重置只能停服 + 备份 + 手工删库（不再提供 CLI），\
             或在 Web 管理面按 vault 删除",
            db_path.display()
        ));
    }
    if db
        .read_instance(vault::setup_secret_hash)
        .map_err(|err| format!("读 setup secret 哈希失败: {err}"))?
        .is_some()
    {
        return Err(format!(
            "数据库 {} 已经 init 过（setup secret 已生成且只打印一次）；\
             如需重置只能停服 + 备份 + 手工删库，或在 Web 管理面删除该 vault",
            db_path.display()
        ));
    }
    if db
        .read_instance(admin_web::stored_token_hash)
        .map_err(|err| format!("读 admin token 哈希失败: {err}"))?
        .is_some()
    {
        return Err(format!(
            "数据库 {} 已有 admin token（只打印一次，不会覆盖）；\
             如需重置只能停服 + 备份 + 手工删库",
            db_path.display()
        ));
    }

    // 两枚独立凭证：setup secret（v1 genesis/旧 loopback 管理）与 admin token（Web 管理）。
    // 先全部生成成功，再在**同一个写事务**里落两枚哈希，避免 CSPRNG 失败留下半初始化库。
    let secret_bytes = random_bytes::<SETUP_SECRET_BYTES>()
        .map_err(|err| format!("CSPRNG 不可用，无法生成 setup secret: {err}"))?;
    let secret = b64url_encode(&secret_bytes);
    let hash = sha256(secret.as_bytes());
    let admin_token = admin_web::generate_token()
        .map_err(|err| format!("CSPRNG 不可用，无法生成 admin token: {err}"))?;
    let admin_hash = admin_web::token_hash(&admin_token);
    db.write_instance_blocking(move |conn| {
        vault::store_setup_secret_hash(conn, &hash)?;
        admin_web::store_token_hash(conn, &admin_hash)
    })
    .map_err(|err| format!("写 setup secret / admin token 哈希失败: {err}"))?;
    let instance = db
        .read_instance(schema::instance_id)
        .map_err(|err| format!("读 instance_id 失败: {err}"))?;

    println!("db={}", db_path.display());
    println!("instance_id={}", hex_lower(&instance));
    println!("setup_secret={secret}");
    println!("setup_secret_sha256={}", hex_lower(&hash));
    // 只打印这一次：不写日志、不落盘、不回显到任何 HTTP 响应。
    println!("admin_token={admin_token}");
    println!("admin_token_sha256={}", hex_lower(&admin_hash));
    println!(
        "# admin token 用于 http://127.0.0.1:18081/admin/login（admin_listen 可配置）；\
         只显示这一次，请立即保存到密码管理器。"
    );
    println!(
        "# 明文 HTTP 下 token/会话可被截获：admin 端口只应在本机或受信任私网/TLS 反向代理之后暴露。"
    );
    println!("# 下一步：serve --config <toml> 后，用 setup_secret 调 POST /v1/vaults 完成 genesis");
    println!("# genesis 响应键 5 是初始游标（seq=0），客户端应写入 sync.relay_cursor");
    Ok(())
}

fn status(db_path: &Path) -> Result<(), String> {
    if !db_path.exists() {
        return Err(format!(
            "数据库不存在: {}（status 只读取已有库，不会建库/迁移）",
            db_path.display()
        ));
    }
    // ADR 0018 §4：`status` 是**只读诊断** —— 只读连接 + `query_only`，绝不打开 writer、
    // 绝不 `schema::apply`（迁移），也不创建库文件；schema 不兼容时明确报错。
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|err| format!("以只读方式打开数据库 {} 失败: {err}", db_path.display()))?;
    conn.busy_timeout(std::time::Duration::from_millis(5_000))
        .map_err(|err| format!("设置只读连接 busy_timeout 失败: {err}"))?;
    conn.pragma_update(None, "query_only", "ON")
        .map_err(|err| format!("设置只读连接 query_only 失败: {err}"))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|err| format!("设置只读连接 foreign_keys 失败: {err}"))?;

    let version_text = schema::meta_text(&conn, schema::META_SCHEMA_VERSION).map_err(|err| {
        format!(
            "读数据库 {} 的 schema_version 失败（不是 relay 库或缺少 relay_meta）: {err}",
            db_path.display()
        )
    })?;
    let Some(version_text) = version_text else {
        return Err(format!(
            "数据库 {} 缺少 schema_version 元数据（不是已初始化的 relay 库）；\
             status 只读，不会迁移/建库",
            db_path.display()
        ));
    };
    let version: i64 = version_text.parse().map_err(|_| {
        format!(
            "数据库 {} 的 schema_version={version_text:?} 非法；status 只读，不会迁移/建库",
            db_path.display()
        )
    })?;
    if version != schema::SCHEMA_VERSION {
        return Err(format!(
            "数据库 {} 的 schema_version={version} 与本二进制期望的 {} 不一致；\
             status 是只读诊断，不会自动迁移。请使用匹配版本的 relay（启动 serve/init 才会执行迁移），\
             或先备份再升级",
            db_path.display(),
            schema::SCHEMA_VERSION
        ));
    }

    println!("db={}", db_path.display());
    println!("schema_version={version}");
    let instance =
        schema::instance_id(&conn).map_err(|err| format!("读 instance_id 失败: {err}"))?;
    println!("instance_id={}", hex_lower(&instance));

    let vault_ids =
        schema::list_vault_ids(&conn).map_err(|err| format!("读 vault 列表失败: {err}"))?;
    println!("vaults={}", vault_ids.len());
    if vault_ids.is_empty() {
        println!("state=uninitialized");
        return Ok(());
    }
    let now = SystemClock.now_secs();
    // 多租户：逐 vault 打印（v1 单 vault 库里就是一段，关键字段名保持不变）。
    for vault_id in vault_ids {
        let scope = VaultScope::new(vault_id);
        let scoped = relay::scope::ScopeConn::new(&conn, scope);
        let vault = vault::load(&scoped).map_err(|err| format!("读 vault 失败: {err}"))?;
        println!("state={}", vault.state.as_str());
        println!("vault_id={}", hex_lower(&vault.vault_id));
        println!("epoch={}", vault.active_epoch);
        println!("control_rev={}", vault.control_rev);
        println!("head_seq={}", vault.head_seq);
        println!("gc_floor={}", vault.gc_floor);

        let checkpoints = checkpoints::published_newest_first(&scoped, 2)
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

        let usage = quota::usage(&scoped).map_err(|err| format!("读配额用量失败: {err}"))?;
        let available = quota::maintenance_available(&scoped, now)
            .map_err(|err| format!("读维护预算失败: {err}"))?;
        println!("normal_bytes={}", usage.normal_bytes);
        println!("maintenance_bytes={}", usage.maintenance_bytes);
        println!("normal_limit={}", vault.normal_limit);
        println!("maintenance_limit={}", vault.maintenance_limit);
        println!("maintenance_available={available}");

        let devices = count_devices(&scoped).map_err(|err| format!("统计设备失败: {err}"))?;
        println!("devices={devices}");
        let statuses = scoped
            .prepare(
                "SELECT status, COUNT(*) FROM devices
                 WHERE vault_id = ?1 GROUP BY status",
            )
            .and_then(|mut stmt| {
                let rows = stmt.query_map([scoped.scope().as_slice()], |row| {
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
    }
    Ok(())
}

fn count_devices(conn: &relay::ScopeConn) -> relay::Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM devices WHERE vault_id = ?1",
        [conn.scope().as_slice()],
        |row| row.get(0),
    )?)
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
        // ADR 0018 S1：admin listener 缺省只绑 loopback，且没配置哈希。
        assert_eq!(
            parsed.admin_listen.map(|addr| addr.to_string()),
            Some(relay::config::DEFAULT_ADMIN_LISTEN.to_string())
        );
        assert!(parsed.config.admin_token_hash.is_none());
        assert!(parsed.admin_allowed_hosts.is_empty());
    }

    #[test]
    fn admin_listen_defaults_and_off_switch() {
        let parsed = parse(&minimal()).expect("default admin listen");
        assert_eq!(
            parsed.admin_listen.map(|addr| addr.to_string()),
            Some("127.0.0.1:18081".to_string())
        );

        for off in ["off", "OFF", "none", "disabled", ""] {
            let text = format!("{}\nadmin_listen = \"{off}\"\n", minimal());
            let parsed = parse(&text).expect("off is valid");
            assert!(
                parsed.admin_listen.is_none(),
                "admin_listen = {off:?} 必须显式关闭 admin listener"
            );
        }

        // 改 LAN 地址允许（生产靠文档/TLS 反向代理约束，不硬拒）。
        let text = format!("{}\nadmin_listen = \"0.0.0.0:18081\"\n", minimal());
        let parsed = parse(&text).expect("lan admin listen");
        assert_eq!(
            parsed.admin_listen.map(|addr| addr.to_string()),
            Some("0.0.0.0:18081".to_string())
        );
    }

    #[test]
    fn admin_listen_must_be_valid_and_differ_from_data_listen() {
        let text = format!("{}\nadmin_listen = \"not-an-address\"\n", minimal());
        let error = parse(&text).expect_err("bad admin listen");
        assert!(error.contains("admin_listen"), "{error}");

        let text = format!("{}\nadmin_listen = \"127.0.0.1:18080\"\n", minimal());
        let error = parse(&text).expect_err("same as data listen");
        assert!(error.contains("不能与数据面"), "{error}");

        // 端口 0 由内核分配，两者都为 0 不算冲突。
        let text = format!(
            "listen = \"127.0.0.1:0\"\ndb = \"{}\"\nadmin_listen = \"127.0.0.1:0\"\n",
            temp_db()
        );
        let parsed = parse(&text).expect("both ephemeral");
        assert!(parsed.admin_listen.is_some());
    }

    #[test]
    fn admin_allowed_hosts_rejects_scheme_path_and_blank_entries() {
        let text = format!(
            "{}\nadmin_allowed_hosts = [\"relay.example.com:18081\"]\n",
            minimal()
        );
        let parsed = parse(&text).expect("valid host entry");
        assert_eq!(
            parsed.admin_allowed_hosts,
            vec!["relay.example.com:18081".to_string()]
        );

        for bad in ["http://relay.example.com", "relay.example.com/admin", " "] {
            let text = format!("{}\nadmin_allowed_hosts = [\"{bad}\"]\n", minimal());
            let error = parse(&text).expect_err("invalid host entry");
            assert!(error.contains("admin_allowed_hosts"), "{error}");
        }
    }

    #[test]
    fn admin_token_sha256_must_be_64_hex_chars() {
        let good = "ab".repeat(32);
        let text = format!("{}\nadmin_token_sha256 = \"{good}\"\n", minimal());
        let parsed = parse(&text).expect("valid admin hash");
        assert_eq!(
            parsed.config.admin_token_hash,
            Some([0xabu8; 32]),
            "hex 解析必须落成 32B"
        );

        let text = format!("{}\nadmin_token_sha256 = \"abcd\"\n", minimal());
        let error = parse(&text).expect_err("short admin hash");
        assert!(error.contains("admin_token_sha256"), "{error}");
    }

    #[test]
    fn relative_db_is_resolved_against_config_dir() {
        let base = std::env::temp_dir().join("relay-relative-db");
        let config: ServeConfig =
            toml::from_str("listen = \"127.0.0.1:18080\"\ndb = \"relay.sqlite3\"\n")
                .expect("parse");
        let parsed = config.validate(&base).expect("relative db");
        assert_eq!(parsed.config.db.path, base.join("relay.sqlite3"));
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
