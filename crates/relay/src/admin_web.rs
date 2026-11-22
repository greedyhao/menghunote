//! **内嵌 Web 管理面**（ADR 0018 S1）：独立 listener、独立 admin token、独立会话。
//!
//! 与数据面（[`crate::api`]）的关系：
//! - **同一 relay 进程、同一个 `Db`**，但**独立 router / 独立 listener**（默认
//!   `127.0.0.1:18081`，见 [`crate::config::DEFAULT_ADMIN_LISTEN`]）；
//! - admin 会话 cookie（[`SESSION_COOKIE`]）只在本模块内解析；数据面**从不**读取 cookie，
//!   因此 admin 登录态无法用于任何 `/v1`/`/v2` 租户路由（cookie 名也刻意与数据面无关）；
//! - 不启用任何 CORS（axum 默认没有 CORS 层），不返回 `Access-Control-*`；
//!
//! 安全边界（ADR 0018 §1）：
//! - 凭证：`init` 生成 32B CSPRNG admin token（与 setup secret/租户 bearer/管理 cap 完全独立），
//!   只落 `SHA256(明文)` 到 `relay_meta`，明文只打印一次；登录用**常数时间**比较；
//! - 会话：32B 随机 session id，内存 map（进程重启即失效），HttpOnly/SameSite=Strict/
//!   host-only；闲置 30 分钟、绝对 8 小时过期；logout 立即删除；换 token（[`AdminWeb::set_token_hash`]）
//!   清空全部会话；
//! - CSRF：登录页与登录后的表单都带一次性/会话绑定的 CSRF token，POST 必须匹配；
//! - Host：**精确白名单**（监听地址 + localhost + 配置条目）；POST 若带 `Origin`/`Referer`，
//!   其 authority 必须与 `Host` 完全一致。`Origin: null`/两者都缺失时放行到 handler，
//!   由一次性/会话绑定 CSRF 决定（隐私浏览器/沙箱 WebView 会把来源置为不透明）；
//!   跨站带来源的 POST 仍直接拒绝；
//! - 响应头：`X-Frame-Options: DENY`、CSP、`no-store`、`no-referrer`、`nosniff`；
//!   页面无第三方资产、无 JavaScript、无 CORS；
//! - 登录尝试：实例级固定窗口 + 按 IP 的失败窗口限流；日志只记 method/path/status
//!   （由 `serve` 的请求日志中间件负责），**不记** body/query/header/cookie/token。
//!
//! S4a 起提供**最小管理界面**（ADR 0018 §6）：
//! - `GET /admin`：vault 列表（registry 只读投影；状态/创建时间/数据面 URL/设备数/
//!   票据过期/删除进度，稳定排序）；
//! - `GET /admin/vaults/{id}`：详情（生命周期、授权/运行状态分开、设备数、epoch/control rev、
//!   票据元数据、用量、时间；不显示票据 hash/明文）；
//! - `GET /admin/vaults/new` + `POST /admin/vaults`：创建预留（自动 `vault_id`/900s/quota=1），
//!   成功 303 到**一次性**领取链接页（链接只在首次 GET 显示，刷新即失效）；
//! - `POST /admin/vaults/{id}/cancel`：取消 `RESERVED` 预留（claim 已赢 → 409）；
//! - `GET /admin/vaults/{id}/delete` + `POST /admin/vaults/{id}/delete`：删除确认
//!   （`confirm` 必须精确等于 `DELETE <32hex id>`）、202 作业页、无 JS 的 meta refresh 进度。
//!
//! 创建/取消/删除全部走 S2 registry 与 S3 [`Deletion`]，**不直接**写租户表/SQL；
//! 领取链接的 relay origin **只**来自配置 [`PublicUrl`]（绝不从 `Host`/`listen`/请求头推导），
//! ticket 只出现在创建响应的一次性页面里，不落日志/磁盘/Referer。

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Path, Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use rusqlite::OptionalExtension;

use crate::admin;
use crate::config::PublicUrl;
use crate::db::{schema, Db};
use crate::deletion::{Deletion, StartOutcome, PURGE_ORDER};
use crate::error::{Error, ErrorCode};
use crate::ids::{
    b64url_decode, b64url_encode, ct_eq, hex_lower, parse_id16_text, random_bytes, sha256,
    sha256_parts, Hash32, Id16,
};
use crate::ratelimit::{RateLimitSpec, RateLimiter};
use crate::registry::{self, CancelOutcome, LifecycleState, VaultDetail, VaultListEntry};
use crate::ticket_link;
use crate::time::Clock;
use crate::Result;

/// admin 登录表单的 body 上限：4 KiB（token 43 字符 + csrf + 余量；超限 413）。
pub const ADMIN_BODY_LIMIT: usize = 4 * 1024;
/// admin 会话 cookie 名（**独立**于数据面，host-only）。
pub const SESSION_COOKIE: &str = "mn_admin_session";
/// 会话闲置过期（30 分钟没活动即失效）。
pub const SESSION_IDLE_SECS: i64 = 30 * 60;
/// 会话绝对过期（8 小时，从登录算起，活动不续期）。
pub const SESSION_ABSOLUTE_SECS: i64 = 8 * 60 * 60;
/// 内存里最多保留的活跃会话数（超出按最久未用淘汰；只是防无界内存）。
const MAX_ACTIVE_SESSIONS: usize = 64;
/// 登录页 CSRF token 的有效期（10 分钟，一次性）。
const LOGIN_CSRF_TTL_SECS: i64 = 10 * 60;
/// 登录 CSRF token 表上限（防无界内存）。
const LOGIN_CSRF_MAX_ENTRIES: usize = 1024;
/// 实例级登录尝试限流：30 次/分钟。
const LOGIN_ATTEMPTS_PER_MINUTE: u32 = 30;
/// 单 IP 失败容忍：5 次失败后第 6 次直接 429（5 分钟窗口）。
const FAILED_LOGINS_PER_IP: u32 = 5;
/// 失败计数窗口（5 分钟）。
const FAILED_LOGIN_WINDOW_SECS: i64 = 5 * 60;
/// 失败计数表上限（防无界内存）。
const MAX_FAILURE_ENTRIES: usize = 256;
/// 登录表单里 token 的最大长度（超出按无效处理，避免对超大输入做哈希）。
const MAX_ADMIN_TOKEN_INPUT: usize = 512;

/// 创建预留的固定 TTL：900 秒（ADR 0018 §2；页面不提供 TTL 输入）。
const CREATE_RESERVATION_TTL_SECS: u64 = admin::DEFAULT_CREATE_TICKET_TTL_SECS as u64;
/// 一次性领取链接页的存活时间：10 分钟（之后即使没被打开也失效）。
const CLAIM_REVEAL_TTL_SECS: i64 = 10 * 60;
/// 同时保留的一次性领取链接上限（防无界内存；超出淘汰最早一条）。
const MAX_CLAIM_REVEALS: usize = 64;
/// 删除作业进度页的 meta refresh 间隔（无 JS 的自动刷新；仅进行中的作业）。
const DELETE_REFRESH_SECS: u32 = 3;

/// 页面 CSP：无脚本、无外部资源、表单只允许同源提交（内联 style 只用于极简排版）。
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; \
     form-action 'self'; base-uri 'none'; frame-ancestors 'none'; img-src 'none'; \
     connect-src 'none'";

/// `init` 生成的 admin token 文本 → 落库哈希（48 字节 base64url → SHA256 文本）。
pub fn token_hash(token: &str) -> Hash32 {
    sha256(token.as_bytes())
}

/// 读库内 admin token 哈希（`relay_meta`；**实例级**元数据，不是 schema 变更）。
pub fn stored_token_hash(conn: &rusqlite::Connection) -> Result<Option<Hash32>> {
    let value: Option<Vec<u8>> = conn
        .query_row(
            "SELECT value FROM relay_meta WHERE key = ?1",
            [schema::META_ADMIN_TOKEN_HASH],
            |row| row.get(0),
        )
        .optional()?;
    match value {
        None => Ok(None),
        Some(bytes) => Ok(Some(bytes.as_slice().try_into().map_err(|_| {
            crate::error::Error::Invariant("relay_meta.admin.token_hash length")
        })?)),
    }
}

/// 写入 admin token 哈希（`init` 调用；**只**落哈希）。已存在时不覆盖（返回是否写入）。
///
/// 调用方必须先检查 [`stored_token_hash`]：这里用 `ON CONFLICT DO NOTHING` 保证"不覆盖
/// 已有 admin token"，避免误把运维记下的 token 弄失效。
pub fn store_token_hash(conn: &rusqlite::Connection, hash: &Hash32) -> Result<()> {
    conn.execute(
        "INSERT INTO relay_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO NOTHING",
        (schema::META_ADMIN_TOKEN_HASH, hash.as_slice()),
    )?;
    Ok(())
}

/// 生成一次性 admin token 明文（32B CSPRNG → base64url 43 字符）。
pub fn generate_token() -> Result<String> {
    Ok(b64url_encode(&random_bytes::<32>()?))
}

/// 生效 admin token 哈希的解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTokenHash {
    /// 实际使用的哈希（`None` = 未初始化，登录一律拒绝）。
    pub hash: Option<Hash32>,
    /// 配置哈希与库内哈希不一致（已按"库内优先"忽略配置值；调用方应打印警告）。
    pub config_conflict: bool,
}

/// 解析生效的 admin token 哈希：**库内 `relay_meta` 优先**，配置哈希只兜底。
///
/// `serve` 启动时调用；**不会**生成/写入任何凭证（未初始化就保持 `None`，由登录页提示先
/// `init`）。这样"空库启动"不会静默产生只存在于内存、运维拿不到的 token。
pub fn resolve_token_hash(db: &Db, configured: Option<Hash32>) -> Result<ResolvedTokenHash> {
    let stored = db.read_instance(stored_token_hash)?;
    match (stored, configured) {
        (Some(stored), Some(configured)) if stored != configured => Ok(ResolvedTokenHash {
            hash: Some(stored),
            config_conflict: true,
        }),
        (Some(stored), _) => Ok(ResolvedTokenHash {
            hash: Some(stored),
            config_conflict: false,
        }),
        (None, configured) => Ok(ResolvedTokenHash {
            hash: configured,
            config_conflict: false,
        }),
    }
}

/// admin Host/Origin **精确**白名单（ADR 0018 §1：不做通配、不做 CORS）。
///
/// 生效条目 = 监听地址的 `host:port` + `localhost`/`127.0.0.1`/`[::1]` 同端口
/// + 配置里的额外条目。监听 `0.0.0.0` 时只有后两类（必须显式配置域名/网卡 IP）。
#[derive(Debug, Clone)]
pub struct AdminHostPolicy {
    allowed: Vec<String>,
}

