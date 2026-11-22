//! relay 的运行配置。

use std::path::PathBuf;

use crate::db::DbConfig;

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
}
