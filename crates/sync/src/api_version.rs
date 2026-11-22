//! ADR 0014 **S3a**：relay API 面（`/v1` / `/v2`）与客户端路径的**唯一**生成点。
//!
//! ## 为什么需要它
//!
//! S3a 之前客户端所有业务路由写死 `/v1/vaults/{id}/...`，`/v2` 只是服务端的镜像
//! （广告 S2c）。多 vault 票创建要求客户端能**显式选择一个面**，并且：
//!
//! - 同一批路径既用于 HTTP 请求，也用于**管理签名**（`management_request_cde` 的
//!   `path` 字段）。两者必须是逐字节相同的字符串，否则 relay 验签失败；
//! - v1 的鉴权是**全局** `/v1/auth/{challenges,tokens}`，v2 是**租户内**
//!   `/v2/vaults/{id}/auth/{challenges,tokens}`——不能简单把 `/v1` 前缀替换成 `/v2`；
//! - v1 与 v2 的路径**不能**混用：一个 vault 的控制记录签名面在创建时固定
//!   （见 [`RelayApiVersion`] 文档），跨面读取会验签失败（fail-closed）。
//!
//! [`RelayApi`] 把"origin + 面"收在一处，[`RelayApi::vault_path`] 等方法是**唯一**的
//! 业务路径拼接入口；`HttpRelayClient` 与各 HTTP helper 都只从它取路径。
//!
//! ## 非目标
//!
//! 本模块不做网络、不存 secret；`RelayApiVersion` 是**公开配置**（可进 JSON），
//! 不含任何凭据。

use crate::base64url;
use crate::{DeviceId, ObjectId, VaultId};

/// relay API 面（ADR 0014 §A：「每 vault 自愿、单向启用 v2」）。
///
/// ## vault 绑定（重要）
///
/// 客户端把 `api_version` 当作**该 vault 的固定签名面**：v2 票据创建的 vault 用
/// `/v2/...` 签管理记录；v1 创建的 vault 用 `/v1/...`。relay 的 `/v1` 与 `/v2`
/// 路由对同一租户都可用（token 按 vault scope 授权），但历史控制记录的签名里
/// **没有** path 字段，客户端只能按配置的面重建 path 验签。因此：
///
/// - 单个 vault 不允许"先 v1 后切 v2"（否则旧记录无法验签）；
/// - 指向错误面的 profile 会以 `RelayWire`/签名失败停机，而不是静默换面。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RelayApiVersion {
    /// `/v1/vaults/{id}/...` + 全局 `/v1/auth/...`（既有 wire，逐字节保留）。
    V1,
    /// `/v2/vaults/{id}/...` + 租户内 `/v2/vaults/{id}/auth/...`（ADR 0014 S2b/S2c）。
    V2,
}

impl RelayApiVersion {
    /// 配置/URL 里的字面值（`v1` / `v2`）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::V2 => "v2",
        }
    }

    /// URL 前缀（`/v1` / `/v2`）。
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::V1 => "/v1",
            Self::V2 => "/v2",
        }
    }

    /// 公开配置里的数字版本（旧配置缺省 = `1`）。
    #[must_use]
    pub const fn as_number(self) -> u32 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
        }
    }

    /// 从配置数字恢复；未知数字返回 `None`（调用方必须显式拒绝，不默认回退）。
    #[must_use]
    pub const fn from_number(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::V1),
            2 => Some(Self::V2),
            _ => None,
        }
    }

    /// 从字面值 `v1`/`v2`（也接受 `1`/`2`）恢复；未知值返回 `None`。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "v1" | "1" => Some(Self::V1),
            "v2" | "2" => Some(Self::V2),
            _ => None,
        }
    }

    /// durable journal 里的稳定编码（只用于 `sync.init_pending`，不是 wire）。
    #[must_use]
    pub(crate) const fn as_code(self) -> u64 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
        }
    }

    /// durable journal 解码；未知编码返回 `None`（fail-closed）。
    #[must_use]
    pub(crate) const fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::V1),
            2 => Some(Self::V2),
            _ => None,
        }
    }
}

impl Default for RelayApiVersion {
    /// 缺省恒为 v1：旧配置/旧调用点不需要任何迁移（ADR 0014 §版本：v2 只显式选择）。
    fn default() -> Self {
        Self::V1
    }
}

impl std::fmt::Display for RelayApiVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// relay origin + API 面的路径生成器（客户端业务路径的唯一入口）。
///
/// 约定：返回的路径是**绝对路径**（以 `/` 开头，含 query），可直接交给
/// [`crate::http_transport::HttpTransport::send`]，也可逐字节用于管理签名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayApi {
    /// canonical origin（`scheme://host[:port]`，不含路径）。
    origin: String,
    version: RelayApiVersion,
}

impl RelayApi {
    /// 显式构造（origin 必须已按唯一 parser canonical 化）。
    #[must_use]
    pub fn new(origin: impl Into<String>, version: RelayApiVersion) -> Self {
        Self {
            origin: origin.into(),
            version,
        }
    }

    /// API 面。
    #[must_use]
    pub const fn version(&self) -> RelayApiVersion {
        self.version
    }