impl AdminHostPolicy {
    /// 按实际绑定地址与配置条目构造。
    pub fn for_listen(listen: SocketAddr, configured: &[String]) -> Self {
        let port = listen.port();
        let mut allowed = vec![
            format!("localhost:{port}"),
            format!("127.0.0.1:{port}"),
            format!("[::1]:{port}"),
        ];
        match listen.ip() {
            IpAddr::V4(ip) if !ip.is_unspecified() => allowed.push(format!("{ip}:{port}")),
            IpAddr::V6(ip) if !ip.is_unspecified() => allowed.push(format!("[{ip}]:{port}")),
            _ => {}
        }
        for host in configured {
            let host = host.trim().to_ascii_lowercase();
            if !host.is_empty() {
                allowed.push(host);
            }
        }
        allowed.sort();
        allowed.dedup();
        Self { allowed }
    }

    /// 白名单条目（诊断/测试；不含秘密）。
    pub fn allowed(&self) -> &[String] {
        &self.allowed
    }

    /// `Host` 头是否**精确**命中白名单（大小写不敏感，但必须逐字匹配 host:port）。
    pub fn allows_host(&self, host: &str) -> bool {
        self.allowed
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(host.trim()))
    }
}

/// 解析 `Origin`/`Referer` 的 `(scheme, authority)`；只接受 http/https，拒绝 userinfo/路径。
fn origin_parts(value: &str) -> Option<(String, String)> {
    let value = value.trim();
    let (scheme, rest) = value.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?.trim();
    if authority.is_empty() || authority.contains('@') || authority.contains('\\') {
        return None;
    }
    Some((scheme, authority.to_ascii_lowercase()))
}

/// 请求来自 https 吗（只用于决定 cookie 是否加 `Secure`；relay 自身不终止 TLS）。
///
/// 依据只取浏览器发送的 `Origin`/`Referer` 的 scheme，**不信任** `X-Forwarded-Proto`
/// （可伪造）。经 TLS 反向代理时浏览器 `Origin` 就是 `https://…`。
fn request_is_https(headers: &HeaderMap) -> bool {
    let from_origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .and_then(origin_parts)
        .map(|(scheme, _)| scheme == "https");
    let from_referer = headers
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
        .and_then(origin_parts)
        .map(|(scheme, _)| scheme == "https");
    from_origin.or(from_referer).unwrap_or(false)
}

fn session_cookie(session_id: &[u8; 32], secure: bool) -> String {
    let mut cookie = format!(
        "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_ABSOLUTE_SECS}",
        b64url_encode(session_id)
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn clear_session_cookie(secure: bool) -> String {
    let mut cookie = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// 从 `Cookie` 头取出 admin 会话 id（只认本模块的 cookie 名）。
fn session_cookie_value(headers: &HeaderMap) -> Option<[u8; 32]> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(text) = value.to_str() else { continue };
        for pair in text.split(';') {
            let Some((name, raw)) = pair.trim().split_once('=') else {
                continue;
            };
            if name.trim() != SESSION_COOKIE {
                continue;
            }
            let Ok(bytes) = b64url_decode(raw.trim()) else {
                return None;
            };
            return bytes.as_slice().try_into().ok();
        }
    }
    None
}

/// 极简 `application/x-www-form-urlencoded` 解析（axum 未启用 `form` feature，不引新依赖）。
///
/// 重复键、非法百分号编码、非 UTF-8 都按 400 处理（避免键语义歧义）。
fn parse_form(body: &[u8]) -> std::result::Result<HashMap<String, String>, ()> {
    if body.len() > ADMIN_BODY_LIMIT {
        return Err(());
    }
    let text = std::str::from_utf8(body).map_err(|_| ())?;
    let mut fields = HashMap::new();
    for pair in text.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(raw_key)?;
        let value = percent_decode(raw_value)?;
        if fields.insert(key, value).is_some() {
            return Err(());
        }
    }
    Ok(fields)
}

fn percent_decode(input: &str) -> std::result::Result<String, ()> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err(());
                }
                let high = hex_nibble(bytes[index + 1]).ok_or(())?;
                let low = hex_nibble(bytes[index + 2]).ok_or(())?;
                out.push((high << 4) | low);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------- 会话/限流/CSRF

#[derive(Debug, Clone, Copy)]
struct TokenState {
    hash: Option<Hash32>,
    fingerprint: Hash32,
}

fn token_fingerprint(hash: Option<&Hash32>) -> Hash32 {
    match hash {
        Some(hash) => sha256_parts(&[b"mn-admin-token-fingerprint", hash]),
        None => sha256(b"mn-admin-token-uninitialized"),
    }
}

#[derive(Debug, Clone, Copy)]
struct SessionEntry {
    created_at: i64,
    last_seen_at: i64,
    csrf: [u8; 32],
    token_fingerprint: Hash32,
}

/// 内存 session 表（进程重启即失效；不落盘）。
#[derive(Debug, Default)]
pub struct AdminSessions {
    inner: Mutex<HashMap<[u8; 32], SessionEntry>>,
}

/// 新建会话的返回值：session id 与随表单下发的 CSRF token。
#[derive(Debug, Clone, Copy)]
pub struct NewSession {
    /// 32B 随机会话 id（base64url 后放 cookie）。
    pub id: [u8; 32],
    /// 32B CSRF token（表单隐藏字段；与 session 绑定）。
    pub csrf: [u8; 32],
}

impl AdminSessions {
    /// 空表。
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<[u8; 32], SessionEntry>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 新建会话；CSPRNG 不可用或表异常时返回 `None`。
    pub fn create(&self, now: i64, token_fingerprint: Hash32) -> Option<NewSession> {
        let id = random_bytes::<32>().ok()?;
        let csrf = random_bytes::<32>().ok()?;
        let mut map = self.lock();
        if map.len() >= MAX_ACTIVE_SESSIONS {
            let oldest = map
                .iter()
                .min_by_key(|(_, entry)| entry.last_seen_at)
                .map(|(id, _)| *id);
            if let Some(oldest) = oldest {
                map.remove(&oldest);
            }
        }
        map.insert(
            id,
            SessionEntry {
                created_at: now,
                last_seen_at: now,
                csrf,
                token_fingerprint,
            },
        );
        Some(NewSession { id, csrf })
    }

    /// 校验会话；过期/换 token/未知都返回 `None`（过期条目顺手删除）。
    ///
    /// 换 token（fingerprint 变化）会清空**全部**会话：旧登录态立即失效。
    pub fn validate(&self, id: &[u8; 32], now: i64, token_fingerprint: Hash32) -> Option<[u8; 32]> {
        let mut map = self.lock();
        let entry = map.get(id)?;
        let (mismatch, expired) = (
            entry.token_fingerprint != token_fingerprint,
            now.saturating_sub(entry.last_seen_at) >= SESSION_IDLE_SECS
                || now.saturating_sub(entry.created_at) >= SESSION_ABSOLUTE_SECS,
        );
        if mismatch {
            map.clear();
            return None;
        }
        if expired {
            map.remove(id);
            return None;
        }
        let entry = map.get_mut(id)?;
        entry.last_seen_at = now;
        Some(entry.csrf)
    }

    /// 删除单个会话（logout）。
    pub fn remove(&self, id: &[u8; 32]) {
        self.lock().remove(id);
    }

