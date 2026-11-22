//! relay 的运行配置。

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::db::DbConfig;
use crate::ids::Hash32;

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

/// relay 进程配置（单实例单 vault；配额/密钥由部署时配置，ADR 0006 §7）。
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// 数据库参数（路径 + 预算）。
    pub db: DbConfig,
    /// 一次性 setup secret（首台设备初始化用；哈希比较，成功后关闭该端点）。
    ///
    /// 生产由服务器控制台/权限受限文件交给本人（ADR 0006 §4）；`None` 表示禁止初始化。
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

    /// SNAPSHOT 对象明文上限。
    pub fn with_max_snapshot_bytes(mut self, bytes: u64) -> Self {
        self.max_snapshot_bytes = bytes;
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
}