    /// canonical origin（**不含**路径；管理 challenge 的 `origin_sha256` 用它）。
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// `POST {prefix}/vaults`（v1 setup secret / v2 创建票据）。
    #[must_use]
    pub fn create_vault_path(&self) -> String {
        format!("{}/vaults", self.version.prefix())
    }

    /// 租户业务路径：`{prefix}/vaults/{b64(vault_id)}{suffix}`（suffix 以 `/` 开头）。
    #[must_use]
    pub fn vault_path(&self, vault_id: &VaultId, suffix: &str) -> String {
        format!(
            "{}/vaults/{}{suffix}",
            self.version.prefix(),
            base64url::encode(vault_id)
        )
    }

    /// 设备对象路径（`objects/{b64(object_id)}/{tail}`）。
    #[must_use]
    pub fn object_path(&self, vault_id: &VaultId, object_id: &ObjectId, tail: &str) -> String {
        format!(
            "{}/objects/{}{tail}",
            self.vault_path(vault_id, ""),
            base64url::encode(object_id)
        )
    }

    /// 鉴权路径：v1 是全局 `/v1/auth/{action}`，v2 是租户内
    /// `/v2/vaults/{id}/auth/{action}`（ADR 0014 S2b）。
    #[must_use]
    pub fn auth_path(&self, vault_id: &VaultId, action: &str) -> String {
        match self.version {
            RelayApiVersion::V1 => format!("/v1/auth/{action}"),
            RelayApiVersion::V2 => format!("{}/auth/{action}", self.vault_path(vault_id, "")),
        }
    }

    /// 管理端吊销路径（v1/v2 同形，只是前缀不同）。
    #[must_use]
    pub fn revoke_device_path(&self, vault_id: &VaultId, target: &DeviceId) -> String {
        format!(
            "{}/devices/{}/revoke",
            self.vault_path(vault_id, ""),
            base64url::encode(target)
        )
    }

    /// 绝对 URL（`{origin}{path}`）；只用于诊断/日志脱敏（请求本身带相对 path）。
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAULT: [u8; 16] = [0x5a; 16];
    const DEVICE: [u8; 16] = [0x04; 16];
    const OBJECT: [u8; 32] = [0x04; 32];

    #[test]
    fn version_literals_and_numbers_are_strict() {
        assert_eq!(RelayApiVersion::V1.as_str(), "v1");
        assert_eq!(RelayApiVersion::V2.as_str(), "v2");
        assert_eq!(RelayApiVersion::V1.prefix(), "/v1");
        assert_eq!(RelayApiVersion::V2.prefix(), "/v2");
        assert_eq!(RelayApiVersion::default(), RelayApiVersion::V1);
        assert_eq!(RelayApiVersion::parse("v1"), Some(RelayApiVersion::V1));
        assert_eq!(RelayApiVersion::parse("V2"), Some(RelayApiVersion::V2));
        assert_eq!(RelayApiVersion::parse("2"), Some(RelayApiVersion::V2));
        for bad in ["", "v3", "one", "local_dev"] {
            assert_eq!(RelayApiVersion::parse(bad), None, "{bad:?} 不能默认接受");
        }
        assert_eq!(RelayApiVersion::from_number(1), Some(RelayApiVersion::V1));
        assert_eq!(RelayApiVersion::from_number(2), Some(RelayApiVersion::V2));
        assert_eq!(RelayApiVersion::from_number(3), None);
    }

    #[test]
    fn v1_paths_are_byte_identical_to_the_frozen_wire() {
        let api = RelayApi::new("http://127.0.0.1:18080", RelayApiVersion::V1);
        assert_eq!(api.create_vault_path(), "/v1/vaults");
        assert_eq!(
            api.vault_path(&VAULT, "/state"),
            "/v1/vaults/WlpaWlpaWlpaWlpaWlpaWg/state"
        );
        assert_eq!(api.auth_path(&VAULT, "challenges"), "/v1/auth/challenges");
        assert_eq!(
            api.object_path(&VAULT, &OBJECT, "/parts/3"),
            "/v1/vaults/WlpaWlpaWlpaWlpaWlpaWg/objects/BAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ/parts/3"
        );
        assert_eq!(
            api.revoke_device_path(&VAULT, &DEVICE),
            "/v1/vaults/WlpaWlpaWlpaWlpaWlpaWg/devices/BAQEBAQEBAQEBAQEBAQEBA/revoke"
        );
    }

    #[test]
    fn v2_paths_are_tenant_scoped_including_auth() {
        let api = RelayApi::new("https://relay.example", RelayApiVersion::V2);
        assert_eq!(api.create_vault_path(), "/v2/vaults");
        assert_eq!(
            api.vault_path(&VAULT, "/controls"),
            "/v2/vaults/WlpaWlpaWlpaWlpaWlpaWg/controls"
        );
        assert_eq!(
            api.auth_path(&VAULT, "tokens"),
            "/v2/vaults/WlpaWlpaWlpaWlpaWlpaWg/auth/tokens"
        );
        assert_eq!(
            api.object_path(&VAULT, &OBJECT, ""),
            "/v2/vaults/WlpaWlpaWlpaWlpaWlpaWg/objects/BAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ"
        );
        assert_eq!(api.url("/v2/vaults"), "https://relay.example/v2/vaults");
        // origin 原样保留（管理 challenge 的 origin_sha256 以它为输入）。
        assert_eq!(api.origin(), "https://relay.example");
    }
}