    /// 清空全部会话（换 token / 进程级吊销）。
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// 当前会话数（诊断/测试）。
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// 是否没有会话。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug)]
struct FailureEntry {
    window_start: i64,
    count: u32,
}

/// 登录限流：实例级尝试窗口 + 按 IP 的失败窗口（都只在内存里，进程重启清零）。
#[derive(Debug)]
struct LoginLimiter {
    attempts: RateLimiter,
    failures: Mutex<HashMap<Option<IpAddr>, FailureEntry>>,
}

impl LoginLimiter {
    fn new() -> Self {
        Self {
            attempts: RateLimiter::new(RateLimitSpec::new(LOGIN_ATTEMPTS_PER_MINUTE, 60)),
            failures: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Option<IpAddr>, FailureEntry>> {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 取一次尝试配额（实例级 30/分钟）。
    fn check_attempt(&self) -> bool {
        self.attempts.acquire().is_ok()
    }

    /// 该 IP 是否已被失败限流（达到 [`FAILED_LOGINS_PER_IP`] 次失败后拒绝下一次尝试）。
    fn is_blocked(&self, key: Option<IpAddr>, now: i64) -> bool {
        let map = self.lock();
        match map.get(&key) {
            None => false,
            Some(entry) if now.saturating_sub(entry.window_start) >= FAILED_LOGIN_WINDOW_SECS => {
                false
            }
            Some(entry) => entry.count >= FAILED_LOGINS_PER_IP,
        }
    }

    /// 记录一次失败（过期条目顺手清理；表有上限，不会无界增长）。
    fn record_failure(&self, key: Option<IpAddr>, now: i64) {
        let mut map = self.lock();
        map.retain(|_, entry| now.saturating_sub(entry.window_start) < FAILED_LOGIN_WINDOW_SECS);
        if !map.contains_key(&key) && map.len() >= MAX_FAILURE_ENTRIES {
            let oldest = map
                .iter()
                .min_by_key(|(_, entry)| entry.window_start)
                .map(|(ip, _)| *ip);
            if let Some(oldest) = oldest {
                map.remove(&oldest);
            }
        }
        match map.get_mut(&key) {
            Some(entry) => {
                if now.saturating_sub(entry.window_start) >= FAILED_LOGIN_WINDOW_SECS {
                    *entry = FailureEntry {
                        window_start: now,
                        count: 1,
                    };
                } else {
                    entry.count = entry.count.saturating_add(1);
                }
            }
            None => {
                map.insert(
                    key,
                    FailureEntry {
                        window_start: now,
                        count: 1,
                    },
                );
            }
        }
    }
}

/// 登录页 CSRF token（一次性；服务端内存表）。
#[derive(Debug, Default)]
struct LoginCsrfStore {
    entries: Mutex<HashMap<String, i64>>,
}

impl LoginCsrfStore {
    fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, i64>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn issue(&self, now: i64) -> std::result::Result<String, ()> {
        let bytes = random_bytes::<32>().map_err(|_| ())?;
        let token = b64url_encode(&bytes);
        let mut map = self.lock();
        map.retain(|_, expires_at| *expires_at > now);
        if map.len() >= LOGIN_CSRF_MAX_ENTRIES {
            let oldest = map
                .iter()
                .min_by_key(|(_, expires_at)| **expires_at)
                .map(|(token, _)| token.clone());
            if let Some(oldest) = oldest {
                map.remove(&oldest);
            }
        }
        map.insert(token.clone(), now.saturating_add(LOGIN_CSRF_TTL_SECS));
        Ok(token)
    }

    fn consume(&self, token: &str, now: i64) -> bool {
        // 到期时刻之后（含恰好到期）一律拒绝：TTL 是"有效到"的严格上界。
        match self.lock().remove(token) {
            Some(expires_at) => expires_at > now,
            None => false,
        }
    }
}

// ---------------------------------------------------------------- 一次性领取链接

/// 创建成功后**只展示一次**的 claim 领取链接（ADR 0018 §1：链接只在创建响应出现一次）。
///
/// 只在内存里短暂保留（10 分钟、上限 64 条、与创建它的会话绑定）；被第一次 GET 取走后
/// 立即删除。**不落盘、不写日志、不放进 URL**——URL 里只有随机 reveal id。
#[derive(Debug, Clone)]
struct RevealedClaim {
    /// 创建它的 admin 会话（其它会话不能取走）。
    session: [u8; 32],
    /// 预留的 `vault_id`（公开）。
    vault_id: Id16,
    /// `mn-pair://create?...` 完整链接（含 fragment 里的 ticket 明文）。
    link: String,
    /// 预留创建时刻。
    created_at: i64,
    /// 票据过期时刻（900s TTL）。
    expires_at: i64,
}

/// 一次性领取链接表（内存；进程重启即失效）。
#[derive(Debug, Default)]
struct ClaimReveals {
    inner: Mutex<HashMap<[u8; 32], RevealedClaim>>,
}

impl ClaimReveals {
    fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<[u8; 32], RevealedClaim>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 存入一条待展示链接（调用方先生成 CSPRNG id；过期条目顺手清理）。
    fn insert(&self, id: [u8; 32], now: i64, claim: RevealedClaim) {
        let mut map = self.lock();
        map.retain(|_, entry| now.saturating_sub(entry.created_at) < CLAIM_REVEAL_TTL_SECS);
        if map.len() >= MAX_CLAIM_REVEALS {
            let oldest = map
                .iter()
                .min_by_key(|(_, entry)| entry.created_at)
                .map(|(id, _)| *id);
            if let Some(oldest) = oldest {
                map.remove(&oldest);
            }
        }
        map.insert(id, claim);
    }

    /// 取走一条（会话必须一致；过期/已取走 = `None`，**不**重复展示）。
    fn take(&self, id: &[u8; 32], session: &[u8; 32], now: i64) -> Option<RevealedClaim> {
        let mut map = self.lock();
        map.retain(|_, entry| now.saturating_sub(entry.created_at) < CLAIM_REVEAL_TTL_SECS);
        let claim = map.get(id)?;
        if claim.session != *session {
            return None;
        }
        let claim = map.remove(id)?;
        Some(claim)
    }

    /// 当前待展示条数（诊断/测试）。
    fn len(&self) -> usize {
        self.lock().len()
    }
}

// ---------------------------------------------------------------- 状态/路由

/// 管理面共享状态：同一个 `Db`、**独立**的凭证/会话/限流/Host 白名单。
pub struct AdminWeb {
    db: Db,
    clock: Arc<dyn Clock>,
    token: Mutex<TokenState>,
    sessions: AdminSessions,
    login_limiter: LoginLimiter,
    login_csrf: LoginCsrfStore,
    policy: AdminHostPolicy,
    /// claim 链接使用的数据面 canonical origin（**只**来自配置；`None` = 未配置）。
    public_url: Option<PublicUrl>,
    /// 创建成功后只展示一次的领取链接（内存、会话绑定、用后即删）。
    reveals: ClaimReveals,
    /// 与数据面**共享**的删除作业运行时（fence 必须同一份）。
    deletion: Arc<Deletion>,
}

impl AdminWeb {
    /// 组装状态。
    ///
    /// `token_hash` 由调用方解析（`serve`：库内 `relay_meta` 优先，配置哈希兜底）。
    /// `None` 表示尚未初始化：admin listener 照常启动，但登录一律拒绝并提示先 `init`。
    /// `public_url` 是 claim 链接的 canonical origin（只来自配置；`None` = 创建页明确报错）。
    pub fn new(
        db: Db,
        clock: Arc<dyn Clock>,
        token_hash: Option<Hash32>,
        public_url: Option<PublicUrl>,
        policy: AdminHostPolicy,
        deletion: Arc<Deletion>,
    ) -> Arc<Self> {
        let fingerprint = token_fingerprint(token_hash.as_ref());
        Arc::new(Self {
            db,
            clock,
            token: Mutex::new(TokenState {
                hash: token_hash,
                fingerprint,
            }),
            sessions: AdminSessions::new(),
            login_limiter: LoginLimiter::new(),
            login_csrf: LoginCsrfStore::new(),
            policy,
            public_url,
            reveals: ClaimReveals::new(),
            deletion,
        })
    }

    fn lock_token(&self) -> MutexGuard<'_, TokenState> {
        self.token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 当前 token 哈希（`None` = 未初始化；**不**返回值给页面/日志）。
    fn token_hash(&self) -> Option<Hash32> {
        self.lock_token().hash
    }

    /// 当前 token 指纹（会话与 token 绑定；变化即全量吊销）。
    fn current_fingerprint(&self) -> Hash32 {
        self.lock_token().fingerprint
    }

    /// 是否已初始化 admin token（页面只显示布尔值，不回显哈希）。
    pub fn has_token(&self) -> bool {
        self.token_hash().is_some()
    }

    /// 替换 admin token 哈希（未来 `--rotate-admin-token`/测试用）。
    ///
    /// 指纹变化时立即清空全部会话：旧 cookie 立即失效，必须用新 token 重新登录。
    pub fn set_token_hash(&self, hash: Option<Hash32>) {
        let fingerprint = token_fingerprint(hash.as_ref());
        let changed = {
            let mut guard = self.lock_token();
            let changed = guard.fingerprint != fingerprint;
            guard.hash = hash;
            guard.fingerprint = fingerprint;
            changed
        };
        if changed {
            self.sessions.clear();
        }
    }

    /// Host/Origin 白名单（诊断/测试）。
    pub fn host_policy(&self) -> &AdminHostPolicy {
        &self.policy
    }

    /// 当前会话数（测试：logout/过期/换 token 后的吊销断言）。
    pub fn sessions_len(&self) -> usize {
        self.sessions.len()
    }

    /// 生效的 claim 链接 origin（只来自配置；`None` = 未配置）。
    pub fn public_url(&self) -> Option<&PublicUrl> {
        self.public_url.as_ref()
    }

    /// 待展示的一次性领取链接条数（测试：用后即删断言）。
    pub fn reveals_len(&self) -> usize {
        self.reveals.len()
    }

    /// 共享的删除作业运行时（诊断/测试）。
    pub fn deletion(&self) -> &Arc<Deletion> {
        &self.deletion
    }
}

/// 从 `ConnectInfo` 取对端 IP（`oneshot` 测试无连接信息时为 `None`；只用于登录失败限流分桶）。
#[derive(Debug, Clone, Copy)]
struct PeerIp(Option<IpAddr>);

impl<S> FromRequestParts<S> for PeerIp
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> std::result::Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0.ip()),
        ))
    }
}

/// 构建 admin 路由（**只**挂在独立 admin listener 上，绝不与数据面路由合并）。
pub fn router(state: Arc<AdminWeb>) -> Router {
    Router::new()
        .route("/admin", get(admin_home))
        .route("/admin/login", get(login_page).post(login_submit))
        .route("/admin/logout", post(logout_submit))
        // S4a：创建预留（POST）与创建表单（GET）。`new`/`created` 是静态段，优先于 `{vault}`。
        .route(
            "/admin/vaults",
            post(create_vault_submit).fallback(not_found),
        )
        .route("/admin/vaults/new", get(new_vault_page))
        .route("/admin/vaults/created/{reveal}", get(claim_reveal_page))
        .route("/admin/vaults/{vault}", get(vault_detail_page))
        .route("/admin/vaults/{vault}/cancel", post(cancel_vault_submit))
        // S3 触发 + S4a 进度/确认页：GET 显示确认表单或作业进度，POST 触发删除。
        .route(
            "/admin/vaults/{vault}/delete",
            get(delete_vault_page).post(delete_vault_submit),
        )
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(ADMIN_BODY_LIMIT))
        .layer(middleware::from_fn_with_state(state.clone(), host_guard))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// 安全响应头（所有 admin 响应，含错误页/重定向）。
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Host/Origin 门禁：精确 Host 白名单 + POST 要求同源 `Origin`/`Referer`。
async fn host_guard(State(web): State<Arc<AdminWeb>>, request: Request, next: Next) -> Response {
    let host = match request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    {
        Some(host) if !host.is_empty() => host.to_ascii_lowercase(),
        _ => {
            return page(
                StatusCode::BAD_REQUEST,
                "请求被拒绝",
                "<p class=\"warn\">缺少合法的 <code>Host</code> 头：管理面只接受精确 Host 白名单的请求。</p>",
            )
        }
    };
    if !web.policy.allows_host(&host) {
        return page(
            StatusCode::FORBIDDEN,
            "Host 不在白名单",
            "<p class=\"warn\">该 <code>Host</code> 不在 admin 白名单里（防 DNS rebinding）。\
             请在 <code>admin_allowed_hosts</code> 里显式加入 <code>host:port</code>。</p>",
        );
    }
    if request.method() == Method::POST {
        let origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok());
        let referer = request
            .headers()
            .get(header::REFERER)
            .and_then(|value| value.to_str().ok());
        let parsed = match origin {
            Some(value) => origin_parts(value),
            None => referer.and_then(origin_parts),
        };
        // Origin/Referer 缺失或为 `null`（沙箱/隐私浏览器的不透明来源）时不在这里拒绝：
        // 交给各 POST handler 校验一次性/会话绑定 CSRF。若来源存在且不同源，仍 fail-closed。
        if let Some((_scheme, authority)) = parsed {
            if !authority.eq_ignore_ascii_case(&host) {
                return page(
                    StatusCode::FORBIDDEN,
                    "跨站请求被拒绝",
                    "<p class=\"warn\">POST 必须来自同源浏览器表单：<code>Origin</code>/<code>Referer</code> \
                     与 <code>Host</code> 不一致（拒绝跨站提交）。</p>",
                );
            }
        }
    }
    next.run(request).await
}

// ---------------------------------------------------------------- handler

async fn not_found() -> Response {
    page(
        StatusCode::NOT_FOUND,
        "页面不存在",
        "<p>未知的 admin 路径。<a href=\"/admin\">返回管理首页</a>（未登录会跳转到登录页）。</p>",
    )
}

