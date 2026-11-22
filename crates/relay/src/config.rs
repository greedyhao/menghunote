//! relay 的运行配置。

use std::path::PathBuf;

use crate::db::DbConfig;
use crate::ids::Hash32;

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