async fn login_page(State(web): State<Arc<AdminWeb>>) -> Response {
    let now = web.clock.now_secs();
    let initialized = web.has_token();
    let Ok(csrf) = web.login_csrf.issue(now) else {
        return internal_error_page();
    };
    let mut body = String::new();
    body.push_str(
        "<p>内嵌 Web 管理面：使用 <code>menghunote-relay init</code> 生成并只显示一次的 admin token 登录。</p>",
    );
    if !initialized {
        body.push_str(
            "<p class=\"warn\">admin token <strong>尚未初始化</strong>：请先在本机执行 \
             <code>menghunote-relay init --db &lt;path&gt;</code>。relay 不会自动生成，也不会自动打印 token。</p>",
        );
    }
    body.push_str(
        "<p class=\"warn\">本页面走明文 HTTP：在局域网或不可信网络上，admin token 与会话 Cookie \
         可能被截获。请只在受信任私网使用，或部署在同机 TLS 反向代理之后（relay 自身不终止 TLS）。</p>",
    );
    body.push_str(&format!(
        "<form method=\"post\" action=\"/admin/login\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
         <label>admin token\
         <input type=\"password\" name=\"token\" autocomplete=\"off\" spellcheck=\"false\" required autofocus>\
         </label>\
         <button type=\"submit\">登录</button>\
         </form>"
    ));
    page(StatusCode::OK, "relay 管理登录", &body)
}

async fn login_submit(
    State(web): State<Arc<AdminWeb>>,
    peer: PeerIp,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let now = web.clock.now_secs();
    let key = peer.0;
    if !web.login_limiter.check_attempt() {
        return rate_limited_page();
    }
    if web.login_limiter.is_blocked(key, now) {
        return rate_limited_page();
    }
    let form =
        match parse_form(&body) {
            Ok(form) => form,
            Err(()) => return page(
                StatusCode::BAD_REQUEST,
                "表单无效",
                "<p class=\"warn\">请求体必须是 <code>application/x-www-form-urlencoded</code>，\
                 且不超过 4 KiB、不得有重复字段。</p>",
            ),
        };
    let csrf_ok = form
        .get("csrf")
        .map(|token| web.login_csrf.consume(token, now))
        .unwrap_or(false);
    if !csrf_ok {
        return page(
            StatusCode::FORBIDDEN,
            "CSRF 校验失败",
            "<p class=\"warn\">CSRF token 缺失、已使用或不匹配：请重新打开登录页再登录。</p>",
        );
    }
    let token = form.get("token").map(String::as_str).unwrap_or_default();
    let Some(expected) = web.token_hash() else {
        web.login_limiter.record_failure(key, now);
        return page(
            StatusCode::UNAUTHORIZED,
            "admin token 未初始化",
            "<p class=\"warn\">本实例还没有 admin token：请先执行 \
             <code>menghunote-relay init --db &lt;path&gt;</code>。登录被拒绝，relay 不会自动生成。</p>",
        );
    };
    if token.is_empty() || token.len() > MAX_ADMIN_TOKEN_INPUT {
        web.login_limiter.record_failure(key, now);
        return unauthorized_page();
    }
    let actual = token_hash(token);
    if !ct_eq(&actual, &expected) {
        // 只累计失败；限流在**下一次**尝试的入口检查（第 5 次失败仍回 401，第 6 次 429）。
        web.login_limiter.record_failure(key, now);
        return unauthorized_page();
    }
    let Some(session) = web.sessions.create(now, web.current_fingerprint()) else {
        return internal_error_page();
    };
    let secure = request_is_https(&headers);
    redirect("/admin", Some(session_cookie(&session.id, secure)))
}

async fn admin_home(State(web): State<Arc<AdminWeb>>, headers: HeaderMap) -> Response {
    let now = web.clock.now_secs();
    let Some(session) = current_session(&web, &headers, now) else {
        return redirect("/admin/login", None);
    };
    let entries = match web
        .db
        .read_instance(move |conn| registry::list_vaults(conn, now))
    {
        Ok(entries) => entries,
        // relay 库模块不写业务日志（canary 审计）：内部错误只体现为 500 页面 + 请求日志。
        Err(_) => return internal_error_page(),
    };
    let mut body = String::new();
    body.push_str("<p><strong>已登录</strong>（admin token 校验通过） · 数据面 URL：");
    body.push_str(&public_url_html(web.public_url.as_ref()));
    body.push_str(
        " <span class=\"muted\">（只读，来自配置 <code>public_url</code>；\
         不从 Host/listen/请求头推导）</span></p>",
    );
    body.push_str(&nav_html());
    body.push_str(&format!(
        "<p>vault 数：<code>{}</code>{}</p>",
        entries.len(),
        if entries.is_empty() {
            "（空库，尚未创建任何 vault）"
        } else {
            ""
        }
    ));
    if entries.is_empty() {
        body.push_str(
            "<p class=\"muted\">空 relay：还没有任何 vault。先点「创建领取链接」生成一次性预留，\
             再把 <code>mn-pair://create</code> 链接私发给要创建实例的设备。</p>",
        );
    } else {
        body.push_str(&vault_table_html(web.public_url.as_ref(), &entries));
    }
    body.push_str(&logout_form(&session));
    body.push_str(PLAIN_HTTP_WARNING);
    page(StatusCode::OK, "relay 管理 · vault 列表", &body)
}

/// 创建领取链接的表单页（无 `vault_id` / TTL / quota 输入：服务器自动 128-bit id、
/// 900s TTL、quota=1）。未配置 `public_url` → 明确 409，不展示可提交表单。
async fn new_vault_page(State(web): State<Arc<AdminWeb>>, headers: HeaderMap) -> Response {
    let now = web.clock.now_secs();
    let Some(session) = current_session(&web, &headers, now) else {
        return redirect("/admin/login", None);
    };
    let Some(public_url) = web.public_url.as_ref() else {
        return page(
            StatusCode::CONFLICT,
            "无法创建领取链接",
            "<p class=\"warn\">未配置 <code>public_url</code>：服务器不知道 claim 链接应指向哪个\
             数据面 origin，因此<strong>不会</strong>生成链接（也绝不从 <code>Host</code>/<code>listen</code>/\
             请求头推导）。</p>\
             <p>请在 <code>serve</code> 配置里加一行（二选一），重启 relay 后再试：</p>\
             <ul>\
               <li>本机开发：<code>public_url = \"http://127.0.0.1:18080\"</code></li>\
               <li>TLS 入口：<code>public_url = \"https://relay.example.com\"</code>\
                   （建议由同机反向代理终止 TLS）</li>\
             </ul>\
             <p><a href=\"/admin\">返回 vault 列表</a></p>",
        );
    };
    let mut body = String::new();
    body.push_str(&nav_html());
    body.push_str(
        "<p>创建后服务器自动生成 <strong>vault id</strong>、<strong>900 秒</strong>有效的\
         一次性创建票据（quota=1），并返回一条 <code>mn-pair://create</code> 领取链接。</p>",
    );
    body.push_str(&format!(
        "<p>数据面 URL：{} <span class=\"muted\">（只读，来自配置 <code>public_url</code>）</span></p>",
        public_url_html(Some(public_url))
    ));
    body.push_str(
        "<p class=\"warn\">领取链接里含一次性票据明文：只在创建成功后的页面显示<strong>一次</strong>，\
         刷新不会再显示，服务器也<strong>不</strong>保存可重印的明文。丢失只能取消该预留后重新创建\
         （旧 vault id 不能复用）。</p>",
    );
    body.push_str(&format!(
        "<form method=\"post\" action=\"/admin/vaults\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{}\">\
         <button type=\"submit\">创建领取链接</button>\
         </form>",
        b64url_encode(&session.csrf)
    ));
    body.push_str(PLAIN_HTTP_WARNING);
    page(StatusCode::OK, "创建领取链接", &body)
}

/// `POST /admin/vaults`：一次性 CSRF → S2 `create_reservation`（内部 registry/票据事务）
/// → 303 到**只展示一次**的领取链接页（URL 里没有票据）。
async fn create_vault_submit(
    State(web): State<Arc<AdminWeb>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let now = web.clock.now_secs();
    let Some(session) = current_session(&web, &headers, now) else {
        return redirect("/admin/login", None);
    };
    let form = match parse_form(&body) {
        Ok(form) => form,
        Err(()) => return invalid_form_page(),
    };
    if !csrf_ok(&form, &session) {
        return csrf_failed_page("创建领取链接");
    }
    let Some(public_url) = web.public_url.as_ref() else {
        return page(
            StatusCode::CONFLICT,
            "无法创建领取链接",
            "<p class=\"warn\">未配置 <code>public_url</code>：本次没有创建任何预留/票据\
             （服务器绝不从请求头推导链接 origin）。请先在 serve 配置里设置后重启。</p>\
             <p><a href=\"/admin\">返回 vault 列表</a></p>",
        );
    };
    // 先取一次性 reveal id：即使后面 CSPRNG/写库失败也不会留下"链接丢了"的预留。
    let reveal_id = match random_bytes::<32>() {
        Ok(id) => id,
        Err(_) => return internal_error_page(),
    };
    let issued = match create_reservation(&web).await {
        Ok(issued) => issued,
        Err(_) => return internal_error_page(),
    };
    let link = ticket_link::create_link_url(public_url.as_str(), issued.vault_id, &issued.token);
    web.reveals.insert(
        reveal_id,
        now,
        RevealedClaim {
            session: session.id,
            vault_id: issued.vault_id,
            link,
            created_at: issued.created_at,
            expires_at: issued.expires_at,
        },
    );
    redirect(
        &format!("/admin/vaults/created/{}", b64url_encode(&reveal_id)),
        None,
    )
}

/// `GET /admin/vaults/created/{reveal}`：一次性领取链接页（取走即删）。
async fn claim_reveal_page(
    State(web): State<Arc<AdminWeb>>,
    Path(reveal): Path<String>,
    headers: HeaderMap,
) -> Response {
    let now = web.clock.now_secs();
    let Some(session) = current_session(&web, &headers, now) else {
        return redirect("/admin/login", None);
    };
    let Some(reveal_id) = b64url_decode(&reveal)
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
    else {
        return reveal_gone_page();
    };
    let Some(claim) = web.reveals.take(&reveal_id, &session.id, now) else {
        return reveal_gone_page();
    };
    let vault_id = hex_lower(&claim.vault_id);
    let mut body = String::new();
    body.push_str(&nav_html());
    body.push_str(
        "<p class=\"warn\">这条领取链接<strong>只显示这一次</strong>。刷新/后退/重新打开本页\
         都不会再显示；服务器只在创建响应里保留过一次，用后即删。</p>",
    );
    body.push_str("<table>");
    body.push_str(&detail_row("vault id", &detail_value(&vault_id)));
    body.push_str(&detail_row(
        "创建时间",
        &detail_value(&format_utc(claim.created_at)),
    ));
    body.push_str(&detail_row(
        "票据过期",
        &detail_value(&format_utc(claim.expires_at)),
    ));
    body.push_str(&detail_row("领取链接", &detail_value(&claim.link)));
    body.push_str("</table>");
    body.push_str(
        "<p>把链接私发给要创建实例的设备；链接含一次性票据明文（只在 fragment），\
         不要截图/发群/写入工单。过期或泄露就取消该预留后重新创建。</p>",
    );
    body.push_str(&format!(
        "<p><a href=\"/admin/vaults/{vault_id}\">查看该 vault 详情</a> · \
         <a href=\"/admin\">返回 vault 列表</a></p>"
    ));
    page(StatusCode::OK, "领取链接（仅显示一次）", &body)
}

/// `GET /admin/vaults/{vault}`：详情（只读 registry 投影；不显示票据 hash/明文）。
async fn vault_detail_page(
    State(web): State<Arc<AdminWeb>>,
    Path(vault): Path<String>,
    headers: HeaderMap,
) -> Response {
    let now = web.clock.now_secs();
    let Some(session) = current_session(&web, &headers, now) else {
        return redirect("/admin/login", None);
    };
    let Some(vault_id) = parse_id16_text(&vault) else {
        return vault_not_found_page();
    };
    let detail = match web
        .db
        .read_instance(move |conn| registry::vault_detail(conn, &vault_id, now))
    {
        Ok(Some(detail)) => detail,
        Ok(None) => return vault_not_found_page(),
        Err(_) => return internal_error_page(),
    };
    let body = detail_page_body(&web, &detail, &session);
    page(StatusCode::OK, "vault 详情", &body)
}

/// `POST /admin/vaults/{vault}/cancel`：取消 `RESERVED` 预留（claim 已赢 = 409）。
async fn cancel_vault_submit(
    State(web): State<Arc<AdminWeb>>,
    Path(vault): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let now = web.clock.now_secs();
    let Some(session) = current_session(&web, &headers, now) else {
        return redirect("/admin/login", None);
    };
    let form = match parse_form(&body) {
        Ok(form) => form,
        Err(()) => return invalid_form_page(),
    };
    if !csrf_ok(&form, &session) {
        return csrf_failed_page("取消预留");
    }
    let Some(vault_id) = parse_id16_text(&vault) else {
        return vault_not_found_page();
    };
    match cancel_reservation(&web, vault_id).await {
        Ok(CancelOutcome::Cancelled { .. }) | Ok(CancelOutcome::AlreadyCancelled) => {
            redirect("/admin", None)
        }
        Ok(CancelOutcome::Expired) => page(
            StatusCode::CONFLICT,
            "预留已过期",
            "<p class=\"warn\">该预留的票据已经过期：本次已把它标成 <code>EXPIRED</code> 并作废票据\
             （或它本来就是 <code>EXPIRED</code>）。没有可取消的活预留；该 vault id 不能复用，\
             如需创建请重新生成领取链接。</p>\
             <p><a href=\"/admin\">返回 vault 列表</a></p>",
        ),
        Err(err) => match err.code() {
            Some(ErrorCode::NotFound) => vault_not_found_page(),
            Some(ErrorCode::CasConflict) | Some(ErrorCode::InvalidState) => {
                state_conflict_page(&web, vault_id, "取消预留")
            }
            _ => internal_error_page(),
        },
    }
}

/// `GET /admin/vaults/{vault}/delete`：`ACTIVE` 显示确认表单；删除三态显示作业/进度。
async fn delete_vault_page(
    State(web): State<Arc<AdminWeb>>,
    Path(vault): Path<String>,
    headers: HeaderMap,
) -> Response {
    let now = web.clock.now_secs();
    let Some(session) = current_session(&web, &headers, now) else {
        return redirect("/admin/login", None);
    };
    let Some(vault_id) = parse_id16_text(&vault) else {
        return vault_not_found_page();
    };
    let detail = match web
        .db
        .read_instance(move |conn| registry::vault_detail(conn, &vault_id, now))
    {
        Ok(Some(detail)) => detail,
        Ok(None) => return vault_not_found_page(),
        Err(_) => return internal_error_page(),
    };
    match detail.entry.state {
        LifecycleState::Active => delete_confirm_page(&detail, &session),
        LifecycleState::Deleting | LifecycleState::DeleteFailed | LifecycleState::Deleted => {
            delete_state_page(&detail, &session)
        }
        _ => delete_not_active_page(&detail),
    }
}

async fn logout_submit(
    State(web): State<Arc<AdminWeb>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let now = web.clock.now_secs();
    let secure = request_is_https(&headers);
    let Some(session) = current_session(&web, &headers, now) else {
        return redirect("/admin/login", Some(clear_session_cookie(secure)));
    };
    let form = match parse_form(&body) {
        Ok(form) => form,
        Err(()) => {
            return page(
                StatusCode::BAD_REQUEST,
                "表单无效",
                "<p class=\"warn\">请求体无效。</p>",
            )
        }
    };
    let expected = b64url_encode(&session.csrf);
    let csrf_ok = form
        .get("csrf")
        .map(|csrf| ct_eq(csrf.as_bytes(), expected.as_bytes()))
        .unwrap_or(false);
    if !csrf_ok {
        return page(
            StatusCode::FORBIDDEN,
            "CSRF 校验失败",
            "<p class=\"warn\">退出登录需要与管理页一致的 CSRF token。</p>",
        );
    }
    web.sessions.remove(&session.id);
    redirect("/admin/login", Some(clear_session_cookie(secure)))
}

/// `POST /admin/vaults/{vault}/delete`：会话 + CSRF + `confirm`（必须是该 vault 的 32 位 hex）。
///
/// 标记事务提交后才返回 `202`/job（ADR 0018 §3 步 2）；重复请求幂等返回同一 job。
async fn delete_vault_submit(
    State(web): State<Arc<AdminWeb>>,
    Path(vault): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let now = web.clock.now_secs();
    let Some(session) = current_session(&web, &headers, now) else {
        return redirect("/admin/login", None);
    };
    let form = match parse_form(&body) {
        Ok(form) => form,
        Err(()) => return invalid_form_page(),
    };
    if !csrf_ok(&form, &session) {
        return csrf_failed_page("删除");
    }
    let Some(vault_id) = parse_id16_text(&vault) else {
        return vault_not_found_page();
    };
    // 确认字符串**严格**：`DELETE <32 位小写 hex>`，大小写/空格都有意义（不做 trim/忽略大小写）。
    let expected_confirm = format!("DELETE {}", hex_lower(&vault_id));
    if form.get("confirm").map(String::as_str) != Some(expected_confirm.as_str()) {
        return page(
            StatusCode::BAD_REQUEST,
            "确认不匹配",
            &format!(
                "<p class=\"warn\">不可逆删除必须显式确认：<code>confirm</code> 必须精确等于\
                 <code>{}</code>（大小写与单个空格都严格；没有 trim）。本次没有改任何状态。</p>\
                 <p><a href=\"/admin/vaults/{}/delete\">返回确认页</a></p>",
                escape_html(&expected_confirm),
                hex_lower(&vault_id),
            ),
        );
    }
    match web.deletion.start(vault_id).await {
        Ok(outcome) => deletion_accepted_page(&outcome),
        Err(err) => match err.code() {
            Some(ErrorCode::NotFound) => vault_not_found_page(),
            Some(ErrorCode::CasConflict) | Some(ErrorCode::InvalidState) => {
                state_conflict_page(&web, vault_id, "删除")
            }
            _ => internal_error_page(),
        },
    }
}

/// `202` 作业页：只展示不透明 id/状态/进度（无租户内容、无 token）。
///
/// **202 只在 `Deletion::start` 的持久标记事务提交后返回**；页面不会把"已接受"当"已完成"。
fn deletion_accepted_page(outcome: &StartOutcome) -> Response {
    let job = outcome.job();
    let vault_id = hex_lower(&job.vault_id);
    let job_text = job
        .job_id
        .map(|id| format!("<code>{}</code>", hex_lower(&id)))
        .unwrap_or_else(|| "（历史墓碑无作业 id）".to_string());
    let (action, note) = match outcome {
        StartOutcome::Started(_) => (
            "已开始（本次新作业）",
            "数据面入口已关闭，作业独立于本页面/连接继续执行，断开浏览器不会取消。",
        ),
        StartOutcome::AlreadyDeleting(_) => (
            "作业已在进行中（幂等：同一 job）",
            "数据面入口保持关闭；进度沿用持久化值。",
        ),
        StartOutcome::Retried(_) => (
            "上次失败，已重新武装同一 job",
            "数据面入口保持关闭；重试沿用同一 job id 与已持久化的进度。",
        ),
        StartOutcome::AlreadyDeleted(_) => (
            "已是删除墓碑（幂等：没有可再删的数据）",
            "该 vault 的租户数据与票据此前已清理；registry 只保留最小墓碑元数据。",
        ),
    };
    let body = format!(
        "<p>删除请求已记录：{action}。{note}</p>\
         <table>\
           <tr><th>vault</th><td><code>{vault_id}</code></td></tr>\
           <tr><th>job</th><td>{job_text}</td></tr>\
           <tr><th>状态</th><td><code>{state}</code></td></tr>\
           <tr><th>进度</th><td><code>{progress}/{total}</code> 张表（含 <code>vaults</code> 行）已清空</td></tr>\
         </table>\
         <p class=\"warn\">删除不可逆：这是逻辑清除，不保证 WAL/备份/客户端副本被擦除；\
         <strong>本页不代表已完成</strong>。完成后 registry 只保留最小墓碑元数据。</p>\
         <p><a href=\"/admin/vaults/{vault_id}/delete\">查看删除进度（可刷新）</a> · \
         <a href=\"/admin/vaults/{vault_id}\">vault 详情</a> · <a href=\"/admin\">vault 列表</a></p>",
        state = escape_html(job.state.as_str()),
        progress = job.progress,
        total = PURGE_ORDER.len(),
    );
    page(StatusCode::ACCEPTED, "删除作业已接受", &body)
}

#[derive(Debug, Clone, Copy)]
struct ActiveSession {
    id: [u8; 32],
    csrf: [u8; 32],
}

fn current_session(web: &AdminWeb, headers: &HeaderMap, now: i64) -> Option<ActiveSession> {
    let id = session_cookie_value(headers)?;
    let csrf = web.sessions.validate(&id, now, web.current_fingerprint())?;
    Some(ActiveSession { id, csrf })
}

// ---------------------------------------------------------------- 数据面 URL / 列表 / 详情辅助

/// 全站导航（只含同源链接；无第三方资产/脚本）。
fn nav_html() -> String {
    "<nav><a href=\"/admin\">vault 列表</a> · \
     <a href=\"/admin/vaults/new\">创建领取链接</a></nav>"
        .to_string()
}

/// 数据面 URL 的只读展示（来自配置；未配置时明确标注）。
fn public_url_html(public_url: Option<&PublicUrl>) -> String {
    match public_url {
        Some(url) => format!("<code>{}</code>", escape_html(url.as_str())),
        None => "<span class=\"warn\">未配置（无法生成领取链接）</span>".to_string(),
    }
}

fn logout_form(session: &ActiveSession) -> String {
    format!(
        "<form method=\"post\" action=\"/admin/logout\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{}\">\
         <button type=\"submit\">退出登录</button>\
         </form>",
        b64url_encode(&session.csrf)
    )
}

/// 生命周期状态（有效状态与 registry 存储状态不同时都显示，例如读时推导的 EXPIRED）。
fn state_html(entry: &VaultListEntry) -> String {
    let effective = entry.state.as_str();
    let stored = entry.stored_state.as_str();
    if effective == stored {
        format!("<code>{}</code>", escape_html(effective))
    } else {
        format!(
            "<code>{}</code> <span class=\"muted\">（registry: <code>{}</code>；读时推导）</span>",
            escape_html(effective),
            escape_html(stored)
        )
    }
}

/// 列表表格：id/状态/创建时间/数据面 URL/设备数/票据过期/删除进度。
fn vault_table_html(public_url: Option<&PublicUrl>, entries: &[VaultListEntry]) -> String {
    let mut html = String::new();
    html.push_str(
        "<table><thead><tr><th>vault id</th><th>状态</th><th>创建时间</th>\
         <th>数据面 URL</th><th>设备数</th><th>票据过期</th><th>删除进度</th></tr></thead><tbody>",
    );
    let url_html = public_url_html(public_url);
    for entry in entries {
        let id = hex_lower(&entry.vault_id);
        let created = escape_html(&format_utc(entry.created_at));
        let expires = match entry.expires_at {
            Some(at) => escape_html(&format_utc(at)),
            None => "—".to_string(),
        };
        let progress = if entry.state.is_delete_state() {
            format!(
                "<code>{}/{}</code>",
                entry.delete_progress,
                PURGE_ORDER.len()
            )
        } else {
            "—".to_string()
        };
        html.push_str(&format!(
            "<tr><td><a href=\"/admin/vaults/{id}\"><code>{id}</code></a></td>\
             <td>{state}</td><td>{created}</td><td>{url_html}</td>\
             <td><code>{devices}</code></td><td>{expires}</td><td>{progress}</td></tr>",
            state = state_html(entry),
            devices = entry.device_count,
        ));
    }
    html.push_str("</tbody></table>");
    html
}

/// 详情页表格行（label 与 value 都由调用方保证；label 一律转义）。
fn detail_row(label: &str, value_html: &str) -> String {
    format!(
        "<tr><th>{}</th><td>{}</td></tr>",
        escape_html(label),
        value_html
    )
}

fn detail_value(value: &str) -> String {
    format!("<code>{}</code>", escape_html(value))
}

fn detail_optional(value: Option<&str>) -> String {
    match value {
        Some(value) => detail_value(value),
        None => "—".to_string(),
    }
}

/// 详情页正文：生命周期 / 授权·运行 / 设备·票据 / 用量分开；不含票据 hash 或明文。
fn detail_page_body(web: &AdminWeb, detail: &VaultDetail, session: &ActiveSession) -> String {
    let id = hex_lower(&detail.entry.vault_id);
    let mut body = String::new();
    body.push_str(&nav_html());
    if detail.entry.state == LifecycleState::Active && !detail.entry.vault_exists {
        body.push_str(
            "<p class=\"warn\">数据异常：registry 是 <code>ACTIVE</code>，但缺少 <code>vaults</code> 行。\
             这是不变量违例，请按运维手册排查（不要手工改 registry）。</p>",
        );
    }
    body.push_str("<h2>生命周期</h2><table>");
    body.push_str(&detail_row("vault id", &detail_value(&id)));
    body.push_str(&detail_row("状态", &state_html(&detail.entry)));
    body.push_str(&detail_row(
        "创建时间",
        &detail_value(&format_utc(detail.entry.created_at)),
    ));
    let ended_label = match detail.entry.state {
        LifecycleState::Deleting | LifecycleState::DeleteFailed => "删除请求时间",
        LifecycleState::Deleted => "删除完成时间",
        _ => "结束时间",
    };
    body.push_str(&detail_row(
        ended_label,
        &match detail.deleted_at {
            Some(at) => detail_value(&format_utc(at)),
            None => "—".to_string(),
        },
    ));
    if detail.entry.state.is_delete_state() {
        body.push_str(&detail_row(
            "删除作业",
            &match detail.delete_job_id {
                Some(job) => detail_value(&hex_lower(&job)),
                None => "—".to_string(),
            },
        ));
        body.push_str(&detail_row(
            "删除进度",
            &format!(
                "<code>{}/{}</code> 张表（含 <code>vaults</code> 行）已清空",
                detail.delete_progress,
                PURGE_ORDER.len()
            ),
        ));
        body.push_str(&detail_row(
            "删除错误",
            &detail_optional(detail.delete_error.as_deref()),
        ));
    }
    body.push_str("</table>");

    body.push_str("<h2>授权 / 运行</h2><table>");
    body.push_str(&detail_row(
        "授权状态",
        &detail_optional(detail.authz_state.as_deref()),
    ));
    body.push_str(&detail_row(
        "运行状态",
        &detail_optional(detail.runtime_state.as_deref()),
    ));
    body.push_str(&detail_row(
        "legacy 默认 vault",
        if detail.legacy_default { "是" } else { "否" },
    ));
    body.push_str(&detail_row(
        "active epoch",
        &match detail.active_epoch {
            Some(epoch) => detail_value(&epoch.to_string()),
            None => "—".to_string(),
        },
    ));
    body.push_str(&detail_row(
        "control rev",
        &match detail.control_rev {
            Some(rev) => detail_value(&rev.to_string()),
            None => "—".to_string(),
        },
    ));
    body.push_str(&detail_row(
        "head seq",
        &match detail.head_seq {
            Some(seq) => detail_value(&seq.to_string()),
            None => "—".to_string(),
        },
    ));
    body.push_str("</table>");

    body.push_str("<h2>设备与票据</h2><table>");
    body.push_str(&detail_row(
        "设备数",
        &format!("<code>{}</code> 台", detail.entry.device_count),
    ));
    body.push_str(&detail_row(
        "票据总数",
        &format!("<code>{}</code>", detail.tickets_total),
    ));
    body.push_str(&detail_row(
        "已消费/已作废票据",
        &format!("<code>{}</code>", detail.tickets_consumed),
    ));
    body.push_str(&detail_row(
        "仍可领取票据",
        &format!("<code>{}</code>", detail.tickets_outstanding),
    ));
    body.push_str(&detail_row(
        "票据过期",
        &match detail.entry.expires_at {
            Some(at) => detail_value(&format_utc(at)),
            None => "—".to_string(),
        },
    ));
    body.push_str("</table>");

    body.push_str("<h2>用量</h2><table>");
    body.push_str(&detail_row(
        "普通配额已用（字节）",
        &match detail.usage_normal_bytes {
            Some(bytes) => detail_value(&bytes.to_string()),
            None => "—".to_string(),
        },
    ));
    body.push_str(&detail_row(
        "维护配额已用（字节）",
        &match detail.usage_maintenance_bytes {
            Some(bytes) => detail_value(&bytes.to_string()),
            None => "—".to_string(),
        },
    ));
    body.push_str("</table>");

    body.push_str(&format!(
        "<p>数据面 URL：{} <span class=\"muted\">（只读，来自配置 <code>public_url</code>）</span></p>",
        public_url_html(web.public_url.as_ref())
    ));

    match detail.entry.state {
        LifecycleState::Reserved => {
            body.push_str(
                "<p class=\"warn\">取消预留会立即作废该 vault 的全部未消费票据；\
                 该 vault id 不能复用（需要重新创建请生成新的领取链接）。\
                 取消与领取（claim）竞争只有一个赢：若已被领取，取消会返回 409。</p>",
            );
            body.push_str(&format!(
                "<form method=\"post\" action=\"/admin/vaults/{id}/cancel\">\
                 <input type=\"hidden\" name=\"csrf\" value=\"{}\">\
                 <button type=\"submit\">取消预留</button>\
                 </form>",
                b64url_encode(&session.csrf)
            ));
        }
        LifecycleState::Active => {
            body.push_str(&format!(
                "<p><a href=\"/admin/vaults/{id}/delete\">删除该 vault（不可逆，需输入确认）</a></p>"
            ));
        }
        LifecycleState::Deleting | LifecycleState::DeleteFailed => {
            body.push_str(&format!(
                "<p><a href=\"/admin/vaults/{id}/delete\">查看删除作业状态/进度</a></p>"
            ));
        }
        LifecycleState::Deleted => {
            body.push_str(
                "<p class=\"muted\">删除墓碑：租户数据与票据已清理，只保留最小管理元数据。\
                 没有恢复/回滚按钮；该 id 永久禁止复用。</p>",
            );
        }
        LifecycleState::Expired | LifecycleState::Cancelled => {
            body.push_str(
                "<p class=\"muted\">该预留已失效（票据已作废），没有可领取/删除的租户数据；\
                 该 id 不能复用。需要创建请生成新的领取链接。</p>",
            );
        }
    }
    body
}

fn vault_not_found_page() -> Response {
    page(
        StatusCode::NOT_FOUND,
        "vault 不存在",
        "<p>registry 里没有该 vault（含墓碑）：请核对 32 位十六进制 id。</p>\
         <p><a href=\"/admin\">返回 vault 列表</a></p>",
    )
}

fn reveal_gone_page() -> Response {
    page(
        StatusCode::NOT_FOUND,
        "一次性页面已失效",
        "<p class=\"warn\">该领取链接页面已失效：链接只在创建成功后的<strong>第一次</strong>打开时\
         显示一次，之后服务器立即删除（刷新/后退不会恢复）。</p>\
         <p>如果链接丢失：请取消该预留（<code>RESERVED</code>）后重新创建；\
         服务器不保存可重印的票据明文。领取链接不会出现在列表/详情页。</p>\
         <p><a href=\"/admin\">返回 vault 列表</a></p>",
    )
}

fn invalid_form_page() -> Response {
    page(
        StatusCode::BAD_REQUEST,
        "表单无效",
        "<p class=\"warn\">请求体必须是 <code>application/x-www-form-urlencoded</code>，\
         且不超过 4 KiB、不得有重复字段。</p>",
    )
}

fn csrf_failed_page(action: &str) -> Response {
    page(
        StatusCode::FORBIDDEN,
        "CSRF 校验失败",
        &format!(
            "<p class=\"warn\">{}</p>",
            escape_html(&format!(
                "{action}需要与管理页一致的会话 CSRF token：请重新打开页面后再提交。"
            ))
        ),
    )
}

fn csrf_ok(form: &HashMap<String, String>, session: &ActiveSession) -> bool {
    let expected = b64url_encode(&session.csrf);
    form.get("csrf")
        .map(|csrf| ct_eq(csrf.as_bytes(), expected.as_bytes()))
        .unwrap_or(false)
}

/// 非 `ACTIVE`/非 `RESERVED` 的状态冲突页（只显示状态，不泄露租户内容）。
fn state_conflict_page(web: &AdminWeb, vault_id: Id16, action: &str) -> Response {
    let state = web
        .db
        .read_instance(move |conn| registry::state_raw(conn, &vault_id))
        .ok()
        .flatten()
        .map(|state| state.as_str())
        .unwrap_or("UNKNOWN");
    page(
        StatusCode::CONFLICT,
        "当前状态不允许该操作",
        &format!(
            "<p class=\"warn\">{action}失败：该 vault 当前状态是 <code>{}</code>。</p>\
             <p>只有 <code>RESERVED</code> 能取消预留；只有 <code>ACTIVE</code> 能开始删除。\
             <code>ACTIVE</code> 表示领取（claim）已经赢了，不能再取消；\
             <code>DELETING</code>/<code>DELETE_FAILED</code>/<code>DELETED</code> 没有「重开」路径。</p>\
             <p><a href=\"/admin/vaults/{}\">查看详情</a> · <a href=\"/admin\">返回 vault 列表</a></p>",
            escape_html(state),
            hex_lower(&vault_id),
        ),
    )
}

fn delete_not_active_page(detail: &VaultDetail) -> Response {
    page(
        StatusCode::CONFLICT,
        "当前状态不允许删除",
        &format!(
            "<p class=\"warn\">该 vault 当前状态是 <code>{}</code>：只有 <code>ACTIVE</code> 能开始删除。\
             <code>RESERVED</code> 请先取消预留；<code>EXPIRED</code>/<code>CANCELLED</code> \
             没有可删除的租户数据；删除三态永不重开 <code>ACTIVE</code>。</p>\
             <p><a href=\"/admin/vaults/{}/delete\">刷新删除页</a> · \
             <a href=\"/admin/vaults/{}\">vault 详情</a> · <a href=\"/admin\">vault 列表</a></p>",
            escape_html(detail.entry.state.as_str()),
            hex_lower(&detail.entry.vault_id),
            hex_lower(&detail.entry.vault_id),
        ),
    )
}

/// `ACTIVE` 的删除确认页：必须输入精确 `DELETE <32hex id>`（大小写/空格严格）。
fn delete_confirm_page(detail: &VaultDetail, session: &ActiveSession) -> Response {
    let id = hex_lower(&detail.entry.vault_id);
    let expected = format!("DELETE {id}");
    let body = format!(
        "{nav}\
         <p class=\"warn\">不可逆删除：将持久标记 <code>DELETING</code>、立即关闭该 vault 的数据面入口，\
         然后分批清理全部租户表、票据与临时认证。没有恢复/回滚；<code>DELETED</code> 后只保留最小墓碑元数据。\
         删除是逻辑清除，不保证 WAL/备份/客户端副本被擦除。</p>\
         <p>确认方式：在下方输入框逐字输入 <code>{expected_html}</code>（大小写、空格都要一致；\
         输入会显示明文，不存在复制按钮）。</p>\
         <form method=\"post\" action=\"/admin/vaults/{id}/delete\">\
           <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
           <label>确认字符串\
             <input type=\"text\" name=\"confirm\" autocomplete=\"off\" spellcheck=\"false\" required>\
           </label>\
           <button type=\"submit\">不可逆删除</button>\
         </form>\
         <p><a href=\"/admin/vaults/{id}\">取消并返回详情</a> · <a href=\"/admin\">返回 vault 列表</a></p>",
        nav = nav_html(),
        expected_html = escape_html(&expected),
        csrf = b64url_encode(&session.csrf),
    );
    page(StatusCode::OK, "确认删除 vault", &body)
}

/// 删除三态（`DELETING`/`DELETE_FAILED`/`DELETED`）的作业/进度页；进行中自动刷新。
fn delete_state_page(detail: &VaultDetail, session: &ActiveSession) -> Response {
    let id = hex_lower(&detail.entry.vault_id);
    let state = detail.entry.state;
    let total = PURGE_ORDER.len();
    let refresh = (state == LifecycleState::Deleting && detail.delete_progress < total as u64)
        .then_some(DELETE_REFRESH_SECS);
    let job = match detail.delete_job_id {
        Some(job) => detail_value(&hex_lower(&job)),
        None => "—".to_string(),
    };
    let mut body = String::new();
    body.push_str(&nav_html());
    body.push_str("<table>");
    body.push_str(&detail_row("vault id", &format!("<code>{id}</code>")));
    body.push_str(&detail_row("状态", &detail_value(state.as_str())));
    body.push_str(&detail_row("job", &job));
    body.push_str(&detail_row(
        "进度",
        &format!(
            "<code>{}/{total}</code> 张表（含 <code>vaults</code> 行）已清空",
            detail.delete_progress
        ),
    ));
    let at_label = match state {
        LifecycleState::Deleted => "删除完成时间",
        _ => "删除请求时间",
    };
    body.push_str(&detail_row(
        at_label,
        &match detail.deleted_at {
            Some(at) => detail_value(&format_utc(at)),
            None => "—".to_string(),
        },
    ));
    body.push_str(&detail_row(
        "错误",
        &detail_optional(detail.delete_error.as_deref()),
    ));
    body.push_str("</table>");
    match state {
        LifecycleState::Deleting => {
            body.push_str(
                "<p class=\"warn\">删除作业进行中：<strong>本页不代表已完成</strong>。\
                 作业独立于浏览器继续执行，断开页面不会取消。页面无 JS，用下面的刷新链接\
                 （或浏览器刷新）查看最新进度。</p>",
            );
        }
        LifecycleState::DeleteFailed => {
            body.push_str(
                "<p class=\"warn\">删除失败：数据面入口保持关闭，<strong>不会</strong>重开 <code>ACTIVE</code>。\
                 可以重试同一个 job（沿用已持久化进度）；也可以用下面的表单重新确认删除。</p>",
            );
            body.push_str(&format!(
                "<form method=\"post\" action=\"/admin/vaults/{id}/delete\">\
                 <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
                 <label>确认字符串（精确）\
                   <input type=\"text\" name=\"confirm\" autocomplete=\"off\" spellcheck=\"false\" required>\
                 </label>\
                 <button type=\"submit\">重试同一删除 job</button>\
                 </form>",
                csrf = b64url_encode(&session.csrf),
            ));
        }
        LifecycleState::Deleted => {
            body.push_str(
                "<p class=\"muted\">已是删除墓碑：租户数据与票据零残留，registry 只保留最小元数据。\
                 该 id 永久禁止复用；没有恢复/回滚按钮。</p>",
            );
        }
        _ => {}
    }
    body.push_str(&format!(
        "<p><a href=\"/admin/vaults/{id}/delete\">刷新本页</a> · \
         <a href=\"/admin/vaults/{id}\">vault 详情</a> · <a href=\"/admin\">vault 列表</a></p>"
    ));
    page_with_refresh(StatusCode::OK, "删除作业进度", &body, refresh)
}

// ---------------------------------------------------------------- 响应辅助

fn render_page(title: &str, body: &str) -> String {
    render_document(title, body, None)
}

/// 带可选 `<meta http-equiv="refresh">` 的页面（无 JS 的进度刷新；CSP 不禁止 meta refresh）。
fn render_document(title: &str, body: &str, refresh_secs: Option<u32>) -> String {
    let title = escape_html(title);
    let mut document = String::with_capacity(body.len() + 1024);
    document.push_str(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"referrer\" content=\"no-referrer\">",
    );
    if let Some(secs) = refresh_secs {
        document.push_str(&format!("<meta http-equiv=\"refresh\" content=\"{secs}\">"));
    }
    document.push_str("<title>");
    document.push_str(&title);
    document.push_str("</title>");
    document.push_str(PAGE_STYLE);
    document.push_str("</head><body><h1>");
    document.push_str(&title);
    document.push_str("</h1>");
    document.push_str(body);
    document.push_str("</body></html>");
    document
}

const PAGE_STYLE: &str = "<style>\
:root{color-scheme:light dark}\
body{font:15px/1.6 system-ui,-apple-system,\"Segoe UI\",sans-serif;max-width:46rem;margin:3rem auto;padding:0 1rem}\
h1{font-size:1.35rem}\
code{background:rgba(127,127,127,.15);padding:.1rem .3rem;border-radius:.25rem}\
.warn{border-left:3px solid #b58900;padding:.5rem .75rem;background:rgba(181,137,0,.08)}\
.muted{opacity:.75;font-size:.9em}\
form{margin:1.25rem 0;display:grid;gap:.65rem;max-width:26rem}\
label{display:grid;gap:.35rem}\
input[type=password]{font:inherit;padding:.45rem .6rem}\
button{font:inherit;padding:.45rem .9rem;cursor:pointer;justify-self:start}\
</style>";

fn html_response(status: StatusCode, document: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        document,
    )
        .into_response()
}

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    html_response(status, render_page(title, body))
}

fn redirect(location: &str, cookie: Option<String>) -> Response {
    let mut response = (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response();
    if let Some(cookie) = cookie {
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

fn unauthorized_page() -> Response {
    page(
        StatusCode::UNAUTHORIZED,
        "登录失败",
        "<p class=\"warn\">admin token 无效（或未初始化）。请核对 <code>init</code> 只显示一次的输出；\
         连续失败会被限流。</p><p><a href=\"/admin/login\">重新登录</a></p>",
    )
}

fn rate_limited_page() -> Response {
    page(
        StatusCode::TOO_MANY_REQUESTS,
        "登录被限流",
        "<p class=\"warn\">尝试次数过多：请等待限流窗口结束后再试。\
         限流按实例与来源 IP 统计，失败次数越多等待越久。</p>",
    )
}

fn internal_error_page() -> Response {
    page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "内部错误",
        "<p class=\"warn\">管理面遇到内部错误（详情只写进 relay 进程 stderr，绝不包含 token/cookie）。</p>",
    )
}

const PLAIN_HTTP_WARNING: &str = "<p class=\"warn\">明文 HTTP 警告：admin token 与会话 Cookie \
     在不可信网络上可被截获；不要把 admin 端口映射到公网。领取链接含票据明文，只私发给\
     目标设备。</p>";

/// HTML 转义（所有动态文本都经这里进页面；静态模板不含用户输入）。
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Unix 秒 → `YYYY-MM-DD HH:MM:SS UTC`（页面只读时间展示；不引 chrono 依赖）。
fn format_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant 的 `civil_from_days`（对负数天也用 euclidean 除法，1970 前同样正确）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

fn page_with_refresh(
    status: StatusCode,
    title: &str,
    body: &str,
    refresh_secs: Option<u32>,
) -> Response {
    html_response(status, render_document(title, body, refresh_secs))
}

/// 走 S2 的预留事务（`write_instance_blocking` 不能在 async handler 线程上跑，放到
/// blocking pool；事务语义/幂等完全沿用 registry，不在这里手写 SQL）。
async fn create_reservation(web: &AdminWeb) -> Result<admin::IssuedCreateTicket> {
    let db = web.db.clone();
    tokio::task::spawn_blocking(move || {
        admin::create_reservation(&db, Duration::from_secs(CREATE_RESERVATION_TTL_SECS))
    })
    .await
    .map_err(|_| Error::Invariant("admin create reservation task"))?
}

/// 走 S2 的取消事务（同上：blocking pool，语义/竞争口径沿用 registry）。
async fn cancel_reservation(web: &AdminWeb, vault_id: Id16) -> Result<CancelOutcome> {
    let db = web.db.clone();
    tokio::task::spawn_blocking(move || registry::cancel_reservation(&db, vault_id))
        .await
        .map_err(|_| Error::Invariant("admin cancel reservation task"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_escape_and_utc_formatting_are_deterministic() {
        assert_eq!(
            escape_html("<script>alert('&\"')</script>"),
            "&lt;script&gt;alert(&#39;&amp;&quot;&#39;)&lt;/script&gt;"
        );
        assert_eq!(escape_html("12008;清理"), "12008;清理");
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(1_800_000_000), "2027-01-15 08:00:00 UTC");
        // 负数（1970 前）也不能 panic/回绕。
        assert_eq!(format_utc(-1), "1969-12-31 23:59:59 UTC");
    }

    #[test]
    fn claim_reveals_are_session_bound_single_use_and_expire() {
        let reveals = ClaimReveals::new();
        let session = [1u8; 32];
        let other = [2u8; 32];
        let id = [3u8; 32];
        reveals.insert(
            id,
            100,
            RevealedClaim {
                session,
                vault_id: [9u8; 16],
                link: "mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1#ticket=x".to_string(),
                created_at: 100,
                expires_at: 1_000,
            },
        );
        assert_eq!(reveals.len(), 1);
        // 其它会话不能取走（不消耗）。
        assert!(reveals.take(&id, &other, 100).is_none());
        assert_eq!(reveals.len(), 1);
        // 本会话取走一次后即删除。
        let claim = reveals.take(&id, &session, 100).expect("claim");
        assert_eq!(claim.vault_id, [9u8; 16]);
        assert!(reveals.take(&id, &session, 100).is_none());
        assert_eq!(reveals.len(), 0);

        // TTL 过后失效（不再返回链接）。
        reveals.insert(
            id,
            200,
            RevealedClaim {
                session,
                vault_id: [9u8; 16],
                link: "mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1#ticket=x".to_string(),
                created_at: 200,
                expires_at: 1_100,
            },
        );
        assert!(reveals
            .take(&id, &session, 200 + CLAIM_REVEAL_TTL_SECS)
            .is_none());
        assert_eq!(reveals.len(), 0);
    }

    #[test]
    fn token_hash_matches_sha256_of_text_and_fingerprint_differs() {
        let token = "admin-token-for-test";
        assert_eq!(token_hash(token), sha256(token.as_bytes()));
        let a = token_fingerprint(Some(&token_hash(token)));
        let b = token_fingerprint(Some(&token_hash("other")));
        assert_ne!(a, b);
        assert_ne!(a, token_fingerprint(None));
    }

    #[test]
    fn host_policy_is_exact_and_includes_loopback_names() {
        let policy = AdminHostPolicy::for_listen(
            "127.0.0.1:18081".parse().expect("addr"),
            &["admin.example.com".to_string()],
        );
        assert!(policy.allows_host("127.0.0.1:18081"));
        assert!(policy.allows_host("LOCALHOST:18081"));
        assert!(policy.allows_host("[::1]:18081"));
        assert!(policy.allows_host("admin.example.com"));
        // 端口必须一致；不做通配/前缀。
        assert!(!policy.allows_host("127.0.0.1:18082"));
        assert!(!policy.allows_host("127.0.0.1"));
        assert!(!policy.allows_host("evil.example.com"));
        assert!(!policy.allows_host("127.0.0.1:18081.evil.com"));
    }

    #[test]
    fn unspecified_listen_requires_configured_host() {
        let policy = AdminHostPolicy::for_listen(
            "0.0.0.0:18081".parse().expect("addr"),
            &["192.168.1.10:18081".to_string()],
        );
        assert!(policy.allows_host("127.0.0.1:18081"));
        assert!(policy.allows_host("192.168.1.10:18081"));
        assert!(!policy.allows_host("10.0.0.5:18081"));
    }

    #[test]
    fn origin_parts_accepts_strict_http_authorities_only() {
        assert_eq!(
            origin_parts("http://127.0.0.1:18081"),
            Some(("http".to_string(), "127.0.0.1:18081".to_string()))
        );
        assert_eq!(
            origin_parts("https://Admin.Example.com/admin"),
            Some(("https".to_string(), "admin.example.com".to_string()))
        );
        assert!(origin_parts("null").is_none());
        assert!(origin_parts("file:///etc/passwd").is_none());
        assert!(origin_parts("http://user@evil.example").is_none());
        assert!(origin_parts("javascript:alert(1)").is_none());
        assert!(origin_parts("http://").is_none());
        assert!(origin_parts("http://a\\b").is_none());
    }

    #[test]
    fn form_parsing_handles_encoding_and_rejects_ambiguity() {
        let form = parse_form(b"csrf=abc%2D123&token=a+b%26c").expect("form");
        assert_eq!(form.get("csrf").map(String::as_str), Some("abc-123"));
        assert_eq!(form.get("token").map(String::as_str), Some("a b&c"));

        assert!(parse_form(b"a=1&a=2").is_err(), "重复键必须拒绝");
        assert!(parse_form(b"a=%zz").is_err());
        assert!(parse_form(b"a=%2").is_err());
        assert!(parse_form(b"\xff=1").is_err());
    }

    #[test]
    fn sessions_expire_by_idle_and_absolute_without_sleeping() {
        let sessions = AdminSessions::new();
        let fingerprint = token_fingerprint(Some(&[7u8; 32]));
        let session = sessions.create(1_000, fingerprint).expect("create");
        assert_eq!(sessions.len(), 1);

        // 闲置边界前仍有效，且访问会续期闲置计时。
        let touched = 1_000 + SESSION_IDLE_SECS - 1;
        assert!(sessions
            .validate(&session.id, touched, fingerprint)
            .is_some());
        // 续期后再过 SESSION_IDLE_SECS 才过期。
        assert!(sessions
            .validate(&session.id, touched + SESSION_IDLE_SECS, fingerprint)
            .is_none());
        assert_eq!(sessions.len(), 0);

        // 绝对过期：即使每 20 分钟活跃一次，8 小时后也失效。
        let session = sessions.create(2_000, fingerprint).expect("create");
        let mut now = 2_000;
        let mut alive = true;
        for _ in 0..24 {
            now += 20 * 60;
            alive = sessions.validate(&session.id, now, fingerprint).is_some();
            if !alive {
                break;
            }
        }
        assert!(!alive, "绝对过期必须生效（活动不能续命）");
    }

    #[test]
    fn active_sessions_are_bounded() {
        let sessions = AdminSessions::new();
        let fingerprint = token_fingerprint(Some(&[3u8; 32]));
        let mut ids = Vec::new();
        for index in 0..MAX_ACTIVE_SESSIONS {
            let session = sessions
                .create(100 + index as i64, fingerprint)
                .expect("create");
            ids.push(session.id);
        }
        assert_eq!(sessions.len(), MAX_ACTIVE_SESSIONS);
        // 超上限时淘汰最久未用（这里是访问时刻最早的那个）。
        let newest = sessions.create(1_000, fingerprint).expect("create");
        assert_eq!(sessions.len(), MAX_ACTIVE_SESSIONS);
        assert!(sessions.validate(&ids[0], 1_000, fingerprint).is_none());
        assert!(sessions.validate(&newest.id, 1_000, fingerprint).is_some());
    }

    #[test]
    fn sessions_are_revoked_when_token_fingerprint_changes() {
        let sessions = AdminSessions::new();
        let fingerprint = token_fingerprint(Some(&[1u8; 32]));
        let session = sessions.create(100, fingerprint).expect("create");
        assert!(sessions.validate(&session.id, 100, fingerprint).is_some());
        assert!(sessions
            .validate(&session.id, 100, token_fingerprint(Some(&[2u8; 32])))
            .is_none());
        assert_eq!(sessions.len(), 0, "换 token 必须清空全部会话");
    }

    #[test]
    fn login_csrf_tokens_are_single_use_and_expire() {
        let csrf = LoginCsrfStore::new();
        let token = csrf.issue(500).expect("issue");
        assert!(csrf.consume(&token, 500));
        assert!(!csrf.consume(&token, 500), "一次性");
        let expiring = csrf.issue(1_000).expect("issue");
        assert!(!csrf.consume(&expiring, 1_000 + LOGIN_CSRF_TTL_SECS));
        assert!(!csrf.consume("not-a-token", 1_000));
    }

    #[test]
    fn login_limiter_blocks_after_repeated_failures() {
        let limiter = LoginLimiter::new();
        let ip = Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        for _ in 0..FAILED_LOGINS_PER_IP - 1 {
            assert!(limiter.check_attempt());
            assert!(!limiter.is_blocked(ip, 0));
            limiter.record_failure(ip, 0);
        }
        limiter.record_failure(ip, 0);
        assert!(limiter.is_blocked(ip, 0), "达到失败预算后下一次必须 429");
        // 窗口过期后恢复。
        assert!(!limiter.is_blocked(ip, FAILED_LOGIN_WINDOW_SECS));
    }

    #[test]
    fn cookie_helpers_are_host_only_and_secure_optional() {
        let id = [9u8; 32];
        let cookie = session_cookie(&id, false);
        assert!(cookie.starts_with(&format!("{SESSION_COOKIE}=")));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(!cookie.contains("Secure"));
        assert!(!cookie.contains("Domain="), "必须 host-only");
        assert!(session_cookie(&id, true).contains("; Secure"));
        assert!(clear_session_cookie(true).contains("Max-Age=0"));
        assert!(clear_session_cookie(true).contains("Secure"));
    }
}
