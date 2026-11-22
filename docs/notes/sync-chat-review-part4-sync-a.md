# 同步评审 Part 4/6：sync 源码 A

@@PART 4/6

> 这是 **Part 4/6**。请先只确认收到，不要开始评审。
> 本部分约 498067 字符；应低于平台约 72 万字符的截断点。
> 内容与 Part 3 连续；全部到齐后再集中评审。代码截止 `1bededa`。

## 收到确认（请只回复这三行）

1. `PART 4/6 RECEIVED`
2. 本部分 `@@SECTION` 数量（应为 17）
3. 最后一个完整文件（若可见）

---

# 本部分附录

@@SECTION code/014 `crates/sync/src/api_version.rs`


`````rust
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

`````



@@SECTION code/015 `crates/sync/src/asset_sync.rs`


`````rust
//! 普通同步的**增量资产上传**所需的本地 durable 事实与探测辅助。
//!
//! 背景（attachment-insert 缺口）：ASSET 以前只随"发布基线/轮换"上传，普通
//! `SyncOnce` 不会上传新资产，B 只拿到 `asset://` 链接而没有字节。现在普通同步在
//! push 之后扫描**被正文引用**的本地资产，按对象增量上传；本模块提供：
//!
//! - `meta` 表里的 durable "已上传/已验证"标记（不新增 schema / 不改 wire）：
//!   键 `sync.asset_uploaded.<hex object_id>`，值绑定 object_id / epoch / 分片数 /
//!   明文总长 / 密文摘要。写入时机是 relay 已 `Accepted`（或幂等 `Duplicate`）之后，
//!   因此它表示"本机确实把这份确定性密文交给 relay 并完成了对象提交"。
//! - relay 探测：分片不存在（NOT_FOUND）与 reservation 冲突（ID_CONFLICT）的语义判定。
//!   同字节跨 reservation 的重放已由 relay 幂等接受（task-121）；`ID_CONFLICT` 只剩
//!   "对象只写了一半、缺失片属于别的预算来源"这类失败恢复与旧 relay 兼容路径。
//!
//! **为什么用 `meta` 而不是 `assets` 加列**：标记是同步侧的本机事实，存进 `meta`
//! 不需要提升 `PRAGMA user_version`；v0.4 旧库可直接打开，旧程序也只会忽略这些键
//! （回滚安全）。`assets` 表保持纯内容寻址语义。
//!
//! **安全边界**：标记只影响"是否重复上传"，不参与内容认证；资产字节仍必须由
//! [`crate::upload::fetch_verified_asset_data`] 的完整校验后才能落盘（B 拉取路径）。
//! 标记损坏/版本不识别时按"不存在"处理（重新探测 relay），不静默标记成功。

use crate::error::SyncError;
use crate::relay::RelayClient;
use crate::upload::UploadedObject;
use crate::{ObjectId, VaultId};
use menghunote_storage::Vault;

/// 已上传资产标记的 `meta` 键前缀。
pub(crate) const ASSET_UPLOADED_META_PREFIX: &str = "sync.asset_uploaded.";

/// 标记值格式版本（独立修订号：未来改变布局时旧值必须按"不存在"处理）。
const ASSET_UPLOADED_MAGIC: &[u8] = b"MN/asset-uploaded/1\0";

/// 一条 durable 的"资产对象已上传"事实。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AssetUploadedFact {
    /// 对象 id（根派生的确定性 HMAC 名）。
    pub(crate) object_id: ObjectId,
    /// 分片数。
    pub(crate) part_count: u32,
    /// 明文总长（含 42B canonical 头 + MIME + 数据）。
    pub(crate) total_plain_len: u64,
    /// 密文对象摘要（与 `ObjectCommit.object_digest` 相同）。
    pub(crate) object_digest: [u8; 32],
    /// 上传时使用的根 epoch。
    pub(crate) key_epoch: u64,
}

impl AssetUploadedFact {
    /// 复用的公开事实集（checkpoint refs / 报告用）。
    pub(crate) fn uploaded_object(&self) -> UploadedObject {
        UploadedObject {
            object_id: self.object_id,
            part_count: self.part_count,
            total_plain_len: self.total_plain_len,
            object_digest: self.object_digest,
        }
    }
}

/// 标记的 `meta` 键（object_id 的 64 位小写十六进制）。
pub(crate) fn meta_key(object_id: &ObjectId) -> String {
    format!(
        "{ASSET_UPLOADED_META_PREFIX}{}",
        crate::ids::hex_of(object_id)
    )
}

/// 读一条标记；不存在、版本不识别、字段不自洽、object_id 不符都返回 `None`。
///
/// 宽进（按不存在处理）而不是 `Err`：标记只是幂等优化，relay 仍是唯一真源；
/// 损坏的本地标记必须允许重新探测/重传，而不是让整轮同步失败。
pub(crate) fn read_uploaded(
    vault: &Vault,
    object_id: &ObjectId,
) -> Result<Option<AssetUploadedFact>, SyncError> {
    let Some(bytes) = vault.get_meta(&meta_key(object_id))? else {
        return Ok(None);
    };
    Ok(decode_uploaded(&bytes).filter(|fact| fact.object_id == *object_id))
}

/// 写入 durable 标记（覆盖写，幂等）。
pub(crate) fn mark_uploaded(vault: &Vault, fact: &AssetUploadedFact) -> Result<(), SyncError> {
    vault.set_meta(&meta_key(&fact.object_id), &encode_uploaded(fact))?;
    Ok(())
}

fn encode_uploaded(fact: &AssetUploadedFact) -> Vec<u8> {
    let mut out = Vec::with_capacity(ASSET_UPLOADED_MAGIC.len() + 104);
    out.extend_from_slice(ASSET_UPLOADED_MAGIC);
    out.extend_from_slice(&fact.object_id);
    out.extend_from_slice(&fact.key_epoch.to_be_bytes());
    out.extend_from_slice(&fact.part_count.to_be_bytes());
    out.extend_from_slice(&fact.total_plain_len.to_be_bytes());
    out.extend_from_slice(&fact.object_digest);
    out
}

fn decode_uploaded(bytes: &[u8]) -> Option<AssetUploadedFact> {
    let body = bytes.strip_prefix(ASSET_UPLOADED_MAGIC)?;
    if body.len() != 32 + 8 + 4 + 8 + 32 {
        return None;
    }
    let object_id: ObjectId = body.get(0..32)?.try_into().ok()?;
    let key_epoch = u64::from_be_bytes(body.get(32..40)?.try_into().ok()?);
    let part_count = u32::from_be_bytes(body.get(40..44)?.try_into().ok()?);
    let total_plain_len = u64::from_be_bytes(body.get(44..52)?.try_into().ok()?);
    let object_digest: [u8; 32] = body.get(52..84)?.try_into().ok()?;
    if part_count == 0 || total_plain_len == 0 {
        return None;
    }
    Some(AssetUploadedFact {
        object_id,
        part_count,
        total_plain_len,
        object_digest,
        key_epoch,
    })
}

/// relay 是否明确回答"该分片/对象不存在"。
pub(crate) fn is_not_found(error: &SyncError) -> bool {
    matches!(error, SyncError::RelayApi { code, .. } if code == "NOT_FOUND")
}

/// relay 是否用 `ID_CONFLICT` 拒绝（补缺失分片时上下文/预算来源不符）。
///
/// 同字节重放不再走这里：relay 对逐字节相同的分片与 reservation 无关（task-121）。
pub(crate) fn is_id_conflict(error: &SyncError) -> bool {
    matches!(error, SyncError::RelayApi { code, .. } if code == "ID_CONFLICT")
}

/// 读一片已存在的 relay 分片：`NOT_FOUND` → `Ok(None)`，其它错误原样上抛。
pub(crate) fn read_part<C: RelayClient>(
    client: &mut C,
    object_id: &ObjectId,
    part_no: u32,
) -> Result<Option<Vec<u8>>, SyncError> {
    match client.part(object_id, part_no) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if is_not_found(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// 普通同步上传 ASSET 时使用的对象上下文（由根/明文头确定性派生）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct LocalAssetObject {
    /// 对象 id（`HMAC(K_asset_id, canonical 资产明文头)`）。
    pub(crate) object_id: ObjectId,
    /// 确定性 message_id（ADR §19）。
    pub(crate) message_id: [u8; 16],
    /// 本 epoch。
    pub(crate) key_epoch: u64,
}

/// 由根与资产元数据派生上传上下文（不需要读资产字节）。
pub(crate) fn local_asset_object(
    root: &crate::keys::RootSecretV1,
    real_sha256: &[u8; 32],
    byte_len: u64,
    media_type: &str,
) -> Result<LocalAssetObject, SyncError> {
    let object_id = crate::checkpoint::asset_object_id(root, real_sha256, byte_len, media_type)?;
    let message_id = crate::upload::asset_message_id(root, &object_id)?;
    Ok(LocalAssetObject {
        object_id,
        message_id,
        key_epoch: root.key_epoch,
    })
}

/// 资产明文总长（canonical 头 42B + MIME + 数据；与 `checkpoint::asset_plaintext` 同口径）。
pub(crate) fn asset_plain_len(byte_len: u64, media_type: &str) -> Result<u64, SyncError> {
    if media_type.is_empty() || media_type.len() > 255 {
        return Err(SyncError::Object("资产 MIME 长度非法"));
    }
    let mime = u64::try_from(media_type.len())
        .map_err(|_| SyncError::LocalCorrupt("资产 MIME 长度超出 u64"))?;
    byte_len
        .checked_add(42)
        .and_then(|total| total.checked_add(mime))
        .ok_or(SyncError::LocalCorrupt("资产明文长度溢出"))
}

/// 由资产元数据构造"完整声明"的 `ObjectCommit` 事实（用于报告/复用）。
pub(crate) fn uploaded_fact_from_object(
    object: &LocalAssetObject,
    part_count: u32,
    total_plain_len: u64,
    object_digest: [u8; 32],
) -> AssetUploadedFact {
    AssetUploadedFact {
        object_id: object.object_id,
        part_count,
        total_plain_len,
        object_digest,
        key_epoch: object.key_epoch,
    }
}

/// 校验 `vault_id` 与根一致（ASSET 加密/AAD 都绑定 vault）。
pub(crate) fn ensure_vault(
    root: &crate::keys::RootSecretV1,
    vault_id: VaultId,
) -> Result<(), SyncError> {
    if root.vault_id != vault_id {
        return Err(SyncError::LocalCorrupt("资产上传的 vault 与根不一致"));
    }
    Ok(())
}

`````



@@SECTION code/016 `crates/sync/src/base64url.rs`


`````rust
//! base64url（无 padding）编解码。
//!
//! relay 的 HTTP JSON 把所有 ID/hash/token 都写成 `base64url` 字符串（见
//! `crates/relay/src/api.rs` 的 `b64url_encode`）。本 crate 不引入额外依赖，
//! 自实现一个小而严格的编解码器（只接受 `A-Za-z0-9-_`，拒绝 padding，拒绝长度 %4==1）。

use crate::error::SyncError;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn value_of(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

/// 编码为 base64url（无 `=` padding）。
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3f) as usize] as char);
        }
    }
    out
}

/// 解码 base64url（只接受无 padding 的规范形式）。
pub fn decode(text: &str) -> Result<Vec<u8>, SyncError> {
    let bytes = text.as_bytes();
    if bytes.len() % 4 == 1 {
        return Err(SyncError::RelayWire("base64url 长度非法"));
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc: u32 = 0;
        for (index, byte) in chunk.iter().enumerate() {
            let value = value_of(*byte).ok_or(SyncError::RelayWire("base64url 字符非法"))?;
            acc |= u32::from(value) << (18 - index * 6);
        }
        out.push(((acc >> 16) & 0xff) as u8);
        if chunk.len() > 2 {
            out.push(((acc >> 8) & 0xff) as u8);
        }
        if chunk.len() > 3 {
            out.push((acc & 0xff) as u8);
        }
    }
    Ok(out)
}

/// 解码并断言恰好 `N` 字节。
pub fn decode_array<const N: usize>(text: &str) -> Result<[u8; N], SyncError> {
    let bytes = decode(text)?;
    bytes
        .try_into()
        .map_err(|_| SyncError::RelayWire("base64url 解码长度不符"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        // RFC 4648 §10 的标准 base64 向量（换成 url-safe 字母表后逐字节一致）
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg");
        assert_eq!(encode(b"fo"), "Zm8");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg");
        assert_eq!(encode(b"fooba"), "Zm9vYmE");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        for value in ["", "Zg", "Zm8", "Zm9v", "Zm9vYg", "Zm9vYmE", "Zm9vYmFy"] {
            assert_eq!(decode(value).unwrap(), decode(value).unwrap());
        }
    }

    #[test]
    fn url_safe_alphabet_and_roundtrip() {
        let bytes: Vec<u8> = (0u8..=255).collect();
        let text = encode(&bytes);
        assert!(!text.contains('+') && !text.contains('/') && !text.contains('='));
        assert_eq!(decode(&text).unwrap(), bytes);
        // 0xfb 0xff 会用到 url-safe 字符 - 和 _
        assert_eq!(encode(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn rejects_malformed() {
        assert!(decode("Z").is_err(), "长度 %4==1 必须拒绝");
        assert!(decode("Zg==").is_err(), "padding 必须拒绝");
        assert!(decode("Zg+").is_err(), "标准 base64 '+' 不同意");
        assert!(decode("Zg/").is_err(), "标准 base64 '/' 不同意");
        assert!(decode_array::<32>("Zg").is_err());
        let ids = [0xaa; 16];
        assert_eq!(decode_array::<16>(&encode(&ids)).unwrap(), ids);
    }
}

`````



@@SECTION code/018 `crates/sync/src/cbor.rs`


`````rust
//! relay 控制面的 CBOR（CDE）编解码辅助（S7-9）。
//!
//! relay 的控制面是 `application/cbor` + 整数键 CDE map（ADR §5/§15），可选字段**省略**。
//! 本模块只做"记录形状"的薄封装：真正的 canonical 编解码由 `sync-proto::cde` 保证
//! （键升序、无重复、拒绝尾随字节），这里不重写编码规则。

use crate::error::SyncError;
pub use menghunote_sync_proto::CdeValue;
use menghunote_sync_proto::{decode_cde, encode_cde};

/// 解码后的整数键 map。
pub type Fields = Vec<(u64, CdeValue)>;

/// 解码一个 CDE map；不是 map/非法 CBOR 都算 relay wire 错误。
pub fn decode(body: &[u8]) -> Result<Fields, SyncError> {
    match decode_cde(body) {
        Ok(CdeValue::Map(fields)) => Ok(fields),
        Ok(_) => Err(SyncError::RelayWire("控制面响应不是 CDE map")),
        Err(_) => Err(SyncError::RelayWire("控制面响应不是合法 CDE")),
    }
}

/// 编码一个整数键 map（编码失败说明调用方构造了非法值）。
pub fn encode(fields: Fields) -> Result<Vec<u8>, SyncError> {
    encode_cde(&CdeValue::Map(fields)).map_err(|_| SyncError::RelayWire("控制面请求无法编码"))
}

/// 编码任意 CDE 值（嵌套数组/记录用）。
pub fn encode_value(value: &CdeValue) -> Result<Vec<u8>, SyncError> {
    encode_cde(value).map_err(|_| SyncError::RelayWire("控制面请求无法编码"))
}

/// u64 → `CdeValue`。
pub fn uint(value: u64) -> CdeValue {
    CdeValue::UInt(value)
}

/// 字节串 → `CdeValue`。
pub fn bytes(value: impl Into<Vec<u8>>) -> CdeValue {
    CdeValue::Bytes(value.into())
}

/// 文本 → `CdeValue`。
pub fn text(value: impl Into<String>) -> CdeValue {
    CdeValue::Text(value.into())
}

/// 数组 → `CdeValue`。
pub fn array(values: Vec<CdeValue>) -> CdeValue {
    CdeValue::Array(values)
}

/// 布尔 → `CdeValue`。
pub fn boolean(value: bool) -> CdeValue {
    CdeValue::Bool(value)
}

fn invalid() -> SyncError {
    SyncError::RelayWire("控制面字段缺失或类型不符")
}

/// 取字段；缺失即 wire 错误。
pub fn field(fields: &Fields, key: u64) -> Result<&CdeValue, SyncError> {
    fields
        .iter()
        .find(|(index, _)| *index == key)
        .map(|(_, value)| value)
        .ok_or_else(invalid)
}

/// 取 optional 字段。
pub fn opt_field(fields: &Fields, key: u64) -> Option<&CdeValue> {
    fields
        .iter()
        .find(|(index, _)| *index == key)
        .map(|(_, value)| value)
}

/// u64 字段。
pub fn uint_field(fields: &Fields, key: u64) -> Result<u64, SyncError> {
    match field(fields, key)? {
        CdeValue::UInt(value) => Ok(*value),
        _ => Err(invalid()),
    }
}

/// optional u64 字段。
pub fn opt_uint_field(fields: &Fields, key: u64) -> Result<Option<u64>, SyncError> {
    match opt_field(fields, key) {
        None => Ok(None),
        Some(CdeValue::UInt(value)) => Ok(Some(*value)),
        Some(_) => Err(invalid()),
    }
}

/// 字节串字段。
pub fn bytes_field(fields: &Fields, key: u64) -> Result<Vec<u8>, SyncError> {
    match field(fields, key)? {
        CdeValue::Bytes(value) => Ok(value.clone()),
        _ => Err(invalid()),
    }
}

/// optional 字节串字段。
pub fn opt_bytes_field(fields: &Fields, key: u64) -> Result<Option<Vec<u8>>, SyncError> {
    match opt_field(fields, key) {
        None => Ok(None),
        Some(CdeValue::Bytes(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid()),
    }
}

/// 定长字节串字段（16B/32B/64B）。
pub fn bytes_fixed<const N: usize>(fields: &Fields, key: u64) -> Result<[u8; N], SyncError> {
    let bytes = bytes_field(fields, key)?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::RelayWire("控制面字段长度不符"))
}

/// 文本字段。
pub fn text_field(fields: &Fields, key: u64) -> Result<String, SyncError> {
    match field(fields, key)? {
        CdeValue::Text(value) => Ok(value.clone()),
        _ => Err(invalid()),
    }
}

/// optional 文本字段。
pub fn opt_text_field(fields: &Fields, key: u64) -> Result<Option<String>, SyncError> {
    match opt_field(fields, key) {
        None => Ok(None),
        Some(CdeValue::Text(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid()),
    }
}

/// 布尔字段。
pub fn bool_field(fields: &Fields, key: u64) -> Result<bool, SyncError> {
    match field(fields, key)? {
        CdeValue::Bool(value) => Ok(*value),
        _ => Err(invalid()),
    }
}

/// 可选数组字段。
pub fn opt_array_field(fields: &Fields, key: u64) -> Result<Option<Vec<CdeValue>>, SyncError> {
    match fields.iter().find(|(field_key, _)| *field_key == key) {
        Some((_, CdeValue::Array(values))) => Ok(Some(values.clone())),
        Some(_) => Err(invalid()),
        None => Ok(None),
    }
}

/// 数组字段。
pub fn array_field(fields: &Fields, key: u64) -> Result<Vec<CdeValue>, SyncError> {
    match field(fields, key)? {
        CdeValue::Array(values) => Ok(values.clone()),
        _ => Err(invalid()),
    }
}

/// 取一个 CDE bytes 值。
pub fn value_bytes(value: &CdeValue) -> Result<Vec<u8>, SyncError> {
    match value {
        CdeValue::Bytes(bytes) => Ok(bytes.clone()),
        _ => Err(invalid()),
    }
}

/// 把一个 `CdeValue` 当整数键 map 读。
pub fn value_map(value: &CdeValue) -> Result<Fields, SyncError> {
    match value {
        CdeValue::Map(fields) => Ok(fields.clone()),
        _ => Err(invalid()),
    }
}

/// 把数组元素当 map 读。
pub fn value_map_field(fields: &Fields, key: u64, index: usize) -> Result<Fields, SyncError> {
    let values = array_field(fields, key)?;
    let value = values.get(index).ok_or_else(invalid)?;
    value_map(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_encodes_sorted_by_key() {
        let encoded = encode(vec![(1, uint(2)), (0, uint(1))]).expect("编码");
        // 0x02: map(2)；0x00/0x01 键；0x01/0x02 值（CDE 的 uint 是 1 字节小整数）。
        assert_eq!(encoded, vec![0xa2, 0x00, 0x01, 0x01, 0x02]);
        let decoded = decode(&encoded).expect("解码");
        assert_eq!(decoded, vec![(0, uint(1)), (1, uint(2))]);
    }

    #[test]
    fn optional_field_roundtrip() {
        let fields = vec![(0, bytes([0xaa; 16])), (1, text("active"))];
        assert_eq!(bytes_fixed::<16>(&fields, 0).unwrap(), [0xaa; 16]);
        assert_eq!(text_field(&fields, 1).unwrap(), "active");
        assert_eq!(opt_uint_field(&fields, 9).unwrap(), None);
        assert!(bytes_fixed::<32>(&fields, 0).is_err());
        assert!(bool_field(&fields, 0).is_err());
    }
}

`````



@@SECTION code/019 `crates/sync/src/checkpoint.rs`


`````rust
//! 初始槽上传 + checkpoint 创建/读回（S7-14；ADR §5/§7）。
//!
//! **初始槽**：把 genesis 本地的 password/recovery 槽体按 KEY_SLOT 对象上传
//! （generation=0、单片 ≤4KiB、AAD 带 descriptor、`context_hash=SHA256(descriptor)`），
//! 再 `PUT P/key-slots/{slot_id}` 登记；password 槽用 CAS 切 active，最后 `GET P/key-slots`
//! 校验 active/staging 与本地一致。槽指针的设备签名消息就是 `CDE(descriptor)`
//! （relay `Expectation::None`，签名覆盖描述符本身）。
//!
//! **checkpoint**：
//! 1. `POST P/checkpoints` 拿 writer lease（固定 S）；
//! 2. 结构文档 + 全部正文文档导出 `SnapshotV1`（`full_history=true`，重放 snapshot+updates
//!    后 `export_snapshot`），逐个上传为 SNAPSHOT 对象（generation>0）；
//! 3. 资产走 `AssetSource`：真实路径用 [`StorageAssets`]（storage 的 `list_assets` +
//!    逐条 `asset_bytes`，不一次性读进内存），object_id 对完整资产明文头做
//!    HMAC（ADR §31：MIME/长度不同必须不同 id）；
//! 4. `ManifestV1`（generation=0）+ `refs_digest`（只覆盖 snapshot/asset）；
//! 5. refs（data + manifest）上传；**prepare 不发布**——`validate` 真读回/导入通过后，
//!    `promote` 才签 `CheckpointCommit` 并 publish（ADR §32）。
//!
//! 发布后不自行删日志；GC 由 relay/后续任务。

use std::collections::{BTreeMap, HashSet};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::asset_sync;
use crate::cbor;
use crate::device::DeviceSigningKey;
use crate::error::SyncError;
use crate::keys::{derive_asset_id_key, derive_checkpoint_receipt_key, RootSecretV1, Secret32};
use crate::relay::{
    CheckpointLease, CheckpointPublish, CheckpointRef, CheckpointRole, RelayClient, RelayLogEvent,
    RelayLogKind, RelayState, SlotRecord, SlotUpload,
};
use crate::slot::KeySlotV1;
use crate::upload::{
    encrypt_asset_parts, fetch_object, fetch_verified_asset, submit_prepared_object,
    upload_object_with_reservation, AssetExpectation, EncryptedObjectParts, UploadedObject,
    VerifiedAssetFacts,
};
use crate::{DeviceId, ObjectId, VaultId};
use menghunote_storage::{
    asset_bytes, list_all_docs, list_assets, load_structure, read_doc_bootstrap, BodyDoc, DocKind,
    StructureDoc, STRUCTURE_DOC_ID,
};
use menghunote_sync_proto::{
    checkpoint_signing_input, checkpoint_validation_input, part_count_for, refs_digest_input,
    CheckpointCommit, CheckpointValidationV2, ManifestAsset, ManifestDoc, ManifestV1, ObjectCommit,
    ObjectRefV1, ObjectType, SnapshotV1, UpdateV1, MAX_ASSET_RAW_BYTES,
};

/// 文档 kind（客户端约定）：结构文档 = 0，正文文档 = 1。
pub const KIND_STRUCTURE: u64 = 0;
/// 正文文档 kind。
pub const KIND_BODY: u64 = 1;

/// 一个资产的元数据（不包含数据，避免一次性读进内存）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetMeta {
    /// 原始字节的 SHA256（资产真实 hash）。
    pub real_sha256: [u8; 32],
    /// 原始字节长度。
    pub byte_len: u64,
    /// MIME（1..=255B）。
    pub media_type: String,
}

/// checkpoint 的资产来源。
///
/// 真实路径用 [`StorageAssets`]（`list_assets` 只读元数据 + 逐条 `asset_bytes`）；
/// 测试/示例可用 [`StaticAssets`]；[`EmptyAssets`] 只用于明确验证"无资产基线"的路径。
pub trait AssetSource {
    /// 列出资产元数据（不加载 data）。
    fn list(&self, vault: &menghunote_storage::Vault) -> Result<Vec<AssetMeta>, SyncError>;
    /// 按需读取单个资产原始字节。
    fn read(
        &self,
        vault: &menghunote_storage::Vault,
        asset: &AssetMeta,
    ) -> Result<Vec<u8>, SyncError>;
}

/// 空资产源：只用于明确验证"无资产基线"的路径（真实路径用 [`StorageAssets`]）。
#[derive(Debug, Default, Clone, Copy)]
pub struct EmptyAssets;

impl AssetSource for EmptyAssets {
    fn list(&self, _vault: &menghunote_storage::Vault) -> Result<Vec<AssetMeta>, SyncError> {
        Ok(Vec::new())
    }

    fn read(
        &self,
        _vault: &menghunote_storage::Vault,
        _asset: &AssetMeta,
    ) -> Result<Vec<u8>, SyncError> {
        Err(SyncError::Unsupported("资产源为空"))
    }
}

/// 真实 storage 资产源：`list_assets` 只读元数据，逐条 `asset_bytes` 读数据。
#[derive(Debug, Default, Clone, Copy)]
pub struct StorageAssets;

impl AssetSource for StorageAssets {
    fn list(&self, vault: &menghunote_storage::Vault) -> Result<Vec<AssetMeta>, SyncError> {
        let mut out = Vec::new();
        for asset in list_assets(vault)? {
            let (Some(byte_len), Some(media_type)) = (asset.byte_len, asset.media_type) else {
                continue; // 未完成/孤儿资产不进入 checkpoint
            };
            if !asset.has_data || byte_len < 0 {
                continue;
            }
            out.push(AssetMeta {
                real_sha256: asset.asset_hash,
                byte_len: byte_len as u64,
                media_type,
            });
        }
        Ok(out)
    }

    fn read(
        &self,
        vault: &menghunote_storage::Vault,
        asset: &AssetMeta,
    ) -> Result<Vec<u8>, SyncError> {
        asset_bytes(vault, asset.real_sha256)?
            .ok_or(SyncError::Unsupported("资产 data 缺失（未完成/孤儿）"))
    }
}

/// 测试/示例用的静态资产源。
#[derive(Debug, Clone, Default)]
pub struct StaticAssets {
    entries: Vec<(AssetMeta, Vec<u8>)>,
}

impl StaticAssets {
    /// 从 `(mime, data)` 列表构造并校验 sha/长度。
    pub fn new(entries: Vec<(String, Vec<u8>)>) -> Result<Self, SyncError> {
        let mut out = Vec::with_capacity(entries.len());
        for (media_type, data) in entries {
            let meta = AssetMeta {
                real_sha256: Sha256::digest(&data).into(),
                byte_len: data.len() as u64,
                media_type,
            };
            out.push((meta, data));
        }
        Ok(Self { entries: out })
    }

    /// 全部资产的真实 SHA256（阶段 E 的完整集合 digest 用）。
    #[must_use]
    pub fn hashes(&self) -> Vec<[u8; 32]> {
        self.entries
            .iter()
            .map(|(meta, _)| meta.real_sha256)
            .collect()
    }
}

impl AssetSource for StaticAssets {
    fn list(&self, _vault: &menghunote_storage::Vault) -> Result<Vec<AssetMeta>, SyncError> {
        Ok(self.entries.iter().map(|(meta, _)| meta.clone()).collect())
    }

    fn read(
        &self,
        _vault: &menghunote_storage::Vault,
        asset: &AssetMeta,
    ) -> Result<Vec<u8>, SyncError> {
        self.entries
            .iter()
            .find(|(meta, _)| meta == asset)
            .map(|(_, data)| data.clone())
            .ok_or(SyncError::Unsupported("静态资产源缺少该资产"))
    }
}

/// 初始槽上传结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotsReport {
    /// password 槽记录（应为 active）。
    pub password: SlotRecord,
    /// recovery 槽记录（应为 staging）。
    pub recovery: SlotRecord,
    /// active password 槽的 `slot_set_rev`。
    pub active_password_rev: u64,
}

/// 已上传的一个文档快照。
#[derive(Debug, Clone)]
pub struct DocSnapshotRef {
    /// 文档 id。
    pub doc_id: String,
    /// kind（[`KIND_STRUCTURE`] / [`KIND_BODY`]）。
    pub kind: u64,
    /// 文档代际。
    pub generation: u64,
    /// 版本向量 CDE。
    pub vv: Vec<u8>,
    /// frontier。
    pub frontier: Vec<u8>,
    /// 已上传对象。
    pub object: UploadedObject,
}

/// 已上传的一个资产。
#[derive(Debug, Clone)]
pub struct AssetRef {
    /// 资产真实 hash。
    pub real_sha256: [u8; 32],
    /// 原始长度。
    pub byte_len: u64,
    /// MIME。
    pub media_type: String,
    /// 已上传对象。
    pub object: UploadedObject,
}

/// 一次 checkpoint 创建结果。
#[derive(Debug, Clone)]
pub struct CheckpointReport {
    /// checkpoint id。
    pub checkpoint_id: [u8; 16],
    /// epoch。
    pub epoch: u64,
    /// 固定的 S。
    pub covered_seq: u64,
    /// 文档快照。
    pub docs: Vec<DocSnapshotRef>,
    /// 资产。
    pub assets: Vec<AssetRef>,
    /// manifest 对象。
    pub manifest: UploadedObject,
    /// 公开 refs 摘要。
    pub refs_digest: [u8; 32],
    /// 发布时间；`prepare` 产物为 `None`（尚未 publish）。
    pub published_at: Option<u64>,
    /// 是否已从 relay 读回并 validate（只有 validate 过的基线才能 promote）。
    pub validated: bool,
    /// promote 成功后签名的 `CheckpointCommit` 字节摘要（SHA256）。
    ///
    /// 轮换 activation T 的 `primary/fallback_commit_digest` 只能取这里——不得再用
    /// report 的公开字段另拼一份（v2 起 commit 还绑定 `validation_digest`）。
    pub commit_digest: Option<[u8; 32]>,
}

/// `prepare` 导出点冻结的本地事实（P0-3 绑定证据）。
///
/// `validate` 只信这里记录的导出点，不再用"当前本地 integrated/VV"替代
/// "这份导出快照是否真的覆盖 S"。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExportFootprint {
    checkpoint_id: [u8; 16],
    epoch: u64,
    /// coverage 扫描绑定的 relay active epoch（= prepare 时 relay `/state` 的 epoch）。
    ///
    /// 候选基线是在 `from_epoch`（旧 epoch）冻结点导出的：checkpoint 自己属于
    /// `to_epoch`（`epoch == to_epoch`），但 activate 之前 relay 仍停在 `from_epoch`，
    /// 日志事件的 epoch/分页 epoch 也都是 `from_epoch`。因此 coverage 证明**只能**
    /// 用这个冻结 epoch 校验 relay 事件，不能拿 checkpoint 的 `to_epoch/key_epoch` 去比。
    coverage_epoch: u64,
    /// 固定 S。
    covered_seq: u64,
    /// 按 `doc_id` 升序。
    docs: Vec<DocFootprint>,
    /// 结构文档的 oplog VV（空 VV 也要显式记录）。
    structure_vv: Vec<u8>,
    /// 资产集合摘要（稳定排序后对完整明文头做 SHA256）。
    asset_set_digest: [u8; 32],
    /// 已导出 ASSET 的 object_id（升序）。
    ///
    /// 只用于 durable 编解码自洽与资产集合绑定；**不是** coverage 证据——不得据此把
    /// 任何 inbox 行直接判为已证明（Engine 不为 ASSET 写 inbox）。
    asset_object_ids: Vec<ObjectId>,
}

/// 单个文档在 prepare 导出点的本地事实。
#[derive(Debug, Clone, PartialEq, Eq)]
struct DocFootprint {
    doc_id: String,
    kind: u64,
    generation: u64,
    /// 导出点的本地 `loro_docs.head_seq`。
    head_seq: u64,
    /// 导出点的本地 `loro_docs.head_vv`。
    head_vv: Vec<u8>,
    /// 从导出文档读到的 oplog VV（full-history 下必须等于 `head_vv`）。
    oplog_vv: Vec<u8>,
}

/// `prepare` 的产物：对象已上传/refs 已登记，但**尚未 validate、未 publish、未签最终 commit**。
///
/// ADR §32：prepare 只固定 S 与导出对象；任何可驱动 GC 的基线必须经
/// [`CheckpointWriter::validate`] 真读回+导入，再由 [`CheckpointWriter::promote`] 签名发布。
///
/// `footprint` 是私有字段：外部无法在 prepare 之后改写"导出点"来让 stale 快照通过
/// validate；它只能由本模块的 [`CheckpointWriter::prepare`] 构造。
#[derive(Debug, Clone)]
pub struct PreparedCheckpoint {
    /// checkpoint 报告（`validated=false`、`published_at=None`）。
    pub report: CheckpointReport,
    /// prepare 时冻结的本地事实；validate 的绑定证据。
    footprint: ExportFootprint,
}

/// `validate` 采集到、`promote` 必须原样签名的不可变事实。
///
/// 字段全部私有；外部不能构造、不能拿到可变引用，也不能在 validate 之后篡改
/// 即将被签名的 `CheckpointCommit` 字段。
#[derive(Debug, Clone, PartialEq, Eq)]
struct BaselineFacts {
    vault_id: VaultId,
    checkpoint_id: [u8; 16],
    epoch: u64,
    /// 固定 S。
    covered_seq: u64,
    /// S 对应游标。
    cursor: String,
    manifest_object_id: ObjectId,
    /// manifest 对象密文摘要。
    manifest_digest: [u8; 32],
    refs_digest: [u8; 32],
    /// validate 实际导入独立空 LoroDoc 的文档数（证明不是只看 manifest 宣称）。
    imported_docs: usize,
    /// 导入后计算出的 actual VV 摘要（按 doc_id 排序的 `len||doc_id||len||vv`）。
    imported_vv_digest: [u8; 32],
    /// validate 实际下载解密并验真实 SHA256/长度/MIME 的资产数。
    verified_assets: usize,
    /// coverage 复核中由本机 durable skipped-asset 记录证明的 ASSET 数。
    coverage_durable_assets: usize,
    /// coverage 复核中真实下载验证（或精确命中正缓存）的候选 ASSET 数。
    coverage_verified_candidates: usize,
    /// 覆盖证明事实摘要（按 seq 排序的 `seq||oid||real_sha||byte_len||digest`）。
    coverage_proof_digest: [u8; 32],
    /// coverage 扫描冻结的 relay active epoch。
    coverage_epoch: u64,
    /// 验证修订 2 的证据摘要（写进 `CheckpointCommit.validation_digest`）。
    validation_digest: [u8; 32],
}

impl BaselineFacts {
    /// 从不可变验证事实重建 v2 证据（用于重算/对撞 `validation_digest`）。
    fn validation_evidence(&self, producer_device_id: &DeviceId) -> CheckpointValidationV2 {
        CheckpointValidationV2 {
            vault_id: self.vault_id,
            checkpoint_id: self.checkpoint_id,
            epoch: self.epoch,
            covered_relay_seq: self.covered_seq,
            coverage_epoch: self.coverage_epoch,
            manifest_object_id: self.manifest_object_id,
            manifest_digest: self.manifest_digest,
            refs_digest: self.refs_digest,
            producer_device_id: *producer_device_id,
            imported_docs: u64::try_from(self.imported_docs).unwrap_or(u64::MAX),
            imported_vv_digest: self.imported_vv_digest,
            verified_assets: u64::try_from(self.verified_assets).unwrap_or(u64::MAX),
            coverage_durable_assets: u64::try_from(self.coverage_durable_assets)
                .unwrap_or(u64::MAX),
            coverage_verified_candidates: u64::try_from(self.coverage_verified_candidates)
                .unwrap_or(u64::MAX),
            coverage_proof_digest: self.coverage_proof_digest,
        }
    }

    /// 重算 `SHA256(checkpoint_validation_input(...))` 并与记录的摘要对撞。
    fn recomputed_validation_digest(
        &self,
        producer_device_id: &DeviceId,
    ) -> Result<[u8; 32], SyncError> {
        let input = checkpoint_validation_input(&self.validation_evidence(producer_device_id))?;
        Ok(Sha256::digest(input).into())
    }
}

/// 从 relay 读回并验证通过的基线句柄；构造后只能由 [`CheckpointWriter::promote`] 提升。
///
/// 内部事实是私有的，只提供只读访问器；`promote` 直接签这里记录的不可变数据，
/// 而不是签调用方仍可修改的公开字段。下面两段都不能编译：
///
/// ```compile_fail
/// use menghunote_sync::checkpoint::VerifiedBaseline;
/// let mut baseline: VerifiedBaseline = unimplemented!();
/// baseline.covered_seq = 0;
/// ```
///
/// ```compile_fail
/// use menghunote_sync::checkpoint::VerifiedBaseline;
/// let forged = VerifiedBaseline { facts: unimplemented!() };
/// ```
#[derive(Debug, Clone)]
pub struct VerifiedBaseline {
    facts: BaselineFacts,
}

impl VerifiedBaseline {
    /// checkpoint id。
    #[must_use]
    pub fn checkpoint_id(&self) -> [u8; 16] {
        self.facts.checkpoint_id
    }

    /// epoch。
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.facts.epoch
    }

    /// 固定 S。
    #[must_use]
    pub fn covered_seq(&self) -> u64 {
        self.facts.covered_seq
    }

    /// S 对应游标。
    #[must_use]
    pub fn cursor(&self) -> &str {
        &self.facts.cursor
    }

    /// manifest 对象 id。
    #[must_use]
    pub fn manifest_object_id(&self) -> ObjectId {
        self.facts.manifest_object_id
    }

    /// manifest 对象密文摘要。
    #[must_use]
    pub fn manifest_digest(&self) -> [u8; 32] {
        self.facts.manifest_digest
    }

    /// 公开 refs 摘要。
    #[must_use]
    pub fn refs_digest(&self) -> [u8; 32] {
        self.facts.refs_digest
    }

    /// validate 实际导入独立空 LoroDoc 的文档数。
    #[must_use]
    pub fn imported_docs(&self) -> usize {
        self.facts.imported_docs
    }

    /// 导入后计算出的 actual VV 摘要。
    #[must_use]
    pub fn imported_vv_digest(&self) -> [u8; 32] {
        self.facts.imported_vv_digest
    }

    /// validate 实际下载解密并验真实 SHA256/长度/MIME 的资产数。
    #[must_use]
    pub fn verified_assets(&self) -> usize {
        self.facts.verified_assets
    }

    /// coverage 复核由本机 durable skipped-asset 记录证明的 ASSET 数。
    #[must_use]
    pub fn coverage_durable_assets(&self) -> usize {
        self.facts.coverage_durable_assets
    }

    /// coverage 复核真实下载验证（或命中正缓存）的候选 ASSET 数。
    #[must_use]
    pub fn coverage_verified_candidates(&self) -> usize {
        self.facts.coverage_verified_candidates
    }

    /// 覆盖证明事实摘要。
    #[must_use]
    pub fn coverage_proof_digest(&self) -> [u8; 32] {
        self.facts.coverage_proof_digest
    }
}

// ---------------------------------------------------------------------------
// restart 候选基线：per-role create intent 与阶段进度（评审 0003 P1-D）
// ---------------------------------------------------------------------------

/// restart 基线创建 request_id 的派生域。
const BASELINE_CREATE_REQUEST_DOMAIN: &[u8] = b"MN/v1/restart-baseline-create\0";

/// 旧 journal 没有 per-role create intent 时 fail-closed（绝不 fresh create）。
pub const BASELINE_INTENT_MISSING: &str =
    "restart journal 缺 per-role create intent（旧 journal）：拒绝 fresh create（保持冻结）";
/// durable intent 与当前 rotation/role/epoch 不一致。
const BASELINE_INTENT_MISMATCH: &str =
    "restart 基线 create intent 与当前 rotation/role/epoch 不一致";

/// 派生候选基线创建请求的幂等 `request_id`。
///
/// **唯一入口**：`HttpRelayClient::create_checkpoint_with_role` 与本地 durable intent
/// 都调用本函数，保证"重试时真正发给 relay 的 id = intent 里持久化的 id"。
/// `epoch` 已唯一标识一次轮换（候选 epoch 由 relay 分配），因此不再额外混入
/// rotation id；HTTP 层没有 rotation 上下文也能稳定派生同一值。
#[must_use]
pub fn baseline_create_request_id(vault_id: &VaultId, role: &str, epoch: u64) -> [u8; 16] {
    let mut input =
        Vec::with_capacity(BASELINE_CREATE_REQUEST_DOMAIN.len() + 16 + 4 + role.len() + 8);
    input.extend_from_slice(BASELINE_CREATE_REQUEST_DOMAIN);
    input.extend_from_slice(vault_id);
    input.extend_from_slice(&(role.len() as u32).to_be_bytes());
    input.extend_from_slice(role.as_bytes());
    input.extend_from_slice(&epoch.to_be_bytes());
    let digest = Sha256::digest(&input);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// restart 候选基线在本地的阶段（只允许前进）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RestartBaselineStage {
    /// create intent 已 DPAPI/本地 durable 落盘，relay 分配尚未知。
    Intent = 0,
    /// relay 已返回 lease（cid/S/reservation）并立即落盘。
    Created = 1,
    /// 全部对象已上传、coverage receipt 已落盘；refs 尚未登记。
    ObjectsUploaded = 2,
    /// data + manifest refs 已登记。
    RefsRegistered = 3,
    /// 读回/导入验证通过，`VerifiedBaseline` 已落盘。
    Validated = 4,
    /// publish（receipt）成功，本地 commit 摘要已落盘。
    Promoted = 5,
}

impl RestartBaselineStage {
    fn code(self) -> u64 {
        self as u64
    }

    fn from_code(code: u64) -> Result<Self, SyncError> {
        Ok(match code {
            0 => Self::Intent,
            1 => Self::Created,
            2 => Self::ObjectsUploaded,
            3 => Self::RefsRegistered,
            4 => Self::Validated,
            5 => Self::Promoted,
            _ => return Err(SyncError::LocalCorrupt("restart 基线阶段非法")),
        })
    }
}

/// 一份 restart 候选基线的创建 intent（公开事实，不含秘密）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartBaselineIntent {
    /// `primary` / `fallback`。
    pub role: String,
    /// 候选 epoch。
    pub epoch: u64,
    /// 本次 restart 的 rotation id。
    pub rotation_id: [u8; 16],
    /// relay create 的幂等 request_id（与 HTTP 层派生的值一致）。
    pub request_id: [u8; 16],
    /// restart 契约里的 CAS 期望（control_rev）。
    pub expected_control_rev: u64,
    /// 首次 prepare 的估算字节数；重试必须复用（relay 幂等哈希覆盖它）。
    pub estimated_bytes: u64,
}

/// restart 候选基线的 durable 状态。
#[derive(Debug, Clone)]
pub struct RestartBaselineEntry {
    /// create intent。
    pub intent: RestartBaselineIntent,
    /// 已完成到哪个阶段。
    pub stage: RestartBaselineStage,
    /// relay 分配的 lease（create 成功后立即落盘）。
    pub lease: Option<CheckpointLease>,
    /// `PreparedCheckpoint` 的 durable 编码（upload 完成后落盘）。
    pub prepared: Option<Vec<u8>>,
    /// `VerifiedBaseline` 的 durable 编码（validate 通过后落盘）。
    pub verified: Option<Vec<u8>>,
    /// 本地 `CheckpointCommit` 摘要（promote/publish 成功后落盘）。
    pub commit_digest: Option<[u8; 32]>,
    /// relay publish 返回的发布时间。
    pub published_at: Option<u64>,
}

/// restart 候选基线的 per-role durable journal。
///
/// 存在独立的 `vault.meta` 键里（不含秘密，故不做 DPAPI）；与 `RotationJournal`
/// 分开是因为创建 intent 发生在 `CheckpointWriter::prepare` 内部，而 writer 只持有
/// `Vault`。**旧 journal 没有本记录 = 无法证明 relay create 是否已落**，必须 fail-closed。
#[derive(Debug, Clone)]
pub struct RestartBaselineJournal {
    /// 归属 vault。
    pub vault_id: VaultId,
    /// 本次 restart 的 rotation id（init 时为空，首次 bind 填入）。
    pub rotation_id: Option<[u8; 16]>,
    /// 候选 epoch（init 时为空，首次 bind 填入）。
    pub to_epoch: Option<u64>,
    /// restart 契约的 CAS 期望（init 时为空，首次 bind 填入）。
    pub expected_control_rev: Option<u64>,
    /// 至多两条（primary/fallback）。
    pub entries: Vec<RestartBaselineEntry>,
}

/// restart 基线 journal 的 vault meta 键。
pub const RESTART_BASELINE_META: &str = "sync.restart_baseline_journal";
const RESTART_BASELINE_VERSION: u64 = 1;

impl RestartBaselineJournal {
    /// 读 journal；不存在/空值返回 `None`。
    pub fn load(vault: &menghunote_storage::Vault) -> Result<Option<Self>, SyncError> {
        let Some(blob) = vault.get_meta(RESTART_BASELINE_META)? else {
            return Ok(None);
        };
        if blob.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self::decode(&blob)?))
    }

    /// DPAPI 之外的本地 durable 写回（只含公开事实）。
    pub fn save(&self, vault: &menghunote_storage::Vault) -> Result<(), SyncError> {
        let bytes = self.encode()?;
        vault.set_meta(RESTART_BASELINE_META, &bytes)?;
        Ok(())
    }

    /// 新 restart 的起点：清空旧记录并写入 vault 归属。
    pub fn reset(vault: &menghunote_storage::Vault, vault_id: VaultId) -> Result<(), SyncError> {
        Self {
            vault_id,
            rotation_id: None,
            to_epoch: None,
            expected_control_rev: None,
            entries: Vec::new(),
        }
        .save(vault)
    }

    /// 删除记录（测试/显式 abort 用；正常运行不调用）。
    pub fn clear(vault: &menghunote_storage::Vault) -> Result<(), SyncError> {
        vault.set_meta(RESTART_BASELINE_META, b"")?;
        Ok(())
    }

    /// 必须有记录，否则视为旧 journal：fail-closed。
    pub fn require(vault: &menghunote_storage::Vault) -> Result<Self, SyncError> {
        Self::load(vault)?.ok_or(SyncError::Unsupported(BASELINE_INTENT_MISSING))
    }

    /// 把记录绑定到本次 restart 的 rotation/epoch/CAS（首次写入，之后只允许相等）。
    pub fn bind(
        &mut self,
        vault: &menghunote_storage::Vault,
        vault_id: VaultId,
        rotation_id: [u8; 16],
        to_epoch: u64,
        expected_control_rev: u64,
    ) -> Result<(), SyncError> {
        if self.vault_id != vault_id {
            return Err(SyncError::LocalCorrupt(
                "restart 基线 journal 归属 vault 不符",
            ));
        }
        let mut changed = false;
        match self.rotation_id {
            Some(current) if current != rotation_id => {
                return Err(SyncError::Unsupported(BASELINE_INTENT_MISMATCH));
            }
            None => {
                self.rotation_id = Some(rotation_id);
                changed = true;
            }
            Some(_) => {}
        }
        match self.to_epoch {
            Some(current) if current != to_epoch => {
                return Err(SyncError::Unsupported(BASELINE_INTENT_MISMATCH));
            }
            None => {
                self.to_epoch = Some(to_epoch);
                changed = true;
            }
            Some(_) => {}
        }
        match self.expected_control_rev {
            Some(current) if current != expected_control_rev => {
                return Err(SyncError::Unsupported(BASELINE_INTENT_MISMATCH));
            }
            None => {
                self.expected_control_rev = Some(expected_control_rev);
                changed = true;
            }
            Some(_) => {}
        }
        if changed {
            self.save(vault)?;
        }
        Ok(())
    }

    /// 读一条 role 记录。
    #[must_use]
    pub fn entry(&self, role: &str) -> Option<&RestartBaselineEntry> {
        self.entries.iter().find(|entry| entry.intent.role == role)
    }

    /// **create 之前** durable 记录 per-role intent；已有则返回持久 intent（重试复用）。
    pub fn start(
        &mut self,
        vault: &menghunote_storage::Vault,
        role: &str,
        epoch: u64,
        estimated_bytes: u64,
    ) -> Result<RestartBaselineIntent, SyncError> {
        if role != "primary" && role != "fallback" {
            return Err(SyncError::Object("restart 基线 role 非法"));
        }
        let rotation_id = self.rotation_id.ok_or(SyncError::LocalCorrupt(
            "restart 基线 journal 未绑定 rotation",
        ))?;
        let expected_control_rev = self
            .expected_control_rev
            .ok_or(SyncError::LocalCorrupt("restart 基线 journal 未绑定 CAS"))?;
        if let Some(entry) = self.entry(role) {
            if entry.intent.epoch != epoch || entry.intent.rotation_id != rotation_id {
                return Err(SyncError::Unsupported(BASELINE_INTENT_MISMATCH));
            }
            // 重试必须用持久估算值（relay 幂等哈希覆盖 estimated_bytes）。
            return Ok(entry.intent.clone());
        }
        if self.entries.len() >= 2 {
            return Err(SyncError::LocalCorrupt("restart 基线 intent 超过两份"));
        }
        let intent = RestartBaselineIntent {
            role: role.to_owned(),
            epoch,
            rotation_id,
            request_id: baseline_create_request_id(&self.vault_id, role, epoch),
            expected_control_rev,
            estimated_bytes,
        };
        self.entries.push(RestartBaselineEntry {
            intent: intent.clone(),
            stage: RestartBaselineStage::Intent,
            lease: None,
            prepared: None,
            verified: None,
            commit_digest: None,
            published_at: None,
        });
        self.save(vault)?;
        Ok(intent)
    }

    /// create 成功后**立即**落盘 relay 分配的 lease。
    pub fn record_lease(
        &mut self,
        vault: &menghunote_storage::Vault,
        role: &str,
        lease: CheckpointLease,
    ) -> Result<(), SyncError> {
        {
            let entry = self.entry_mut(role)?;
            if lease.epoch != entry.intent.epoch || lease.checkpoint_id == [0u8; 16] {
                return Err(SyncError::RelayWire("restart 基线 lease 与 intent 不一致"));
            }
            if let Some(existing) = &entry.lease {
                if existing != &lease {
                    return Err(SyncError::LocalCorrupt("restart 基线 lease 被改写"));
                }
            }
            entry.lease = Some(lease);
            entry.stage = entry.stage.max(RestartBaselineStage::Created);
        }
        self.save(vault)
    }

    /// 全部对象上传完成（refs 之前）落盘 prepared 快照。
    pub fn record_uploaded(
        &mut self,
        vault: &menghunote_storage::Vault,
        role: &str,
        prepared: Vec<u8>,
    ) -> Result<(), SyncError> {
        {
            let entry = self.entry_mut(role)?;
            if entry.stage < RestartBaselineStage::Created {
                return Err(SyncError::LocalCorrupt(
                    "restart 基线上传早于 create lease 落盘",
                ));
            }
            if let Some(existing) = &entry.prepared {
                if existing != &prepared {
                    return Err(SyncError::LocalCorrupt("restart 基线 prepared 字节被改写"));
                }
            }
            entry.prepared = Some(prepared);
            entry.stage = entry.stage.max(RestartBaselineStage::ObjectsUploaded);
        }
        self.save(vault)
    }

    /// data + manifest refs 登记完成。
    pub fn record_refs(
        &mut self,
        vault: &menghunote_storage::Vault,
        role: &str,
    ) -> Result<(), SyncError> {
        {
            let entry = self.entry_mut(role)?;
            if entry.stage < RestartBaselineStage::ObjectsUploaded || entry.prepared.is_none() {
                return Err(SyncError::LocalCorrupt(
                    "restart 基线 refs 早于 prepared 落盘",
                ));
            }
            entry.stage = entry.stage.max(RestartBaselineStage::RefsRegistered);
        }
        self.save(vault)
    }

    /// validate 通过后落盘不可变验证事实。
    pub fn record_validated(
        &mut self,
        vault: &menghunote_storage::Vault,
        role: &str,
        verified: Vec<u8>,
    ) -> Result<(), SyncError> {
        {
            let entry = self.entry_mut(role)?;
            if entry.stage < RestartBaselineStage::RefsRegistered {
                return Err(SyncError::LocalCorrupt(
                    "restart 基线 validate 早于 refs 登记",
                ));
            }
            if let Some(existing) = &entry.verified {
                if existing != &verified {
                    return Err(SyncError::LocalCorrupt("restart 基线 verified 字节被改写"));
                }
            }
            entry.verified = Some(verified);
            entry.stage = entry.stage.max(RestartBaselineStage::Validated);
        }
        self.save(vault)
    }

    /// publish 成功后**立即**落盘本地 commit receipt 与发布时间。
    pub fn record_promoted(
        &mut self,
        vault: &menghunote_storage::Vault,
        role: &str,
        commit_digest: [u8; 32],
        published_at: u64,
    ) -> Result<(), SyncError> {
        {
            let entry = self.entry_mut(role)?;
            if entry.stage < RestartBaselineStage::Validated || entry.verified.is_none() {
                return Err(SyncError::LocalCorrupt(
                    "restart 基线 receipt 早于 validate 落盘",
                ));
            }
            if commit_digest == [0u8; 32] {
                return Err(SyncError::Object("restart 基线 commit 摘要为空"));
            }
            if let Some(existing) = entry.commit_digest {
                if existing != commit_digest {
                    return Err(SyncError::LocalCorrupt(
                        "restart 基线 commit receipt 被改写",
                    ));
                }
            }
            entry.commit_digest = Some(commit_digest);
            entry.published_at = Some(published_at);
            entry.stage = entry.stage.max(RestartBaselineStage::Promoted);
        }
        self.save(vault)
    }

    fn entry_mut(&mut self, role: &str) -> Result<&mut RestartBaselineEntry, SyncError> {
        self.entries
            .iter_mut()
            .find(|entry| entry.intent.role == role)
            .ok_or(SyncError::LocalCorrupt(
                "restart 基线 journal 缺 role intent",
            ))
    }

    fn encode(&self) -> Result<Vec<u8>, SyncError> {
        let mut fields = vec![
            (0, cbor::uint(RESTART_BASELINE_VERSION)),
            (1, cbor::bytes(self.vault_id.to_vec())),
            (
                2,
                cbor::array(self.entries.iter().map(baseline_entry_cde).collect()),
            ),
        ];
        if let Some(rotation_id) = self.rotation_id {
            fields.push((3, cbor::bytes(rotation_id.to_vec())));
        }
        if let Some(epoch) = self.to_epoch {
            fields.push((4, cbor::uint(epoch)));
        }
        if let Some(rev) = self.expected_control_rev {
            fields.push((5, cbor::uint(rev)));
        }
        fields.sort_by_key(|(key, _)| *key);
        cbor::encode(fields)
    }

    fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        let map = cbor::decode(bytes)?;
        let version = cbor::uint_field(&map, 0)?;
        if version != RESTART_BASELINE_VERSION {
            return Err(SyncError::LocalCorrupt("restart 基线 journal 版本不支持"));
        }
        let vault_id: VaultId = cbor::bytes_fixed(&map, 1)?;
        let rotation_id = cbor::opt_bytes_field(&map, 3)?
            .map(|value| fixed16(value.as_slice()))
            .transpose()?;
        let to_epoch = cbor::opt_uint_field(&map, 4)?;
        let expected_control_rev = cbor::opt_uint_field(&map, 5)?;
        let entries = cbor::array_field(&map, 2)?
            .iter()
            .map(decode_baseline_entry)
            .collect::<Result<Vec<_>, _>>()?;
        let mut seen = HashSet::new();
        for entry in &entries {
            if !seen.insert(entry.intent.role.clone()) {
                return Err(SyncError::LocalCorrupt("restart 基线 journal role 重复"));
            }
            if entry.stage >= RestartBaselineStage::Created && entry.lease.is_none() {
                return Err(SyncError::LocalCorrupt(
                    "restart 基线 journal 阶段越过 create 但缺 lease",
                ));
            }
            if entry.stage >= RestartBaselineStage::ObjectsUploaded && entry.prepared.is_none() {
                return Err(SyncError::LocalCorrupt(
                    "restart 基线 journal 阶段越过 upload 但缺 prepared",
                ));
            }
            if entry.stage >= RestartBaselineStage::Validated && entry.verified.is_none() {
                return Err(SyncError::LocalCorrupt(
                    "restart 基线 journal 阶段越过 validate 但缺 verified",
                ));
            }
            if entry.stage >= RestartBaselineStage::Promoted && entry.commit_digest.is_none() {
                return Err(SyncError::LocalCorrupt(
                    "restart 基线 journal 阶段越过 promote 但缺 commit receipt",
                ));
            }
        }
        Ok(Self {
            vault_id,
            rotation_id,
            to_epoch,
            expected_control_rev,
            entries,
        })
    }
}

fn baseline_entry_cde(entry: &RestartBaselineEntry) -> cbor::CdeValue {
    let mut fields = vec![
        (0, cbor::text(entry.intent.role.clone())),
        (1, cbor::uint(entry.intent.epoch)),
        (2, cbor::bytes(entry.intent.rotation_id.to_vec())),
        (3, cbor::bytes(entry.intent.request_id.to_vec())),
        (4, cbor::uint(entry.intent.expected_control_rev)),
        (5, cbor::uint(entry.intent.estimated_bytes)),
        (6, cbor::uint(entry.stage.code())),
    ];
    if let Some(lease) = &entry.lease {
        fields.push((7, checkpoint_lease_cde(lease)));
    }
    if let Some(prepared) = &entry.prepared {
        fields.push((8, cbor::bytes(prepared.clone())));
    }
    if let Some(verified) = &entry.verified {
        fields.push((9, cbor::bytes(verified.clone())));
    }
    if let Some(digest) = entry.commit_digest {
        fields.push((10, cbor::bytes(digest.to_vec())));
    }
    if let Some(published_at) = entry.published_at {
        fields.push((11, cbor::uint(published_at)));
    }
    fields.sort_by_key(|(key, _)| *key);
    cbor::CdeValue::Map(fields)
}

fn decode_baseline_entry(value: &cbor::CdeValue) -> Result<RestartBaselineEntry, SyncError> {
    let map = cbor::value_map(value)?;
    let role = cbor::text_field(&map, 0)?;
    if role != "primary" && role != "fallback" {
        return Err(SyncError::LocalCorrupt("restart 基线 journal role 非法"));
    }
    let lease = cbor::opt_field(&map, 7)
        .map(decode_checkpoint_lease)
        .transpose()?;
    Ok(RestartBaselineEntry {
        intent: RestartBaselineIntent {
            role,
            epoch: cbor::uint_field(&map, 1)?,
            rotation_id: cbor::bytes_fixed(&map, 2)?,
            request_id: cbor::bytes_fixed(&map, 3)?,
            expected_control_rev: cbor::uint_field(&map, 4)?,
            estimated_bytes: cbor::uint_field(&map, 5)?,
        },
        stage: RestartBaselineStage::from_code(cbor::uint_field(&map, 6)?)?,
        lease,
        prepared: cbor::opt_bytes_field(&map, 8)?,
        verified: cbor::opt_bytes_field(&map, 9)?,
        commit_digest: cbor::opt_bytes_field(&map, 10)?
            .map(|value| fixed32(value.as_slice()))
            .transpose()?,
        published_at: cbor::opt_uint_field(&map, 11)?,
    })
}

fn checkpoint_lease_cde(lease: &CheckpointLease) -> cbor::CdeValue {
    cbor::CdeValue::Map(vec![
        (0, cbor::bytes(lease.checkpoint_id.to_vec())),
        (1, cbor::uint(lease.epoch)),
        (2, cbor::uint(lease.covered_seq)),
        (3, cbor::bytes(lease.reservation_id.to_vec())),
        (4, cbor::uint(lease.expires_at)),
    ])
}

fn decode_checkpoint_lease(value: &cbor::CdeValue) -> Result<CheckpointLease, SyncError> {
    let map = cbor::value_map(value)?;
    Ok(CheckpointLease {
        checkpoint_id: cbor::bytes_fixed(&map, 0)?,
        epoch: cbor::uint_field(&map, 1)?,
        covered_seq: cbor::uint_field(&map, 2)?,
        reservation_id: cbor::bytes_fixed(&map, 3)?,
        expires_at: cbor::uint_field(&map, 4)?,
    })
}

fn fixed16(bytes: &[u8]) -> Result<[u8; 16], SyncError> {
    bytes
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("restart 基线 journal 16B 字段长度非法"))
}

fn fixed32(bytes: &[u8]) -> Result<[u8; 32], SyncError> {
    bytes
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("restart 基线 journal 32B 字段长度非法"))
}

/// `promote` 后的可交给轮换/activate 的基线。
#[derive(Debug, Clone)]
pub struct PromotedBaseline {
    pub checkpoint_id: [u8; 16],
    pub epoch: u64,
    pub covered_seq: u64,
    pub cursor: String,
    pub manifest_object_id: ObjectId,
    pub refs_digest: [u8; 32],
    /// relay 返回的发布时间（只有真正 publish 成功才有）。
    pub published_at: u64,
    /// 已签名的 `CheckpointCommit` 字节 SHA256（与 relay 持久化字节逐字节一致）。
    pub commit_digest: [u8; 32],
}

/// 从 relay 读回并验证后的 checkpoint。
#[derive(Debug, Clone)]
pub struct VerifiedCheckpoint {
    /// `(doc_id, 正文 text；结构文档为 None)`。
    pub docs: Vec<(String, Option<String>)>,
    /// 资产数量。
    pub asset_count: usize,
    /// manifest 与 refs_digest 是否一致。
    pub manifest_checked: bool,
}

/// restart 基线 durable 编码版本（v2 起带 `ExportFootprint.coverage_epoch`）。
const PREPARED_DURABLE_VERSION: u64 = 2;

/// `VerifiedBaseline` durable receipt 版本（独立验证修订号）。
///
/// v1/v2 的 verified 字节只证明"旧实现的检查通过"（coverage 从未下载 ASSET），
/// 不得作为新链的已验证基线被 restart/rotation 快捷恢复消费。v3 追加覆盖复核的
/// 私有验证事实（durable 资产数/实证候选数/证明摘要）。v4（task-122）再追加
/// `coverage_epoch` 与 `validation_digest`，与签名 commit 的 v2 字段逐字节绑定。
/// 旧修订**拒绝**，禁止把旧字节改号升级。
const VERIFIED_DURABLE_VERSION: u64 = 4;

impl PreparedCheckpoint {
    /// 把 prepare 产物（公开 report + 私有导出点事实）编码成可 durable 的字节。
    ///
    /// 只在 restart 候选基线里落盘；普通 active epoch 的 prepare 不写这里。
    pub fn encode_durable(&self) -> Result<Vec<u8>, SyncError> {
        let report = &self.report;
        let footprint = &self.footprint;
        let mut fields = vec![
            (0, cbor::uint(PREPARED_DURABLE_VERSION)),
            (1, cbor::bytes(report.checkpoint_id.to_vec())),
            (2, cbor::uint(report.epoch)),
            (3, cbor::uint(report.covered_seq)),
            (
                4,
                cbor::array(report.docs.iter().map(doc_snapshot_ref_cde).collect()),
            ),
            (
                5,
                cbor::array(report.assets.iter().map(asset_ref_cde).collect()),
            ),
            (6, uploaded_object_cde(&report.manifest)),
            (7, cbor::bytes(report.refs_digest.to_vec())),
            (9, cbor::boolean(report.validated)),
            (10, cbor::bytes(footprint.checkpoint_id.to_vec())),
            (11, cbor::uint(footprint.epoch)),
            (12, cbor::uint(footprint.covered_seq)),
            (
                13,
                cbor::array(footprint.docs.iter().map(doc_footprint_cde).collect()),
            ),
            (14, cbor::bytes(footprint.structure_vv.clone())),
            (15, cbor::bytes(footprint.asset_set_digest.to_vec())),
            (
                16,
                cbor::array(
                    footprint
                        .asset_object_ids
                        .iter()
                        .map(|object_id| cbor::bytes(object_id.to_vec()))
                        .collect(),
                ),
            ),
            // v2：coverage 扫描绑定的冻结 relay epoch（与 checkpoint epoch 可不同）。
            (17, cbor::uint(footprint.coverage_epoch)),
        ];
        if let Some(published_at) = report.published_at {
            fields.push((8, cbor::uint(published_at)));
        }
        fields.sort_by_key(|(key, _)| *key);
        cbor::encode(fields)
    }

    /// 从 durable 字节恢复 prepare 产物；任何字段不自洽都 fail-closed。
    pub fn decode_durable(bytes: &[u8]) -> Result<Self, SyncError> {
        let map = cbor::decode(bytes)?;
        if cbor::uint_field(&map, 0)? != PREPARED_DURABLE_VERSION {
            return Err(SyncError::LocalCorrupt("prepared durable 版本不支持"));
        }
        let checkpoint_id: [u8; 16] = cbor::bytes_fixed(&map, 1)?;
        let epoch = cbor::uint_field(&map, 2)?;
        let covered_seq = cbor::uint_field(&map, 3)?;
        let docs = cbor::array_field(&map, 4)?
            .iter()
            .map(decode_doc_snapshot_ref)
            .collect::<Result<Vec<_>, _>>()?;
        let assets = cbor::array_field(&map, 5)?
            .iter()
            .map(decode_asset_ref)
            .collect::<Result<Vec<_>, _>>()?;
        let manifest = decode_uploaded_object(cbor::field(&map, 6)?)?;
        let refs_digest: [u8; 32] = cbor::bytes_fixed(&map, 7)?;
        let published_at = cbor::opt_uint_field(&map, 8)?;
        let validated = cbor::bool_field(&map, 9)?;
        let footprint_checkpoint_id: [u8; 16] = cbor::bytes_fixed(&map, 10)?;
        let footprint_epoch = cbor::uint_field(&map, 11)?;
        let footprint_covered_seq = cbor::uint_field(&map, 12)?;
        let doc_footprints = cbor::array_field(&map, 13)?
            .iter()
            .map(decode_doc_footprint)
            .collect::<Result<Vec<_>, _>>()?;
        let structure_vv = cbor::bytes_field(&map, 14)?;
        let asset_set_digest: [u8; 32] = cbor::bytes_fixed(&map, 15)?;
        let asset_object_ids = cbor::array_field(&map, 16)?
            .iter()
            .map(|value| {
                cbor::value_bytes(value)?
                    .as_slice()
                    .try_into()
                    .map_err(|_| SyncError::LocalCorrupt("prepared asset object id 长度非法"))
            })
            .collect::<Result<Vec<ObjectId>, _>>()?;
        // v2 必填：缺失时 fail-closed（不能默默回落到 checkpoint epoch，那正是
        // "候选基线的 coverage 用 to_epoch 校验 from_epoch 日志"的原始缺陷）。
        let coverage_epoch = cbor::uint_field(&map, 17)?;
        // 交叉校验：report 是公开字段，durable 恢复后仍必须与私有导出点绑定。
        if checkpoint_id != footprint_checkpoint_id
            || epoch != footprint_epoch
            || covered_seq != footprint_covered_seq
        {
            return Err(SyncError::LocalCorrupt(
                "prepared durable 的 report 与导出点不一致",
            ));
        }
        // 候选基线的 coverage 冻结在 from_epoch（= epoch-1），普通基线等于自身 epoch；
        // 任何 "coverage_epoch > epoch" 都是被改写/跨代错配，不能作为扫描锚点。
        if coverage_epoch > epoch {
            return Err(SyncError::LocalCorrupt(
                "prepared durable 的 coverage_epoch 超出 checkpoint epoch",
            ));
        }
        if validated || published_at.is_some() {
            return Err(SyncError::LocalCorrupt(
                "prepared durable 不能携带 validate/publish 之后的字段",
            ));
        }
        if doc_footprints.len() != docs.len()
            || doc_footprints
                .iter()
                .any(|footprint| !docs.iter().any(|doc| doc.doc_id == footprint.doc_id))
            || docs.iter().any(|doc| {
                !doc_footprints
                    .iter()
                    .any(|footprint| footprint.doc_id == doc.doc_id)
            })
        {
            return Err(SyncError::LocalCorrupt(
                "prepared durable 的文档集合与导出点不一致",
            ));
        }
        for doc in &docs {
            let footprint = doc_footprints
                .iter()
                .find(|footprint| footprint.doc_id == doc.doc_id)
                .ok_or(SyncError::LocalCorrupt("prepared durable 缺文档导出点"))?;
            if footprint.kind != doc.kind || footprint.generation != doc.generation {
                return Err(SyncError::LocalCorrupt(
                    "prepared durable 的文档 kind/generation 不一致",
                ));
            }
        }
        if asset_object_ids.len() != assets.len()
            || asset_object_ids.iter().any(|object_id| {
                !assets
                    .iter()
                    .any(|asset| asset.object.object_id == *object_id)
            })
        {
            return Err(SyncError::LocalCorrupt(
                "prepared durable 的资产集合与导出点不一致",
            ));
        }
        Ok(Self {
            report: CheckpointReport {
                checkpoint_id,
                epoch,
                covered_seq,
                docs,
                assets,
                manifest,
                refs_digest,
                published_at,
                validated,
                commit_digest: None,
            },
            footprint: ExportFootprint {
                checkpoint_id,
                epoch,
                coverage_epoch,
                covered_seq,
                docs: doc_footprints,
                structure_vv,
                asset_set_digest,
                asset_object_ids,
            },
        })
    }
}

impl VerifiedBaseline {
    /// 把 validate 的不可变事实编码成可 durable 的字节（receipt）。
    pub fn encode_durable(&self) -> Result<Vec<u8>, SyncError> {
        let facts = &self.facts;
        cbor::encode(vec![
            (0, cbor::uint(VERIFIED_DURABLE_VERSION)),
            (1, cbor::bytes(facts.vault_id.to_vec())),
            (2, cbor::bytes(facts.checkpoint_id.to_vec())),
            (3, cbor::uint(facts.epoch)),
            (4, cbor::uint(facts.covered_seq)),
            (5, cbor::text(facts.cursor.clone())),
            (6, cbor::bytes(facts.manifest_object_id.to_vec())),
            (7, cbor::bytes(facts.manifest_digest.to_vec())),
            (8, cbor::bytes(facts.refs_digest.to_vec())),
            (
                9,
                cbor::uint(u64::try_from(facts.imported_docs).unwrap_or(u64::MAX)),
            ),
            (10, cbor::bytes(facts.imported_vv_digest.to_vec())),
            (
                11,
                cbor::uint(u64::try_from(facts.verified_assets).unwrap_or(u64::MAX)),
            ),
            (
                12,
                cbor::uint(u64::try_from(facts.coverage_durable_assets).unwrap_or(u64::MAX)),
            ),
            (
                13,
                cbor::uint(u64::try_from(facts.coverage_verified_candidates).unwrap_or(u64::MAX)),
            ),
            (14, cbor::bytes(facts.coverage_proof_digest.to_vec())),
            // v4（task-122）：coverage 冻结 epoch + 验证修订 2 的证据摘要。
            (15, cbor::uint(facts.coverage_epoch)),
            (16, cbor::bytes(facts.validation_digest.to_vec())),
        ])
    }

    /// 从 durable receipt 恢复验证事实；调用方仍须核对 cid/epoch/vault。
    pub fn decode_durable(bytes: &[u8]) -> Result<Self, SyncError> {
        let map = cbor::decode(bytes)?;
        if cbor::uint_field(&map, 0)? != VERIFIED_DURABLE_VERSION {
            return Err(SyncError::LocalCorrupt(
                "verified durable 版本不支持（旧修订必须完整重验，禁止改号升级）",
            ));
        }
        let imported_docs = cbor::uint_field(&map, 9)?
            .try_into()
            .map_err(|_| SyncError::LocalCorrupt("verified imported_docs 溢出"))?;
        let verified_assets = cbor::uint_field(&map, 11)?
            .try_into()
            .map_err(|_| SyncError::LocalCorrupt("verified verified_assets 溢出"))?;
        let coverage_durable_assets = cbor::uint_field(&map, 12)?
            .try_into()
            .map_err(|_| SyncError::LocalCorrupt("verified coverage_durable_assets 溢出"))?;
        let coverage_verified_candidates = cbor::uint_field(&map, 13)?
            .try_into()
            .map_err(|_| SyncError::LocalCorrupt("verified coverage_verified_candidates 溢出"))?;
        Ok(Self {
            facts: BaselineFacts {
                vault_id: cbor::bytes_fixed(&map, 1)?,
                checkpoint_id: cbor::bytes_fixed(&map, 2)?,
                epoch: cbor::uint_field(&map, 3)?,
                covered_seq: cbor::uint_field(&map, 4)?,
                cursor: cbor::text_field(&map, 5)?,
                manifest_object_id: cbor::bytes_fixed(&map, 6)?,
                manifest_digest: cbor::bytes_fixed(&map, 7)?,
                refs_digest: cbor::bytes_fixed(&map, 8)?,
                imported_docs,
                imported_vv_digest: cbor::bytes_fixed(&map, 10)?,
                verified_assets,
                coverage_durable_assets,
                coverage_verified_candidates,
                coverage_proof_digest: cbor::bytes_fixed(&map, 14)?,
                coverage_epoch: cbor::uint_field(&map, 15)?,
                validation_digest: cbor::bytes_fixed(&map, 16)?,
            },
        })
    }
}

/// 本地 durable "已发布基线验证回执" 的 meta 键前缀。
///
/// restart 快捷恢复（`rotation_restart::baseline_from_journal`）只认这里的新修订
/// 回执；旧 journal 的 `verified=true` 没有这份回执 → 拒绝，必须完整重验/重建。
pub(crate) const VERIFIED_RECEIPT_META_PREFIX: &str = "sync.checkpoint.verified_receipt.";
/// 回执格式修订号：task-122 起 v2（带 HMAC + validation_digest）。旧 v1 一律不得消费。
const VERIFIED_RECEIPT_MAGIC: &[u8] = b"MN/cp-verified/2\0";

/// promote 成功后落盘的本地回执：把"这份基线的验证/发布事实"与 commit digest 绑定。
///
/// task-122：整个 body 用 `K_receipt`（epoch 根 HKDF 派生）做 HMAC-SHA256 认证；
/// 只认 MAC 通过的字节。旧 v1（只有 SHA256 校验和）无法证明没有被本地篡改，必须拒绝。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedReceipt {
    /// checkpoint id。
    pub(crate) checkpoint_id: [u8; 16],
    /// vault 身份。
    pub(crate) vault_id: VaultId,
    /// epoch。
    pub(crate) epoch: u64,
    /// 固定 S。
    pub(crate) covered_seq: u64,
    /// `CheckpointCommit` 编码的 SHA256（与 journal 里的 commit_digest 对齐）。
    pub(crate) commit_digest: [u8; 32],
    /// v2 commit 的验证证据摘要（与签名 commit 逐字节绑定）。
    pub(crate) validation_digest: [u8; 32],
}

fn verified_receipt_key(checkpoint_id: &[u8; 16]) -> String {
    format!(
        "{VERIFIED_RECEIPT_META_PREFIX}{}",
        crate::base64url::encode(checkpoint_id)
    )
}

impl VerifiedReceipt {
    fn encode(&self, key: &Secret32) -> Result<Vec<u8>, SyncError> {
        let mut out =
            Vec::with_capacity(VERIFIED_RECEIPT_MAGIC.len() + 16 + 16 + 8 + 8 + 32 + 32 + 32);
        out.extend_from_slice(VERIFIED_RECEIPT_MAGIC);
        out.extend_from_slice(&self.vault_id);
        out.extend_from_slice(&self.checkpoint_id);
        out.extend_from_slice(&self.epoch.to_be_bytes());
        out.extend_from_slice(&self.covered_seq.to_be_bytes());
        out.extend_from_slice(&self.commit_digest);
        out.extend_from_slice(&self.validation_digest);
        let mac = receipt_mac(key, crate::domain::CHECKPOINT_RECEIPT_KIND_VERIFIED, &out)?;
        out.extend_from_slice(&mac);
        Ok(out)
    }

    fn decode(bytes: &[u8], key: &Secret32) -> Result<Self, SyncError> {
        let fixed = VERIFIED_RECEIPT_MAGIC.len() + 16 + 16 + 8 + 8 + 32 + 32 + 32;
        if bytes.len() != fixed || !bytes.starts_with(VERIFIED_RECEIPT_MAGIC) {
            return Err(SyncError::LocalCorrupt(
                "verified receipt 版本不支持（旧修订必须完整重验）",
            ));
        }
        let (body, mac) = bytes.split_at(bytes.len() - 32);
        if !receipt_mac_verify(
            key,
            crate::domain::CHECKPOINT_RECEIPT_KIND_VERIFIED,
            body,
            mac,
        ) {
            return Err(SyncError::LocalCorrupt(
                "verified receipt HMAC 不符（伪造/篡改或根不匹配）",
            ));
        }
        let mut input = &body[VERIFIED_RECEIPT_MAGIC.len()..];
        Ok(Self {
            vault_id: take_array::<16>(&mut input)?,
            checkpoint_id: take_array::<16>(&mut input)?,
            epoch: take_u64(&mut input)?,
            covered_seq: take_u64(&mut input)?,
            commit_digest: take_array::<32>(&mut input)?,
            validation_digest: take_array::<32>(&mut input)?,
        })
    }
}

/// 读取本地 durable 验证回执；缺失返回 `None`（调用方必须 fail-closed）。
pub(crate) fn load_verified_receipt(
    vault: &menghunote_storage::Vault,
    root: &RootSecretV1,
    checkpoint_id: &[u8; 16],
) -> Result<Option<VerifiedReceipt>, SyncError> {
    let Some(bytes) = vault.get_meta(&verified_receipt_key(checkpoint_id))? else {
        return Ok(None);
    };
    let key = derive_checkpoint_receipt_key(root)?;
    let receipt = VerifiedReceipt::decode(&bytes, &key)?;
    if receipt.checkpoint_id != *checkpoint_id {
        return Err(SyncError::LocalCorrupt(
            "verified receipt 与 checkpoint id 不一致",
        ));
    }
    if receipt.epoch != root.key_epoch {
        return Err(SyncError::LocalCorrupt(
            "verified receipt 的 epoch 与校验根不一致",
        ));
    }
    Ok(Some(receipt))
}

fn uploaded_object_cde(object: &UploadedObject) -> cbor::CdeValue {
    cbor::CdeValue::Map(vec![
        (0, cbor::bytes(object.object_id.to_vec())),
        (1, cbor::uint(u64::from(object.part_count))),
        (2, cbor::uint(object.total_plain_len)),
        (3, cbor::bytes(object.object_digest.to_vec())),
    ])
}

fn decode_uploaded_object(value: &cbor::CdeValue) -> Result<UploadedObject, SyncError> {
    let map = cbor::value_map(value)?;
    Ok(UploadedObject {
        object_id: cbor::bytes_fixed(&map, 0)?,
        part_count: u32::try_from(cbor::uint_field(&map, 1)?)
            .map_err(|_| SyncError::LocalCorrupt("prepared part_count 溢出"))?,
        total_plain_len: cbor::uint_field(&map, 2)?,
        object_digest: cbor::bytes_fixed(&map, 3)?,
    })
}

fn doc_snapshot_ref_cde(doc: &DocSnapshotRef) -> cbor::CdeValue {
    cbor::CdeValue::Map(vec![
        (0, cbor::text(doc.doc_id.clone())),
        (1, cbor::uint(doc.kind)),
        (2, cbor::uint(doc.generation)),
        (3, cbor::bytes(doc.vv.clone())),
        (4, cbor::bytes(doc.frontier.clone())),
        (5, uploaded_object_cde(&doc.object)),
    ])
}

fn decode_doc_snapshot_ref(value: &cbor::CdeValue) -> Result<DocSnapshotRef, SyncError> {
    let map = cbor::value_map(value)?;
    Ok(DocSnapshotRef {
        doc_id: cbor::text_field(&map, 0)?,
        kind: cbor::uint_field(&map, 1)?,
        generation: cbor::uint_field(&map, 2)?,
        vv: cbor::bytes_field(&map, 3)?,
        frontier: cbor::bytes_field(&map, 4)?,
        object: decode_uploaded_object(cbor::field(&map, 5)?)?,
    })
}

fn asset_ref_cde(asset: &AssetRef) -> cbor::CdeValue {
    cbor::CdeValue::Map(vec![
        (0, cbor::bytes(asset.real_sha256.to_vec())),
        (1, cbor::uint(asset.byte_len)),
        (2, cbor::text(asset.media_type.clone())),
        (3, uploaded_object_cde(&asset.object)),
    ])
}

fn decode_asset_ref(value: &cbor::CdeValue) -> Result<AssetRef, SyncError> {
    let map = cbor::value_map(value)?;
    Ok(AssetRef {
        real_sha256: cbor::bytes_fixed(&map, 0)?,
        byte_len: cbor::uint_field(&map, 1)?,
        media_type: cbor::text_field(&map, 2)?,
        object: decode_uploaded_object(cbor::field(&map, 3)?)?,
    })
}

fn doc_footprint_cde(footprint: &DocFootprint) -> cbor::CdeValue {
    cbor::CdeValue::Map(vec![
        (0, cbor::text(footprint.doc_id.clone())),
        (1, cbor::uint(footprint.kind)),
        (2, cbor::uint(footprint.generation)),
        (3, cbor::uint(footprint.head_seq)),
        (4, cbor::bytes(footprint.head_vv.clone())),
        (5, cbor::bytes(footprint.oplog_vv.clone())),
    ])
}

fn decode_doc_footprint(value: &cbor::CdeValue) -> Result<DocFootprint, SyncError> {
    let map = cbor::value_map(value)?;
    Ok(DocFootprint {
        doc_id: cbor::text_field(&map, 0)?,
        kind: cbor::uint_field(&map, 1)?,
        generation: cbor::uint_field(&map, 2)?,
        head_seq: cbor::uint_field(&map, 3)?,
        head_vv: cbor::bytes_field(&map, 4)?,
        oplog_vv: cbor::bytes_field(&map, 5)?,
    })
}

struct PreparedDoc {
    doc_id: String,
    kind: u64,
    generation: u64,
    head_seq: u64,
    head_vv: Vec<u8>,
    oplog_vv: Vec<u8>,
    frontier: Vec<u8>,
    codec_version: String,
    app_doc_schema: u32,
    min_reader: u32,
    snapshot_bytes: Vec<u8>,
}

/// checkpoint 写入器（持 vault/客户端/身份/根）。
pub struct CheckpointWriter<'a, C: RelayClient> {
    vault: &'a menghunote_storage::Vault,
    client: &'a mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: DeviceSigningKey,
    root: RootSecretV1,
    peer_id: u64,
    /// 同一 writer 生命周期内已上传资产（object_id 由 real_sha256 决定，必须复用密文）。
    uploaded_assets: std::collections::HashMap<[u8; 32], AssetRef>,
    /// 候选 epoch 的固定 role（primary/fallback）；None 用于 active epoch。
    role: Option<String>,
    /// restart 候选基线的 `(rotation_id, expected_control_rev)`；设置后
    /// prepare/validate/promote 的每个阶段都写 [`RestartBaselineJournal`]。
    restart_baseline: Option<([u8; 16], u64)>,
    /// 候选 epoch 的 coverage 验证根（旧 active 根）。
    ///
    /// 候选基线在 `from_epoch` 冻结点导出，`<= S` 的 ASSET 日志事件是用**旧 active
    /// 根**加密的；manifest refs 里的资产则用**新 epoch 根**。两者不能混用，也绝不
    /// 能从 checkpoint 根"猜"旧根：缺这个字段时 coverage 复核显式暂停/报错。
    coverage_root: Option<RootSecretV1>,
}

/// `validate` 重算 refs_digest 用的公开引用集合（只含 snapshot/asset）。
fn manifest_refs_for_validate(manifest: &ManifestV1) -> Vec<ObjectRefV1> {
    let mut refs = Vec::with_capacity(manifest.docs.len() + manifest.assets.len());
    refs.extend(manifest.docs.iter().map(|doc| ObjectRefV1 {
        object_id: doc.snapshot_object_id,
        object_type: ObjectType::Snapshot,
        generation: doc.generation,
        key_epoch: doc.key_epoch,
        part_count: doc.part_count,
        total_plain_len: doc.total_plain_len,
        object_digest: doc.object_digest,
    }));
    refs.extend(manifest.assets.iter().map(|asset| ObjectRefV1 {
        object_id: asset.object_id,
        object_type: ObjectType::Asset,
        generation: 0,
        key_epoch: asset.key_epoch,
        part_count: asset.part_count,
        total_plain_len: asset.total_plain_len,
        object_digest: asset.object_digest,
    }));
    refs
}

/// task-122 消费者门禁：只接受当前验证修订（v2）的 `CheckpointCommit`。
///
/// 旧修订（v1）不是"兼容读取"：v1 的签名基线没有覆盖复核证据绑定，消费它等于静默
/// 接受旧安全基线。所有客户端/relay 消费入口都必须先过这里；未知版本在 decode 阶段
/// 已被 `ProtoError::UnsupportedVersion` 拒绝。
pub(crate) fn require_current_checkpoint_validation(
    commit: &CheckpointCommit,
) -> Result<(), SyncError> {
    commit
        .require_current_validation()
        .map_err(SyncError::Proto)
}

/// 从不可变验证事实重算 `CheckpointCommit` 摘要（restart receipt 恢复用）。
fn checkpoint_commit_digest_from_facts(
    vault_id: VaultId,
    producer_device_id: DeviceId,
    facts: &BaselineFacts,
) -> Result<[u8; 32], SyncError> {
    // 重算证据摘要并与 validate 记录的值对撞：journal/receipt 被篡改时这里先失败。
    if facts.validation_digest != facts.recomputed_validation_digest(&producer_device_id)? {
        return Err(SyncError::LocalCorrupt(
            "verified 事实的 validation_digest 与重算不符",
        ));
    }
    let commit = CheckpointCommit {
        vault_id,
        checkpoint_id: facts.checkpoint_id,
        epoch: facts.epoch,
        covered_relay_seq: facts.covered_seq,
        manifest_object_id: facts.manifest_object_id,
        manifest_digest: facts.manifest_digest,
        refs_digest: facts.refs_digest,
        producer_device_id,
        validation_version: CheckpointCommit::CURRENT_VALIDATION_VERSION,
        validation_digest: facts.validation_digest,
    };
    Ok(Sha256::digest(commit.encode()?).into())
}

/// ADR §20：上传初始槽所需的 IKM 材料（password 或恢复种子；可只给其一）。
#[derive(Clone, Copy, Default)]
pub struct SlotSecrets<'a> {
    /// 口令（password 槽的 Argon2 IKM）。
    pub password: Option<&'a str>,
    /// 恢复种子（recovery 槽的 IKM）。
    pub recovery_seed: Option<&'a crate::recovery::RecoverySeed>,
}

impl<'a, C: RelayClient> CheckpointWriter<'a, C> {
    /// 构造。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vault: &'a menghunote_storage::Vault,
        client: &'a mut C,
        vault_id: VaultId,
        device_id: DeviceId,
        signing_key: DeviceSigningKey,
        root: RootSecretV1,
        peer_id: u64,
    ) -> Self {
        Self {
            vault,
            client,
            vault_id,
            device_id,
            signing_key,
            root,
            peer_id,
            uploaded_assets: std::collections::HashMap::new(),
            role: None,
            restart_baseline: None,
            coverage_root: None,
        }
    }

    /// 候选 epoch 固定 role（primary/fallback）；创建 checkpoint 时传给 relay。
    pub fn with_role(mut self, role: &str) -> Self {
        self.role = Some(role.to_owned());
        self
    }

    /// 候选 epoch 的 coverage 验证根（旧 active 根）；普通同 epoch checkpoint 不设置
    /// （coverage_root = 当前根）。缺旧根时 coverage 复核不会猜根。
    pub fn with_coverage_root(mut self, root: RootSecretV1) -> Self {
        self.coverage_root = Some(root);
        self
    }

    /// restart 候选基线：启用 per-role create intent 与分阶段 durable receipt。
    ///
    /// `rotation_id`/`expected_control_rev` 来自 relay 分配后的 restart plan；
    /// 必须与 role 一起设置，否则 restart 基线会 fail-closed（拒绝无 intent fresh create）。
    pub fn with_restart_baseline(
        mut self,
        rotation_id: [u8; 16],
        expected_control_rev: u64,
    ) -> Self {
        self.restart_baseline = Some((rotation_id, expected_control_rev));
        self
    }

    /// 上传两个初始槽并校验 active/staging。
    pub fn upload_initial_slots(
        &mut self,
        password_slot: &KeySlotV1,
        recovery_slot: &KeySlotV1,
        secrets: &SlotSecrets<'_>,
    ) -> Result<SlotsReport, SyncError> {
        // 初始槽是一次性上传（改口令走独立的重包路径），沿用随机 message id；
        // 需要"可重跑"的发布基线用 [`Self::upload_recovery_slot`] 传入确定性 id。
        let password_message: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
        let password = self.put_slot(password_slot, secrets, password_message)?;
        let recovery_message: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
        let recovery = self.put_slot(recovery_slot, secrets, recovery_message)?;
        let before = self.client.list_key_slots()?;
        let active = before
            .iter()
            .find(|slot| slot.kind == password.kind && slot.state == "active");
        let active_rev = if active.is_some_and(|slot| slot.slot_id == password.slot_id) {
            password.slot_set_rev
        } else {
            let expected = before
                .iter()
                .filter(|slot| slot.kind == password.kind && slot.state == "active")
                .map(|slot| slot.slot_set_rev)
                .max()
                .unwrap_or(0);
            // §30.3：被签字节是 proto `SlotActivateBodyV1{0:expected,1:new_slot_id}`。
            let signed_message = menghunote_sync_proto::SlotActivateBodyV1 {
                expected_slot_set_rev: expected,
                new_slot_id: password.slot_id,
            }
            .encode()
            .map_err(SyncError::Proto)?;
            let signature = self.signing_key.sign(&signed_message);
            self.client.activate_password_slot(
                expected,
                password.slot_id,
                signed_message,
                signature,
            )?
        };
        let after = self.client.list_key_slots()?;
        let password_ok = after.iter().any(|slot| {
            slot.slot_id == password.slot_id
                && slot.state == "active"
                && slot.descriptor == password.descriptor
        });
        let recovery_ok = match after.iter().find(|slot| slot.slot_id == recovery.slot_id) {
            Some(slot) => slot.descriptor == recovery.descriptor && slot.kind == recovery.kind,
            // relay 的 `GET key-slots` 只返回 active/retiring；staging 的 recovery 槽
            // 不在列表里是正常现象，以 PUT 响应为准。
            None => recovery.state == "staging",
        };
        if !password_ok || !recovery_ok {
            return Err(SyncError::RelayWire("槽上传后 active/staging 与本地不一致"));
        }
        let password_record = after
            .iter()
            .find(|slot| slot.slot_id == password.slot_id)
            .cloned()
            .unwrap_or(password);
        let recovery_record = after
            .iter()
            .find(|slot| slot.slot_id == recovery.slot_id)
            .cloned()
            .unwrap_or(recovery);
        Ok(SlotsReport {
            password: password_record,
            recovery: recovery_record,
            active_password_rev: active_rev,
        })
    }

    /// 只上传**一个 recovery 槽**（staging），返回 relay 的槽记录。
    ///
    /// 没有口令的流程（ADR 0009 切片 4b 的 dev-local Create/发布基线）用这条入口：
    /// 与 [`Self::upload_initial_slots`] 共用同一条 `put_slot` 路径（KEY_SLOT MNE1 加密
    /// + `PUT key-slots` 登记），但**不触碰** password 槽与 `password:activate`。
    ///
    /// `message_id` 由调用方给定而不是随机：它进 MNE1 header/AAD，relay 对同
    /// `object_id` 换了 `message_id` 会返回 409 ID_CONFLICT。发布基线是**可重跑**的动作，
    /// 调用方必须按槽材料算出确定性 id（`dev_local` 的 `key_slot_message_id`），
    /// 否则"第一次传完槽、后面 checkpoint 失败"的重试会被 409 卡死。
    pub fn upload_recovery_slot(
        &mut self,
        recovery_slot: &KeySlotV1,
        seed: &crate::recovery::RecoverySeed,
        message_id: [u8; 16],
    ) -> Result<SlotRecord, SyncError> {
        if recovery_slot.descriptor.kind != menghunote_sync_proto::SlotKind::Recovery {
            return Err(SyncError::SlotKind {
                expected: "recovery",
                actual: "password",
            });
        }
        let secrets = SlotSecrets {
            password: None,
            recovery_seed: Some(seed),
        };
        self.put_slot(recovery_slot, &secrets, message_id)
    }

    /// 创建并验证一份 checkpoint（`prepare → validate → promote` 的便捷入口）。
    pub fn create_checkpoint(
        &mut self,
        assets: &dyn AssetSource,
    ) -> Result<CheckpointReport, SyncError> {
        let prepared = self.prepare(assets)?;
        let baseline = self.validate(&prepared)?;
        let promoted = self.promote(&baseline)?;
        let mut report = prepared.report;
        report.validated = true;
        report.published_at = Some(promoted.published_at);
        report.commit_digest = Some(promoted.commit_digest);
        Ok(report)
    }

    /// 上传/复用一个 ASSET 对象（checkpoint 路径）。
    ///
    /// relay 对**同字节**重放是幂等的（task-121：逐字节相同的分片与 reservation 无关，
    /// 对象只存一份、不重复计费），对**补缺失分片**仍要求"同一对象的全部分片来自同一
    /// 预算来源"。所以：
    /// - relay 上**没有**该对象 → 用自己的维护 reservation 上传全部缺失分片；
    /// - relay 上**已有同一片**（普通同步/上一份 checkpoint/轮换传过）→ 直接按维护
    ///   reservation 重放，relay 回 200 duplicate；不需要预先 GET；
    /// - relay 上**部分分片**且对象属于另一个预算来源（例如轮换传了一半、rotation
    ///   reservation 已失效）→ 首次 PUT 返回 `ID_CONFLICT`，再按确定性密文逐片读回
    ///   校验 + 提交；缺失片仍按对象既有预算来源补齐（普通预算对象才允许普通 PUT）。
    ///   分片内容确定性相同，不会混拼两份密文。
    fn sync_asset_object(
        &mut self,
        object_id: &ObjectId,
        message_id: [u8; 16],
        encrypted: &EncryptedObjectParts,
        reservation: &[u8; 16],
    ) -> Result<UploadedObject, SyncError> {
        // 先按维护预算直接上传（PUT 对同字节幂等，重复片不重复计费）：
        // - 全新对象 / 同一 lease 的中断续传 / 轮换期已由 rotation reservation 写好的
        //   完整对象：一次成功，不做任何 GET（validate 的 "refs 资产只读回一次"依赖
        //   prepare 不预先探测）；同字节跨 reservation 的幂等复用见 task-121；
        // - 对象只写了一半、且已有分片属于别的预算来源：relay 报 ID_CONFLICT，再走复用探测。
        let mut saw_conflict = false;
        for (part_no, envelope, _) in &encrypted.parts {
            match self.client.put_part_with_reservation(
                object_id,
                *part_no,
                envelope,
                Some(reservation),
            ) {
                Ok(()) => {}
                Err(error) if asset_sync::is_id_conflict(&error) => {
                    saw_conflict = true;
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if !saw_conflict {
            self.submit_asset_object(object_id, message_id, encrypted)?;
            return Ok(uploaded_object_from_parts(object_id, encrypted));
        }

        // 复用已有对象：逐片按确定性密文校验，只提交（幂等），缺失片按对象既有
        // 预算来源补齐（普通预算对象才允许普通 PUT；维护预算对象在别人的
        // reservation 下无法续传，直接失败）。
        let Some(first) = asset_sync::read_part(self.client, object_id, 0)? else {
            return Err(SyncError::LocalCorrupt(
                "checkpoint 资产对象 ID_CONFLICT 但第 0 片不存在",
            ));
        };
        // 确定性加密：同 object_id 的分片字节必须一致；不一致 fail-closed。
        if first != encrypted.parts[0].1 {
            return Err(SyncError::Object(
                "checkpoint 资产对象已有密文与本地确定性密文不一致",
            ));
        }
        let mut missing = Vec::new();
        for (part_no, envelope, _) in encrypted.parts.iter().skip(1) {
            match asset_sync::read_part(self.client, object_id, *part_no)? {
                Some(stored) => {
                    if stored != *envelope {
                        return Err(SyncError::Object(
                            "checkpoint 资产对象已有分片与本地确定性密文不一致",
                        ));
                    }
                }
                None => missing.push((*part_no, envelope)),
            }
        }
        if !missing.is_empty() {
            // 普通预算对象（普通同步中断留下的）才允许无 reservation 续传；若对象
            // 属于其它维护 reservation，这里会再次 ID_CONFLICT 并 fail-closed。
            for (part_no, envelope) in &missing {
                self.client.put_part(object_id, *part_no, envelope)?;
            }
        }
        self.submit_asset_object(object_id, message_id, encrypted)?;
        Ok(uploaded_object_from_parts(object_id, encrypted))
    }

    /// 提交一个 checkpoint ASSET 对象（`ObjectCommit` 全字段确定性派生）。
    fn submit_asset_object(
        &mut self,
        object_id: &ObjectId,
        message_id: [u8; 16],
        encrypted: &EncryptedObjectParts,
    ) -> Result<(), SyncError> {
        submit_prepared_object(
            self.client,
            self.vault_id,
            &self.signing_key,
            ObjectType::Asset,
            *object_id,
            message_id,
            0,
            self.root.key_epoch,
            0,
            encrypted.part_count,
            encrypted.total_plain_len,
            &encrypted.object_digest,
        )
    }

    /// prepare：只固定 S（lease）并导出/上传对象与 refs。
    ///
    /// ADR §32：**不 publish、不签最终 CheckpointCommit**；返回值不可作为可驱动 GC
    /// 的基线，必须再经 [`Self::validate`] 与 [`Self::promote`]。
    pub fn prepare(&mut self, assets: &dyn AssetSource) -> Result<PreparedCheckpoint, SyncError> {
        let epoch = self.root.key_epoch;
        // restart 候选基线：先看 durable journal；upload 已完成时绝不重新 create/upload。
        let mut baseline_journal = self.open_restart_baseline_journal(epoch)?;
        if let Some(prepared) = self.resume_uploaded_baseline(baseline_journal.as_mut())? {
            return Ok(prepared);
        }
        let asset_metas = assets.list(self.vault)?;
        let estimated = asset_metas
            .iter()
            .map(|asset| asset.byte_len)
            .sum::<u64>()
            .saturating_add(4 << 20);
        // coverage 冻结在 prepare 时 relay 的 **active** epoch。候选基线导出的是
        // from_epoch 冻结点上的本地事实，activate 前 relay 仍是 from_epoch；把它记进
        // footprint/receipt，validate 才能用正确的 epoch 校验 relay 日志事件。
        let relay_state = self.client.state()?;
        if relay_state.vault_id != self.vault_id {
            return Err(SyncError::Object(
                "prepare: coverage scan 的 relay vault 与本地不一致",
            ));
        }
        let coverage_epoch = relay_state.epoch;
        // 先固定 S，再导出；此后 checkpoint 的内容覆盖到 S 为止。
        // restart 路径在 create **之前**把 per-role intent 落盘，且重试复用同一 request_id。
        let lease = self.acquire_baseline_lease(baseline_journal.as_mut(), epoch, estimated)?;
        let covered_seq = lease.covered_seq;
        let prepared = self.prepare_docs()?;
        if prepared.is_empty() {
            return Err(SyncError::LocalCorrupt(
                "checkpoint 需要至少 structure 文档",
            ));
        }
        // 导出点冻结：head VV 与导出的 oplog VV 必须一致；不一致说明本地重放/导出
        // 已经不可信，不能让 checkpoint 带着"自相信"的 VV 上传。
        let mut doc_footprints = Vec::with_capacity(prepared.len());
        for doc in &prepared {
            // 空 VV 在库里是 NULL（空字节），Loro 的 `encode()` 却是 `[0]`；按语义比较。
            if decode_vv(&doc.head_vv)? != decode_vv(&doc.oplog_vv)? {
                return Err(SyncError::LocalCorrupt(
                    "导出快照的 oplog VV 与本地 head VV 不一致",
                ));
            }
            doc_footprints.push(DocFootprint {
                doc_id: doc.doc_id.clone(),
                kind: doc.kind,
                generation: doc.generation,
                head_seq: doc.head_seq,
                head_vv: doc.head_vv.clone(),
                oplog_vv: doc.oplog_vv.clone(),
            });
        }
        doc_footprints.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
        let structure_vv = doc_footprints
            .iter()
            .find(|doc| doc.doc_id == STRUCTURE_DOC_ID && doc.kind == KIND_STRUCTURE)
            .map(|doc| doc.oplog_vv.clone())
            .ok_or(SyncError::LocalCorrupt("checkpoint 缺少 structure 文档"))?;
        let asset_set_digest = asset_set_digest_from_metas(&asset_metas);

        let mut docs = Vec::with_capacity(prepared.len());
        for doc in &prepared {
            let snapshot = SnapshotV1 {
                doc_id: doc.doc_id.clone(),
                kind: doc.kind,
                generation: doc.generation,
                codec_version: doc.codec_version.clone(),
                app_doc_schema: doc.app_doc_schema,
                min_reader: doc.min_reader,
                full_history: true,
                vv: doc.oplog_vv.clone(),
                frontier: doc.frontier.clone(),
                loro_snapshot_bytes: doc.snapshot_bytes.clone(),
            };
            let bytes = snapshot.encode()?;
            let message_id: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
            let object = upload_object_with_reservation(
                self.client,
                self.vault_id,
                self.device_id,
                &self.signing_key,
                &self.root,
                ObjectType::Snapshot,
                crate::random::array(&mut crate::random::OsRandom)?,
                message_id,
                doc.generation,
                0,
                &bytes,
                &[],
                None,
                &lease.reservation_id,
            )?;
            docs.push(DocSnapshotRef {
                doc_id: doc.doc_id.clone(),
                kind: doc.kind,
                generation: doc.generation,
                vv: doc.oplog_vv.clone(),
                frontier: doc.frontier.clone(),
                object,
            });
        }

        let mut asset_refs = Vec::with_capacity(asset_metas.len());
        for meta in &asset_metas {
            // 同一 writer 生命周期内同资产只上传/复用一次。
            if let Some(cached) = self.uploaded_assets.get(&meta.real_sha256).cloned() {
                if cached.byte_len == meta.byte_len && cached.media_type == meta.media_type {
                    asset_refs.push(cached);
                    continue;
                }
            }
            let total_plain_len = asset_sync::asset_plain_len(meta.byte_len, &meta.media_type)?;
            let part_count = u32::try_from(part_count_for(total_plain_len, ObjectType::Asset))
                .map_err(|_| SyncError::LocalCorrupt("资产分片数超出 u32"))?;
            let object_id = asset_object_id(
                &self.root,
                &meta.real_sha256,
                meta.byte_len,
                &meta.media_type,
            )?;
            // 普通同步 / 上一份 checkpoint / 轮换若已上传并提交过同一确定性对象，直接
            // 复用 durable 标记：不再重传分片，也不再申请一份新的 reservation 预算。
            // （ASSET 的 object_id/nonce/message_id 都由根与完整明文头确定性派生，
            // 同一 object_id 只可能对应逐字节相同的密文。）
            if let Some(fact) = asset_sync::read_uploaded(self.vault, &object_id)? {
                if fact.key_epoch == self.root.key_epoch
                    && fact.part_count == part_count
                    && fact.total_plain_len == total_plain_len
                {
                    let entry = AssetRef {
                        real_sha256: meta.real_sha256,
                        byte_len: meta.byte_len,
                        media_type: meta.media_type.clone(),
                        object: fact.uploaded_object(),
                    };
                    self.uploaded_assets.insert(meta.real_sha256, entry.clone());
                    asset_refs.push(entry);
                    continue;
                }
            }
            let data = assets.read(self.vault, meta)?;
            let real: [u8; 32] = Sha256::digest(&data).into();
            if real != meta.real_sha256 || data.len() as u64 != meta.byte_len {
                return Err(SyncError::Object("资产字节与元数据不一致"));
            }
            let plaintext = asset_plaintext(&real, meta.byte_len, &meta.media_type, &data)?;
            let encrypted = encrypt_asset_parts(&self.root, self.vault_id, object_id, &plaintext)?;
            if encrypted.total_plain_len != total_plain_len || encrypted.part_count != part_count {
                return Err(SyncError::LocalCorrupt("资产确定性分片事实与元数据不一致"));
            }
            // ASSET 重传必须字节一致（ADR §19/§31）：object_id 已提交完整明文头，
            // nonce 与 message_id 都确定性派生。
            let message_id = crate::upload::asset_message_id(&self.root, &object_id)?;
            let object =
                self.sync_asset_object(&object_id, message_id, &encrypted, &lease.reservation_id)?;
            asset_sync::mark_uploaded(
                self.vault,
                &asset_sync::AssetUploadedFact {
                    object_id,
                    part_count: object.part_count,
                    total_plain_len: object.total_plain_len,
                    object_digest: object.object_digest,
                    key_epoch: self.root.key_epoch,
                },
            )?;
            let entry = AssetRef {
                real_sha256: real,
                byte_len: meta.byte_len,
                media_type: meta.media_type.clone(),
                object,
            };
            self.uploaded_assets.insert(real, entry.clone());
            asset_refs.push(entry);
        }
        let mut asset_object_ids: Vec<ObjectId> = asset_refs
            .iter()
            .map(|asset| asset.object.object_id)
            .collect();
        asset_object_ids.sort();

        let manifest_docs = docs
            .iter()
            .map(|doc| ManifestDoc {
                doc_id: doc.doc_id.clone(),
                kind: doc.kind,
                generation: doc.generation,
                key_epoch: epoch,
                vv: doc.vv.clone(),
                frontier: doc.frontier.clone(),
                snapshot_object_id: doc.object.object_id,
                part_count: doc.object.part_count,
                total_plain_len: doc.object.total_plain_len,
                object_digest: doc.object.object_digest,
            })
            .collect();
        let manifest_assets = asset_refs
            .iter()
            .map(|asset| ManifestAsset {
                real_sha256: asset.real_sha256,
                byte_len: asset.byte_len,
                media_type: asset.media_type.clone(),
                object_id: asset.object.object_id,
                key_epoch: epoch,
                part_count: asset.object.part_count,
                total_plain_len: asset.object.total_plain_len,
                object_digest: asset.object.object_digest,
            })
            .collect();
        let min_reader = prepared
            .iter()
            .map(|doc| doc.min_reader)
            .min()
            .ok_or(SyncError::LocalCorrupt("checkpoint 没有文档"))?;
        let manifest = ManifestV1 {
            vault_id: self.vault_id,
            key_epoch: epoch,
            checkpoint_id: lease.checkpoint_id,
            covered_relay_seq: covered_seq,
            min_reader,
            structure_doc_id: STRUCTURE_DOC_ID.to_owned(),
            docs: manifest_docs,
            assets: manifest_assets,
        };
        let manifest_bytes = manifest.encode()?;
        let manifest_message: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
        let manifest_object = upload_object_with_reservation(
            self.client,
            self.vault_id,
            self.device_id,
            &self.signing_key,
            &self.root,
            ObjectType::Manifest,
            crate::random::array(&mut crate::random::OsRandom)?,
            manifest_message,
            0,
            0,
            &manifest_bytes,
            &[],
            None,
            &lease.reservation_id,
        )?;

        let data_refs = checkpoint_data_refs(&docs, &asset_refs, epoch);
        let refs_digest: [u8; 32] = Sha256::digest(refs_digest_input(&data_refs)?).into();

        // ADR §32：prepare 到此为止——不签最终 commit、不 publish。publish 只在
        // promote（validate 全部通过之后）发生，否则候选对象绝不能驱动 GC/bootstrap。
        let report = CheckpointReport {
            checkpoint_id: lease.checkpoint_id,
            epoch,
            covered_seq,
            docs,
            assets: asset_refs,
            manifest: manifest_object,
            refs_digest,
            published_at: None,
            validated: false,
            commit_digest: None,
        };
        let footprint = ExportFootprint {
            checkpoint_id: lease.checkpoint_id,
            epoch,
            coverage_epoch,
            covered_seq,
            docs: doc_footprints,
            structure_vv,
            asset_set_digest,
            asset_object_ids,
        };
        let prepared = PreparedCheckpoint { report, footprint };
        // 评审 0003 P0-B：prepare 只把"固定起点 + 缺口里的完整 ASSET 声明"落成
        // pending 候选，**不生成成功覆盖证明**。结构/分页/写盘异常显式返回（不得吞掉
        // 证明错误）；validate 才是安全门：候选必须逐条实证后才合并覆盖区间。
        self.try_record_coverage_receipt(&prepared.footprint)?;
        // 评审 0003 P1-D：**先** durable 已上传对象/prepared，**再**登记 refs。
        // 之后任何阶段被杀（refs 已登记/未登记）都按同一 cid/lease 恢复：
        // 已登记的 refs 重放是幂等的（同 object_id/role 被 relay 忽略），绝不重新 create。
        if let Some(journal) = baseline_journal.as_mut() {
            let role = self
                .role
                .as_deref()
                .ok_or(SyncError::LocalCorrupt("restart 基线缺固定 role"))?;
            journal.record_uploaded(self.vault, role, prepared.encode_durable()?)?;
        }
        self.register_prepared_refs(&prepared)?;
        if let Some(journal) = baseline_journal.as_mut() {
            let role = self
                .role
                .as_deref()
                .ok_or(SyncError::LocalCorrupt("restart 基线缺固定 role"))?;
            journal.record_refs(self.vault, role)?;
        }
        Ok(prepared)
    }

    /// 只在 restart 候选基线模式下打开 per-role journal；其它路径返回 `None`。
    fn open_restart_baseline_journal(
        &self,
        epoch: u64,
    ) -> Result<Option<RestartBaselineJournal>, SyncError> {
        let Some((rotation_id, expected_control_rev)) = self.restart_baseline else {
            return Ok(None);
        };
        let role = self
            .role
            .as_deref()
            .ok_or(SyncError::LocalCorrupt("restart 基线缺固定 role"))?;
        if role != "primary" && role != "fallback" {
            return Err(SyncError::Object("restart 基线 role 非法"));
        }
        let mut journal = RestartBaselineJournal::require(self.vault)?;
        journal.bind(
            self.vault,
            self.vault_id,
            rotation_id,
            epoch,
            expected_control_rev,
        )?;
        Ok(Some(journal))
    }

    /// 从 durable journal 恢复已上传的 prepared；`ObjectsUploaded` 阶段补做 refs 登记。
    fn resume_uploaded_baseline(
        &mut self,
        journal: Option<&mut RestartBaselineJournal>,
    ) -> Result<Option<PreparedCheckpoint>, SyncError> {
        let Some(journal) = journal else {
            return Ok(None);
        };
        let role = self
            .role
            .as_deref()
            .ok_or(SyncError::LocalCorrupt("restart 基线缺固定 role"))?
            .to_owned();
        let Some(entry) = journal.entry(&role) else {
            return Ok(None);
        };
        if entry.stage < RestartBaselineStage::ObjectsUploaded {
            return Ok(None);
        }
        let bytes = entry.prepared.clone().ok_or(SyncError::LocalCorrupt(
            "restart 基线阶段越过 upload 但缺 prepared",
        ))?;
        let stage = entry.stage;
        let prepared = PreparedCheckpoint::decode_durable(&bytes)?;
        self.check_prepared_against_entry(&prepared, entry)?;
        if stage == RestartBaselineStage::ObjectsUploaded {
            // refs 可能已部分/全部登记（add_refs 幂等：同 object_id/role 直接跳过）。
            self.register_prepared_refs(&prepared)?;
            journal.record_refs(self.vault, &role)?;
        }
        Ok(Some(prepared))
    }

    /// 取（或恢复）候选基线 lease：有 durable lease 就直接复用，绝不 fresh create。
    fn acquire_baseline_lease(
        &mut self,
        journal: Option<&mut RestartBaselineJournal>,
        epoch: u64,
        estimated: u64,
    ) -> Result<CheckpointLease, SyncError> {
        let Some(journal) = journal else {
            return self.client.create_checkpoint_with_role(
                estimated,
                Some(epoch),
                self.role.as_deref(),
            );
        };
        let role = self
            .role
            .as_deref()
            .ok_or(SyncError::LocalCorrupt("restart 基线缺固定 role"))?
            .to_owned();
        // 没有 intent 时在 create **之前**落盘；已有则原样复用（含持久估算值）。
        let intent = match journal.entry(&role) {
            Some(entry) => entry.intent.clone(),
            None => journal.start(self.vault, &role, epoch, estimated)?,
        };
        if intent.epoch != epoch {
            return Err(SyncError::Unsupported(BASELINE_INTENT_MISMATCH));
        }
        if let Some(lease) = journal.entry(&role).and_then(|entry| entry.lease.clone()) {
            self.check_lease_against_intent(&lease, &intent)?;
            return Ok(lease);
        }
        let lease = self.client.create_checkpoint_with_role(
            intent.estimated_bytes,
            Some(epoch),
            Some(&role),
        )?;
        self.check_lease_against_intent(&lease, &intent)?;
        journal.record_lease(self.vault, &role, lease.clone())?;
        Ok(lease)
    }

    fn check_lease_against_intent(
        &self,
        lease: &CheckpointLease,
        intent: &RestartBaselineIntent,
    ) -> Result<(), SyncError> {
        if lease.epoch != intent.epoch || lease.checkpoint_id == [0u8; 16] {
            return Err(SyncError::RelayWire(
                "restart 基线 lease 与 durable intent 不一致",
            ));
        }
        Ok(())
    }

    fn check_prepared_against_entry(
        &self,
        prepared: &PreparedCheckpoint,
        entry: &RestartBaselineEntry,
    ) -> Result<(), SyncError> {
        let lease = entry
            .lease
            .as_ref()
            .ok_or(SyncError::LocalCorrupt("restart 基线缺 durable lease"))?;
        let report = &prepared.report;
        if report.epoch != self.root.key_epoch
            || report.epoch != entry.intent.epoch
            || report.checkpoint_id != lease.checkpoint_id
            || report.covered_seq != lease.covered_seq
        {
            return Err(SyncError::LocalCorrupt(
                "restart 基线 prepared 与 durable lease/intent 不一致",
            ));
        }
        Ok(())
    }

    /// 从 prepared 的公开事实重建并登记 refs（幂等；供 upload 阶段恢复）。
    fn register_prepared_refs(&mut self, prepared: &PreparedCheckpoint) -> Result<(), SyncError> {
        let report = &prepared.report;
        let data_refs = checkpoint_data_refs(&report.docs, &report.assets, report.epoch);
        let recomputed: [u8; 32] = Sha256::digest(refs_digest_input(&data_refs)?).into();
        if recomputed != report.refs_digest {
            return Err(SyncError::Object("prepared refs_digest 与导出事实不符"));
        }
        let mut refs: Vec<CheckpointRef> = data_refs
            .iter()
            .map(|reference| CheckpointRef {
                object_id: reference.object_id,
                role: CheckpointRole::Data,
            })
            .collect();
        refs.push(CheckpointRef {
            object_id: report.manifest.object_id,
            role: CheckpointRole::Manifest,
        });
        self.client
            .add_checkpoint_refs(report.checkpoint_id, refs)?;
        Ok(())
    }

    /// validate 的 restart 恢复入口：已 durable 的验证事实直接复用。
    fn resume_validated_baseline(
        &self,
        journal: Option<&RestartBaselineJournal>,
        prepared: &PreparedCheckpoint,
    ) -> Result<Option<VerifiedBaseline>, SyncError> {
        let Some(journal) = journal else {
            return Ok(None);
        };
        let role = self
            .role
            .as_deref()
            .ok_or(SyncError::LocalCorrupt("restart 基线缺固定 role"))?;
        let Some(entry) = journal.entry(role) else {
            return Ok(None);
        };
        if entry.stage < RestartBaselineStage::Validated {
            return Ok(None);
        }
        let bytes = entry.verified.clone().ok_or(SyncError::LocalCorrupt(
            "restart 基线阶段越过 validate 但缺 verified",
        ))?;
        let verified = VerifiedBaseline::decode_durable(&bytes)?;
        if verified.facts.vault_id != self.vault_id
            || verified.facts.checkpoint_id != prepared.report.checkpoint_id
            || verified.facts.epoch != prepared.report.epoch
            || verified.facts.covered_seq != prepared.report.covered_seq
        {
            return Err(SyncError::LocalCorrupt(
                "restart 基线 verified 与 prepared 不一致",
            ));
        }
        Ok(Some(verified))
    }

    /// promote 的 restart 恢复入口：已落盘的 publish receipt 直接复用。
    fn resume_promoted_baseline(
        &self,
        journal: Option<&RestartBaselineJournal>,
        baseline: &VerifiedBaseline,
    ) -> Result<Option<PromotedBaseline>, SyncError> {
        let Some(journal) = journal else {
            return Ok(None);
        };
        let role = self
            .role
            .as_deref()
            .ok_or(SyncError::LocalCorrupt("restart 基线缺固定 role"))?;
        let Some(entry) = journal.entry(role) else {
            return Ok(None);
        };
        if entry.stage != RestartBaselineStage::Promoted {
            return Ok(None);
        }
        let bytes = entry
            .verified
            .as_ref()
            .ok_or(SyncError::LocalCorrupt("restart 基线缺 verified receipt"))?;
        let verified = VerifiedBaseline::decode_durable(bytes)?;
        let facts = &baseline.facts;
        if verified.facts.vault_id != facts.vault_id
            || verified.facts.checkpoint_id != facts.checkpoint_id
            || verified.facts.epoch != facts.epoch
            || verified.facts.covered_seq != facts.covered_seq
            || verified.facts.cursor != facts.cursor
            || verified.facts.manifest_object_id != facts.manifest_object_id
            || verified.facts.manifest_digest != facts.manifest_digest
            || verified.facts.refs_digest != facts.refs_digest
            || verified.facts.imported_docs != facts.imported_docs
            || verified.facts.imported_vv_digest != facts.imported_vv_digest
            || verified.facts.verified_assets != facts.verified_assets
        {
            return Err(SyncError::LocalCorrupt(
                "restart 基线 promote 的 verified 事实与当前句柄不一致",
            ));
        }
        let commit_digest =
            checkpoint_commit_digest_from_facts(self.vault_id, self.device_id, facts)?;
        if entry.commit_digest != Some(commit_digest) {
            return Err(SyncError::LocalCorrupt(
                "restart 基线 commit receipt 与 verified 事实不一致",
            ));
        }
        let published_at = entry
            .published_at
            .ok_or(SyncError::LocalCorrupt("restart 基线缺 publish 时间"))?;
        Ok(Some(PromotedBaseline {
            checkpoint_id: facts.checkpoint_id,
            epoch: facts.epoch,
            covered_seq: facts.covered_seq,
            cursor: facts.cursor.clone(),
            manifest_object_id: facts.manifest_object_id,
            refs_digest: facts.refs_digest,
            published_at,
            commit_digest,
        }))
    }

    /// validate：restart 路径优先复用 durable receipt；否则真读回并落盘。
    pub fn validate(
        &mut self,
        prepared: &PreparedCheckpoint,
    ) -> Result<VerifiedBaseline, SyncError> {
        let epoch = prepared.report.epoch;
        let mut journal = self.open_restart_baseline_journal(epoch)?;
        if let Some(verified) = self.resume_validated_baseline(journal.as_ref(), prepared)? {
            return Ok(verified);
        }
        let verified = self.validate_inner(prepared)?;
        if let Some(journal) = journal.as_mut() {
            let role = self
                .role
                .as_deref()
                .ok_or(SyncError::LocalCorrupt("restart 基线缺固定 role"))?;
            journal.record_validated(self.vault, role, verified.encode_durable()?)?;
        }
        Ok(verified)
    }

    /// validate 的真实实现（重启路径复用 receipt 后不再走这里）。
    ///
    /// ADR §32 的检查顺序：
    /// 1. report 字段必须与 prepare 冻结的导出点一致（report 是公开字段，不能当证据）；
    /// 2. manifest 解密、摘要、checkpoint 字段与 `refs_digest`；
    /// 3. 每个 snapshot 下载解密、信封字段，导入**独立空 LoroDoc** 得到 actual VV/frontier：
    ///    actual 必须覆盖 manifest 声明，也必须覆盖 prepare 导出点记录的 head/oplog VV；
    /// 4. `<= S` 的每个 UPDATE（inbox 的 verified/integrated 行）的 `end_vv` 必须被对应
    ///    导出快照覆盖；资产集合必须与 prepare 导出点一致（ASSET 行按 object_id 识别）；
    /// 5. 本地账本没有 `<= S` 的 pending/quarantine 因果洞（仍保留为独立检查，但**不能**
    ///    用"当前 integrated >= S"替代第 3/4 步的导出点绑定）。
    ///
    /// 注意：MNE1 读回路径没有每个对象的独立签名（ObjectCommit 签名由 relay 保存），
    /// 本函数用 MNE1 AEAD + object_digest + manifest refs_digest 绑定密文内容。
    fn validate_inner(
        &mut self,
        prepared: &PreparedCheckpoint,
    ) -> Result<VerifiedBaseline, SyncError> {
        let report = &prepared.report;
        let footprint = &prepared.footprint;
        if report.validated {
            return Err(SyncError::Object("checkpoint 不能重复 validate"));
        }
        // report 是公开字段（rotation 流程会回填 validated/published_at）；安全判断只能用
        // 私有 footprint。这里先证明两者仍是 prepare 时的同一组事实。
        if report.checkpoint_id != footprint.checkpoint_id
            || report.epoch != footprint.epoch
            || report.covered_seq != footprint.covered_seq
        {
            return Err(SyncError::Object(
                "validate: prepared report 与 prepare 导出点不一致（被篡改）",
            ));
        }
        let manifest = report.manifest;
        let (manifest_header, manifest_bytes) = fetch_object(
            self.client,
            &self.root,
            ObjectType::Manifest,
            &manifest.object_id,
            manifest.part_count,
            manifest.total_plain_len,
            &manifest.object_digest,
            &[],
        )?;
        if manifest_header.object_type != ObjectType::Manifest
            || manifest_header.generation != 0
            || manifest_header.key_epoch != footprint.epoch
        {
            return Err(SyncError::Object("validate: manifest 信封字段不符"));
        }
        let decoded = ManifestV1::decode(&manifest_bytes)?;
        if decoded.vault_id != self.vault_id
            || decoded.checkpoint_id != footprint.checkpoint_id
            || decoded.covered_relay_seq != footprint.covered_seq
            || decoded.key_epoch != footprint.epoch
        {
            return Err(SyncError::Object(
                "validate: manifest 与 prepare 导出点不符",
            ));
        }
        if decoded.docs.is_empty()
            || !decoded
                .docs
                .iter()
                .any(|doc| doc.doc_id == decoded.structure_doc_id)
        {
            return Err(SyncError::Object(
                "validate: manifest 缺文档或 structure 文档",
            ));
        }
        let refs = manifest_refs_for_validate(&decoded);
        let refs_digest: [u8; 32] = Sha256::digest(refs_digest_input(&refs)?).into();
        if refs_digest != report.refs_digest {
            return Err(SyncError::Object("validate: refs_digest 不符"));
        }

        // 文档：按 manifest 逐个下载/解密，再导入独立空 LoroDoc 计算 actual VV/frontier。
        let mut vv_input = Vec::new();
        let mut actual_vvs = std::collections::HashMap::with_capacity(decoded.docs.len());
        for doc in &decoded.docs {
            let (header, bytes) = fetch_object(
                self.client,
                &self.root,
                ObjectType::Snapshot,
                &doc.snapshot_object_id,
                doc.part_count,
                doc.total_plain_len,
                &doc.object_digest,
                &[],
            )?;
            if header.generation != doc.generation
                || header.key_epoch != decoded.key_epoch
                || header.sender_seq != 0
            {
                return Err(SyncError::Object("validate: snapshot 信封字段不符"));
            }
            let snapshot = SnapshotV1::decode(&bytes)?;
            if snapshot.doc_id != doc.doc_id
                || snapshot.kind != doc.kind
                || snapshot.generation != doc.generation
                || !snapshot.full_history
            {
                return Err(SyncError::Object("validate: snapshot 与 manifest 不符"));
            }
            let actual_vv = verify_snapshot_against_manifest(&snapshot, doc)?;
            // P0-3：actual VV 必须覆盖 prepare 导出点记录的**本地**事实，而不是只和
            // manifest/snapshot 自报的 VV 比。prepare 时本地落后 → 这里必然失败。
            let fp = footprint
                .docs
                .iter()
                .find(|fp| fp.doc_id == doc.doc_id)
                .ok_or(SyncError::Object(
                    "validate: manifest 含 prepare 导出点没有的文档",
                ))?;
            if fp.kind != doc.kind || fp.generation != doc.generation {
                return Err(SyncError::Object(
                    "validate: 文档 kind/generation 与 prepare 导出点不符",
                ));
            }
            let prepare_head_vv = decode_vv(&fp.head_vv)?;
            let prepare_oplog_vv = decode_vv(&fp.oplog_vv)?;
            if !vv_covers(&actual_vv, &prepare_head_vv) || !vv_covers(&actual_vv, &prepare_oplog_vv)
            {
                return Err(SyncError::Object(
                    "validate: 导出快照未覆盖 prepare 导出点的本地 VV（prepare 时落后，请重新 prepare）",
                ));
            }
            let doc_id = doc.doc_id.as_bytes();
            let encoded = actual_vv.encode();
            vv_input.extend_from_slice(
                &u32::try_from(doc_id.len())
                    .map_err(|_| SyncError::Object("validate: doc_id 超长"))?
                    .to_be_bytes(),
            );
            vv_input.extend_from_slice(doc_id);
            vv_input.extend_from_slice(
                &u32::try_from(encoded.len())
                    .map_err(|_| SyncError::Object("validate: VV 超长"))?
                    .to_be_bytes(),
            );
            vv_input.extend_from_slice(&encoded);
            if actual_vvs.insert(doc.doc_id.clone(), actual_vv).is_some() {
                return Err(SyncError::Object("validate: manifest 重复文档"));
            }
        }
        if decoded.docs.len() != footprint.docs.len()
            || footprint
                .docs
                .iter()
                .any(|fp| !actual_vvs.contains_key(&fp.doc_id))
        {
            return Err(SyncError::Object(
                "validate: manifest 文档集合与 prepare 导出点不一致",
            ));
        }
        // 结构文档 VV 单独绑定（要求 1 把 structure VV 列为导出点事实）。
        let structure_actual = actual_vvs
            .get(STRUCTURE_DOC_ID)
            .ok_or(SyncError::Object("validate: 导出快照缺 structure 文档"))?;
        let structure_footprint = decode_vv(&footprint.structure_vv)?;
        if !vv_covers(structure_actual, &structure_footprint) {
            return Err(SyncError::Object(
                "validate: structure 快照未覆盖 prepare 导出点的 VV",
            ));
        }
        let imported_vv_digest: [u8; 32] = Sha256::digest(&vv_input).into();

        // 资产：object_id 必须由完整明文头派生；再走共享验证器真实读回/解密/校验，
        // 并把成功结果写进完整声明键的正缓存（coverage 复核精确命中时只读回一次）。
        let mut verified_cache = VerifiedAssetCache::default();
        for asset in &decoded.assets {
            let expected_id = asset_object_id(
                &self.root,
                &asset.real_sha256,
                asset.byte_len,
                &asset.media_type,
            )?;
            if expected_id != asset.object_id {
                return Err(SyncError::Object(
                    "validate: 资产 object_id 与完整明文头不符",
                ));
            }
            let facts = fetch_verified_asset_cached(
                &mut *self.client,
                &mut verified_cache,
                &self.root,
                self.vault_id,
                MAX_ASSET_RAW_BYTES as u64,
                &AssetExpectation {
                    object_id: &asset.object_id,
                    object_type: ObjectType::Asset,
                    generation: 0,
                    message_id: None,
                    sender_seq: 0,
                    part_count: asset.part_count,
                    total_len: asset.total_plain_len,
                    object_digest: &asset.object_digest,
                    key_epoch: decoded.key_epoch,
                },
            )?;
            if facts.real_sha256 != asset.real_sha256
                || facts.byte_len != asset.byte_len
                || facts.media_type != asset.media_type
            {
                return Err(SyncError::Object("validate: 资产真实摘要/长度/MIME 不符"));
            }
        }

        // 资产集合绑定：manifest 的资产集合必须与 prepare 导出点完全一致。
        let manifest_asset_digest = asset_set_digest_from_manifest(&decoded.assets);
        if manifest_asset_digest != footprint.asset_set_digest
            || decoded.assets.len() != footprint.asset_object_ids.len()
        {
            return Err(SyncError::Object(
                "validate: 资产集合与 prepare 导出点不一致",
            ));
        }

        // 评审 0003 P0-B：唯一的 coverage 证明入口。inbox 行（导出快照 VV 绑定）+
        // prepare 落盘的 pending 候选 + 连续日志扫描 + 候选资产的真实下载验证，
        // 四者累计证明 `1..=S` 全覆盖。任何"空页/游标停滞/序号缺口/候选未实证"
        // 都不能再自动 Ok。
        //
        // 候选基线的 ASSET 用旧 active 根加密；普通同 epoch checkpoint 的 coverage
        // 根就是当前根。缺旧根时**不猜根**：`Paused(EpochMismatch)`。
        let coverage_root = if footprint.coverage_epoch == footprint.epoch {
            self.root.clone()
        } else {
            let root = self
                .coverage_root
                .clone()
                .ok_or(SyncError::Paused(crate::relay::PauseReason::EpochMismatch))?;
            if root.vault_id != self.vault_id || root.key_epoch != footprint.coverage_epoch {
                return Err(SyncError::Object(
                    "validate: coverage_root 与 prepare 冻结的 coverage epoch 不符",
                ));
            }
            root
        };
        let coverage_proof = self.ensure_coverage_proven(
            footprint,
            &actual_vvs,
            Some(&coverage_root),
            &mut verified_cache,
        )?;

        let state = self.client.state()?;
        if state.head_seq < footprint.covered_seq {
            return Err(SyncError::Object("validate: relay head 未覆盖 S"));
        }
        let mut facts = BaselineFacts {
            vault_id: self.vault_id,
            checkpoint_id: footprint.checkpoint_id,
            epoch: footprint.epoch,
            covered_seq: footprint.covered_seq,
            cursor: format!("c{}", footprint.covered_seq),
            manifest_object_id: manifest.object_id,
            manifest_digest: manifest.object_digest,
            refs_digest: report.refs_digest,
            imported_docs: decoded.docs.len(),
            imported_vv_digest,
            verified_assets: decoded.assets.len(),
            coverage_durable_assets: coverage_proof.durable_assets,
            coverage_verified_candidates: coverage_proof.verified_candidates,
            coverage_proof_digest: coverage_proof.proof_digest,
            coverage_epoch: footprint.coverage_epoch,
            validation_digest: [0u8; 32],
        };
        // task-122：验证修订 2 的证据摘要。它把覆盖复核/实际导入事实绑进生产者签名，
        // 任何字段变化都会让 `CheckpointCommit.validation_digest` 与本地 durable 回执
        // 对不上。旧修订没有这个字段，消费者必须显式拒绝。
        facts.validation_digest = facts.recomputed_validation_digest(&self.device_id)?;
        Ok(VerifiedBaseline { facts })
    }

    /// promote：restart 路径优先复用已 durable 的 publish receipt；否则签 commit 并 publish。
    ///
    /// receipt 在 publish 成功后被**立即**写回 [`RestartBaselineJournal`]（评审 0003 P1-D），
    /// 因此 fallback 半程失败重跑时 primary 的 cid/commit 摘要绝不重新生成。
    pub fn promote(&mut self, baseline: &VerifiedBaseline) -> Result<PromotedBaseline, SyncError> {
        let epoch = baseline.epoch();
        let mut journal = self.open_restart_baseline_journal(epoch)?;
        if let Some(promoted) = self.resume_promoted_baseline(journal.as_ref(), baseline)? {
            // 恢复路径同样必须留下新修订的本地回执（旧 journal 只有 verified=true
            // 时不能作为 restart 快捷恢复证据）。
            let commit_digest = checkpoint_commit_digest_from_facts(
                self.vault_id,
                self.device_id,
                &baseline.facts,
            )?;
            self.write_verified_receipt(baseline, commit_digest)?;
            return Ok(promoted);
        }
        let (promoted, commit_digest) = self.promote_inner(baseline)?;
        self.write_verified_receipt(baseline, commit_digest)?;
        if let Some(journal) = journal.as_mut() {
            let role = self
                .role
                .as_deref()
                .ok_or(SyncError::LocalCorrupt("restart 基线缺固定 role"))?;
            journal.record_promoted(self.vault, role, commit_digest, promoted.published_at)?;
        }
        Ok(promoted)
    }

    /// 把"已成功验证并发布"的基线事实落成新修订的本地回执。
    fn write_verified_receipt(
        &self,
        baseline: &VerifiedBaseline,
        commit_digest: [u8; 32],
    ) -> Result<(), SyncError> {
        let facts = &baseline.facts;
        if facts.vault_id != self.vault_id || facts.epoch != self.root.key_epoch {
            return Err(SyncError::Object(
                "verified receipt 的 vault/epoch 与 writer 不符",
            ));
        }
        if facts.validation_digest != facts.recomputed_validation_digest(&self.device_id)? {
            return Err(SyncError::LocalCorrupt(
                "verified receipt 的 validation_digest 与重算不符",
            ));
        }
        let key = derive_checkpoint_receipt_key(&self.root)?;
        let receipt = VerifiedReceipt {
            checkpoint_id: facts.checkpoint_id,
            vault_id: facts.vault_id,
            epoch: facts.epoch,
            covered_seq: facts.covered_seq,
            commit_digest,
            validation_digest: facts.validation_digest,
        };
        self.vault.set_meta(
            &verified_receipt_key(&facts.checkpoint_id),
            &receipt.encode(&key)?,
        )?;
        Ok(())
    }

    /// promote 的真实实现（restart 恢复命中 receipt 时不会走这里）。
    fn promote_inner(
        &mut self,
        baseline: &VerifiedBaseline,
    ) -> Result<(PromotedBaseline, [u8; 32]), SyncError> {
        let facts = &baseline.facts;
        // 句柄必须属于当前 writer 的 vault/epoch；签名只覆盖句柄内部的不可变事实。
        if facts.vault_id != self.vault_id {
            return Err(SyncError::Object("promote: 基线 vault 与 writer 不符"));
        }
        if facts.epoch != self.root.key_epoch {
            return Err(SyncError::Object("promote: 基线 epoch 与 writer 不符"));
        }
        if facts.validation_digest != facts.recomputed_validation_digest(&self.device_id)? {
            return Err(SyncError::LocalCorrupt(
                "promote: 验证事实的 validation_digest 与重算不符",
            ));
        }
        let commit = CheckpointCommit {
            vault_id: self.vault_id,
            checkpoint_id: facts.checkpoint_id,
            epoch: facts.epoch,
            covered_relay_seq: facts.covered_seq,
            manifest_object_id: facts.manifest_object_id,
            manifest_digest: facts.manifest_digest,
            refs_digest: facts.refs_digest,
            producer_device_id: self.device_id,
            validation_version: CheckpointCommit::CURRENT_VALIDATION_VERSION,
            validation_digest: facts.validation_digest,
        };
        let signed_commit = commit.encode()?;
        let commit_digest: [u8; 32] = Sha256::digest(&signed_commit).into();
        let signature = self.signing_key.sign(&checkpoint_signing_input(&commit)?);
        let published = self.client.publish_checkpoint(CheckpointPublish {
            checkpoint_id: facts.checkpoint_id,
            covered_seq: facts.covered_seq,
            manifest_oid: facts.manifest_object_id,
            refs_digest: facts.refs_digest,
            signed_commit,
            signature,
        })?;
        if published.checkpoint_id != facts.checkpoint_id
            || published.covered_seq != facts.covered_seq
        {
            return Err(SyncError::Object("promote: relay publish 回执与基线不符"));
        }
        Ok((
            PromotedBaseline {
                checkpoint_id: facts.checkpoint_id,
                epoch: facts.epoch,
                covered_seq: facts.covered_seq,
                cursor: facts.cursor.clone(),
                manifest_object_id: facts.manifest_object_id,
                refs_digest: facts.refs_digest,
                published_at: published.published_at,
                commit_digest,
            },
            commit_digest,
        ))
    }

    /// 从 relay 读回并验证一份 checkpoint（快照/资产解密、manifest/refs 一致性、导入）。
    pub fn read_back(
        &mut self,
        report: &CheckpointReport,
    ) -> Result<VerifiedCheckpoint, SyncError> {
        let mut docs = Vec::with_capacity(report.docs.len());
        for doc in &report.docs {
            let (_, bytes) = fetch_object(
                self.client,
                &self.root,
                ObjectType::Snapshot,
                &doc.object.object_id,
                doc.object.part_count,
                doc.object.total_plain_len,
                &doc.object.object_digest,
                &[],
            )?;
            let snapshot = SnapshotV1::decode(&bytes)?;
            if snapshot.doc_id != doc.doc_id
                || snapshot.generation != doc.generation
                || !snapshot.full_history
            {
                return Err(SyncError::Object("读回 SnapshotV1 与 manifest 不一致"));
            }
            let text = if doc.kind == KIND_BODY {
                let body = BodyDoc::restore(
                    snapshot.doc_id.clone(),
                    self.peer_id,
                    Some(&snapshot.loro_snapshot_bytes),
                    &[],
                )?;
                Some(body.text())
            } else {
                StructureDoc::restore(self.peer_id, Some(&snapshot.loro_snapshot_bytes), &[])?;
                None
            };
            docs.push((doc.doc_id.clone(), text));
        }
        for asset in &report.assets {
            let expected_id = asset_object_id(
                &self.root,
                &asset.real_sha256,
                asset.byte_len,
                &asset.media_type,
            )?;
            if expected_id != asset.object.object_id {
                return Err(SyncError::Object("读回资产 object_id 与完整明文头不符"));
            }
            let (_, bytes) = fetch_object(
                self.client,
                &self.root,
                ObjectType::Asset,
                &asset.object.object_id,
                asset.object.part_count,
                asset.object.total_plain_len,
                &asset.object.object_digest,
                &[],
            )?;
            verify_asset_plaintext(&bytes, asset)?;
        }
        let (_, manifest_bytes) = fetch_object(
            self.client,
            &self.root,
            ObjectType::Manifest,
            &report.manifest.object_id,
            report.manifest.part_count,
            report.manifest.total_plain_len,
            &report.manifest.object_digest,
            &[],
        )?;
        let manifest = ManifestV1::decode(&manifest_bytes)?;
        if manifest.docs.len() != report.docs.len() || manifest.assets.len() != report.assets.len()
        {
            return Err(SyncError::Object("manifest 文档/资产数量不符"));
        }
        // manifest 自身不保存 total_plain_len，无法单独重算 relay 侧的 refs_digest；
        // 用 report（创建时的事实）重算，并校验 manifest 的 id 集合与 report 一致。
        let mut manifest_snapshots: Vec<ObjectId> = manifest
            .docs
            .iter()
            .map(|doc| doc.snapshot_object_id)
            .collect();
        let mut report_snapshots: Vec<ObjectId> =
            report.docs.iter().map(|doc| doc.object.object_id).collect();
        manifest_snapshots.sort();
        report_snapshots.sort();
        if manifest_snapshots != report_snapshots {
            return Err(SyncError::Object("manifest snapshot id 集合不符"));
        }
        let mut manifest_assets: Vec<ObjectId> = manifest
            .assets
            .iter()
            .map(|asset| asset.object_id)
            .collect();
        let mut report_assets: Vec<ObjectId> = report
            .assets
            .iter()
            .map(|asset| asset.object.object_id)
            .collect();
        manifest_assets.sort();
        report_assets.sort();
        if manifest_assets != report_assets {
            return Err(SyncError::Object("manifest asset id 集合不符"));
        }
        let data_refs = checkpoint_data_refs(&report.docs, &report.assets, report.epoch);
        let recomputed: [u8; 32] = Sha256::digest(refs_digest_input(&data_refs)?).into();
        if recomputed != report.refs_digest {
            return Err(SyncError::Object("refs_digest 与创建事实不符"));
        }
        Ok(VerifiedCheckpoint {
            docs,
            asset_count: report.assets.len(),
            manifest_checked: true,
        })
    }

    /// 评审 0003 P0-B：证明 `1..=S` 的每个序号都被**这份导出快照**覆盖。
    ///
    /// 证明只能由持久事实累计：
    ///
    /// 1. `sync_inbox` 行：`integrated`/`verified` 行的 `UpdateV1.end_vv` 必须被导出
    ///    快照的 actual VV 覆盖；连续 `integrated` 前缀单独计入。`quarantine` 直接拒绝。
    ///    **不存在**"OID 在 manifest 资产集合里就视为已证明"的捷径：Engine 从不为
    ///    ASSET 写 inbox，inbox 行只能按 UPDATE 语义绑定实际导出水位。
    /// 2. `SyncEngine::pull` 在推进游标之前落盘的 durable skipped-asset 记录（v3，受
    ///    设备签名密钥 HMAC 保护并绑定完整 ObjectCommit）：只有新修订（真实下载验证过）
    ///    的记录能覆盖"无 inbox 行"的缺口；旧修订记录一律拒绝。
    /// 3. prepare 落盘的 **pending 候选** receipt：只记录缺口里看到的完整 ASSET 声明；
    ///    validate 必须从同一固定起点重新看到**逐字节相同**的 ObjectCommit，并用旧
    ///    active 根（`coverage_root`）真实下载验证资产后，才允许合并覆盖区间。
    /// 4. 缺口必须由连续日志扫描补齐；空页、`next_cursor` 不前进/回退、日志序号
    ///    缺口/乱序/重复/声明变化一律失败。可信锚（游标序号）必须 `<= S`。
    fn ensure_coverage_proven(
        &mut self,
        footprint: &ExportFootprint,
        actual_vvs: &std::collections::HashMap<String, loro::VersionVector>,
        coverage_root: Option<&RootSecretV1>,
        cache: &mut VerifiedAssetCache,
    ) -> Result<CoverageProof, SyncError> {
        let covered_seq = footprint.covered_seq;
        if covered_seq == 0 {
            return Ok(CoverageProof::empty());
        }
        let ledger = self.local_coverage(footprint, actual_vvs)?;
        let mut missing = missing_ranges(&ledger.proven, covered_seq);
        if missing.is_empty() {
            return Ok(CoverageProof::empty());
        }
        let state = self.client.state()?;
        if state.vault_id != self.vault_id {
            return Err(SyncError::Object(
                "validate: coverage scan 的 relay vault 与本地不一致",
            ));
        }
        if state.epoch != footprint.coverage_epoch {
            return Err(SyncError::Object(
                "validate: coverage scan 的 relay epoch 与 prepare 时冻结的 epoch 不一致",
            ));
        }
        if state.head_seq < covered_seq {
            return Err(SyncError::Object("validate: relay head 未覆盖 S"));
        }
        if ledger
            .log_id
            .is_some_and(|local_log_id| local_log_id != state.log_id)
        {
            return Err(SyncError::Object(
                "validate: 本地 sync log_id 与 relay state 不一致",
            ));
        }
        let proof = match self.load_coverage_receipt(&footprint.checkpoint_id)? {
            Some(receipt) => self.verify_coverage_with_receipt(
                &receipt,
                &mut missing,
                &ledger,
                footprint,
                &state,
                coverage_root,
                cache,
            )?,
            None => {
                self.reject_unproven_coverage(&missing, &ledger, footprint)?;
                CoverageProof::empty()
            }
        };
        if !missing.is_empty() {
            return Err(SyncError::Object("validate: 未证明覆盖到 S"));
        }
        Ok(proof)
    }

    /// 从 `sync_inbox` + integrated 前缀构造本地账簿证明。
    ///
    /// 只把**能绑定到导出快照**的事实记为已证明：`verified` 行必须解出 `UpdateV1`
    /// 且 `end_vv` 被对应文档的 actual VV 覆盖；否则保留为缺口，交给日志扫描/持久 receipt。
    fn local_coverage(
        &self,
        footprint: &ExportFootprint,
        actual_vvs: &std::collections::HashMap<String, loro::VersionVector>,
    ) -> Result<LocalCoverage, SyncError> {
        let covered_seq = footprint.covered_seq;
        let cursor = menghunote_storage::sync_cursor_state(self.vault)?;
        let mut rows = BTreeMap::new();
        let mut proven = Vec::new();
        if covered_seq > 0 {
            let limit = i64::try_from(covered_seq)
                .map_err(|_| SyncError::Object("validate: S 超出 i64 范围"))?;
            let row_values = match cursor.log_id {
                Some(log_id) => self.vault.query(
                    "SELECT relay_seq,object_id,state,verified_payload FROM sync_inbox WHERE log_id=?1 AND relay_seq<=?2 ORDER BY relay_seq",
                    &[
                        menghunote_storage::Value::Blob(log_id.to_vec()),
                        menghunote_storage::Value::Integer(limit),
                    ],
                )?,
                None => {
                    // 没有 log_id 时无法把 inbox 行归属到当前 relay log；只要库里没有任何
                    // ≤S 的入站行（典型是只有 ASSET 日志事件，Engine 不写 inbox），就没有
                    // 需要逐个绑定的文档 UPDATE。有行则拒绝，而不是默认放行。
                    let any = self.vault.query(
                        "SELECT relay_seq FROM sync_inbox WHERE relay_seq<=?1 LIMIT 1",
                        &[menghunote_storage::Value::Integer(limit)],
                    )?;
                    if !any.is_empty() {
                        return Err(SyncError::Object(
                            "validate: 本地没有 sync log_id，无法证明 <=S 的 inbox UPDATE 已被导出快照覆盖",
                        ));
                    }
                    Vec::new()
                }
            };
            for row in &row_values {
                let relay_seq = match row.first() {
                    Some(menghunote_storage::Value::Integer(seq)) => u64::try_from(*seq)
                        .map_err(|_| SyncError::Object("validate: inbox relay_seq 非法"))?,
                    _ => return Err(SyncError::Object("validate: inbox relay_seq 类型非法")),
                };
                let object_id = match row.get(1) {
                    Some(menghunote_storage::Value::Blob(bytes)) if bytes.len() == 32 => {
                        let mut id = [0u8; 32];
                        id.copy_from_slice(bytes);
                        id
                    }
                    _ => return Err(SyncError::Object("validate: inbox object_id 类型非法")),
                };
                let state = match row.get(2) {
                    Some(menghunote_storage::Value::Text(state)) => state.as_str(),
                    _ => return Err(SyncError::Object("validate: inbox state 类型非法")),
                };
                let payload = match row.get(3) {
                    Some(menghunote_storage::Value::Blob(payload)) => Some(payload.as_slice()),
                    _ => None,
                };
                if state == "quarantine" {
                    return Err(SyncError::Object(
                        "validate: <= S 存在 quarantine 入站对象（因果洞）",
                    ));
                }
                // 这里**不得**用 `footprint.asset_object_ids` 给 inbox 行放行：Engine 从不为
                // ASSET 写 inbox，唯一能让 inbox 行 OID 恰好等于 manifest 资产 OID 的路径，
                // 是恶意成员伪造一条 OID 指向资产对象的 UPDATE。资产覆盖必须由 durable
                // skipped-asset 记录或 prepare/validate 的真实下载实证单独证明。
                let proven_row = if state == "integrated" || state == "verified" {
                    // 即使已经 integrated，也必须用**导出快照的 actual VV**再验证一次：
                    // prepare 之后本地又 integrate 的更新不能靠"当前水位"冒充快照覆盖。
                    match payload {
                        None => false,
                        Some(payload) => {
                            let update = UpdateV1::decode(payload).map_err(|_| {
                                SyncError::Object("validate: inbox verified_payload 解码失败")
                            })?;
                            let Some(actual_vv) = actual_vvs.get(&update.doc_id) else {
                                return Err(SyncError::Object(
                                    "validate: <= S 的 UPDATE 所属文档不在导出快照中",
                                ));
                            };
                            let end_vv = if update.end_vv.is_empty() {
                                loro::VersionVector::new()
                            } else {
                                loro::VersionVector::decode(&update.end_vv)
                                    .map_err(|_| SyncError::Object("validate: inbox end_vv 非法"))?
                            };
                            if !vv_covers(actual_vv, &end_vv) {
                                return Err(SyncError::Object(
                                    "validate: <= S 的 UPDATE 未被导出快照覆盖（prepare 后本地又 integrate 了该更新，请重新 prepare）",
                                ));
                            }
                            true
                        }
                    }
                } else {
                    false
                };
                if proven_row {
                    proven.push(SeqRange {
                        start: relay_seq,
                        end: relay_seq,
                    });
                }
                rows.insert(
                    relay_seq,
                    InboxCoverage {
                        object_id,
                        proven: proven_row,
                    },
                );
            }
        }
        // 评审 0003 P0-B 残余：缺口只能由 engine 在 pull 时落盘的 durable
        // skipped-asset 记录证明。记录受设备签名密钥派生的 HMAC 保护并绑定完整
        // ObjectCommit；任何结构/认证/键序号不自洽都 fail-closed。
        let mut skipped_assets = BTreeMap::new();
        if covered_seq > 0 {
            if let Some(log_id) = cursor.log_id {
                let records = crate::engine::load_skipped_asset_records(
                    self.vault,
                    &self.signing_key,
                    &self.vault_id,
                    &log_id,
                )
                .map_err(|_| {
                    SyncError::Object("validate: skipped-asset 记录无效（被篡改/旧修订/冲突）")
                })?;
                for record in records {
                    if record.relay_seq == 0 || record.relay_seq > covered_seq {
                        continue;
                    }
                    if record.epoch > footprint.coverage_epoch {
                        return Err(SyncError::Object(
                            "validate: skipped-asset 记录的 epoch 超出 prepare 冻结 epoch",
                        ));
                    }
                    if rows.contains_key(&record.relay_seq) {
                        return Err(SyncError::Object(
                            "validate: 同一序号同时存在 inbox 与 skipped-asset 证据",
                        ));
                    }
                    let relay_seq = record.relay_seq;
                    if skipped_assets.insert(relay_seq, record).is_some() {
                        return Err(SyncError::Object("validate: skipped-asset 记录重复"));
                    }
                    proven.push(SeqRange {
                        start: relay_seq,
                        end: relay_seq,
                    });
                }
            }
        }
        if cursor.integrated_seq >= 1 {
            proven.push(SeqRange {
                start: 1,
                end: cursor.integrated_seq.min(covered_seq),
            });
        }
        Ok(LocalCoverage {
            log_id: cursor.log_id,
            rows,
            proven: merge_ranges(proven),
            skipped_assets,
        })
    }

    fn load_coverage_receipt(
        &self,
        checkpoint_id: &[u8; 16],
    ) -> Result<Option<CoverageReceipt>, SyncError> {
        let Some(bytes) = self.vault.get_meta(&coverage_receipt_key(checkpoint_id))? else {
            return Ok(None);
        };
        let key = derive_checkpoint_receipt_key(&self.root)?;
        let receipt = CoverageReceipt::decode(&bytes, &key)?;
        if receipt.checkpoint_id != *checkpoint_id {
            return Err(SyncError::Object(
                "validate: coverage receipt 与 checkpoint id 不一致",
            ));
        }
        Ok(Some(receipt))
    }

    /// receipt 路径：重扫日志逐字节核对完整 ObjectCommit → 精确命中正缓存则复用，
    /// 否则用旧 active 根真实下载验证；**全部候选成功后**才合并覆盖区间并返回私有事实。
    #[allow(clippy::too_many_arguments)]
    fn verify_coverage_with_receipt(
        &mut self,
        receipt: &CoverageReceipt,
        missing: &mut Vec<SeqRange>,
        ledger: &LocalCoverage,
        footprint: &ExportFootprint,
        state: &RelayState,
        coverage_root: Option<&RootSecretV1>,
        cache: &mut VerifiedAssetCache,
    ) -> Result<CoverageProof, SyncError> {
        if receipt.vault_id != self.vault_id
            || receipt.checkpoint_id != footprint.checkpoint_id
            || receipt.log_id != state.log_id
            || receipt.coverage_epoch != footprint.coverage_epoch
            || receipt.covered_seq != footprint.covered_seq
        {
            return Err(SyncError::Object(
                "validate: coverage receipt 与 prepare 导出点不一致（被篡改）",
            ));
        }
        // 可信锚必须不越过 S，否则"从游标连续扫描到 S"没有意义。
        if receipt.cursor_seq > receipt.covered_seq {
            return Err(SyncError::Object(
                "validate: coverage receipt 的起点序号越过 S（可信游标必须 <= S）",
            ));
        }
        if receipt.claims.is_empty() {
            return Err(SyncError::Object("validate: 未证明覆盖到 S"));
        }
        let mut candidates: BTreeMap<u64, (ObjectId, Vec<u8>)> = BTreeMap::new();
        let mut claim_ranges = Vec::new();
        for claim in &receipt.claims {
            match claim {
                CoverageClaim::PendingAsset { relay_seq, commit } => {
                    if *relay_seq <= receipt.cursor_seq || *relay_seq > receipt.covered_seq {
                        return Err(SyncError::Object(
                            "validate: coverage receipt 的候选项序号越界",
                        ));
                    }
                    let decoded = ObjectCommit::decode(commit)
                        .map_err(|_| SyncError::Object("validate: 候选项 commit 非法"))?;
                    if decoded.vault_id != self.vault_id
                        || decoded.object_type != ObjectType::Asset
                        || decoded.key_epoch != receipt.coverage_epoch
                        || decoded.generation != 0
                        || decoded.sender_seq != 0
                        || decoded.total_len == 0
                        || decoded.total_len > MAX_ASSET_RAW_BYTES as u64
                        || u64::from(decoded.part_count)
                            != part_count_for(decoded.total_len, ObjectType::Asset)
                    {
                        return Err(SyncError::Object(
                            "validate: coverage receipt 的候选不是合法 ASSET 声明",
                        ));
                    }
                    if let Some(recorded) = ledger.skipped_assets.get(relay_seq) {
                        if recorded.object_id != decoded.object_id || recorded.commit != *commit {
                            return Err(SyncError::Object(
                                "validate: coverage receipt 与 skipped-asset 记录的 ASSET 冲突",
                            ));
                        }
                    }
                    if candidates
                        .insert(*relay_seq, (decoded.object_id, commit.clone()))
                        .is_some()
                    {
                        return Err(SyncError::Object(
                            "validate: coverage receipt 的 ASSET 声明重复",
                        ));
                    }
                    claim_ranges.push(SeqRange {
                        start: *relay_seq,
                        end: *relay_seq,
                    });
                }
            }
        }
        if let Some(first) = missing.first() {
            if first.start <= receipt.cursor_seq {
                return Err(SyncError::Object(
                    "validate: 未证明覆盖到 S（游标之前的缺口缺少 durable skipped-asset 记录）",
                ));
            }
        }
        let mut observed: HashSet<u64> = HashSet::with_capacity(candidates.len());
        let mut durable: HashSet<u64> = HashSet::new();
        let report = scan_coverage_log(
            &mut *self.client,
            receipt.coverage_epoch,
            receipt.covered_seq,
            &receipt.cursor,
            Some(receipt.cursor_seq),
            |event| {
                let seq = event.relay_seq;
                if let Some((object_id, commit)) = candidates.get(&seq) {
                    if event.kind != RelayLogKind::Asset
                        || event.object_id != Some(*object_id)
                        || event.epoch != receipt.coverage_epoch
                        || event.event != *commit
                    {
                        return Err(SyncError::Object(
                            "validate: coverage receipt 候选与日志事件不一致（声明变化）",
                        ));
                    }
                    observed.insert(seq);
                    return Ok(());
                }
                // engine 在推进游标前落盘的 durable 证据：必须逐字节命中同一条
                // ObjectCommit，并且记录本身是新修订（真实验证过）。
                if let Some(recorded) = ledger.skipped_assets.get(&seq) {
                    if event.kind != RelayLogKind::Asset
                        || event.object_id != Some(recorded.object_id)
                        || event.epoch != recorded.epoch
                        || event.event != recorded.commit
                    {
                        return Err(SyncError::Object(
                            "validate: 日志事件与 skipped-asset 记录不一致",
                        ));
                    }
                    durable.insert(seq);
                    return Ok(());
                }
                if let Some(row) = ledger.rows.get(&seq) {
                    if row.proven {
                        if event.object_id != Some(row.object_id) {
                            return Err(SyncError::Object(
                                "validate: 日志事件与本地 inbox 记录不一致",
                            ));
                        }
                        return Ok(());
                    }
                    return Err(SyncError::Object("validate: 未证明覆盖到 S"));
                }
                if range_contains(&ledger.proven, seq) {
                    return Ok(());
                }
                Err(SyncError::Object("validate: 未证明覆盖到 S"))
            },
        )?;
        if report.cursor_seq != receipt.cursor_seq {
            return Err(SyncError::Object(
                "validate: coverage scan 的起点序号与持久 receipt 不一致",
            ));
        }
        for relay_seq in candidates.keys() {
            if !observed.contains(relay_seq) {
                return Err(SyncError::Object(
                    "validate: coverage receipt 的 ASSET 候选未被日志复核",
                ));
            }
        }
        if report.first_page_empty && report.through_seq < receipt.covered_seq {
            return Err(SyncError::Object("validate: 未证明覆盖到 S"));
        }
        if !report.reached_covered_seq && !missing.is_empty() {
            return Err(SyncError::Object("validate: 未证明覆盖到 S"));
        }
        // 候选必须真实下载验证（旧 active 根）。任何一条失败都在合并覆盖区间之前返回。
        let root =
            coverage_root.ok_or(SyncError::Paused(crate::relay::PauseReason::EpochMismatch))?;
        if root.vault_id != self.vault_id || root.key_epoch != receipt.coverage_epoch {
            return Err(SyncError::Object(
                "validate: coverage_root 与 prepare 冻结的 coverage epoch 不符",
            ));
        }
        let mut facts_by_seq: BTreeMap<u64, VerifiedAssetFacts> = BTreeMap::new();
        for (relay_seq, (_object_id, commit)) in &candidates {
            let decoded = ObjectCommit::decode(commit)
                .map_err(|_| SyncError::Object("validate: 候选项 commit 非法"))?;
            let facts = fetch_verified_asset_cached(
                &mut *self.client,
                cache,
                root,
                self.vault_id,
                MAX_ASSET_RAW_BYTES as u64,
                &AssetExpectation {
                    object_id: &decoded.object_id,
                    object_type: decoded.object_type,
                    generation: decoded.generation,
                    message_id: Some(decoded.message_id),
                    sender_seq: decoded.sender_seq,
                    part_count: decoded.part_count,
                    total_len: decoded.total_len,
                    object_digest: &decoded.object_digest,
                    key_epoch: decoded.key_epoch,
                },
            )?;
            facts_by_seq.insert(*relay_seq, facts);
        }
        // durable skipped-asset 记录：新修订已由 engine 真实下载验证；这里再做
        // root/epoch 绑定（同 epoch 可重算 OID），并核对完整 commit。
        let mut proof_entries = Vec::with_capacity(durable.len() + facts_by_seq.len());
        for relay_seq in &durable {
            let record = ledger
                .skipped_assets
                .get(relay_seq)
                .ok_or(SyncError::Object("validate: skipped-asset 记录缺失"))?;
            if record.epoch == receipt.coverage_epoch {
                let derived_message = crate::upload::asset_message_id(root, &record.object_id)?;
                let commit = ObjectCommit::decode(&record.commit)
                    .map_err(|_| SyncError::Object("validate: durable ASSET commit 非法"))?;
                if commit.message_id != derived_message {
                    return Err(SyncError::Object(
                        "validate: durable ASSET message_id 不是 coverage 根确定性派生值",
                    ));
                }
                let derived = asset_object_id(
                    root,
                    &record.real_sha256,
                    record.byte_len,
                    &record.media_type,
                )?;
                if derived != record.object_id {
                    return Err(SyncError::Object(
                        "validate: durable ASSET 的 object_id 与 coverage 根不符",
                    ));
                }
            }
            proof_entries.push((
                *relay_seq,
                record.object_id,
                record.real_sha256,
                record.byte_len,
                record.object_digest,
            ));
        }
        for (relay_seq, facts) in &facts_by_seq {
            proof_entries.push((
                *relay_seq,
                candidates
                    .get(relay_seq)
                    .map(|(object_id, _)| *object_id)
                    .ok_or(SyncError::Object("validate: 候选序号丢失"))?,
                facts.real_sha256,
                facts.byte_len,
                facts.object_digest,
            ));
        }
        let proof = CoverageProof {
            durable_assets: durable.len(),
            verified_candidates: facts_by_seq.len(),
            proof_digest: coverage_proof_digest(proof_entries),
        };
        // 全部候选成功后才合并覆盖区间。
        *missing = subtract_ranges(missing, &merge_ranges(claim_ranges));
        Ok(proof)
    }

    /// 无 receipt 路径：从持久游标严格扫描一遍，只为了给出明确失败原因；
    /// 任何缺口都不允许放行（ASSET 缺口要求 pending receipt + 实证）。
    fn reject_unproven_coverage(
        &mut self,
        missing: &[SeqRange],
        ledger: &LocalCoverage,
        footprint: &ExportFootprint,
    ) -> Result<(), SyncError> {
        let Some(cursor_bytes) = self.vault.get_meta(crate::engine::RELAY_CURSOR_META)? else {
            return Err(SyncError::Object(
                "validate: 本地没有 relay 游标，无法证明 S 已拉取",
            ));
        };
        let cursor = String::from_utf8(cursor_bytes)
            .map_err(|_| SyncError::Object("validate: relay 游标不是 UTF-8"))?;
        let first_missing = missing
            .first()
            .ok_or(SyncError::Object("validate: 未证明覆盖到 S"))?
            .start;
        // 扫描必须锚在 prepare 冻结 epoch：候选基线的 S 覆盖的是 from_epoch 的事件。
        let report = scan_coverage_log(
            &mut *self.client,
            footprint.coverage_epoch,
            footprint.covered_seq,
            &cursor,
            None,
            |event| {
                let seq = event.relay_seq;
                if seq < first_missing || range_contains(&ledger.proven, seq) {
                    return Ok(());
                }
                if event.kind == RelayLogKind::Asset {
                    return Err(SyncError::Object(
                        "validate: 未证明覆盖到 S（<=S 的 ASSET 缺口缺少 durable/pending 证据）",
                    ));
                }
                Err(SyncError::Object("validate: 未证明覆盖到 S"))
            },
        )?;
        if report.first_page_empty && report.through_seq < footprint.covered_seq {
            return Err(SyncError::Object("validate: 未证明覆盖到 S"));
        }
        if report.cursor_seq >= first_missing {
            return Err(SyncError::Object(
                "validate: 未证明覆盖到 S（持久游标之前存在无 durable 证据覆盖的缺口）",
            ));
        }
        Err(SyncError::Object("validate: 未证明覆盖到 S"))
    }

    /// prepare 侧：把"固定起点 + 缺口里看到的完整 ASSET 声明"落成 **pending 候选**。
    ///
    /// 只记录候选声明，**不生成成功覆盖证明**；结构/分页/写盘异常显式返回（prepare
    /// 失败），不得把证明错误吞掉。validate 必须逐条实证后才会合并覆盖区间。
    fn try_record_coverage_receipt(
        &mut self,
        footprint: &ExportFootprint,
    ) -> Result<(), SyncError> {
        let covered_seq = footprint.covered_seq;
        if covered_seq == 0 {
            return Ok(());
        }
        let cursor_state = menghunote_storage::sync_cursor_state(self.vault)?;
        let Some(cursor_bytes) = self.vault.get_meta(crate::engine::RELAY_CURSOR_META)? else {
            return Ok(());
        };
        let Ok(cursor) = String::from_utf8(cursor_bytes) else {
            return Ok(());
        };
        if cursor.is_empty() {
            return Ok(());
        }
        let state = self.client.state()?;
        // coverage 必须锚在 prepare 时冻结的 relay epoch：候选基线（to_epoch）在
        // activate 前 relay 仍是 from_epoch，不能用 checkpoint epoch 比较。
        if state.vault_id != self.vault_id
            || state.epoch != footprint.coverage_epoch
            || state.head_seq < covered_seq
        {
            return Ok(());
        }
        if cursor_state
            .log_id
            .is_some_and(|log_id| log_id != state.log_id)
        {
            return Ok(());
        }
        // 本地已"消费"的序号：integrated 前缀 + 有 inbox 行的序号。是否被导出快照
        // 覆盖由 validate 决定；这里只负责找出需要 pending 候选的缺口。
        let mut consumed = Vec::new();
        if cursor_state.integrated_seq >= 1 {
            consumed.push(SeqRange {
                start: 1,
                end: cursor_state.integrated_seq.min(covered_seq),
            });
        }
        let limit = i64::try_from(covered_seq)
            .map_err(|_| SyncError::Object("validate: S 超出 i64 范围"))?;
        let row_values = match cursor_state.log_id {
            Some(log_id) => self.vault.query(
                "SELECT relay_seq FROM sync_inbox WHERE log_id=?1 AND relay_seq<=?2 ORDER BY relay_seq",
                &[
                    menghunote_storage::Value::Blob(log_id.to_vec()),
                    menghunote_storage::Value::Integer(limit),
                ],
            )?,
            None => {
                let any = self.vault.query(
                    "SELECT relay_seq FROM sync_inbox WHERE relay_seq<=?1 LIMIT 1",
                    &[menghunote_storage::Value::Integer(limit)],
                )?;
                if !any.is_empty() {
                    return Ok(());
                }
                Vec::new()
            }
        };
        for row in &row_values {
            if let Some(menghunote_storage::Value::Integer(seq)) = row.first() {
                let seq = u64::try_from(*seq)
                    .map_err(|_| SyncError::Object("validate: inbox relay_seq 非法"))?;
                if seq >= 1 && seq <= covered_seq {
                    consumed.push(SeqRange {
                        start: seq,
                        end: seq,
                    });
                }
            } else {
                return Err(SyncError::Object("validate: inbox relay_seq 类型非法"));
            }
        }
        // engine pull 落盘的 durable skipped-asset 记录同样属于"已消费"；它们是游标
        // 之前缺口唯一允许的持久证明。读取/认证失败时 prepare 显式失败（旧修订必须
        // 重新拉取，不能当证据）。
        if let Some(log_id) = cursor_state.log_id {
            let records = crate::engine::load_skipped_asset_records(
                self.vault,
                &self.signing_key,
                &self.vault_id,
                &log_id,
            )?;
            for record in records {
                if record.relay_seq >= 1
                    && record.relay_seq <= covered_seq
                    && record.epoch <= footprint.coverage_epoch
                {
                    consumed.push(SeqRange {
                        start: record.relay_seq,
                        end: record.relay_seq,
                    });
                }
            }
        }
        let consumed = merge_ranges(consumed);
        let missing = missing_ranges(&consumed, covered_seq);
        if missing.is_empty() {
            return Ok(());
        }
        let first_missing = missing[0].start;
        let mut observed: Vec<(u64, ObjectId, Vec<u8>)> = Vec::new();
        let mut observed_seqs = HashSet::new();
        let report = scan_coverage_log(
            &mut *self.client,
            footprint.coverage_epoch,
            covered_seq,
            &cursor,
            None,
            |event| {
                let seq = event.relay_seq;
                if seq < first_missing || range_contains(&consumed, seq) {
                    return Ok(());
                }
                if event.kind == RelayLogKind::Asset {
                    let object_id = event
                        .object_id
                        .ok_or(SyncError::Object("prepare: ASSET 事件缺少 object_id"))?;
                    let commit = ObjectCommit::decode(&event.event)?;
                    if commit.vault_id != self.vault_id
                        || commit.object_id != object_id
                        || commit.object_type != ObjectType::Asset
                        || commit.key_epoch != footprint.coverage_epoch
                        || commit.generation != 0
                        || commit.sender_seq != 0
                    {
                        return Err(SyncError::Object(
                            "prepare: ASSET 日志事件的 ObjectCommit 与事件不符",
                        ));
                    }
                    if commit.total_len == 0
                        || commit.total_len > MAX_ASSET_RAW_BYTES as u64
                        || u64::from(commit.part_count)
                            != part_count_for(commit.total_len, ObjectType::Asset)
                    {
                        return Err(SyncError::Object("prepare: ASSET 声明总长/分片数非法"));
                    }
                    if observed_seqs.insert(seq) {
                        observed.push((seq, object_id, event.event.clone()));
                    }
                    return Ok(());
                }
                // 未证明的 UPDATE 缺口：pending 候选不能凭空补；不写 receipt，
                // 由 validate 明确拒绝（这里不是扫描/结构异常，不提前失败）。
                Ok(())
            },
        )?;
        if missing[0].start <= report.cursor_seq {
            // 游标之前的缺口只能由 engine 的 durability 记录证明；receipt 不再生成
            // `ConsumedAsset` 推断，残留缺口直接让 validate 失败（这里不写 receipt）。
            return Ok(());
        }
        if covered_seq > report.cursor_seq {
            if !report.reached_covered_seq {
                return Ok(());
            }
            let tail_missing =
                clamp_ranges(&missing, report.cursor_seq.saturating_add(1), covered_seq);
            if ranges_len(&tail_missing) != observed.len() as u128 {
                return Ok(());
            }
            if observed
                .iter()
                .any(|(seq, _, _)| !range_contains(&missing, *seq))
            {
                return Ok(());
            }
        }
        if observed.is_empty() {
            return Ok(());
        }
        let mut claims = Vec::with_capacity(observed.len());
        for (relay_seq, _, commit) in observed {
            claims.push(CoverageClaim::PendingAsset { relay_seq, commit });
        }
        let receipt = CoverageReceipt {
            vault_id: self.vault_id,
            checkpoint_id: footprint.checkpoint_id,
            log_id: state.log_id,
            coverage_epoch: footprint.coverage_epoch,
            covered_seq,
            cursor,
            cursor_seq: report.cursor_seq,
            claims,
        };
        let key = derive_checkpoint_receipt_key(&self.root)?;
        let bytes = receipt.encode(&key)?;
        self.vault
            .set_meta(&coverage_receipt_key(&footprint.checkpoint_id), &bytes)?;
        Ok(())
    }

    fn put_slot(
        &mut self,
        slot: &KeySlotV1,
        secrets: &SlotSecrets<'_>,
        message_id: [u8; 16],
    ) -> Result<SlotRecord, SyncError> {
        let descriptor = slot.descriptor.encode()?;
        let plaintext = slot.wrapped_bytes();
        // ADR §20：KEY_SLOT 不用 root 派生的 K_object，而是 password/recovery IKM 派生的
        // K_slot_mne；恢复设备在拿到 root 之前才能解槽。
        let key = slot.mne_key(secrets.password.map(str::as_bytes), secrets.recovery_seed)?;
        crate::upload::upload_object_with_key(
            self.client,
            self.vault_id,
            self.device_id,
            &self.signing_key,
            &self.root,
            ObjectType::KeySlot,
            slot.descriptor.slot_id,
            message_id,
            0,
            0,
            &plaintext,
            &descriptor,
            Some(slot.wrapped.nonce),
            &key,
        )?;
        let signature = self.signing_key.sign(&descriptor);
        self.client.put_key_slot(SlotUpload {
            slot_id: slot.descriptor.slot_id,
            epoch: slot.descriptor.key_epoch,
            kind: slot.descriptor.kind,
            descriptor: descriptor.clone(),
            signed_message: descriptor,
            signature,
        })
    }

    fn prepare_docs(&mut self) -> Result<Vec<PreparedDoc>, SyncError> {
        let mut docs = Vec::new();
        // `list_all_docs` 遍历 loro_docs 全表：结构、正文、回收站/墓碑都在内。
        for meta in list_all_docs(self.vault)? {
            let kind = match meta.kind {
                DocKind::Structure => KIND_STRUCTURE,
                DocKind::Body => KIND_BODY,
            };
            let (snapshot_bytes, oplog_vv) = if meta.kind == DocKind::Structure {
                let (structure, _head) = load_structure(self.vault, self.peer_id)?;
                (structure.export_snapshot()?, structure.vv().encode())
            } else {
                let bootstrap = read_doc_bootstrap(self.vault, &meta.doc_id)?
                    .ok_or_else(|| SyncError::LocalCorrupt("loro_docs 缺少正文文档"))?;
                let payloads: Vec<Vec<u8>> = bootstrap
                    .updates_since_snapshot
                    .iter()
                    .map(|update| update.payload.clone())
                    .collect();
                let body = BodyDoc::restore(
                    meta.doc_id.clone(),
                    self.peer_id,
                    bootstrap.snapshot.as_deref(),
                    &payloads,
                )?;
                (body.export_snapshot()?, body.vv().encode())
            };
            docs.push(PreparedDoc {
                doc_id: meta.doc_id,
                kind,
                generation: meta.generation,
                head_seq: meta.head_seq,
                head_vv: meta.head_vv.unwrap_or_default(),
                oplog_vv,
                frontier: meta.head_frontier.unwrap_or_default(),
                codec_version: meta.codec_version,
                app_doc_schema: meta.app_doc_schema,
                min_reader: meta.min_reader,
                snapshot_bytes,
            });
        }
        Ok(docs)
    }
}

/// 解码 manifest/snapshot 里的 VV（空字节 = 空 VV，见 `prepare_docs` 的 `unwrap_or_default`）。
fn decode_vv(bytes: &[u8]) -> Result<loro::VersionVector, SyncError> {
    if bytes.is_empty() {
        return Ok(loro::VersionVector::new());
    }
    loro::VersionVector::decode(bytes).map_err(|_| SyncError::Object("validate: 快照 VV 非法"))
}

/// `actual` 是否覆盖 `required`（required 的每个 peer 计数都 ≤ actual）。
fn vv_covers(actual: &loro::VersionVector, required: &loro::VersionVector) -> bool {
    required.sub_iter(actual).next().is_none()
}

/// 资产集合摘要的域分隔（与对象加密的 AAD 域无关，纯证据口径）。
const CHECKPOINT_ASSET_SET_DOMAIN: &[u8] = b"MN/v1/checkpoint-asset-set\0";

/// 对 prepare 导出点列出的资产做集合摘要（按 `real_sha256` 稳定排序）。
fn asset_set_digest_from_metas(metas: &[AssetMeta]) -> [u8; 32] {
    let entries = metas
        .iter()
        .map(|meta| (meta.real_sha256, meta.byte_len, meta.media_type.as_str()))
        .collect();
    asset_set_digest(entries)
}

/// 对 manifest 声明的资产做同一口径的集合摘要。
fn asset_set_digest_from_manifest(assets: &[ManifestAsset]) -> [u8; 32] {
    let entries = assets
        .iter()
        .map(|asset| (asset.real_sha256, asset.byte_len, asset.media_type.as_str()))
        .collect();
    asset_set_digest(entries)
}

fn asset_set_digest(mut entries: Vec<([u8; 32], u64, &str)>) -> [u8; 32] {
    entries.sort_by_key(|entry| entry.0);
    let mut hasher = Sha256::new();
    hasher.update(CHECKPOINT_ASSET_SET_DOMAIN);
    for (real_sha256, byte_len, media_type) in entries {
        hasher.update(real_sha256);
        hasher.update(byte_len.to_be_bytes());
        hasher.update((media_type.len() as u16).to_be_bytes());
        hasher.update(media_type.as_bytes());
    }
    hasher.finalize().into()
}

/// 导入 snapshot 后校验 Loro 侧确实是 full-history。
///
/// `SnapshotV1.full_history` 只是载荷自报字段；Loro shallow snapshot 的
/// `oplog_vv` 仍可能等于完整 VV，只比较 VV/frontier 会把被裁掉历史的快照
/// 当成合法基线。因此三个 snapshot 导入点（checkpoint validate / bootstrap /
/// rotation_stage）都必须再用 Loro 自己的 shallow 状态兜底。
pub(crate) fn ensure_snapshot_full_history(doc: &loro::LoroDoc) -> Result<(), SyncError> {
    let shallow_since = doc.shallow_since_vv();
    if doc.is_shallow() || !shallow_since.is_empty() {
        return Err(SyncError::Object("快照不是 full-history（shallow）"));
    }
    Ok(())
}

/// ADR §32：把 snapshot 导入**独立空 LoroDoc**，用 actual VV/frontier 验证 manifest 声明。
///
/// - snapshot 载荷自报的 `full_history` 与 Loro shallow 状态都必须是 full-history；
/// - snapshot 载荷自报的 VV 必须等于 actual（防止载荷宣称高于真实历史）；
/// - manifest 声明的 VV 必须被 actual 覆盖（虚高/缺 op 必须失败）；
/// - frontier 必须与 actual 状态一致。
fn verify_snapshot_against_manifest(
    snapshot: &SnapshotV1,
    manifest_doc: &ManifestDoc,
) -> Result<loro::VersionVector, SyncError> {
    if snapshot.vv != manifest_doc.vv || snapshot.frontier != manifest_doc.frontier {
        return Err(SyncError::Object(
            "validate: snapshot 载荷宣称与 manifest 不一致",
        ));
    }
    let doc = loro::LoroDoc::new();
    doc.import(&snapshot.loro_snapshot_bytes)
        .map_err(|_| SyncError::Object("validate: 快照 Loro 数据非法"))?;
    ensure_snapshot_full_history(&doc)?;
    let actual_vv = doc.oplog_vv();
    let payload_vv = decode_vv(&snapshot.vv)?;
    if actual_vv != payload_vv {
        return Err(SyncError::Object(
            "validate: 快照载荷宣称 VV 与实际 Loro 历史不符",
        ));
    }
    let claimed_vv = decode_vv(&manifest_doc.vv)?;
    if claimed_vv.sub_iter(&actual_vv).next().is_some() {
        return Err(SyncError::Object(
            "validate: 实际 Loro 历史未覆盖 manifest 声明的 VV（虚高/缺 op）",
        ));
    }
    let actual_frontier = doc.state_frontiers();
    let actual_has_frontier = actual_frontier.iter().next().is_some();
    if manifest_doc.frontier.is_empty() {
        if actual_has_frontier {
            return Err(SyncError::Object(
                "validate: manifest frontier 为空但快照有实际 frontier",
            ));
        }
    } else if actual_frontier.encode() != manifest_doc.frontier {
        return Err(SyncError::Object(
            "validate: 快照 frontier 与实际 Loro 状态不符",
        ));
    }
    Ok(actual_vv)
}

fn checkpoint_data_refs(
    docs: &[DocSnapshotRef],
    assets: &[AssetRef],
    epoch: u64,
) -> Vec<ObjectRefV1> {
    let mut refs = Vec::with_capacity(docs.len() + assets.len());
    refs.extend(docs.iter().map(|doc| ObjectRefV1 {
        object_id: doc.object.object_id,
        object_type: ObjectType::Snapshot,
        generation: doc.generation,
        key_epoch: epoch,
        part_count: doc.object.part_count,
        total_plain_len: doc.object.total_plain_len,
        object_digest: doc.object.object_digest,
    }));
    refs.extend(assets.iter().map(|asset| ObjectRefV1 {
        object_id: asset.object.object_id,
        object_type: ObjectType::Asset,
        generation: 0,
        key_epoch: epoch,
        part_count: asset.object.part_count,
        total_plain_len: asset.object.total_plain_len,
        object_digest: asset.object.object_digest,
    }));
    refs
}

pub(crate) fn asset_plaintext(
    real_sha256: &[u8; 32],
    byte_len: u64,
    media_type: &str,
    data: &[u8],
) -> Result<Vec<u8>, SyncError> {
    if media_type.is_empty() || media_type.len() > 255 {
        return Err(SyncError::Object("资产 MIME 长度非法"));
    }
    let mut out = Vec::with_capacity(32 + 8 + 2 + media_type.len() + data.len());
    out.extend_from_slice(real_sha256);
    out.extend_from_slice(&byte_len.to_be_bytes());
    out.extend_from_slice(&(media_type.len() as u16).to_be_bytes());
    out.extend_from_slice(media_type.as_bytes());
    out.extend_from_slice(data);
    Ok(out)
}

/// 由确定性加密结果构造 manifest/refs 使用的公开事实。
fn uploaded_object_from_parts(
    object_id: &ObjectId,
    encrypted: &EncryptedObjectParts,
) -> UploadedObject {
    UploadedObject {
        object_id: *object_id,
        part_count: encrypted.part_count,
        total_plain_len: encrypted.total_plain_len,
        object_digest: encrypted.object_digest,
    }
}

pub(crate) fn verify_asset_plaintext(bytes: &[u8], asset: &AssetRef) -> Result<(), SyncError> {
    if bytes.len() < 32 + 8 + 2 {
        return Err(SyncError::Object("资产明文前缀不完整"));
    }
    let mut sha = [0u8; 32];
    sha.copy_from_slice(&bytes[..32]);
    let mut len = [0u8; 8];
    len.copy_from_slice(&bytes[32..40]);
    let mut mime_len = [0u8; 2];
    mime_len.copy_from_slice(&bytes[40..42]);
    let mime_len = u16::from_be_bytes(mime_len) as usize;
    let body_start = 42 + mime_len;
    let data = bytes
        .get(body_start..)
        .ok_or(SyncError::Object("资产 MIME 越界"))?;
    if sha != asset.real_sha256
        || u64::from_be_bytes(len) != asset.byte_len
        || Sha256::digest(data).as_slice() != &asset.real_sha256[..]
        || &bytes[42..body_start] != asset.media_type.as_bytes()
    {
        return Err(SyncError::Object("资产明文校验失败"));
    }
    Ok(())
}

/// ASSET 对象名的 HMAC 域（ADR §31，v1 冻结前的 wire 修正）。
///
/// `object_id` 必须提交**完整资产明文头部 + 数据摘要**，否则同一份数据换个 MIME
/// 会派生同一个 `object_id`/nonce，导致 XChaCha20-Poly1305 在 nonce 复用下加密不同明文。
const ASSET_OBJECT_DOMAIN: &[u8] = b"MN/v1/asset-object\0";

/// 资产对象 id（ADR §31）：
/// `HMAC-SHA256(K_asset_id, "MN/v1/asset-object\0" || real_sha256
///             || BE64(byte_len) || BE16(mime_len) || mime)`。
///
/// 输入就是 `asset_plaintext` 的完整头部；MIME/长度不同 → 不同 object_id，因此
/// §19 的确定性 nonce（由 object_id 派生）在存在 nonce 复用时**必然**对应同一明文头。
pub fn asset_object_id(
    root: &RootSecretV1,
    real_sha256: &[u8; 32],
    byte_len: u64,
    media_type: &str,
) -> Result<ObjectId, SyncError> {
    if media_type.is_empty() || media_type.len() > 255 {
        return Err(SyncError::Object("资产 MIME 长度非法"));
    }
    let key = derive_asset_id_key(root)?;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key.expose_slice())
        .map_err(|_| SyncError::Object("资产 HMAC 密钥非法"))?;
    mac.update(ASSET_OBJECT_DOMAIN);
    mac.update(real_sha256);
    mac.update(&byte_len.to_be_bytes());
    mac.update(&(media_type.len() as u16).to_be_bytes());
    mac.update(media_type.as_bytes());
    let digest = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

/// 一段已证明被覆盖的 relay 序号闭区间（含首尾）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeqRange {
    start: u64,
    end: u64,
}

/// 单条 inbox 行在 coverage 证明中的结论。
#[derive(Debug, Clone, Copy)]
struct InboxCoverage {
    object_id: ObjectId,
    proven: bool,
}

/// 本地持久账簿能单独证明的 `1..=S` 覆盖。
#[derive(Debug, Default)]
struct LocalCoverage {
    log_id: Option<[u8; 16]>,
    rows: BTreeMap<u64, InboxCoverage>,
    proven: Vec<SeqRange>,
    /// engine pull 在推进游标前落盘的 durable skipped-asset 记录（v3，已验 HMAC 并
    /// 绑定完整 ObjectCommit）。
    ///
    /// key 必须受 HMAC 保护：checkpoint 只认这些记录，不再从"无 inbox 行 + 在游标
    /// 之前"推断 ASSET。
    skipped_assets: BTreeMap<u64, crate::engine::SkippedAssetRecord>,
}

/// 覆盖复核返回的**私有验证事实**（只在本 crate 内流转，不进 wire）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CoverageProof {
    /// 由 durable skipped-asset 记录（新修订）证明的 ASSET 数。
    durable_assets: usize,
    /// 重扫日志命中 pending 候选并真实下载验证的 ASSET 数。
    verified_candidates: usize,
    /// 覆盖证明事实摘要（按 seq 排序后 `seq||oid||real_sha||byte_len||digest`）。
    proof_digest: [u8; 32],
}

impl CoverageProof {
    fn empty() -> Self {
        Self {
            durable_assets: 0,
            verified_candidates: 0,
            proof_digest: coverage_proof_digest(Vec::new()),
        }
    }
}

/// 覆盖证明事实摘要的域分离前缀。
const COVERAGE_PROOF_DOMAIN: &[u8] = b"MN/v1/coverage-proof\0";

/// 覆盖证明的一条事实：`(relay_seq, object_id, real_sha256, byte_len, object_digest)`。
type CoverageProofEntry = (u64, ObjectId, [u8; 32], u64, [u8; 32]);

fn coverage_proof_digest(mut entries: Vec<CoverageProofEntry>) -> [u8; 32] {
    entries.sort_by_key(|entry| entry.0);
    let mut hasher = Sha256::new();
    hasher.update(COVERAGE_PROOF_DOMAIN);
    for (relay_seq, object_id, real_sha256, byte_len, object_digest) in entries {
        hasher.update(relay_seq.to_be_bytes());
        hasher.update(object_id);
        hasher.update(real_sha256);
        hasher.update(byte_len.to_be_bytes());
        hasher.update(object_digest);
    }
    hasher.finalize().into()
}

/// ASSET 正缓存的验证器修订号（改动验证口径时必须递增，旧缓存不得命中）。
const ASSET_VERIFY_REVISION: u32 = 2;

/// 完整声明键：同一 OID 的不同声明、不同根/epoch 绝不命中同一条正缓存。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct AssetDeclKey {
    validator_revision: u32,
    root_fingerprint: [u8; 32],
    vault_id: VaultId,
    epoch: u64,
    object_id: ObjectId,
    object_type_tag: u8,
    generation: u64,
    message_id: [u8; 16],
    sender_seq: u64,
    part_count: u32,
    total_len: u64,
    object_digest: [u8; 32],
}

fn object_type_tag(object_type: ObjectType) -> u8 {
    match object_type {
        ObjectType::Update => 0,
        ObjectType::Snapshot => 1,
        ObjectType::Manifest => 2,
        ObjectType::Asset => 3,
        ObjectType::KeySlot => 4,
    }
}

/// 验证根的指纹（域分离 SHA256；不泄露根明文，但不同根必然不同）。
fn root_fingerprint(root: &RootSecretV1) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"MN/v1/verify-root\0");
    hasher.update(root.root.expose_slice());
    hasher.finalize().into()
}

fn asset_decl_key(
    root: &RootSecretV1,
    vault_id: VaultId,
    expected: &AssetExpectation<'_>,
) -> Result<AssetDeclKey, SyncError> {
    let message_id = crate::upload::asset_message_id(root, expected.object_id)?;
    if let Some(declared) = expected.message_id {
        if declared != message_id {
            return Err(SyncError::Object(
                "ASSET 声明的 message_id 不是本根确定性派生值",
            ));
        }
    }
    Ok(AssetDeclKey {
        validator_revision: ASSET_VERIFY_REVISION,
        root_fingerprint: root_fingerprint(root),
        vault_id,
        epoch: expected.key_epoch,
        object_id: *expected.object_id,
        object_type_tag: object_type_tag(expected.object_type),
        generation: expected.generation,
        message_id,
        sender_seq: expected.sender_seq,
        part_count: expected.part_count,
        total_len: expected.total_len,
        object_digest: *expected.object_digest,
    })
}

/// validate 单次运行内的 ASSET 正缓存（只缓存成功验证的事实）。
///
/// 不缓存明文/密文；键包含验证器修订、根指纹、vault/epoch 与完整声明，因此
/// "同 OID 不同 digest/parts/长度/epoch"与"跨根"都不可能命中。
#[derive(Debug, Default)]
struct VerifiedAssetCache {
    entries: std::collections::HashMap<AssetDeclKey, VerifiedAssetFacts>,
}

/// 精确命中正缓存则复用，否则真实下载验证并只把**成功**结果写回缓存。
fn fetch_verified_asset_cached<C: RelayClient>(
    client: &mut C,
    cache: &mut VerifiedAssetCache,
    root: &RootSecretV1,
    vault_id: VaultId,
    max_total_len: u64,
    expected: &AssetExpectation<'_>,
) -> Result<VerifiedAssetFacts, SyncError> {
    let key = asset_decl_key(root, vault_id, expected)?;
    if let Some(hit) = cache.entries.get(&key) {
        return Ok(hit.clone());
    }
    let facts = fetch_verified_asset(client, root, vault_id, max_total_len, expected)?;
    cache.entries.insert(key, facts.clone());
    Ok(facts)
}

/// 回执 HMAC 的初始化/校验。密钥只存在于栈上，不 Debug/不落日志。
fn receipt_mac(key: &Secret32, kind: u8, body: &[u8]) -> Result<[u8; 32], SyncError> {
    let mut mac: Hmac<Sha256> = Hmac::new_from_slice(key.expose_slice())
        .map_err(|_| SyncError::Object("checkpoint receipt HMAC 初始化失败"))?;
    mac.update(crate::domain::CHECKPOINT_RECEIPT_MAC_DOMAIN);
    mac.update(&[kind]);
    mac.update(body);
    Ok(mac.finalize().into_bytes().into())
}

fn receipt_mac_verify(key: &Secret32, kind: u8, body: &[u8], tag: &[u8]) -> bool {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(key.expose_slice()) else {
        return false;
    };
    mac.update(crate::domain::CHECKPOINT_RECEIPT_MAC_DOMAIN);
    mac.update(&[kind]);
    mac.update(body);
    mac.verify_slice(tag).is_ok()
}

/// 持久 coverage receipt 里的一条**待验证候选**（不是成功覆盖证明）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum CoverageClaim {
    /// prepare 时从固定起点实际看到的 ASSET 事件：走完整 `ObjectCommit` 字节。
    ///
    /// validate 必须从同一固定起点重新看到**逐字节相同**的 commit，并真实下载验证
    /// 该资产（旧 active 根）；任何字段变化/缺对象都不得合并覆盖区间。
    PendingAsset { relay_seq: u64, commit: Vec<u8> },
}

/// prepare 落盘的 coverage **pending 候选**：固定 `log_id/coverage_epoch/cursor/起始 seq`
/// + 缺口里看到的完整 ASSET 声明。
///
/// 该结构只证明"prepare 看到了这些声明"，不证明"覆盖已成立"；成功覆盖只能由
/// [`CheckpointWriter::verify_coverage_with_receipt`] 实证后返回。
#[derive(Debug, Clone, PartialEq, Eq)]
struct CoverageReceipt {
    vault_id: VaultId,
    checkpoint_id: [u8; 16],
    log_id: [u8; 16],
    /// prepare 时 relay 的 **active** epoch（候选基线 = `from_epoch`，不是 `to_epoch`）。
    coverage_epoch: u64,
    covered_seq: u64,
    cursor: String,
    cursor_seq: u64,
    claims: Vec<CoverageClaim>,
}

/// coverage receipt 的 meta 键前缀（沿用现有 `meta` 表，不新增 schema）。
const COVERAGE_RECEIPT_META_PREFIX: &str = "sync.checkpoint.coverage_receipt.";
/// pending 候选格式版本：v3 只记录完整 ObjectCommit 候选，**不含**任何成功覆盖声明，
/// 并用 `K_receipt` 做 HMAC-SHA256。旧 v1/v2（无 HMAC）一律拒绝；必须重新 prepare。
const COVERAGE_RECEIPT_MAGIC: &[u8] = b"MN/cp-pending/3\0";
const COVERAGE_RECEIPT_CLAIM_PENDING_ASSET: u8 = 1;
const COVERAGE_RECEIPT_MAX_CLAIMS: usize = 4096;
const COVERAGE_RECEIPT_MAX_CURSOR: usize = 4096;
/// 单条候选保存的 ObjectCommit 编码上限（当前编码 ≤ 160B）。
const COVERAGE_RECEIPT_MAX_COMMIT: usize = 512;
const COVERAGE_SCAN_PAGE_LIMIT: u32 = 256;
const COVERAGE_SCAN_MAX_PAGES: usize = 4096;

fn coverage_receipt_key(checkpoint_id: &[u8; 16]) -> String {
    format!(
        "{COVERAGE_RECEIPT_META_PREFIX}{}",
        crate::base64url::encode(checkpoint_id)
    )
}

impl CoverageReceipt {
    fn encode(&self, key: &Secret32) -> Result<Vec<u8>, SyncError> {
        if self.cursor.len() > COVERAGE_RECEIPT_MAX_CURSOR
            || self.claims.len() > COVERAGE_RECEIPT_MAX_CLAIMS
        {
            return Err(SyncError::Object("coverage receipt 超出长度上限"));
        }
        let cursor_len = u32::try_from(self.cursor.len())
            .map_err(|_| SyncError::Object("coverage receipt 游标超长"))?;
        let claim_count = u32::try_from(self.claims.len())
            .map_err(|_| SyncError::Object("coverage receipt 声明过多"))?;
        let mut out = Vec::with_capacity(128 + self.cursor.len() + self.claims.len() * 64);
        out.extend_from_slice(COVERAGE_RECEIPT_MAGIC);
        out.extend_from_slice(&self.vault_id);
        out.extend_from_slice(&self.checkpoint_id);
        out.extend_from_slice(&self.log_id);
        out.extend_from_slice(&self.coverage_epoch.to_be_bytes());
        out.extend_from_slice(&self.covered_seq.to_be_bytes());
        out.extend_from_slice(&self.cursor_seq.to_be_bytes());
        out.extend_from_slice(&cursor_len.to_be_bytes());
        out.extend_from_slice(self.cursor.as_bytes());
        out.extend_from_slice(&claim_count.to_be_bytes());
        for claim in &self.claims {
            match claim {
                CoverageClaim::PendingAsset { relay_seq, commit } => {
                    if commit.is_empty() || commit.len() > COVERAGE_RECEIPT_MAX_COMMIT {
                        return Err(SyncError::Object("coverage receipt 候选 commit 长度非法"));
                    }
                    out.push(COVERAGE_RECEIPT_CLAIM_PENDING_ASSET);
                    out.extend_from_slice(&relay_seq.to_be_bytes());
                    out.extend_from_slice(&(commit.len() as u32).to_be_bytes());
                    out.extend_from_slice(commit);
                }
            }
        }
        let mac = receipt_mac(key, crate::domain::CHECKPOINT_RECEIPT_KIND_COVERAGE, &out)?;
        out.extend_from_slice(&mac);
        Ok(out)
    }

    fn decode(bytes: &[u8], key: &Secret32) -> Result<Self, SyncError> {
        // 固定部分：vault16 + cid16 + log16 + coverage_epoch/S/cursor_seq 三个 u64 +
        // cursor_len u32 + claim_count u32 + HMAC32。
        let fixed = COVERAGE_RECEIPT_MAGIC.len() + 16 + 16 + 16 + 8 * 3 + 4 + 4 + 32;
        if bytes.len() < fixed || !bytes.starts_with(COVERAGE_RECEIPT_MAGIC) {
            return Err(SyncError::Object("coverage receipt 结构非法"));
        }
        let (body, mac) = bytes.split_at(bytes.len() - 32);
        if !receipt_mac_verify(
            key,
            crate::domain::CHECKPOINT_RECEIPT_KIND_COVERAGE,
            body,
            mac,
        ) {
            return Err(SyncError::Object(
                "coverage receipt HMAC 不符（伪造/篡改或根不匹配）",
            ));
        }
        let mut input = &body[COVERAGE_RECEIPT_MAGIC.len()..];
        let vault_id = take_array::<16>(&mut input)?;
        let checkpoint_id = take_array::<16>(&mut input)?;
        let log_id = take_array::<16>(&mut input)?;
        let coverage_epoch = take_u64(&mut input)?;
        let covered_seq = take_u64(&mut input)?;
        let cursor_seq = take_u64(&mut input)?;
        let cursor_len = take_u32(&mut input)? as usize;
        if cursor_len > COVERAGE_RECEIPT_MAX_CURSOR {
            return Err(SyncError::Object("coverage receipt 游标超长"));
        }
        let cursor = String::from_utf8(take(&mut input, cursor_len)?.to_vec())
            .map_err(|_| SyncError::Object("coverage receipt 游标不是 UTF-8"))?;
        let claim_count = take_u32(&mut input)? as usize;
        if claim_count > COVERAGE_RECEIPT_MAX_CLAIMS {
            return Err(SyncError::Object("coverage receipt 声明过多"));
        }
        let mut claims = Vec::with_capacity(claim_count);
        for _ in 0..claim_count {
            let tag = take(&mut input, 1)?[0];
            let claim = match tag {
                COVERAGE_RECEIPT_CLAIM_PENDING_ASSET => {
                    let relay_seq = take_u64(&mut input)?;
                    let commit_len = take_u32(&mut input)? as usize;
                    if commit_len == 0 || commit_len > COVERAGE_RECEIPT_MAX_COMMIT {
                        return Err(SyncError::Object("coverage receipt 候选 commit 长度非法"));
                    }
                    let commit = take(&mut input, commit_len)?.to_vec();
                    let decoded = ObjectCommit::decode(&commit)
                        .map_err(|_| SyncError::Object("coverage receipt 候选 commit 非法"))?;
                    if decoded.object_type != ObjectType::Asset
                        || decoded.generation != 0
                        || decoded.sender_seq != 0
                        || decoded.key_epoch == 0
                        || decoded.total_len == 0
                        || decoded.total_len > MAX_ASSET_RAW_BYTES as u64
                    {
                        return Err(SyncError::Object(
                            "coverage receipt 候选不是合法 ASSET 声明",
                        ));
                    }
                    CoverageClaim::PendingAsset { relay_seq, commit }
                }
                // 旧版 `ConsumedAsset`（tag=2）是"无 inbox 行即在游标前 → 推断 ASSET"
                // 的证据，评审 0003 P0-B 残余已删除该推断；旧 receipt 一律 fail-closed，
                // 必须重新 prepare 让 engine 的 durable 记录来证明。
                _ => return Err(SyncError::Object("coverage receipt 声明类型非法")),
            };
            claims.push(claim);
        }
        if !input.is_empty() {
            return Err(SyncError::Object("coverage receipt 尾部有多余字节"));
        }
        Ok(Self {
            vault_id,
            checkpoint_id,
            log_id,
            coverage_epoch,
            covered_seq,
            cursor,
            cursor_seq,
            claims,
        })
    }
}

fn take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8], SyncError> {
    if input.len() < len {
        return Err(SyncError::Object("coverage receipt 长度非法"));
    }
    let (head, tail) = input.split_at(len);
    *input = tail;
    Ok(head)
}

fn take_array<const N: usize>(input: &mut &[u8]) -> Result<[u8; N], SyncError> {
    let bytes = take(input, N)?;
    bytes
        .try_into()
        .map_err(|_| SyncError::Object("coverage receipt 字段长度非法"))
}

fn take_u32(input: &mut &[u8]) -> Result<u32, SyncError> {
    let bytes = take(input, 4)?;
    let value: [u8; 4] = bytes
        .try_into()
        .map_err(|_| SyncError::Object("coverage receipt 字段长度非法"))?;
    Ok(u32::from_be_bytes(value))
}

fn take_u64(input: &mut &[u8]) -> Result<u64, SyncError> {
    let bytes = take(input, 8)?;
    let value: [u8; 8] = bytes
        .try_into()
        .map_err(|_| SyncError::Object("coverage receipt 字段长度不足"))?;
    Ok(u64::from_be_bytes(value))
}

/// 一次连续日志扫描的结果。
struct LogScanReport {
    /// 请求游标指向的序号（relay 通过 `through_seq`/首事件证明）。
    cursor_seq: u64,
    through_seq: u64,
    first_page_empty: bool,
    reached_covered_seq: bool,
}

/// 从固定游标连续扫描到 `covered_seq`（含）。
///
/// 检查：
/// - page epoch 必须等于 checkpoint epoch；
/// - 非空页 `next_cursor` 必须前进且不得回退到已见 token；
/// - 页内/跨页序号必须严格连续（首事件紧接游标位置）；
/// - 空页只允许出现在第一页（表示游标已在 S 之后），由调用方决定是否接受。
fn scan_coverage_log<C: RelayClient>(
    client: &mut C,
    epoch: u64,
    covered_seq: u64,
    cursor: &str,
    expected_cursor_seq: Option<u64>,
    mut visit: impl FnMut(&RelayLogEvent) -> Result<(), SyncError>,
) -> Result<LogScanReport, SyncError> {
    let mut current = cursor.to_owned();
    if current.is_empty() {
        return Err(SyncError::Object("validate: coverage scan 游标为空"));
    }
    let mut seen = HashSet::new();
    seen.insert(current.clone());
    let mut cursor_seq = expected_cursor_seq;
    let mut expected_next: Option<u64> = None;
    let mut prev_through: Option<u64> = None;
    let mut first_page_empty = false;
    for page_index in 0..COVERAGE_SCAN_MAX_PAGES {
        let page = client.log(Some(&current), COVERAGE_SCAN_PAGE_LIMIT)?;
        if page.epoch != epoch {
            return Err(SyncError::Object(
                "validate: coverage scan 的 page epoch 与 checkpoint 不一致",
            ));
        }
        if page.query_head < covered_seq {
            return Err(SyncError::Object("validate: relay head 未覆盖 S"));
        }
        if page.events.is_empty() {
            if page_index == 0 {
                first_page_empty = true;
                if let Some(expected) = expected_cursor_seq {
                    if page.through_seq != expected {
                        return Err(SyncError::Object(
                            "validate: coverage scan 的起点序号与持久 receipt 不一致",
                        ));
                    }
                } else {
                    cursor_seq = Some(page.through_seq);
                }
            } else {
                return Err(SyncError::Object(
                    "validate: 未证明覆盖到 S（coverage scan 中途返回空页）",
                ));
            }
            let cursor_seq =
                cursor_seq.ok_or(SyncError::Object("validate: coverage scan 起点未知"))?;
            return Ok(LogScanReport {
                cursor_seq,
                through_seq: page.through_seq,
                first_page_empty,
                reached_covered_seq: page.through_seq >= covered_seq,
            });
        }
        let first_seq = page.events[0].relay_seq;
        if cursor_seq.is_none() {
            cursor_seq = Some(first_seq.saturating_sub(1));
        }
        if page.next_cursor == current || seen.contains(&page.next_cursor) {
            return Err(SyncError::Object(
                "validate: coverage scan 的 next_cursor 未前进/回退",
            ));
        }
        seen.insert(page.next_cursor.clone());
        if let Some(prev) = prev_through {
            if page.through_seq <= prev {
                return Err(SyncError::Object(
                    "validate: coverage scan 的 through_seq 未前进",
                ));
            }
        }
        let mut last = 0u64;
        for event in &page.events {
            match expected_next {
                Some(expected) if event.relay_seq < expected => {
                    return Err(SyncError::Object("validate: coverage scan 日志事件乱序"));
                }
                Some(expected) if event.relay_seq > expected => {
                    return Err(SyncError::Object("validate: coverage scan 日志序号缺口"));
                }
                None => {
                    let anchor =
                        cursor_seq.ok_or(SyncError::Object("validate: coverage scan 起点未知"))?;
                    let expected = anchor
                        .checked_add(1)
                        .ok_or(SyncError::Object("validate: coverage scan 起点溢出"))?;
                    if event.relay_seq < expected {
                        return Err(SyncError::Object("validate: coverage scan 日志事件乱序"));
                    }
                    if event.relay_seq > expected {
                        return Err(SyncError::Object("validate: coverage scan 日志序号缺口"));
                    }
                }
                _ => {}
            }
            expected_next = Some(
                event
                    .relay_seq
                    .checked_add(1)
                    .ok_or(SyncError::Object("validate: coverage scan 序号溢出"))?,
            );
            last = event.relay_seq;
            if event.relay_seq <= covered_seq {
                visit(event)?;
            }
        }
        if page.through_seq < last {
            return Err(SyncError::Object(
                "validate: coverage scan 的 through_seq 早于最后事件",
            ));
        }
        if page.through_seq >= covered_seq {
            let cursor_seq =
                cursor_seq.ok_or(SyncError::Object("validate: coverage scan 起点未知"))?;
            return Ok(LogScanReport {
                cursor_seq,
                through_seq: page.through_seq,
                first_page_empty,
                reached_covered_seq: true,
            });
        }
        prev_through = Some(page.through_seq);
        current = page.next_cursor;
    }
    Err(SyncError::Object(
        "validate: 未证明覆盖到 S（coverage scan 页数超过上限）",
    ))
}

fn merge_ranges(mut ranges: Vec<SeqRange>) -> Vec<SeqRange> {
    ranges.retain(|range| range.start >= 1 && range.start <= range.end);
    ranges.sort_by_key(|range| range.start);
    let mut out: Vec<SeqRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(last) = out.last_mut() {
            if range.start <= last.end.saturating_add(1) {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        out.push(range);
    }
    out
}

fn missing_ranges(covered: &[SeqRange], up_to: u64) -> Vec<SeqRange> {
    let mut out = Vec::new();
    let mut next = 1u64;
    for range in covered {
        if range.end < next {
            continue;
        }
        if range.start > up_to {
            break;
        }
        if range.start > next {
            out.push(SeqRange {
                start: next,
                end: range.start - 1,
            });
        }
        next = range.end.saturating_add(1);
        if next > up_to {
            return out;
        }
    }
    if next <= up_to {
        out.push(SeqRange {
            start: next,
            end: up_to,
        });
    }
    out
}

fn range_contains(ranges: &[SeqRange], seq: u64) -> bool {
    ranges
        .iter()
        .any(|range| range.start <= seq && seq <= range.end)
}

fn clamp_ranges(ranges: &[SeqRange], low: u64, high: u64) -> Vec<SeqRange> {
    ranges
        .iter()
        .filter_map(|range| {
            let start = range.start.max(low);
            let end = range.end.min(high);
            (start <= end).then_some(SeqRange { start, end })
        })
        .collect()
}

fn ranges_len(ranges: &[SeqRange]) -> u128 {
    ranges
        .iter()
        .map(|range| u128::from(range.end - range.start) + 1)
        .sum()
}

fn subtract_ranges(base: &[SeqRange], remove: &[SeqRange]) -> Vec<SeqRange> {
    let mut out = Vec::new();
    for base_range in base {
        let mut pieces = vec![*base_range];
        for removed in remove {
            let mut next = Vec::new();
            for piece in pieces {
                if removed.end < piece.start || removed.start > piece.end {
                    next.push(piece);
                    continue;
                }
                if removed.start > piece.start {
                    next.push(SeqRange {
                        start: piece.start,
                        end: removed.start - 1,
                    });
                }
                if removed.end < piece.end {
                    next.push(SeqRange {
                        start: removed.end + 1,
                        end: piece.end,
                    });
                }
            }
            pieces = next;
        }
        out.extend(pieces);
    }
    merge_ranges(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let doc = loro::LoroDoc::new();
        doc.get_text("body").insert(0, "hello").expect("insert");
        doc.commit();
        (
            doc.export(loro::ExportMode::Snapshot).expect("snapshot"),
            doc.oplog_vv().encode(),
            doc.state_frontiers().encode(),
        )
    }

    fn manifest_doc(vv: Vec<u8>, frontier: Vec<u8>) -> ManifestDoc {
        ManifestDoc {
            doc_id: "doc-1".to_owned(),
            kind: KIND_BODY,
            generation: 1,
            key_epoch: 1,
            vv,
            frontier,
            snapshot_object_id: [0x11; 32],
            part_count: 1,
            total_plain_len: 1,
            object_digest: [0x22; 32],
        }
    }

    fn snapshot(loro: Vec<u8>, vv: Vec<u8>, frontier: Vec<u8>) -> SnapshotV1 {
        SnapshotV1 {
            doc_id: "doc-1".to_owned(),
            kind: KIND_BODY,
            generation: 1,
            codec_version: "loro-v1".to_owned(),
            app_doc_schema: 1,
            min_reader: 1,
            full_history: true,
            vv,
            frontier,
            loro_snapshot_bytes: loro,
        }
    }

    #[test]
    fn validate_imports_empty_loro_and_rejects_inflated_claimed_vv() {
        let (loro, vv, frontier) = document();
        let honest = manifest_doc(vv.clone(), frontier.clone());
        verify_snapshot_against_manifest(
            &snapshot(loro.clone(), vv.clone(), frontier.clone()),
            &honest,
        )
        .expect("真实历史必须通过");

        // manifest 与载荷都宣称一个不存在的 peer 的 ops（虚高）：实际导入必须识破。
        let mut inflated = loro::VersionVector::decode(&vv).expect("vv");
        inflated.insert(0xDEAD, 9);
        let inflated = inflated.encode();
        let lying_manifest = manifest_doc(inflated.clone(), frontier.clone());
        assert!(
            verify_snapshot_against_manifest(
                &snapshot(loro, inflated, frontier.clone()),
                &lying_manifest
            )
            .is_err(),
            "虚高 VV 必须被实际 Loro 导入拒绝"
        );
    }

    #[test]
    fn validate_imports_empty_loro_and_rejects_missing_ops() {
        let (_, vv, frontier) = document();
        let claimed = manifest_doc(vv.clone(), frontier.clone());
        // 空 Loro 历史（缺 op），但载荷 VV 仍宣称有 ops。
        let empty = loro::LoroDoc::new()
            .export(loro::ExportMode::Snapshot)
            .expect("empty");
        assert!(
            verify_snapshot_against_manifest(&snapshot(empty, vv, frontier), &claimed).is_err(),
            "缺 op 的快照必须被空 LoroDoc 导入拒绝"
        );
    }

    //
    // ======================================================== 候选基线 coverage epoch
    //

    fn prepared_durable(coverage_epoch: u64) -> PreparedCheckpoint {
        let checkpoint_id = [0x71; 16];
        let object = UploadedObject {
            object_id: [0x72; 32],
            part_count: 1,
            total_plain_len: 4,
            object_digest: [0x73; 32],
        };
        PreparedCheckpoint {
            report: CheckpointReport {
                checkpoint_id,
                epoch: 2,
                covered_seq: 3,
                commit_digest: None,
                docs: vec![DocSnapshotRef {
                    doc_id: "doc-1".to_owned(),
                    kind: KIND_BODY,
                    generation: 1,
                    vv: vec![0x01],
                    frontier: Vec::new(),
                    object,
                }],
                assets: Vec::new(),
                manifest: object,
                refs_digest: [0x74; 32],
                published_at: None,
                validated: false,
            },
            footprint: ExportFootprint {
                checkpoint_id,
                epoch: 2,
                coverage_epoch,
                covered_seq: 3,
                docs: vec![DocFootprint {
                    doc_id: "doc-1".to_owned(),
                    kind: KIND_BODY,
                    generation: 1,
                    head_seq: 1,
                    head_vv: vec![0x01],
                    oplog_vv: vec![0x01],
                }],
                structure_vv: Vec::new(),
                asset_set_digest: [0x75; 32],
                asset_object_ids: Vec::new(),
            },
        }
    }

    #[test]
    fn prepared_durable_roundtrip_preserves_coverage_epoch() {
        // 候选基线：checkpoint epoch=2（to_epoch），coverage 冻结在 1（from_epoch）。
        let prepared = prepared_durable(1);
        let bytes = prepared.encode_durable().expect("encode");
        let back = PreparedCheckpoint::decode_durable(&bytes).expect("decode");
        assert_eq!(back.report.epoch, 2);
        assert_eq!(
            back.footprint.coverage_epoch, 1,
            "covered epoch 不能被回落成 to_epoch"
        );
        assert_eq!(back.footprint.epoch, 2);
        assert_eq!(back.footprint.covered_seq, 3);
    }

    #[test]
    fn prepared_durable_rejects_coverage_epoch_beyond_checkpoint_epoch() {
        // coverage_epoch 属于未来 epoch 只能是被改写/跨代错配；decode 必须 fail-closed。
        let bytes = prepared_durable(3).encode_durable().expect("encode");
        let err = PreparedCheckpoint::decode_durable(&bytes).expect_err("越界必须拒绝");
        assert!(
            matches!(err, SyncError::LocalCorrupt(_)),
            "coverage_epoch 越界必须是 LocalCorrupt，实际 {err:?}"
        );
    }

    //
    // ======================================================== ASSET 正缓存键
    //

    #[test]
    fn asset_decl_cache_key_binds_full_declaration_and_root() {
        let vault_id = [0x11; 16];
        let root_a = RootSecretV1::new(vault_id, 2, crate::keys::Secret32::from_array([0xA1; 32]));
        let root_b = RootSecretV1::new(vault_id, 2, crate::keys::Secret32::from_array([0xB2; 32]));
        let root_c = RootSecretV1::new(vault_id, 3, crate::keys::Secret32::from_array([0xA1; 32]));
        let object_id = crate::checkpoint::asset_object_id(&root_a, &[0x01; 32], 5, "text/plain")
            .expect("asset id");
        let digest = [0x02; 32];
        let base = AssetExpectation {
            object_id: &object_id,
            object_type: ObjectType::Asset,
            generation: 0,
            message_id: None,
            sender_seq: 0,
            part_count: 1,
            total_len: 47,
            object_digest: &digest,
            key_epoch: 2,
        };
        let key_a = asset_decl_key(&root_a, vault_id, &base).expect("key a");
        assert_eq!(
            key_a,
            asset_decl_key(&root_a, vault_id, &base).expect("同声明必须同键")
        );

        // 同 OID 但 digest/parts/长度/epoch 不同 → 不同键（不得命中）。
        let mut changed = base;
        let other_digest = [0x03; 32];
        changed.object_digest = &other_digest;
        assert_ne!(
            key_a,
            asset_decl_key(&root_a, vault_id, &changed).expect("digest 键")
        );
        changed = base;
        changed.part_count = 2;
        assert_ne!(
            key_a,
            asset_decl_key(&root_a, vault_id, &changed).expect("parts 键")
        );
        changed = base;
        changed.total_len = 48;
        assert_ne!(
            key_a,
            asset_decl_key(&root_a, vault_id, &changed).expect("长度键")
        );
        changed = base;
        changed.generation = 1;
        assert_ne!(
            key_a,
            asset_decl_key(&root_a, vault_id, &changed).expect("generation 键")
        );

        // 跨根（同 epoch 不同根 / 同根不同 epoch）都不得复用。
        let mut root_b_base = base;
        root_b_base.key_epoch = root_b.key_epoch;
        assert_ne!(
            key_a,
            asset_decl_key(&root_b, vault_id, &root_b_base).expect("跨根键")
        );
        let mut root_c_base = base;
        root_c_base.key_epoch = root_c.key_epoch;
        root_c_base.object_id = &object_id;
        assert_ne!(
            key_a,
            asset_decl_key(&root_c, vault_id, &root_c_base).expect("跨 epoch 键")
        );

        // message_id 声明与根派生不符时，键构造就失败（不会出现"脏命中"）。
        let mut forged = base;
        forged.message_id = Some([0xEE; 16]);
        assert!(asset_decl_key(&root_a, vault_id, &forged).is_err());
    }

    //
    // ======================================================== 旧修订拒绝
    //

    fn sample_verified() -> VerifiedBaseline {
        VerifiedBaseline {
            facts: BaselineFacts {
                vault_id: [0x11; 16],
                checkpoint_id: [0x22; 16],
                epoch: 2,
                covered_seq: 7,
                cursor: "c7".to_owned(),
                manifest_object_id: [0x33; 32],
                manifest_digest: [0x44; 32],
                refs_digest: [0x55; 32],
                imported_docs: 3,
                imported_vv_digest: [0x66; 32],
                verified_assets: 2,
                coverage_durable_assets: 1,
                coverage_verified_candidates: 1,
                coverage_proof_digest: [0x77; 32],
                coverage_epoch: 2,
                validation_digest: [0x99; 32],
            },
        }
    }

    fn receipt_key() -> Secret32 {
        derive_checkpoint_receipt_key(&RootSecretV1::new(
            [0x11; 16],
            2,
            Secret32::from_array([0xAA; 32]),
        ))
        .expect("receipt key")
    }

    fn sample_coverage_receipt() -> CoverageReceipt {
        CoverageReceipt {
            vault_id: [0x11; 16],
            checkpoint_id: [0x22; 16],
            log_id: [0x33; 16],
            coverage_epoch: 1,
            covered_seq: 7,
            cursor: "c7".to_owned(),
            cursor_seq: 6,
            claims: Vec::new(),
        }
    }

    #[test]
    fn verified_durable_roundtrips_at_current_revision() {
        let bytes = sample_verified().encode_durable().expect("encode");
        let decoded = VerifiedBaseline::decode_durable(&bytes).expect("decode");
        assert_eq!(decoded.facts, sample_verified().facts);
    }

    #[test]
    fn verified_durable_rejects_old_revision_instead_of_upgrading_bytes() {
        let bytes = sample_verified().encode_durable().expect("encode");
        let mut fields = cbor::decode(&bytes).expect("cbor");
        for (key, value) in &mut fields {
            if *key == 0 {
                *value = cbor::uint(VERIFIED_DURABLE_VERSION - 1);
            }
        }
        let old = cbor::encode(fields).expect("encode old");
        let error = VerifiedBaseline::decode_durable(&old)
            .expect_err("旧 verified 修订必须拒绝（不得把旧字节改号升级）");
        assert!(format!("{error}").contains("版本不支持"), "{error}");
    }

    #[test]
    fn coverage_receipt_rejects_old_magic() {
        let old = b"MN/cp-coverage/1\0covered-by-relay-claim".to_vec();
        let error = CoverageReceipt::decode(&old, &receipt_key()).expect_err("旧 receipt 必须拒绝");
        assert!(format!("{error}").contains("结构非法"), "{error}");
    }

    #[test]
    fn verified_receipt_rejects_old_magic() {
        let mut bytes = sample_verified().encode_durable().expect("verified bytes");
        // 直接构造旧格式 verified receipt 头（不是 VerifiedBaseline 的 CBOR）。
        bytes[..4].copy_from_slice(b"MN/c");
        let error =
            VerifiedReceipt::decode(&bytes, &receipt_key()).expect_err("旧 receipt 头必须拒绝");
        assert!(format!("{error}").contains("版本不支持"), "{error}");
    }

    #[test]
    fn coverage_receipt_hmac_rejects_tamper_and_wrong_key() {
        let key = receipt_key();
        let bytes = sample_coverage_receipt().encode(&key).expect("encode");
        assert_eq!(
            CoverageReceipt::decode(&bytes, &key).expect("decode"),
            sample_coverage_receipt()
        );

        // 改 body 任一字节：HMAC 必须失败（不能只靠 SHA256 校验和）。
        let mut tampered = bytes.clone();
        tampered[20] ^= 0x01;
        let error = CoverageReceipt::decode(&tampered, &key).expect_err("篡改必须拒绝");
        assert!(format!("{error}").contains("HMAC"), "{error}");

        // 换根派生的密钥：同字节也不能通过。
        let other = derive_checkpoint_receipt_key(&RootSecretV1::new(
            [0x11; 16],
            2,
            Secret32::from_array([0xBB; 32]),
        ))
        .expect("other key");
        assert!(CoverageReceipt::decode(&bytes, &other).is_err());
    }

    #[test]
    fn verified_receipt_hmac_rejects_tamper_and_old_revision() {
        let key = receipt_key();
        let receipt = VerifiedReceipt {
            checkpoint_id: [0x22; 16],
            vault_id: [0x11; 16],
            epoch: 2,
            covered_seq: 7,
            commit_digest: [0x33; 32],
            validation_digest: [0x44; 32],
        };
        let bytes = receipt.encode(&key).expect("encode");
        assert_eq!(
            VerifiedReceipt::decode(&bytes, &key).expect("decode"),
            receipt
        );

        let mut tampered = bytes.clone();
        tampered[VERIFIED_RECEIPT_MAGIC.len() + 4] ^= 0x80;
        let error = VerifiedReceipt::decode(&tampered, &key).expect_err("篡改必须拒绝");
        assert!(format!("{error}").contains("HMAC"), "{error}");

        // 旧 v1 magic 且无 HMAC：结构层直接拒绝。
        let mut old = bytes.clone();
        old[..VERIFIED_RECEIPT_MAGIC.len()].copy_from_slice(b"MN/cp-verified/1\0");
        assert!(VerifiedReceipt::decode(&old, &key).is_err());
    }
}

`````



@@SECTION code/020 `crates/sync/src/control_pin.rs`


`````rust
//! durable 控制链 pin：`sync.control_pin`（ADR 0013 §2 / ADR 0006 §35）。
//!
//! ## 为什么要单独的 durable pin
//!
//! `identity.control_rev` 只是**下界**（不知道同 rev 的 `record_hash`），
//! `sync.pinned_control_hash` 是 **bootstrap 摘要**（公式不同、不能当控制链
//! `record_hash`）。两者都不能识别"同 rev 替换 genesis/重写链"。因此本模块把
//! **已验证的控制链链尾**按 canonical CDE v1 落到 SQLite meta：
//!
//! ```text
//! {v:1, vault_id:b16, instance_id:b16,
//!  record:{rev, hash?},
//!  bootstrap?:{rev, hash, epoch, log_id}}
//! ```
//!
//! - `record` 与 `bootstrap` **各自带 rev**，不允许把旧 hash 挂到 `max(rev)` 上；
//! - `record.hash` 可为空：历史实例可能只有 `(instance, rev)` 下界。空 hash 只能当
//!   下界用，前进/同 rev 校验一律 [`SyncError::NeedsTrust`]（fail-closed），
//!   绝不"补成"一个没验证过的 hash；
//! - 读取失败、版本未知、字段残缺、vault 绑定不符都是**明确错误**，**不得**当成
//!   "没有 pin"（ADR 0013 §2「缺 record/损坏 → 停止，不得退回 unpinned」）。
//!
//! ## 提交规则（ADR 0006 §35 / ADR 0013 §2）
//!
//! [`commit_verified_head`] 只接受由已验证路径构造的 [`VerifiedHead`]：
//!
//! 1. `expected` 是调用方读到的当前 pin（`None` = 首次建立）；
//! 2. instance 必须相等（换实例 = `InstanceMismatch`）；
//! 3. 同 rev 的 `record_hash` 必须相等（否则 `Equivocation`）；旧 pin 没有 hash 时
//!    `NeedsTrust`（无法证明同 rev 历史完整性）；
//! 4. 高 rev 必须是**已知旧前缀**才允许前进：旧 pin 必须带 `record_hash`，且调用方
//!    已在完整验证过的链上确认该 hash 仍是新链前缀（`http_relay` 的
//!    `verify_control_pin`）；否则 `NeedsTrust`；
//! 5. 低 rev = `ControlRollback`；
//! 6. 写入在**同一事务**里做 CAS（`UPDATE ... WHERE value = <expected 的 canonical
//!    字节>` / 首次 `INSERT ... WHERE NOT EXISTS`），守卫要求恰好 1 行；冲突时整笔
//!    回滚并返回 [`SyncError::PinConflict`]，**不覆盖赢家**。
//!
//! ## `VerifiedHead` 的构造约束
//!
//! 字段全部私有，唯一构造点是 crate 内的
//! [`crate::http_relay::HttpRelayClient::verified_head`]（`/control` 全链验签、分页
//! 连续性与现有 pin 前缀检查都通过之后）。外部 crate 无法用裸响应 hash 构造它；
//! 测试夹具也只能在 crate 内部构造。

use std::cmp::Ordering;

use menghunote_storage::{AffectedGuard, Statement, StorageError, Value, Vault};
use menghunote_sync_proto::{decode_cde, encode_cde, CdeValue};

use crate::bootstrap::control_head_hash_at;
use crate::error::SyncError;
use crate::http_relay::ControlPin;
use crate::relay::ManagerChainEntry;

/// meta 键：durable 控制链 pin（canonical CDE v1）。
pub const META_CONTROL_PIN: &str = "sync.control_pin";

/// pin 结构版本（canonical CDE 键 0）。
pub const CONTROL_PIN_VERSION: u64 = 1;

const KEY_VERSION: u64 = 0;
const KEY_VAULT_ID: u64 = 1;
const KEY_INSTANCE_ID: u64 = 2;
const KEY_RECORD: u64 = 3;
const KEY_BOOTSTRAP: u64 = 4;

const RECORD_KEY_REV: u64 = 0;
const RECORD_KEY_HASH: u64 = 1;

const BOOTSTRAP_KEY_REV: u64 = 0;
const BOOTSTRAP_KEY_HASH: u64 = 1;
const BOOTSTRAP_KEY_EPOCH: u64 = 2;
const BOOTSTRAP_KEY_LOG_ID: u64 = 3;

/// 控制记录锚：`{rev, hash?}`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinAnchorV1 {
    /// 已认证的 control_rev。
    pub rev: u64,
    /// 该 rev 的链尾 `record_hash`；`None` = 只有 rev 下界（历史实例），不得用于前进/同 rev 校验。
    pub hash: Option<[u8; 32]>,
}

/// bootstrap 锚：`{rev, hash, epoch, log_id}`（摘要公式见
/// [`crate::bootstrap::control_head_hash`]，**不是** record hash）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapAnchorV1 {
    /// bootstrap 视图对应的 control_rev（独立于 record rev）。
    pub rev: u64,
    /// `control_head_hash(state, manager_chain)`。
    pub hash: [u8; 32],
    /// 该 bootstrap 视图的 epoch。
    pub epoch: u64,
    /// relay log id（restore 会换新，不能静默归零）。
    pub log_id: [u8; 16],
}

/// durable 控制链 pin（`sync.control_pin` 的语义结构）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinV1 {
    /// pin 绑定的 vault id（换库/换身份即绑定矛盾）。
    pub vault_id: [u8; 16],
    /// relay instance id（换实例即 fail-stop）。
    pub instance_id: [u8; 16],
    /// 控制链链尾锚。
    pub record: PinAnchorV1,
    /// bootstrap 锚（可选；与 record 各自独立验证）。
    pub bootstrap: Option<BootstrapAnchorV1>,
}

impl PinV1 {
    /// 转成 `http_relay` 使用的本地 [`ControlPin`]。
    ///
    /// bootstrap 摘要只在**同一 rev** 下有意义（否则不能把旧 hash 挂到新 rev 上）；
    /// rev 不同时只保留 record 下界（前进安全由 `verify_control_pin` 的前缀检查覆盖）。
    #[must_use]
    pub fn to_control_pin(&self) -> ControlPin {
        let mut pin = ControlPin::new(self.instance_id, self.record.rev);
        if let Some(hash) = self.record.hash {
            pin = pin.with_record_hash(hash);
        }
        // bootstrap 锚**独立携带** rev/epoch/log_id：rev 不同不代表可以丢（ADR 0013 §2/§3）。
        // 旧实现"只在 rev 相等时带上 hash"会让 identity/record rev 前进后旧锚被静默忽略；
        // 现在整枚锚交给 `verify_control_pin` 按历史视图验证（同 rev 用当前视图、
        // 异 rev 用锚自己的 rev/epoch/log/管理链前缀），验证不通过就 fail-stop。
        if let Some(bootstrap) = &self.bootstrap {
            pin = pin.with_bootstrap_anchor(*bootstrap);
        }
        pin
    }
}

/// 已验证的控制链链尾。
///
/// **只能**由 [`crate::http_relay::HttpRelayClient::verified_head`] 构造（正文见模块
/// 文档）。字段私有，因此其它 crate 无法用裸响应 hash 拼一个出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedHead {
    vault_id: [u8; 16],
    instance_id: [u8; 16],
    record_rev: u64,
    record_hash: [u8; 32],
    bootstrap: Option<BootstrapAnchorV1>,
}

impl VerifiedHead {
    /// `http_relay` 在 `/control` 全链校验通过后调用（crate 内部构造点）。
    pub(crate) fn from_verified_control(
        vault_id: [u8; 16],
        instance_id: [u8; 16],
        record_rev: u64,
        record_hash: [u8; 32],
    ) -> Self {
        Self {
            vault_id,
            instance_id,
            record_rev,
            record_hash,
            bootstrap: None,
        }
    }

    /// 由 bootstrap 验证路径附加 bootstrap 锚（ADR 0013 §3；S2 使用）。
    ///
    /// **本方法自己做历史视图验证**，不再"只做结构承载"：
    ///
    /// - 锚的 rev 不得超前于已验证的链尾（否则就是把未来/错位的 hash 挂到 head 上）；
    /// - 按锚**自己的** `rev/epoch/log_id` + 截断到该 rev 的管理链复算摘要，必须逐字节
    ///   相等（[`crate::bootstrap::control_head_hash_at`]）——rev 前进时这证明当前链仍
    ///   包含旧锚认证过的前缀，rev 相同则证明锚与当前管理链视图一致；
    /// - 复算不出历史视图 → [`SyncError::NeedsTrust`]（**不**降级成"没有锚"），
    ///   摘要不符 → [`SyncError::Equivocation`]。
    ///
    /// 同 rev 的 `epoch/log_id` 与 relay 当前 `/state` 的逐字段比对由
    /// [`crate::http_relay`] 的 `verify_control_pin` 负责（它同时看到 state 与链）。
    pub fn with_bootstrap(
        mut self,
        anchor: BootstrapAnchorV1,
        chain: &[ManagerChainEntry],
    ) -> Result<Self, SyncError> {
        if anchor.rev > self.record_rev {
            return Err(SyncError::Equivocation(
                "bootstrap 锚的 rev 超前于已验证控制链链尾",
            ));
        }
        let computed = control_head_hash_at(self.vault_id, self.instance_id, &anchor, chain)
            .ok_or(SyncError::NeedsTrust(
                "bootstrap 锚的 rev 不在当前管理链视图内，无法证明历史前缀",
            ))?;
        if computed != anchor.hash {
            return Err(SyncError::Equivocation(
                "bootstrap 锚与当前管理链的历史视图不一致",
            ));
        }
        self.bootstrap = Some(anchor);
        Ok(self)
    }

    /// pin 绑定到的 vault id。
    #[must_use]
    pub const fn vault_id(&self) -> [u8; 16] {
        self.vault_id
    }

    /// 已验证的 relay instance id。
    #[must_use]
    pub const fn instance_id(&self) -> [u8; 16] {
        self.instance_id
    }

    /// 已验证的 control_rev。
    #[must_use]
    pub const fn record_rev(&self) -> u64 {
        self.record_rev
    }

    /// 该 rev 的链尾 `record_hash`。
    #[must_use]
    pub const fn record_hash(&self) -> [u8; 32] {
        self.record_hash
    }

    /// bootstrap 锚（没有则为 `None`）。
    #[must_use]
    pub const fn bootstrap(&self) -> Option<&BootstrapAnchorV1> {
        self.bootstrap.as_ref()
    }
}

/// canonical CDE 编码（唯一编码器；写库/测试夹具共用）。
pub fn encode_pin_v1(pin: &PinV1) -> Result<Vec<u8>, SyncError> {
    let mut fields = vec![
        (KEY_VERSION, CdeValue::UInt(CONTROL_PIN_VERSION)),
        (KEY_VAULT_ID, CdeValue::Bytes(pin.vault_id.to_vec())),
        (KEY_INSTANCE_ID, CdeValue::Bytes(pin.instance_id.to_vec())),
        (KEY_RECORD, encode_anchor(&pin.record)),
    ];
    if let Some(bootstrap) = &pin.bootstrap {
        fields.push((KEY_BOOTSTRAP, encode_bootstrap(bootstrap)));
    }
    encode_cde(&CdeValue::Map(fields))
        .map_err(|_| SyncError::PinSaveFailed("sync.control_pin 无法 canonical 编码".to_owned()))
}

/// canonical CDE 解码 + 严格字段/版本/长度校验（损坏即
/// [`SyncError::PinCorrupt`]，**不**返回 `None`）。
pub fn decode_pin_v1(bytes: &[u8]) -> Result<PinV1, SyncError> {
    let CdeValue::Map(fields) = decode_cde(bytes)
        .map_err(|_| SyncError::PinCorrupt("sync.control_pin 不是合法 canonical CDE"))?
    else {
        return Err(SyncError::PinCorrupt("sync.control_pin 不是 CDE map"));
    };
    ensure_keys(
        &fields,
        &[
            KEY_VERSION,
            KEY_VAULT_ID,
            KEY_INSTANCE_ID,
            KEY_RECORD,
            KEY_BOOTSTRAP,
        ],
        "sync.control_pin 含未知字段",
    )?;
    let version = uint_field(&fields, KEY_VERSION, "sync.control_pin 缺少版本")?;
    if version != CONTROL_PIN_VERSION {
        return Err(SyncError::PinCorrupt("sync.control_pin 版本不支持"));
    }
    let vault_id = fixed_field::<16>(&fields, KEY_VAULT_ID, "sync.control_pin 的 vault_id 非法")?;
    let instance_id = fixed_field::<16>(
        &fields,
        KEY_INSTANCE_ID,
        "sync.control_pin 的 instance_id 非法",
    )?;
    let record = decode_anchor(field(&fields, KEY_RECORD, "sync.control_pin 缺少 record")?)?;
    let bootstrap = match opt_field(&fields, KEY_BOOTSTRAP) {
        None => None,
        Some(value) => Some(decode_bootstrap(value)?),
    };
    Ok(PinV1 {
        vault_id,
        instance_id,
        record,
        bootstrap,
    })
}

/// 读取 durable pin：不存在 → `Ok(None)`；损坏/版本/绑定不符 → 明确错误。
pub fn load_control_pin(
    vault: &Vault,
    expected_vault: [u8; 16],
) -> Result<Option<PinV1>, SyncError> {
    let Some(bytes) = vault.get_meta(META_CONTROL_PIN)? else {
        return Ok(None);
    };
    let pin = decode_pin_v1(&bytes)?;
    if pin.vault_id != expected_vault {
        return Err(SyncError::PinCorrupt(
            "sync.control_pin 的 vault 绑定与本地 vault 不一致",
        ));
    }
    Ok(Some(pin))
}

/// 提交**已验证**的控制链 head（ADR 0006 §35：事务重读 + CAS，只进不退）。
///
/// `expected` 必须来自 [`load_control_pin`]（或在同一批验证里得到的等价快照）。
/// 失败时整笔事务回滚，库里仍是冲突赢家写的值。
pub fn commit_verified_head(
    vault: &Vault,
    expected: Option<&PinV1>,
    verified: VerifiedHead,
) -> Result<(), SyncError> {
    let new_pin = validate_transition(expected, &verified)?;
    let new_bytes = encode_pin_v1(&new_pin)?;
    let key = Value::Text(META_CONTROL_PIN.to_owned());
    let statement = match expected {
        None => Statement::new(
            // 首次：只有 key 仍不存在时才插入（同一事务内判定，不是先查后写）。
            "INSERT INTO meta(key, value) SELECT ?1, ?2 \
             WHERE NOT EXISTS (SELECT 1 FROM meta WHERE key = ?1)",
            vec![key, Value::Blob(new_bytes)],
        ),
        Some(pin) => {
            let old_bytes = encode_pin_v1(pin)?;
            Statement::new(
                // CAS：比较**存储里的 canonical 字节**；类型/内容任一不符都不更新。
                "UPDATE meta SET value = ?2 \
                 WHERE key = ?1 AND typeof(value) = 'blob' AND value = ?3",
                vec![key, Value::Blob(new_bytes), Value::Blob(old_bytes)],
            )
        }
    };
    match vault.exec_transaction(&[statement], &[AffectedGuard::exactly(0, 1)]) {
        Ok(_) => Ok(()),
        Err(StorageError::GuardFailed { .. }) => Err(concurrent_conflict(vault, expected)?),
        Err(error) => Err(SyncError::PinSaveFailed(format!(
            "sync.control_pin 写入失败：{error}"
        ))),
    }
}

/// 把"只有 `(instance, rev)` 下界"的旧 pin 升级成带 `record.hash` 的 canonical pin。
///
/// 这是**唯一**允许在同一 rev 上补 `record.hash` 的路径（ADR 0013 §3 的
/// `VerifiedMigration` / FreshRecovery 崩溃续跑）：[`commit_verified_head`] 对
/// "同 rev 但本地缺 hash" 一律 [`SyncError::NeedsTrust`]（不能凭空补一个没验证过的
/// hash），而这里补 hash 的依据是**已提交的 bootstrap 锚**：
///
/// 1. `expected` 必须仍是库里的下界 pin（调用方从 [`load_control_pin`] 读到的快照）；
/// 2. `verified` 必须来自已验证路径（[`crate::http_relay::HttpRelayClient::verified_head_with_anchor`]）；
/// 3. `verified.bootstrap` 必须存在，且**锚 rev == 已验证链尾 rev**——这正是"锚当初
///    认证的就是这个视图"的证明；否则返回 [`SyncError::NeedsTrust`]，绝不猜；
/// 4. vault/instance 必须一致；写入仍是 canonical 字节 CAS，冲突不覆盖赢家。
///
/// rev 已经前进（`expected.record.rev < verified.record_rev`）时**不**在这里补：
/// 旧 hash 缺失就无法证明旧前缀，调用方必须 `NeedsTrust`。
pub fn upgrade_lower_bound_pin(
    vault: &Vault,
    expected: &PinV1,
    verified: VerifiedHead,
) -> Result<(), SyncError> {
    if expected.vault_id != verified.vault_id {
        return Err(SyncError::PinCorrupt(
            "已验证 head 的 vault 与本地 pin 的 vault 不一致",
        ));
    }
    if expected.instance_id != verified.instance_id {
        return Err(SyncError::InstanceMismatch);
    }
    if expected.record.hash.is_some() {
        return Err(SyncError::Unsupported(
            "本地 pin 已带 record hash：走 commit_verified_head，不做下界升级",
        ));
    }
    if expected.record.rev != verified.record_rev {
        return Err(SyncError::NeedsTrust(
            "旧 pin 只有 rev 下界且链尾已前进：无法证明同 rev 历史完整性",
        ));
    }
    let Some(anchor) = verified.bootstrap else {
        return Err(SyncError::NeedsTrust(
            "缺少与链尾同视图的 bootstrap 锚：无法证明该 rev 的历史完整性",
        ));
    };
    if anchor.rev != verified.record_rev {
        return Err(SyncError::NeedsTrust(
            "bootstrap 锚的 rev 与已验证链尾不同：不是同一验证视图",
        ));
    }
    let new_pin = PinV1 {
        vault_id: expected.vault_id,
        instance_id: verified.instance_id,
        record: PinAnchorV1 {
            rev: verified.record_rev,
            hash: Some(verified.record_hash),
        },
        bootstrap: Some(anchor),
    };
    let new_bytes = encode_pin_v1(&new_pin)?;
    let old_bytes = encode_pin_v1(expected)?;
    match vault.exec_transaction(
        &[Statement::new(
            "UPDATE meta SET value = ?2 \
             WHERE key = ?1 AND typeof(value) = 'blob' AND value = ?3",
            vec![
                Value::Text(META_CONTROL_PIN.to_owned()),
                Value::Blob(new_bytes),
                Value::Blob(old_bytes),
            ],
        )],
        &[AffectedGuard::exactly(0, 1)],
    ) {
        Ok(_) => Ok(()),
        Err(StorageError::GuardFailed { .. }) => Err(concurrent_conflict(vault, Some(expected))?),
        Err(error) => Err(SyncError::PinSaveFailed(format!(
            "sync.control_pin 升级写入失败：{error}"
        ))),
    }
}

/// 语义校验（全部相对调用方读到的 `expected`；CAS 保证校验对象未被并发改写）。
fn validate_transition(
    expected: Option<&PinV1>,
    verified: &VerifiedHead,
) -> Result<PinV1, SyncError> {
    let record = PinAnchorV1 {
        rev: verified.record_rev,
        hash: Some(verified.record_hash),
    };
    let Some(expected) = expected else {
        // 首次建立：head 由已验证路径给出；这里只固化，不做"盲信 continue"之外的推断。
        return Ok(PinV1 {
            vault_id: verified.vault_id,
            instance_id: verified.instance_id,
            record,
            bootstrap: verified.bootstrap,
        });
    };
    if expected.vault_id != verified.vault_id {
        return Err(SyncError::PinCorrupt(
            "已验证 head 的 vault 与本地 pin 的 vault 不一致",
        ));
    }
    if expected.instance_id != verified.instance_id {
        return Err(SyncError::InstanceMismatch);
    }
    match verified.record_rev.cmp(&expected.record.rev) {
        Ordering::Less => {
            return Err(SyncError::ControlRollback {
                expected: expected.record.rev,
                actual: verified.record_rev,
            })
        }
        Ordering::Equal => match expected.record.hash {
            Some(hash) if hash == verified.record_hash => {}
            Some(_) => return Err(SyncError::Equivocation("同 rev 的 record hash 不一致")),
            None => {
                return Err(SyncError::NeedsTrust(
                    "同 rev 的本地 pin 缺少 record hash，无法证明同 rev 历史完整性",
                ))
            }
        },
        Ordering::Greater => {
            if expected.record.hash.is_none() {
                return Err(SyncError::NeedsTrust(
                    "高 rev 前进缺少旧 record hash 前缀 anchor；必须先建立可信同 rev pin",
                ));
            }
        }
    }
    if let Some(anchor) = verified.bootstrap.as_ref() {
        // 防"把旧 hash 挂到 max(rev)"：锚可以有更低的 rev，但绝不能超前于已验证链尾。
        if anchor.rev > verified.record_rev {
            return Err(SyncError::Equivocation(
                "bootstrap 锚的 rev 超前于已验证控制链链尾",
            ));
        }
    }
    let bootstrap = merge_bootstrap(expected.bootstrap.as_ref(), verified.bootstrap.as_ref())?;
    Ok(PinV1 {
        vault_id: expected.vault_id,
        instance_id: verified.instance_id,
        record,
        bootstrap,
    })
}

/// bootstrap 锚合并：不能因 record 前进丢 bootstrap；同 rev 必须一致。
fn merge_bootstrap(
    old: Option<&BootstrapAnchorV1>,
    new: Option<&BootstrapAnchorV1>,
) -> Result<Option<BootstrapAnchorV1>, SyncError> {
    match (old, new) {
        (None, None) => Ok(None),
        (Some(old), None) => Ok(Some(*old)),
        (None, Some(new)) => Ok(Some(*new)),
        (Some(old), Some(new)) => match new.rev.cmp(&old.rev) {
            Ordering::Less => Err(SyncError::ControlRollback {
                expected: old.rev,
                actual: new.rev,
            }),
            Ordering::Equal => {
                if new.hash != old.hash || new.epoch != old.epoch || new.log_id != old.log_id {
                    Err(SyncError::Equivocation("同 rev 的 bootstrap 锚内容不一致"))
                } else {
                    Ok(Some(*new))
                }
            }
            Ordering::Greater => {
                // 前进由调用方按旧 rev/epoch/log 视图验证历史 anchor（ADR 0013 §3）；
                // 这里只拒绝明显的 epoch 回退。
                if new.epoch < old.epoch {
                    Err(SyncError::Equivocation("bootstrap 锚的 epoch 回退"))
                } else {
                    Ok(Some(*new))
                }
            }
        },
    }
}

/// CAS 未命中后的**只读**分类：重读当前 pin，与 `expected` 比较。
fn concurrent_conflict(vault: &Vault, expected: Option<&PinV1>) -> Result<SyncError, SyncError> {
    let Some(bytes) = vault.get_meta(META_CONTROL_PIN).map_err(|error| {
        SyncError::PinSaveFailed(format!("sync.control_pin 冲突后重读失败：{error}"))
    })?
    else {
        return Ok(SyncError::PinConflict);
    };
    let current = decode_pin_v1(&bytes)?;
    if expected.is_some_and(|pin| pin == &current) {
        // 内容没变却 CAS 失败：不是并发冲突，明确报保存失败（不重试覆盖）。
        return Ok(SyncError::PinSaveFailed(
            "sync.control_pin CAS 未命中但内容未变".to_owned(),
        ));
    }
    Ok(SyncError::PinConflict)
}

fn encode_anchor(anchor: &PinAnchorV1) -> CdeValue {
    let mut fields = vec![(RECORD_KEY_REV, CdeValue::UInt(anchor.rev))];
    if let Some(hash) = anchor.hash {
        fields.push((RECORD_KEY_HASH, CdeValue::Bytes(hash.to_vec())));
    }
    CdeValue::Map(fields)
}

fn decode_anchor(value: &CdeValue) -> Result<PinAnchorV1, SyncError> {
    let CdeValue::Map(fields) = value else {
        return Err(SyncError::PinCorrupt("sync.control_pin 的 record 不是 map"));
    };
    ensure_keys(
        fields,
        &[RECORD_KEY_REV, RECORD_KEY_HASH],
        "sync.control_pin 的 record 含未知字段",
    )?;
    let rev = uint_field(fields, RECORD_KEY_REV, "sync.control_pin 的 record 缺 rev")?;
    let hash = match opt_field(fields, RECORD_KEY_HASH) {
        None => None,
        Some(CdeValue::Bytes(bytes)) => Some(
            <[u8; 32]>::try_from(bytes.as_slice())
                .map_err(|_| SyncError::PinCorrupt("sync.control_pin 的 record hash 长度非法"))?,
        ),
        Some(_) => {
            return Err(SyncError::PinCorrupt(
                "sync.control_pin 的 record hash 类型非法",
            ))
        }
    };
    Ok(PinAnchorV1 { rev, hash })
}

fn encode_bootstrap(anchor: &BootstrapAnchorV1) -> CdeValue {
    CdeValue::Map(vec![
        (BOOTSTRAP_KEY_REV, CdeValue::UInt(anchor.rev)),
        (BOOTSTRAP_KEY_HASH, CdeValue::Bytes(anchor.hash.to_vec())),
        (BOOTSTRAP_KEY_EPOCH, CdeValue::UInt(anchor.epoch)),
        (
            BOOTSTRAP_KEY_LOG_ID,
            CdeValue::Bytes(anchor.log_id.to_vec()),
        ),
    ])
}

fn decode_bootstrap(value: &CdeValue) -> Result<BootstrapAnchorV1, SyncError> {
    let CdeValue::Map(fields) = value else {
        return Err(SyncError::PinCorrupt(
            "sync.control_pin 的 bootstrap 不是 map",
        ));
    };
    ensure_keys(
        fields,
        &[
            BOOTSTRAP_KEY_REV,
            BOOTSTRAP_KEY_HASH,
            BOOTSTRAP_KEY_EPOCH,
            BOOTSTRAP_KEY_LOG_ID,
        ],
        "sync.control_pin 的 bootstrap 含未知字段",
    )?;
    Ok(BootstrapAnchorV1 {
        rev: uint_field(
            fields,
            BOOTSTRAP_KEY_REV,
            "sync.control_pin 的 bootstrap 缺 rev",
        )?,
        hash: fixed_field::<32>(
            fields,
            BOOTSTRAP_KEY_HASH,
            "sync.control_pin 的 bootstrap hash 非法",
        )?,
        epoch: uint_field(
            fields,
            BOOTSTRAP_KEY_EPOCH,
            "sync.control_pin 的 bootstrap 缺 epoch",
        )?,
        log_id: fixed_field::<16>(
            fields,
            BOOTSTRAP_KEY_LOG_ID,
            "sync.control_pin 的 bootstrap log_id 非法",
        )?,
    })
}

fn ensure_keys(
    fields: &[(u64, CdeValue)],
    allowed: &[u64],
    message: &'static str,
) -> Result<(), SyncError> {
    for (key, _) in fields {
        if !allowed.contains(key) {
            return Err(SyncError::PinCorrupt(message));
        }
    }
    Ok(())
}

fn field<'a>(
    fields: &'a [(u64, CdeValue)],
    key: u64,
    message: &'static str,
) -> Result<&'a CdeValue, SyncError> {
    fields
        .iter()
        .find(|(index, _)| *index == key)
        .map(|(_, value)| value)
        .ok_or(SyncError::PinCorrupt(message))
}

fn opt_field(fields: &[(u64, CdeValue)], key: u64) -> Option<&CdeValue> {
    fields
        .iter()
        .find(|(index, _)| *index == key)
        .map(|(_, value)| value)
}

fn uint_field(
    fields: &[(u64, CdeValue)],
    key: u64,
    message: &'static str,
) -> Result<u64, SyncError> {
    match field(fields, key, message)? {
        CdeValue::UInt(value) => Ok(*value),
        _ => Err(SyncError::PinCorrupt(message)),
    }
}

fn fixed_field<const N: usize>(
    fields: &[(u64, CdeValue)],
    key: u64,
    message: &'static str,
) -> Result<[u8; N], SyncError> {
    let CdeValue::Bytes(bytes) = field(fields, key, message)? else {
        return Err(SyncError::PinCorrupt(message));
    };
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::PinCorrupt(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rotation::ManagerTransitionV1;

    const VAULT: [u8; 16] = [0x11; 16];
    const INSTANCE: [u8; 16] = [0x22; 16];

    fn fresh() -> (tempfile::TempDir, Vault) {
        let base = std::env::var("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/tmp"));
        std::fs::create_dir_all(&base).expect("target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("control-pin-")
            .tempdir_in(&base)
            .expect("临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
        (dir, vault)
    }

    fn head(rev: u64, hash: [u8; 32]) -> VerifiedHead {
        VerifiedHead::from_verified_control(VAULT, INSTANCE, rev, hash)
    }

    fn head_with_bootstrap(rev: u64, hash: [u8; 32], bootstrap: BootstrapAnchorV1) -> VerifiedHead {
        head(rev, hash)
            .with_bootstrap(bootstrap, &test_chain())
            .expect("测试锚必须与测试管理链一致")
    }

    /// 测试管理链：rev1 genesis + rev2 转换（epoch 1 → 2）。
    fn test_chain() -> Vec<ManagerChainEntry> {
        let transition = ManagerTransitionV1 {
            vault_id: VAULT,
            instance_id: INSTANCE,
            rotation_id: [0x0c; 16],
            from_epoch: 1,
            to_epoch: 2,
            old_rev: 1,
            new_rev: 2,
            new_pubkey: [0x02; 32],
            covered_seq: 0,
        }
        .encode()
        .expect("测试转换编码");
        vec![
            ManagerChainEntry {
                rev: 1,
                pubkey: [0x01; 32],
                transition: vec![0xA1, 0xA2],
                old_rev: 1,
                old_signature: None,
                new_signature: None,
            },
            ManagerChainEntry {
                rev: 2,
                pubkey: [0x02; 32],
                transition,
                old_rev: 1,
                old_signature: Some([0x03; 64]),
                new_signature: Some([0x04; 64]),
            },
        ]
    }

    /// 与 [`test_chain`] 自洽的锚摘要（rev/epoch/log 由调用方决定）。
    ///
    /// 历史视图按**生效 epoch** 截断（rev1 + `to_epoch <= epoch` 的转换）。
    fn anchor_hash(rev: u64, epoch: u64, log_id: [u8; 16]) -> [u8; 32] {
        let chain = test_chain();
        let end = if epoch >= 2 { chain.len() } else { 1 };
        crate::bootstrap::control_head_hash_parts(
            VAULT,
            INSTANCE,
            log_id,
            epoch,
            rev,
            &chain[..end],
        )
    }

    fn load(vault: &Vault) -> Option<PinV1> {
        load_control_pin(vault, VAULT).expect("读取 pin")
    }

    fn write_raw(vault: &Vault, pin: &PinV1) {
        vault
            .set_meta(META_CONTROL_PIN, &encode_pin_v1(pin).expect("编码"))
            .expect("写 raw pin");
    }

    #[test]
    fn first_commit_is_durable_and_reloads_identically() {
        let (_dir, vault) = fresh();
        assert!(load(&vault).is_none(), "首次没有 pin");

        let verified = head(1, [0xA1; 32]);
        commit_verified_head(&vault, None, verified).expect("首次提交");
        let pin = load(&vault).expect("必须能读回");
        assert_eq!(pin.vault_id, VAULT);
        assert_eq!(pin.instance_id, INSTANCE);
        assert_eq!(pin.record.rev, 1);
        assert_eq!(pin.record.hash, Some([0xA1; 32]));
        assert_eq!(pin.bootstrap, None);
        // canonical 编码可往返。
        let bytes = encode_pin_v1(&pin).expect("编码");
        assert_eq!(decode_pin_v1(&bytes).expect("解码"), pin);
        // 存储的是 canonical 字节，不是裸响应 hash。
        assert_eq!(
            vault
                .get_meta(META_CONTROL_PIN)
                .expect("meta")
                .expect("存在"),
            bytes
        );
    }

    #[test]
    fn same_rev_with_different_hash_is_equivocation_and_does_not_overwrite() {
        let (_dir, vault) = fresh();
        commit_verified_head(&vault, None, head(2, [0xB1; 32])).expect("首次");
        let expected = load(&vault).expect("pin");
        let error = commit_verified_head(&vault, Some(&expected), head(2, [0xB2; 32]))
            .expect_err("同 rev 换 hash 必须拒绝");
        assert!(matches!(error, SyncError::Equivocation(_)), "{error:?}");
        assert_eq!(load(&vault).expect("pin").record.hash, Some([0xB1; 32]));
    }

    #[test]
    fn same_rev_without_a_known_hash_needs_trust() {
        let (_dir, vault) = fresh();
        write_raw(
            &vault,
            &PinV1 {
                vault_id: VAULT,
                instance_id: INSTANCE,
                record: PinAnchorV1 { rev: 4, hash: None },
                bootstrap: None,
            },
        );
        let expected = load(&vault).expect("pin");
        let error = commit_verified_head(&vault, Some(&expected), head(4, [0xC1; 32]))
            .expect_err("同 rev 缺 hash 不能补");
        assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");
    }

    #[test]
    fn lower_rev_is_rejected_as_rollback() {
        let (_dir, vault) = fresh();
        commit_verified_head(&vault, None, head(3, [0xD1; 32])).expect("首次");
        let expected = load(&vault).expect("pin");
        let error = commit_verified_head(&vault, Some(&expected), head(2, [0xD2; 32]))
            .expect_err("低 rev 必须拒绝");
        assert!(
            matches!(
                error,
                SyncError::ControlRollback {
                    expected: 3,
                    actual: 2
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn instance_change_is_rejected() {
        let (_dir, vault) = fresh();
        commit_verified_head(&vault, None, head(1, [0xE1; 32])).expect("首次");
        let expected = load(&vault).expect("pin");
        let other = VerifiedHead::from_verified_control(VAULT, [0x33; 16], 2, [0xE2; 32]);
        let error =
            commit_verified_head(&vault, Some(&expected), other).expect_err("换实例必须拒绝");
        assert!(matches!(error, SyncError::InstanceMismatch), "{error:?}");
    }

    #[test]
    fn advancing_without_a_known_prefix_needs_trust() {
        let (_dir, vault) = fresh();
        write_raw(
            &vault,
            &PinV1 {
                vault_id: VAULT,
                instance_id: INSTANCE,
                record: PinAnchorV1 { rev: 1, hash: None },
                bootstrap: None,
            },
        );
        let expected = load(&vault).expect("pin");
        let error = commit_verified_head(&vault, Some(&expected), head(2, [0xF1; 32]))
            .expect_err("无旧前缀不得前进");
        assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");
    }

    #[test]
    fn advancing_keeps_bootstrap_when_verified_head_has_none() {
        let (_dir, vault) = fresh();
        let bootstrap = BootstrapAnchorV1 {
            rev: 1,
            hash: anchor_hash(1, 1, [0x52; 16]),
            epoch: 1,
            log_id: [0x52; 16],
        };
        commit_verified_head(&vault, None, head_with_bootstrap(1, [0x53; 32], bootstrap))
            .expect("首次");
        let expected = load(&vault).expect("pin");
        commit_verified_head(&vault, Some(&expected), head(2, [0x54; 32])).expect("前进");
        let pin = load(&vault).expect("pin");
        assert_eq!(pin.record.rev, 2);
        assert_eq!(
            pin.bootstrap,
            Some(bootstrap),
            "不得因 record 前进丢 bootstrap"
        );
        // 同 rev 换 hash（与旧锚自洽但内容不同）：`with_bootstrap` 的历史视图就对不上。
        let inconsistent = head(2, [0x54; 32]).with_bootstrap(
            BootstrapAnchorV1 {
                hash: [0x55; 32],
                ..bootstrap
            },
            &test_chain(),
        );
        assert!(matches!(inconsistent, Err(SyncError::Equivocation(_))));

        // 自洽但与旧锚不同的同 rev 锚：`with_bootstrap` 能过（它只看锚自己的视图），
        // commit 的 merge 必须拒绝——不能因为"自洽"就替掉已提交的同 rev 锚。
        let other = BootstrapAnchorV1 {
            hash: anchor_hash(1, 1, [0x53; 16]),
            log_id: [0x53; 16],
            ..bootstrap
        };
        let error = commit_verified_head(
            &vault,
            Some(&pin),
            head_with_bootstrap(2, [0x54; 32], other),
        )
        .expect_err("同 rev bootstrap 不一致必须拒绝");
        assert!(matches!(error, SyncError::Equivocation(_)), "{error:?}");
    }

    /// 下界 pin 的同 rev 升级：必须同时满足"同视图锚 + 同 rev + 同实例"。
    #[test]
    fn lower_bound_upgrade_requires_a_same_view_anchor() {
        let (_dir, vault) = fresh();
        let bootstrap = BootstrapAnchorV1 {
            rev: 2,
            hash: anchor_hash(2, 1, [0x62; 16]),
            epoch: 1,
            log_id: [0x62; 16],
        };
        let lower = PinV1 {
            vault_id: VAULT,
            instance_id: INSTANCE,
            record: PinAnchorV1 { rev: 2, hash: None },
            bootstrap: Some(bootstrap),
        };
        write_raw(&vault, &lower);
        let expected = load(&vault).expect("pin");
        let verified = head_with_bootstrap(2, [0x63; 32], bootstrap);
        upgrade_lower_bound_pin(&vault, &expected, verified).expect("同 rev + 同视图锚可升级");
        let pin = load(&vault).expect("pin");
        assert_eq!(pin.record.rev, 2);
        assert_eq!(pin.record.hash, Some([0x63; 32]));
        assert_eq!(pin.bootstrap, Some(bootstrap));

        // 已经是 canonical pin 时不再走升级路径（明确拒绝，避免语义混淆）。
        let error =
            upgrade_lower_bound_pin(&vault, &pin, head_with_bootstrap(2, [0x63; 32], bootstrap))
                .expect_err("已带 hash 不得再升级");
        assert!(matches!(error, SyncError::Unsupported(_)), "{error:?}");

        // 每个失败模式都在写库前拒绝，且不改库里的 pin。
        for (label, expected, verified) in [
            (
                "链尾前进但旧 pin 只有下界",
                lower,
                head(3, [0x64; 32]).with_bootstrap(
                    BootstrapAnchorV1 {
                        rev: 3,
                        hash: anchor_hash(3, 1, [0x62; 16]),
                        ..bootstrap
                    },
                    &test_chain(),
                ),
            ),
            ("同 rev 但锚不同视图", lower, Ok(head(2, [0x63; 32]))),
            (
                "锚 rev 与链尾不同",
                lower,
                head(1, [0x65; 32]).with_bootstrap(
                    BootstrapAnchorV1 {
                        rev: 1,
                        hash: anchor_hash(1, 1, [0x62; 16]),
                        ..bootstrap
                    },
                    &test_chain(),
                ),
            ),
        ] {
            let verified = match verified {
                Ok(verified) => verified,
                Err(error) => panic!("{label}: 测试夹具必须是已验证 head：{error:?}"),
            };
            let error = upgrade_lower_bound_pin(&vault, &expected, verified)
                .expect_err(&format!("{label} 必须拒绝"));
            assert!(
                matches!(error, SyncError::NeedsTrust(_)),
                "{label}: {error:?}"
            );
            assert_eq!(load(&vault).expect("pin"), pin, "{label} 不得改写 pin");
        }

        // instance 不符（在写库前拒绝，不产生 CAS 冲突噪音）。
        let other_instance = PinV1 {
            instance_id: [0x44; 16],
            ..lower
        };
        let error = upgrade_lower_bound_pin(
            &vault,
            &other_instance,
            head_with_bootstrap(2, [0x63; 32], bootstrap),
        )
        .expect_err("换实例必须拒绝");
        assert!(matches!(error, SyncError::InstanceMismatch), "{error:?}");
    }

    #[test]
    fn corrupt_or_wrong_version_pins_are_errors_not_missing() {
        let (_dir, vault) = fresh();
        let blobs: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![0xFF, 0x00],
            // 版本 2。
            encode_cde(&CdeValue::Map(vec![
                (KEY_VERSION, CdeValue::UInt(2)),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_INSTANCE_ID, CdeValue::Bytes(INSTANCE.to_vec())),
                (
                    KEY_RECORD,
                    CdeValue::Map(vec![(RECORD_KEY_REV, CdeValue::UInt(1))]),
                ),
            ]))
            .expect("cde"),
            // 缺 vault_id。
            encode_cde(&CdeValue::Map(vec![
                (KEY_VERSION, CdeValue::UInt(CONTROL_PIN_VERSION)),
                (KEY_INSTANCE_ID, CdeValue::Bytes(INSTANCE.to_vec())),
                (
                    KEY_RECORD,
                    CdeValue::Map(vec![(RECORD_KEY_REV, CdeValue::UInt(1))]),
                ),
            ]))
            .expect("cde"),
            // 未知字段。
            encode_cde(&CdeValue::Map(vec![
                (KEY_VERSION, CdeValue::UInt(CONTROL_PIN_VERSION)),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_INSTANCE_ID, CdeValue::Bytes(INSTANCE.to_vec())),
                (
                    KEY_RECORD,
                    CdeValue::Map(vec![(RECORD_KEY_REV, CdeValue::UInt(1))]),
                ),
                (9, CdeValue::UInt(1)),
            ]))
            .expect("cde"),
            // record hash 长度错。
            encode_cde(&CdeValue::Map(vec![
                (KEY_VERSION, CdeValue::UInt(CONTROL_PIN_VERSION)),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_INSTANCE_ID, CdeValue::Bytes(INSTANCE.to_vec())),
                (
                    KEY_RECORD,
                    CdeValue::Map(vec![
                        (RECORD_KEY_REV, CdeValue::UInt(1)),
                        (RECORD_KEY_HASH, CdeValue::Bytes(vec![0; 31])),
                    ]),
                ),
            ]))
            .expect("cde"),
        ];
        for blob in blobs {
            vault.set_meta(META_CONTROL_PIN, &blob).expect("写坏 pin");
            let error = load_control_pin(&vault, VAULT).expect_err("损坏必须报错，不能当缺失");
            assert!(matches!(error, SyncError::PinCorrupt(_)), "{error:?}");
        }
    }

    #[test]
    fn vault_binding_mismatch_is_rejected() {
        let (_dir, vault) = fresh();
        commit_verified_head(&vault, None, head(1, [0x61; 32])).expect("首次");
        let error = load_control_pin(&vault, [0x99; 16]).expect_err("换 vault 读取必须报错");
        assert!(matches!(error, SyncError::PinCorrupt(_)), "{error:?}");
    }

    #[test]
    fn concurrent_insert_does_not_overwrite_the_winner() {
        let (_dir, vault) = fresh();
        commit_verified_head(&vault, None, head(1, [0x71; 32])).expect("赢家");
        // 第二个写者仍以 expected=None 提交：语义上允许首次，但 CAS 发现已存在。
        let error =
            commit_verified_head(&vault, None, head(1, [0x72; 32])).expect_err("并发首次必须冲突");
        assert!(matches!(error, SyncError::PinConflict), "{error:?}");
        assert_eq!(load(&vault).expect("pin").record.hash, Some([0x71; 32]));
    }

    #[test]
    fn concurrent_update_does_not_overwrite_the_winner() {
        let (_dir, vault) = fresh();
        commit_verified_head(&vault, None, head(1, [0x81; 32])).expect("首次");
        let stale = load(&vault).expect("pin");
        // 另一个写者先推进到 rev2。
        commit_verified_head(&vault, Some(&stale), head(2, [0x82; 32])).expect("赢家推进");
        // 过期写者用同一个 stale expected 提交 rev2 的另一条 hash。
        let error = commit_verified_head(&vault, Some(&stale), head(2, [0x83; 32]))
            .expect_err("过期 CAS 必须冲突");
        assert!(matches!(error, SyncError::PinConflict), "{error:?}");
        let pin = load(&vault).expect("pin");
        assert_eq!(pin.record.rev, 2);
        assert_eq!(pin.record.hash, Some([0x82; 32]), "不得覆盖赢家");
    }

    #[test]
    fn bootstrap_anchor_regressions_are_rejected() {
        let (_dir, vault) = fresh();
        let bootstrap = BootstrapAnchorV1 {
            rev: 3,
            hash: anchor_hash(3, 5, [0xA8; 16]),
            epoch: 5,
            log_id: [0xA8; 16],
        };
        commit_verified_head(&vault, None, head_with_bootstrap(3, [0xA9; 32], bootstrap))
            .expect("首次");
        let pin = load(&vault).expect("pin");

        // 低 rev 但自洽的锚：`with_bootstrap` 放行（它只验历史视图），commit 必须回退拒绝。
        let lower = head_with_bootstrap(
            4,
            [0xAA; 32],
            BootstrapAnchorV1 {
                rev: 2,
                hash: anchor_hash(2, 5, [0xA8; 16]),
                ..bootstrap
            },
        );
        assert!(matches!(
            commit_verified_head(&vault, Some(&pin), lower),
            Err(SyncError::ControlRollback { .. })
        ));

        // 同 rev 换 hash（自洽的另一枚锚）：merge 必须报 equivocation。
        let swapped = head_with_bootstrap(
            4,
            [0xAA; 32],
            BootstrapAnchorV1 {
                hash: anchor_hash(3, 5, [0xAB; 16]),
                log_id: [0xAB; 16],
                ..bootstrap
            },
        );
        assert!(matches!(
            commit_verified_head(&vault, Some(&pin), swapped),
            Err(SyncError::Equivocation(_))
        ));

        // 同 rev epoch 回退（同样是自洽锚）：merge 必须拒绝，不能让 epoch 静默回退。
        let epoch_back = head_with_bootstrap(
            4,
            [0xAA; 32],
            BootstrapAnchorV1 {
                hash: anchor_hash(3, 4, [0xA8; 16]),
                epoch: 4,
                ..bootstrap
            },
        );
        assert!(matches!(
            commit_verified_head(&vault, Some(&pin), epoch_back),
            Err(SyncError::Equivocation(_))
        ));

        // 锚 rev 超前于已验证链尾：不能把"未来"的锚挂到当前 head 上。
        let ahead = head(2, [0xAC; 32]).with_bootstrap(
            BootstrapAnchorV1 {
                rev: 3,
                hash: anchor_hash(3, 5, [0xA8; 16]),
                ..bootstrap
            },
            &test_chain(),
        );
        assert!(matches!(ahead, Err(SyncError::Equivocation(_))));
    }

    /// 异 rev 的旧锚只能按**历史视图**验证：链前缀被重写即拒，链未覆盖锚 rev 则 NeedsTrust。
    #[test]
    fn historical_bootstrap_anchor_requires_the_chain_prefix() {
        let anchor = BootstrapAnchorV1 {
            rev: 1,
            hash: anchor_hash(1, 1, [0x52; 16]),
            epoch: 1,
            log_id: [0x52; 16],
        };
        // 同一枚锚 + 原链：通过（rev 前进也能证明旧前缀）。
        head(3, [0x61; 32])
            .with_bootstrap(anchor, &test_chain())
            .expect("原链必须包含旧锚认证过的前缀");

        // rev1 的 genesis 被重写（管理链前缀变了）→ 旧摘要不再成立。
        let mut rewritten = test_chain();
        rewritten[0].pubkey = [0x09; 32];
        assert!(matches!(
            head(3, [0x61; 32]).with_bootstrap(anchor, &rewritten),
            Err(SyncError::Equivocation(_))
        ));

        // 链根本没覆盖锚的 rev（空链/只有更晚的 rev）→ NeedsTrust，不能当"没有锚"。
        assert!(matches!(
            head(3, [0x61; 32]).with_bootstrap(anchor, &[]),
            Err(SyncError::NeedsTrust(_))
        ));
        let later_only = &test_chain()[1..];
        assert!(matches!(
            head(3, [0x61; 32]).with_bootstrap(anchor, later_only),
            Err(SyncError::NeedsTrust(_)) | Err(SyncError::Equivocation(_))
        ));

        // 锚 epoch 与摘要不匹配（锚自己的坐标被改）：也要拒。
        assert!(matches!(
            head(3, [0x61; 32])
                .with_bootstrap(BootstrapAnchorV1 { epoch: 2, ..anchor }, &test_chain()),
            Err(SyncError::Equivocation(_))
        ));
    }

    #[test]
    fn pin_to_control_pin_keeps_the_bootstrap_anchor_across_revs() {
        let bootstrap = BootstrapAnchorV1 {
            rev: 2,
            hash: [0xB9; 32],
            epoch: 1,
            log_id: [0xBA; 16],
        };
        let pin = PinV1 {
            vault_id: VAULT,
            instance_id: INSTANCE,
            record: PinAnchorV1 {
                rev: 2,
                hash: Some([0xBB; 32]),
            },
            bootstrap: Some(bootstrap),
        };
        let control = pin.to_control_pin();
        assert_eq!(control.instance_id, INSTANCE);
        assert_eq!(control.control_rev, 2);
        assert_eq!(control.record_hash, Some([0xBB; 32]));
        assert_eq!(control.bootstrap, Some(bootstrap));

        // record rev 前进：旧 bootstrap 锚**不能丢**，也不能被改写成新 rev 的 hash；
        // 它整枚（rev/epoch/log_id）随 pin 携带，由 `verify_control_pin` 按历史视图验证。
        let advanced = PinV1 {
            record: PinAnchorV1 {
                rev: 3,
                hash: Some([0xBC; 32]),
            },
            ..pin
        };
        let control = advanced.to_control_pin();
        assert_eq!(control.control_rev, 3);
        assert_eq!(
            control.bootstrap,
            Some(bootstrap),
            "bootstrap 锚必须独立携带自己的 rev（不能挂到 max(rev) 上）"
        );
    }
}

`````



@@SECTION code/021 `crates/sync/src/create_link.rs`


`````rust
//! ADR 0014 **S3a**：一次性创建票据 / `mn-pair://create` 链接。
//!
//! ## 为什么需要它
//!
//! 多 vault 的 `POST /v2/vaults` 授权来自运维发行的一次性 **32B 创建票据**
//! （relay 只存 `SHA256`，ADR 0014 §A / S2b）。客户端需要一条**可粘贴**的创建链接，
//! 让新实例在不接触 setup secret 的情况下建库：
//!
//! ```text
//! mn-pair://create?v=1&relay=<percent-encoded canonical origin>&vault=<hex32>[&pin=<hex32>]#ticket=<base64url>
//! ```
//!
//! ## 安全边界
//!
//! - **ticket 只在 fragment**：不进 query、不进 referrer/服务端日志；本模块的 `Debug`
//!   与全部错误文案都不回显 ticket/完整 URL；
//! - **唯一 URL parser**：`relay` 百分号解码后交给
//!   [`crate::relay_policy::parse_relay_url`]——`https` 只接受远端 canonical origin，
//!   `http` 只接受显式 loopback 白名单；userinfo/query/fragment/非根 path 一律拒绝；
//! - **不自动换 pin**：可选的 `pin` 是邀请方已保存的 relay instance 公开锚，调用方
//!   必须在 bootstrap/`/state` 回执里逐字节比对；本模块只解析、不信任、不改写；
//! - **ticket 不落盘**：本模块不提供任何持久化 API；调用方只在内存里把它交给
//!   `POST /v2/vaults`（[`crate::remote_init::create_vault_with_ticket`]）。
//!
//! ## 非目标（S3a 明确）
//!
//! 不做 Windows scheme 注册/QR/剪贴板；不做 App 表单（S3b）；不自动创建票据
//! （票据由 relay Web 管理面「创建领取链接」发行，ADR 0018 S4a/S4b）。

use std::fmt;

use zeroize::Zeroize;

use crate::api_version::RelayApiVersion;
use crate::error::SyncError;
use crate::http_transport::TransportTimeouts;
use crate::relay_context::RelayContext;
use crate::relay_policy::{parse_relay_url, RelayMode};
use crate::VaultId;

/// `mn-pair://` 的创建 host。
pub const CREATE_LINK_HOST: &str = "create";
/// 创建链接格式版本（`v=1`；与 relay API 面无关，面由票据端点固定为 v2）。
pub const CREATE_LINK_VERSION: &str = "1";
/// 票据明文长度（relay 发行/消费契约，ADR 0014 S2b）。
pub const CREATE_TICKET_LEN: usize = 32;

// ---------------------------------------------------------------------------
// 票据
// ---------------------------------------------------------------------------

/// 一次性创建票据（32B 明文；**只活内存**，不落库/日志/配置）。
///
/// - `Debug` 脱敏；`Drop` 擦除（`zeroize`），避免进程内长时间残留；
/// - 不可 `Clone`：多一份副本就多一次泄漏面（需要重试时由调用方保留同一实例）；
/// - `PartialEq` 走常量时间比较（只用于"同一票据"的断言，不做授权判断）。
pub struct CreateTicket {
    bytes: [u8; CREATE_TICKET_LEN],
}

impl CreateTicket {
    /// 从 relay 发行的 32B 明文构造。
    #[must_use]
    pub const fn from_bytes(bytes: [u8; CREATE_TICKET_LEN]) -> Self {
        Self { bytes }
    }

    /// 从 slice 构造（长度必须恰好 32，否则拒绝；错误只含长度）。
    pub fn from_slice(bytes: &[u8]) -> Result<Self, CreateLinkError> {
        let bytes: [u8; CREATE_TICKET_LEN] = bytes
            .try_into()
            .map_err(|_| CreateLinkError::TicketLength)?;
        Ok(Self { bytes })
    }

    /// 请求字段只在该 crate 内读取（`POST /v2/vaults` 的键 0）。
    pub(crate) const fn as_bytes(&self) -> &[u8; CREATE_TICKET_LEN] {
        &self.bytes
    }

    /// 常量时间的相等比较（测试/诊断；不代表票据已被授权）。
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        use subtle::ConstantTimeEq as _;
        bool::from(self.bytes.ct_eq(&other.bytes))
    }
}

impl PartialEq for CreateTicket {
    fn eq(&self, other: &Self) -> bool {
        self.matches(other)
    }
}

impl Eq for CreateTicket {}

impl fmt::Debug for CreateTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreateTicket")
            .field("bytes", &"<redacted: one-time create ticket>")
            .finish()
    }
}

impl Drop for CreateTicket {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

// ---------------------------------------------------------------------------
// 链接
// ---------------------------------------------------------------------------

/// 解析后的创建链接（ticket 仍在内存里；`Debug` 脱敏）。
///
/// `url` 是**由 canonical 组件重新生成**的完整链接（含 fragment ticket）：解析时不保留
/// 任意原始文本，避免把夹带内容原样带给 UI/日志。
pub struct CreateLink {
    /// 链接来源的 relay 模式（由 scheme 决定：`https`=remote_tls，`http`=local_dev）。
    pub mode: RelayMode,
    /// canonical relay origin（公开）。
    pub relay: String,
    /// 预留的 vault id（票据绑定；创建必须使用同一个 id）。
    pub vault_id: VaultId,
    /// 一次性票据（秘密；只在内存）。
    pub ticket: CreateTicket,
    /// 可选公开锚：创建端已保存的 relay instance（16B），用于要求同实例。
    pub pin: Option<[u8; 16]>,
    /// canonical 完整 URL（含 fragment；**不得**打印/入库）。
    pub url: String,
}

impl CreateLink {
    /// 严格解析创建链接（唯一入口）。
    ///
    /// 拒绝：非 `mn-pair://create`、userinfo/额外 path/端口、缺/重复/未知 query 字段、
    /// `v=1` 之外版本、非 32 位 hex 的 vault/pin、fragment 里非 `ticket=` 的内容、
    /// 非 32B base64url ticket、非法百分号编码、以及唯一权威 relay parser 拒绝的 origin。
    pub fn parse(raw: &str) -> Result<Self, CreateLinkError> {
        let text = raw.trim();
        if text.is_empty() {
            return Err(CreateLinkError::Empty);
        }
        let rest = text
            .strip_prefix("mn-pair://")
            .or_else(|| text.strip_prefix("MN-PAIR://"))
            .ok_or(CreateLinkError::Scheme)?;
        // fragment 只允许一个 `#`：ticket 只从 fragment 取。
        let (head, fragment) = rest.split_once('#').ok_or(CreateLinkError::Fragment)?;
        let (authority, query) = head.split_once('?').ok_or(CreateLinkError::Query)?;
        // host 必须精确是 create：这同时拒绝 userinfo（`@`）、端口和额外 path。
        if !authority.eq_ignore_ascii_case(CREATE_LINK_HOST) {
            return Err(CreateLinkError::Host);
        }
        let ticket = parse_fragment(fragment)?;
        let fields = parse_query(query)?;
        let version = fields.version.ok_or(CreateLinkError::Missing("v"))?;
        if version != CREATE_LINK_VERSION {
            return Err(CreateLinkError::Version);
        }
        let relay_text = fields.relay.ok_or(CreateLinkError::Missing("relay"))?;
        let relay_plain = percent_decode(relay_text)?;
        let mode = if relay_plain.starts_with("https://") {
            RelayMode::RemoteTls
        } else if relay_plain.starts_with("http://") {
            RelayMode::LocalDev
        } else {
            return Err(CreateLinkError::RelayScheme);
        };
        let origin = parse_relay_url(&relay_plain, mode)
            .map_err(|error| CreateLinkError::Relay(error.to_string()))?;
        let vault_id = parse_hex32(fields.vault.ok_or(CreateLinkError::Missing("vault"))?)?;
        let pin = match fields.pin {
            Some(text) => Some(parse_hex32(text)?),
            None => None,
        };
        let relay = origin.as_str();
        // 用 canonical 组件重建（不保留任意原始文本）。
        let url = build_url(&relay, &vault_id, &ticket, pin.as_ref());
        Ok(Self {
            mode,
            relay,
            vault_id,
            ticket,
            pin,
            url,
        })
    }

    /// 由 canonical 组件生成创建链接（唯一编码入口；ticket 只进 fragment）。
    pub fn encode(
        mode: RelayMode,
        relay: &str,
        vault_id: VaultId,
        ticket: CreateTicket,
        pin: Option<[u8; 16]>,
    ) -> Result<Self, CreateLinkError> {
        let origin = parse_relay_url(relay, mode)
            .map_err(|error| CreateLinkError::Relay(error.to_string()))?;
        let relay = origin.as_str();
        let url = build_url(&relay, &vault_id, &ticket, pin.as_ref());
        Ok(Self {
            mode,
            relay,
            vault_id,
            ticket,
            pin,
            url,
        })
    }

    /// 本链接对应的 relay context：**固定 v2 面**（票据端点就是 `/v2/vaults`）。
    ///
    /// 解析/编码时已保证 origin 被唯一 parser 接受；这里仍按模式重新构造，
    /// 因此不会把 `http` 当成远端或把 `https` 当成本地开发。
    pub fn relay_context(&self, timeouts: TransportTimeouts) -> Result<RelayContext, SyncError> {
        Ok(RelayContext::from_mode(self.mode, &self.relay, timeouts)?
            .with_api_version(RelayApiVersion::V2))
    }

    /// 是否要求同 relay instance（公开锚存在）。
    #[must_use]
    pub const fn requires_instance(&self) -> bool {
        self.pin.is_some()
    }

    /// vault id 的 32 位 hex（公开；UI 预填/展示用）。
    #[must_use]
    pub fn vault_id_hex(&self) -> String {
        crate::ids::hex_of(&self.vault_id)
    }
}

impl fmt::Debug for CreateLink {
    /// ticket 与 URL 都不打印；只给公开定位信息。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreateLink")
            .field("mode", &self.mode)
            .field("relay", &self.relay)
            .field("vault_id", &crate::ids::hex_of(&self.vault_id))
            .field("pin", &self.pin.as_ref().map(|pin| crate::ids::hex_of(pin)))
            .field("ticket", &"<redacted: one-time create ticket>")
            .field("url", &"<redacted: contains ticket>")
            .finish()
    }
}

/// 创建链接解析/编码失败原因（**不含 ticket/完整 URL**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateLinkError {
    /// 空输入。
    Empty,
    /// 不是 `mn-pair://`。
    Scheme,
    /// host 不是 `create`（或带了 userinfo/端口/额外 path）。
    Host,
    /// 缺 fragment / 多余 `#`。
    Fragment,
    /// 缺 query。
    Query,
    /// 缺必需字段。
    Missing(&'static str),
    /// 重复字段。
    Duplicate(&'static str),
    /// 未知字段（拒绝 `?next=` 之类的恶意跳转）。
    Unknown(&'static str),
    /// 版本不是 1。
    Version,
    /// fragment ticket 非法（长度/编码）。
    Ticket,
    /// 票据长度不是 32B。
    TicketLength,
    /// 百分号编码非法。
    Percent,
    /// `vault`/`pin` 不是 32 位 hex。
    Hex(&'static str),
    /// relay 不是受支持的 scheme（http/https）。
    RelayScheme,
    /// relay 被唯一权威 URL parser 拒绝（文案只含公开定位信息）。
    Relay(String),
}

impl fmt::Display for CreateLinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "创建链接为空"),
            Self::Scheme => write!(f, "不是 mn-pair:// 创建链接"),
            Self::Host => write!(
                f,
                "创建链接的 host 必须是 create（不接受 userinfo/端口/path）"
            ),
            Self::Fragment => write!(f, "创建链接缺 fragment（#ticket=…）或含多余 #"),
            Self::Query => write!(f, "创建链接缺 query（?v=1&relay=…&vault=…）"),
            Self::Missing(field) => write!(f, "创建链接缺字段 {field}=…"),
            Self::Duplicate(field) => write!(f, "创建链接字段重复：{field}"),
            Self::Unknown(field) => write!(f, "创建链接含未知字段 {field}（拒绝跳转/夹带）"),
            Self::Version => write!(f, "创建链接版本不是 1"),
            Self::Ticket => write!(f, "创建链接票据格式非法（应为 32 字节 base64url）"),
            Self::TicketLength => write!(f, "创建票据长度必须是 32 字节"),
            Self::Percent => write!(f, "创建链接含非法百分号编码"),
            Self::Hex(field) => write!(f, "创建链接字段 {field} 不是 32 位十六进制"),
            Self::RelayScheme => write!(f, "创建链接的 relay 必须是 http:// 或 https://"),
            Self::Relay(message) => write!(f, "创建链接的 relay 不合法：{message}"),
        }
    }
}

impl std::error::Error for CreateLinkError {}

impl From<CreateLinkError> for SyncError {
    fn from(error: CreateLinkError) -> Self {
        Self::InviteLink(error.to_string())
    }
}

// ---------------------------------------------------------------------------
// 内部编解码（严格，唯一实现）
// ---------------------------------------------------------------------------

fn build_url(
    relay: &str,
    vault_id: &VaultId,
    ticket: &CreateTicket,
    pin: Option<&[u8; 16]>,
) -> String {
    let relay = percent_encode(relay);
    let vault = crate::ids::hex_of(vault_id);
    let mut url =
        format!("mn-pair://{CREATE_LINK_HOST}?v={CREATE_LINK_VERSION}&relay={relay}&vault={vault}");
    if let Some(pin) = pin {
        url.push_str("&pin=");
        url.push_str(&crate::ids::hex_of(pin));
    }
    url.push_str("#ticket=");
    url.push_str(&crate::base64url::encode(ticket.as_bytes()));
    url
}

fn parse_fragment(fragment: &str) -> Result<CreateTicket, CreateLinkError> {
    if fragment.contains('#') {
        return Err(CreateLinkError::Fragment);
    }
    let (key, value) = fragment.split_once('=').ok_or(CreateLinkError::Ticket)?;
    if key != "ticket" {
        return Err(CreateLinkError::Ticket);
    }
    let decoded = percent_decode(value)?;
    let bytes = crate::base64url::decode(&decoded).map_err(|_| CreateLinkError::Ticket)?;
    CreateTicket::from_slice(&bytes)
}

fn percent_decode(value: &str) -> Result<String, CreateLinkError> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let high = *bytes.get(index + 1).ok_or(CreateLinkError::Percent)?;
                let low = *bytes.get(index + 2).ok_or(CreateLinkError::Percent)?;
                let high = hex_value(high).ok_or(CreateLinkError::Percent)?;
                let low = hex_value(low).ok_or(CreateLinkError::Percent)?;
                out.push(high << 4 | low);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| CreateLinkError::Percent)
}

/// 只保留 RFC 3986 unreserved 字符；`:` `/` 等一律百分号编码（query 值里必须转义）。
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    out
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'A' + (value - 10)) as char,
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_hex32(text: &str) -> Result<[u8; 16], CreateLinkError> {
    if text.len() != 32 {
        return Err(CreateLinkError::Hex("vault/pin"));
    }
    let mut out = [0u8; 16];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let high = hex_value(chunk[0]).ok_or(CreateLinkError::Hex("vault/pin"))?;
        let low = hex_value(chunk[1]).ok_or(CreateLinkError::Hex("vault/pin"))?;
        out[index] = high << 4 | low;
    }
    Ok(out)
}

struct QueryFields<'a> {
    version: Option<&'a str>,
    relay: Option<&'a str>,
    vault: Option<&'a str>,
    pin: Option<&'a str>,
}

fn parse_query(query: &str) -> Result<QueryFields<'_>, CreateLinkError> {
    let mut fields = QueryFields {
        version: None,
        relay: None,
        vault: None,
        pin: None,
    };
    for pair in query.split('&') {
        if pair.is_empty() {
            return Err(CreateLinkError::Query);
        }
        let (key, value) = pair.split_once('=').ok_or(CreateLinkError::Query)?;
        if value.is_empty() {
            return Err(CreateLinkError::Missing("query 字段值"));
        }
        let slot = match key {
            "v" => &mut fields.version,
            "relay" => &mut fields.relay,
            "vault" => &mut fields.vault,
            "pin" => &mut fields.pin,
            // 未知字段一律拒绝（拒绝恶意跳转/夹带）；不把未知字段名回显给用户。
            _other => return Err(CreateLinkError::Unknown("未知字段")),
        };
        if slot.is_some() {
            return Err(CreateLinkError::Duplicate("query 字段"));
        }
        *slot = Some(value);
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAULT: [u8; 16] = [0x5a; 16];
    const PIN: [u8; 16] = [0x22; 16];
    const TICKET: [u8; 32] = [0x11; 32];

    fn local_link(pin: Option<[u8; 16]>) -> CreateLink {
        CreateLink::encode(
            RelayMode::LocalDev,
            "http://127.0.0.1:18080",
            VAULT,
            CreateTicket::from_bytes(TICKET),
            pin,
        )
        .expect("local link")
    }

    #[test]
    fn create_link_roundtrips_and_ticket_stays_in_fragment() {
        let link = local_link(Some(PIN));
        assert!(link
            .url
            .starts_with("mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault="));
        assert!(link.url.contains("#ticket="));
        assert!(!link.url[..link.url.find('#').expect("fragment")].contains("ticket="));
        let parsed = CreateLink::parse(&link.url).expect("parse");
        assert_eq!(parsed.vault_id, VAULT);
        assert_eq!(parsed.pin, Some(PIN));
        assert_eq!(parsed.relay, "http://127.0.0.1:18080");
        assert_eq!(parsed.mode, RelayMode::LocalDev);
        assert!(parsed.ticket.matches(&CreateTicket::from_bytes(TICKET)));
        // canonical 重建：再次解析/编码得到逐字节相同的 URL。
        assert_eq!(parsed.url, link.url);
        let again = CreateLink::parse(&parsed.url).expect("reparse");
        assert_eq!(again.url, link.url);
    }

    #[test]
    fn remote_link_requires_https_and_canonicalizes() {
        let link = CreateLink::encode(
            RelayMode::RemoteTls,
            "https://Relay.Example:443/",
            VAULT,
            CreateTicket::from_bytes(TICKET),
            None,
        )
        .expect("remote link");
        assert_eq!(link.relay, "https://relay.example");
        assert_eq!(link.pin, None);
        // 明文 http 不能编码成 remote 链接；localhost 也不能冒充远端。
        assert!(CreateLink::encode(
            RelayMode::RemoteTls,
            "http://127.0.0.1:18080",
            VAULT,
            CreateTicket::from_bytes(TICKET),
            None,
        )
        .is_err());
        // 远端解析同样只接受 https。
        let parsed = CreateLink::parse(&link.url).expect("remote parse");
        assert_eq!(parsed.mode, RelayMode::RemoteTls);
    }

    #[test]
    fn parse_rejects_bad_shape_without_echoing_the_ticket() {
        let good = local_link(None).url;
        let ticket = good
            .split_once("#ticket=")
            .map(|(_, value)| value.to_owned())
            .expect("ticket");
        for bad in [
            String::new(),
            format!("mn-pair://join?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            format!("mn-pair://create?relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            // 版本
            format!("mn-pair://create?v=2&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            // 未知字段 / 额外 path / userinfo / 端口
            format!("mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}&next=x#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            format!("mn-pair://create/extra?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            format!("mn-pair://user@create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            format!("mn-pair://create:8080?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            // relay 非 https / 非 loopback http
            format!("mn-pair://create?v=1&relay=http%3A%2F%2Frelay.example&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            format!("mn-pair://create?v=1&relay=https%3A%2F%2Frelay.example%2Fpath&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            // ticket 非 32B / 非法 base64url
            format!("mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket=abc", crate::ids::hex_of(&VAULT)),
            format!("mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={ticket}%20", crate::ids::hex_of(&VAULT)),
            // vault 非 32 位 hex
            "mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault=zz#ticket=x".to_owned(),
        ] {
            let error = CreateLink::parse(&bad).expect_err("必须拒绝");
            let message = format!("{error}");
            assert!(!message.contains(&ticket), "错误文案不得回显 ticket：{bad}");
            assert!(!format!("{error:?}").contains(&ticket));
        }
    }

    #[test]
    fn debug_and_errors_never_contain_the_ticket_or_url() {
        let link = local_link(Some(PIN));
        let parsed = CreateLink::parse(&link.url).expect("parse");
        let ticket = crate::base64url::encode(&TICKET);
        let debug = format!("{parsed:?}");
        assert!(debug.contains("<redacted: one-time create ticket>"));
        assert!(!debug.contains(&ticket));
        assert!(!debug.contains(&parsed.url));
        assert!(!format!("{:?}", parsed.ticket).contains(&ticket));
        assert!(!format!("{:?}", CreateTicket::from_bytes(TICKET)).contains(&ticket));
    }

    /// fragment 的 ticket 允许百分号编码（解析时必须严格解码；非法编码拒绝）。
    #[test]
    fn ticket_fragment_accepts_percent_encoding() {
        let link = local_link(None);
        let (head, value) = link.url.split_once("#ticket=").expect("fragment");
        // 把 base64url 的第一个字符编码成百分号形式（值本身不变）。
        let mut chars = value.chars();
        let first = chars.next().expect("非空");
        let rest = chars.as_str();
        let encoded = format!("{head}#ticket=%{:02X}{rest}", first as u32);
        let parsed = CreateLink::parse(&encoded).expect("百分号编码的 ticket 应可解析");
        assert!(parsed.ticket.matches(&CreateTicket::from_bytes(TICKET)));
        assert_eq!(
            parsed.url, link.url,
            "解析后按 canonical 重建（不保留原始文本）"
        );

        // 非法百分号编码明确拒绝，且不回显。
        let broken = format!("{head}#ticket=%ZZ{rest}");
        let error = CreateLink::parse(&broken).expect_err("非法百分号必须拒绝");
        assert!(!format!("{error}").contains(value));
    }

    #[test]
    fn link_context_is_always_v2_and_mode_matched() {
        let link = local_link(None);
        let context = link
            .relay_context(crate::http_transport::DEFAULT_TRANSPORT_TIMEOUTS)
            .expect("context");
        assert_eq!(context.mode, RelayMode::LocalDev);
        assert_eq!(context.api_version(), RelayApiVersion::V2);
        assert_eq!(context.origin_str(), "http://127.0.0.1:18080");
        assert_eq!(
            context.api().create_vault_path(),
            "/v2/vaults",
            "票据端点必须走 v2 面"
        );
    }
}

`````



@@SECTION code/022 `crates/sync/src/dev_local.rs`


`````rust
//! ADR 0009 切片 4b：**GPUI-free** 的 dev-local 同步编排（Create / 发布初始基线 / Join / SyncOnce）。
//!
//! 本模块只做"把 `crates/sync` 已有的真实步骤按 ADR 0009 的顺序串起来"，不新增协议、
//! 不改 wire、不碰 UI/worker（切片 4c 才接 `SyncService`）。它存在的理由是：同样的
//! 编排写在 `examples/e2e_common.rs` 里只能给 example 用，而 app 的 dev-sync worker
//! 需要一个**可测、无 GPUI**的库入口。
//!
//! ## 编排
//!
//! 1. [`DevLocal::create`]：`POST /v1/vaults`（genesis）→ 新建本地 SQLite vault →
//!    落 `sync.relay_cursor`（relay 提供时）/ `sync.log_id` / 展示标签 /
//!    **初始 recovery 槽材料** → **上传初始 recovery 槽**（KEY_SLOT staging）并落
//!    「已上传」标记。槽必须在恢复码还在内存里的这一刻上传：KEY_SLOT 对象的 MNE1
//!    密钥 `K_slot_mne = HKDF(恢复码种子)` 不可逆，恢复码又不落盘，事后无法补传；
//! 2. 发布初始基线有两条入口，共用同一条核心路径（sync + 两份 checkpoint）：
//!    - [`DevLocal::publish_initial_pair`]（CLI）：校验恢复码与本地管理身份一致 →
//!      上传/重传初始 recovery 槽（幂等）→ 核心；
//!    - [`DevLocal::publish_initial_pair_with_identity`]（面板 worker）：**不接收恢复码**，
//!      信任锚是受保护身份里的 `manager_seed`（Join 设备没有 → 明确拒绝），初始槽以
//!      Create 落下的「已上传」标记为前提（旧实例明确失败并指向 CLI 路径）；
//!
//!    两条入口随后都跑一轮完整 `SyncOnce`（让覆盖证明只认 durable 事实）→ 用
//!    [`CheckpointWriter`] 发布**两份**真实同 epoch checkpoint（manifest/snapshot/
//!    asset/refs），并把最新一份落成 durable 事实；
//! 3. [`DevLocal::join`]：只接收 `vault_id + 恢复码 + 新路径`。`RecoverySeed →
//!    ManagementKey::from_recovery` → `recovery_enroll::self_enroll` 建管理信任锚并
//!    自批新设备 → `bootstrap::run_with_pins` 认证读取并导入基线 → pins/cursor 由
//!    bootstrap 落库，identity 只返回给调用方（**不读 A 的目录/identity/root**，
//!    也不在 vault 里写任何私钥或恢复码）；
//! 4. [`DevLocal::sync_once`]：构造借用型 [`SyncEngine`]，按 `encrypt_pending → push →
//!    pull → integrate → ack` 跑一轮，返回 [`SyncReport`]（游标由 engine 自己推进）；
//! 5. ADR 0010 切片 2a-2 的两个**只读/管理**入口，给 dev-only 设备列表 UI 用：
//!    [`DevLocal::members`]（已验证控制链的只读成员视图）与 [`DevLocal::revoke_device`]
//!    （管理密钥 + 管理端 helper 移除一台设备的访问，**不轮换密钥**、不删本地身份）。
//!
//! ## 凭据模型（dev-only）
//!
//! dev-local 只有**恢复码**一种凭据：`create` 不接收口令，因此 password 槽材料只存在于
//! 进程内、不上传、不落盘（口令槽 onboarding 属于切片 4c 的正式流程）。所以
//! `create` / [`DevLocal::publish_initial_pair`] 上传的是 **genesis 生成的那一个 recovery
//! 槽**（staging），Join 全程只靠恢复码解出根。
//!
//! 面板的「发布基线」走 [`DevLocal::publish_initial_pair_with_identity`]：它**不需要恢复码**
//! （恢复码在 Create 之后就不在进程里了），只要求身份里有管理私钥，且初始槽已由 Create
//! 上传过（[`INITIAL_SLOT_UPLOADED_META`]）。这是 ADR 0009 切片 4c-4 的取舍：为了让面板
//! 能一键发布，把"只有恢复码能做的事"（上传初始槽）前移到 Create，发布本身只做
//! "同步 + 两份 checkpoint"。
//!
//! ## 安全与非目标
//!
//! - relay 接入统一走 [`crate::relay_context::RelayContext`]：本模块的每个操作都从
//!   **同一个** context 取客户端/传输（不再各自拼 `StdTcpTransport`）；URL 兼容入口
//!   只是先构造一个 `local_dev` context。`remote_tls` 的 Create/Join 初始化在
//!   [`crate::remote_init`]（S3b），本模块保持 local_dev 语义。普通同步的
//!   生产 TLS 由 app 按公开配置的模式用 `remote_tls` context 打开；本模块不实现验签——测试用
//!   `menghunote-dev-relay`，它是**放行验签**的测试脚手架，**非生产**；正式鉴权负例
//!   必须走 relay 生产 hook；
//! - 不在这里写 app 的受保护身份文件（`DevIdentityStore` 负责 DPAPI/Linux 主钥封装）：
//!   identity 只经返回值交给调用方，绝不进 vault meta/明文配置/日志；
//! - 恢复码只以 `String` 形式交给调用方（ADR 0009：仅临时显示，提交后清除），本模块
//!   自身不打印、不落盘；`setup_secret` 只回显、不持久化；
//! - 初始 recovery 槽材料（描述符 + 包裹根密文）会写入 vault meta
//!   [`INITIAL_RECOVERY_SLOT_META`]：那是**已经要发到 relay 的密文**，不含根/恢复码明文；
//!   它让"发布基线"可重复且幂等（同一 slot_id/描述符，relay 只登记一次）。
//!
//! ## 失败可恢复性
//!
//! - `create` 是新建库：路径已存在直接拒绝；任何一步失败都**不留下**可被误用的本地
//!   vault（已创建的文件会被清掉）。relay 侧可能留下一个"没有槽、没有 checkpoint"
//!   的空 vault，它无法被任何设备使用，重试 `create` 会建新的；
//! - `publish_initial_pair` 先做恢复码一致性校验（错了在**任何上传之前**失败），再做
//!   槽上传/同步/两份 checkpoint；它是幂等的，失败后重跑会补齐（relay 只把最新的两份
//!   published 作为 `/bootstrap` 的候选）；
//! - `join` 失败不写 cursor/identity：bootstrap 只在全部导入通过后一个事务推进水位；
//!   失败时本模块清掉新建的本地文件（不在新目录留半初始化库），调用方重新 Join 即可。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::checkpoint::{CheckpointReport, CheckpointWriter, StorageAssets};
use crate::device::{DeviceSigningKey, ManagementKey};
use crate::device_management::{RevokeContext, RevokeOutcome};
use crate::engine::{
    AckReport, AssetSyncReport, EngineConfig, EngineIdentity, IntegrateConfig, IntegrateReport,
    PullReport, PushReport, SyncEngine, SyncReport, RELAY_CURSOR_META,
};
use crate::error::SyncError;
use crate::genesis;
use crate::http_relay::{Clock, HttpRelayClient, MemberView, SystemClock};
use crate::http_transport::{HttpTransport, TransportTimeouts, DEFAULT_TRANSPORT_TIMEOUTS};
use crate::identity::LocalIdentityV1;
use crate::ids;
use crate::init_session::InitSession;
use crate::keys::RootSecretV1;
use crate::random::{array, OsRandom};
use crate::recovery::{management_seed, RecoverySeed};
use crate::relay::RelayClient as _;
use crate::relay_context::{RelayContext, RelayTarget};
use crate::slot::{KeySlotV1, WrappedRoot};
use crate::{DeviceId, VaultId};
use menghunote_storage::{
    ensure_structure_row, load_head, materialize_body, pending_outbox, set_sync_log_id, Vault,
    STRUCTURE_DOC_ID,
};
use menghunote_sync_proto::{SlotDescriptorV1, SlotKind};

// ADR 0009 Phase 2（切片 5）：轮换的库级入口与结果类型，与 Create/Join/SyncOnce 同层导出。
pub use crate::dev_local_rotation::{ApplyOutcome, RotationOutcome};

/// `meta` 键：dev-local 实例的展示标签（仅本机，非 wire；Join 不导入）。
pub const VAULT_LABEL_META: &str = "dev.local.vault_label";
/// `meta` 键：genesis 生成的初始 recovery 槽材料（描述符 CDE + `nonce||ciphertext`）。
///
/// **只含密文**：包裹根用 `HKDF(恢复码)` 派生 KEK 加 AEAD，槽体与上传到 relay 的
/// KEY_SLOT 明文逐字节相同；恢复码本身不落盘。写入后只由
/// [`DevLocal::upload_initial_slot`] 读取（复用同一 slot_id，relay 幂等）。
pub const INITIAL_RECOVERY_SLOT_META: &str = "dev.local.initial_recovery_slot.v1";
/// `meta` 键：初始 recovery 槽**已上传到 relay** 的 durable 事实（ADR 0009 切片 4c-4）。
///
/// 记录 `slot_id`（32B）+ 确定性 `message_id`（16B），两者都是公开事实，不含恢复码/根。
/// 它是"只凭身份发布基线"（[`DevLocal::publish_initial_pair_with_identity`]）的前置条件：
/// 上传 KEY_SLOT 需要的 `K_slot_mne` 只能由恢复码种子派生，恢复码不落盘 → 事后无法补传，
/// 所以缺这条事实的旧实例必须明确失败并指向 CLI 的恢复码路径（不静默发布一个 B 无法
/// Join 的假基线）。
pub const INITIAL_SLOT_UPLOADED_META: &str = "dev.local.initial_slot_uploaded.v1";
/// `meta` 键：最近一次发布基线的**较新**那份 checkpoint。
pub const PUBLISHED_BASELINE_META: &str = "dev.local.published_baseline.v1";

/// 初始 recovery 槽记录 magic（"MenghuNote Dev Local Slot v1"）。
const INITIAL_SLOT_MAGIC: &[u8; 8] = b"MN/DLS1\0";
/// 初始 recovery 槽「已上传」记录 magic。
const INITIAL_SLOT_UPLOADED_MAGIC: &[u8; 8] = b"MN/DLS2\0";
/// 已发布基线记录 magic。
const PUBLISHED_BASELINE_MAGIC: &[u8; 8] = b"MN/DLB1\0";
/// 槽描述符 CDE 的合理上限（`SlotDescriptorV1` 现在是 ~70B，留余量防止坏长度读爆内存）。
const MAX_SLOT_DESCRIPTOR_LEN: usize = 4096;
/// KEY_SLOT `message_id` 派生的域分隔（非秘密；换它等于换对象字节，会导致 409）。
const KEY_SLOT_MESSAGE_DOMAIN: &[u8] = b"MN/v1/dev-local/key-slot-message\0";

/// 一轮同步的可选注入项（ADR 0012 S1）：**传输预算** + **集成前守卫**。
///
/// 为什么需要它：`DevLocal` 过去自己构造 transport（超时预算接不进来），而且
/// `integrate` 一旦开始就没有"取消/授权复核"的边界。两者都必须由调用方（app 的
/// worker）决定，所以收进一个显式结构，而不是在 `DevLocal` 内部硬编。
///
/// - `transport = None`：保持历史默认（连接/读写 30s、无整次请求预算）；
/// - `before_integrate = None`：不做额外复核（CLI/测试路径）。
#[derive(Default)]
pub struct SyncOnceOptions<'a> {
    /// 传输超时预算；`None` = 历史默认（向后兼容）。
    pub transport: Option<TransportTimeouts>,
    /// `integrate` 之前最后一道门：返回 `Err` 即**不 append**（取消/授权已失效）。
    pub before_integrate: Option<&'a mut (dyn FnMut() -> Result<(), SyncError> + 'a)>,
}

/// dev-local 编排入口（纯函数集合，无状态）。
pub struct DevLocal;

/// `create` 的结果：本地 vault + 正式身份 + 恢复码。
///
/// **不实现 `Debug`/`Display`**：`identity` 与 `recovery_code` 都是秘密材料
/// （`LocalIdentityV1` 本身也不实现，避免误打印）。
pub struct CreatedVault {
    /// 新开的本地库（已建 structure 行、游标、log_id 与标签）。
    pub vault: Vault,
    /// genesis 身份（含管理私钥；app 由 `DevIdentityStore` 受保护落盘）。
    pub identity: LocalIdentityV1,
    /// 新 vault 的 id（Join 时展示/输入的公开标识）。
    pub vault_id: VaultId,
    /// 一次性展示给用户的恢复码（`mnrec1...`，**不得**写日志/配置）。
    pub recovery_code: String,
    /// 回显调用方传入的 setup secret（dev-only；**不得**落盘/写日志）。
    pub setup_secret: String,
}

/// `join` 的结果：新设备自己的 vault + 设备身份。
pub struct JoinedVault {
    /// 新开的本地库（已由 bootstrap 导入基线并落 pins/cursor）。
    pub vault: Vault,
    /// 新设备身份（`manager_seed = None`：普通设备没有管理私钥）。
    pub identity: LocalIdentityV1,
    /// 加入的 vault id。
    pub vault_id: VaultId,
}

/// [`DevLocal::published_baseline`] 读出的 durable 事实（本地 meta，非 wire）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedBaseline {
    /// 较新那份 checkpoint 的 id。
    pub checkpoint_id: [u8; 16],
    /// 该 checkpoint 的 epoch。
    pub epoch: u64,
    /// 该 checkpoint 固定的 S。
    pub covered_seq: u64,
    /// relay 记录的发布时间（Unix 秒）。
    pub published_at: u64,
}

/// [`DevLocal::revoke_device`] 的结果：relay 事实 + **刷新后**的成员视图（ADR 0010 切片 2a-2）。
///
/// 两个字段都不含秘密：`outcome` 只有目标设备/`changed`/`auth_rev`/`control_rev`；
/// `members` 是只读投影（`MemberView` 的 `Debug` 只打印 id/status/fingerprint）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokedDevice {
    /// 管理端请求返回的事实（`control_rev` 不变、`auth_rev` 可能已前进）。
    pub outcome: RevokeOutcome,
    /// 吊销**之后**重新拉取的成员视图（已按 `device_id` 稳定升序）。
    pub members: Vec<MemberView>,
}

/// 两阶段同步 **Prepare** 阶段的报告（encrypt/push/pull；不含 integrate/ack）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrepareReport {
    /// 本轮新加密的本地意图数。
    pub encrypted: usize,
    /// 推送结果。
    pub push: PushReport,
    /// 增量资产上传结果（正文 push 之后、pull 之前）。
    pub assets: AssetSyncReport,
    /// 拉取结果（游标已 durable 推进）。
    pub pull: PullReport,
}

/// 两阶段同步 **Integrate** 阶段的报告。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegratePhaseReport {
    /// 集成结果（integrated/deferred/批次/是否投影）。
    pub integrate: IntegrateReport,
    /// 本阶段 head **真的前进**的文档（结构 + 正文；UI 据此判断交付/刷新）。
    pub changed_heads: Vec<String>,
    /// `watch_doc` 是否真的前进（= 需要交付）。
    pub watch_doc_changed: bool,
    /// 本阶段按 durable outbox 触发物化的正文篇数（changed heads → 物化/索引更新）。
    pub materialized: usize,
    /// 物化失败篇数（失败只记日志，不影响已 durable 的 CRDT 集成结论）。
    pub materialize_failed: usize,
}

/// 两阶段同步 **Finish** 阶段的报告（push tail + ack）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FinishReport {
    /// 上报的 received/integrated 游标。
    pub ack: AckReport,
    /// tail 阶段新加密的本地意图数。
    pub encrypted: usize,
    /// tail 推送结果。
    pub push: PushReport,
}

impl DevLocal {
    /// Create：对真实 relay 走 genesis，并新建（**必须是新路径**）本地 vault。
    ///
    /// 初始化走 ADR 0013 S2b 的受限能力 [`InitSession::new_genesis`]：
    ///
    /// 1. 先在本地生成 vault/device **材料**（不联网），再开新库并写 durable pending
    ///    （`sync.init_pending`）——顺序是"先记录意图、再联网"；
    /// 2. `POST /v1/vaults`（写前先推进 `GenesisRequested`）；成功后**不重复**创建；
    /// 3. 用本地提交证书复算 rev1 `record_hash`，验证 relay 当前链/前缀，再 durable
    ///    提交强 pin（`sync.control_pin`，rev1 record 锚）；
    /// 4. 写本地事实（structure/label/cursor/log_id/初始槽材料）→ 上传初始 recovery 槽
    ///    → pending 进入 `Done` 并清理（app 走 [`Self::create_deferring_identity`]：
    ///    停在 `PinCommitted`，身份 rename 成功后再推进 `IdentityWritten → Done`）。
    ///
    /// 失败语义沿用 ADR 0009（app 验收依赖它）：**任何返回错误都清掉本次新建的库文件**
    /// （relay 侧可能留下一个没有槽/没有 checkpoint 的空 vault，重试会建新的）；
    /// pending 的用途是**崩溃**（进程消失、清理代码没跑）之后的对账，见
    /// [`crate::init_session::reconcile`]。
    ///
    /// 口令：dev-local 不接收口令，genesis 的 password 槽材料用一次随机口令生成后**丢弃**
    /// （不上传、不落盘），因此该 vault 只能靠恢复码加入——这是 ADR 0009 Phase 1 的
    /// dev-only 取舍，口令 onboarding 在切片 4c。
    pub fn create(
        relay_url: &str,
        setup_secret: &str,
        vault_path: &Path,
        vault_label: &str,
    ) -> Result<CreatedVault, SyncError> {
        Self::create_inner(relay_url, setup_secret, vault_path, vault_label, true)
    }

    /// **app 专用**：与 [`Self::create`] 完全相同，但 pending 停在 `PinCommitted`。
    ///
    /// 调用方必须在身份**原子落盘**（`DevIdentityStore::persist_protected` 的临时文件 +
    /// rename）之后调用 [`crate::init_session::mark_identity_written`] 与
    /// `finish_identity_written`。这样"relay 已建 vault、本地身份还没落盘"的崩溃窗口
    /// 有 durable pending 可对账（ADR 0013 §2/S3），而不是靠重跑 genesis。
    ///
    /// 失败语义与 [`Self::create`] 一致：返回错误时清掉本次新建的本地文件。
    pub fn create_deferring_identity(
        relay_url: &str,
        setup_secret: &str,
        vault_path: &Path,
        vault_label: &str,
    ) -> Result<CreatedVault, SyncError> {
        Self::create_inner(relay_url, setup_secret, vault_path, vault_label, false)
    }

    fn create_inner(
        relay_url: &str,
        setup_secret: &str,
        vault_path: &Path,
        vault_label: &str,
        finish_identity: bool,
    ) -> Result<CreatedVault, SyncError> {
        if vault_path.as_os_str().is_empty() {
            return Err(SyncError::LocalCorrupt("Create 需要明确的 vault 路径"));
        }
        if vault_path.exists() {
            return Err(SyncError::LocalCorrupt(
                "Create 只接受尚不存在的 vault 路径（已有库请按其身份使用）",
            ));
        }

        // 0. 按唯一权威策略构造 context（local_dev 只接受 http + loopback 白名单）。
        let context = RelayContext::local_dev(relay_url, DEFAULT_TRANSPORT_TIMEOUTS)?;

        // 1. 本地材料：vault_id/device_id 必须**先进 pending**，之后才能发请求。
        let vault_id: VaultId = array(&mut OsRandom)?;
        let device_id: DeviceId = array(&mut OsRandom)?;
        let throwaway_password = throwaway_password()?;
        let materials = genesis::build_materials(
            vault_id,
            device_id,
            &throwaway_password,
            SystemClock.now_secs(),
            &mut OsRandom,
        )?;
        let session = InitSession::new_genesis(context, &materials)?;
        let context = session.context().clone();

        // 2. 新库（pending 需要 SQLite；genesis 失败时会连同 pending 一起清掉）。
        let vault = match Vault::open(vault_path) {
            Ok(vault) => vault,
            Err(error) => {
                remove_vault_files(vault_path);
                return Err(SyncError::Storage(error));
            }
        };
        let created = (|| -> Result<(LocalIdentityV1, String), SyncError> {
            session.prepare_pending(&vault, SystemClock.now_secs())?;
            let receipt = session.request_genesis(&vault, &materials, setup_secret)?;
            let report = session.finalize_genesis(&vault, &materials, &receipt)?;

            ensure_structure_row(&vault)?;
            vault.set_meta(VAULT_LABEL_META, vault_label.as_bytes())?;
            // 初始游标：genesis 响应键 5（旧 relay 可能没有 —— 那就留空，publish 时用
            // `GET /state` 键 14 兜底，且绝不把"缺失"当成 0 号游标）。
            if let Some(cursor) = &receipt.initial_cursor {
                vault.set_meta(RELAY_CURSOR_META, cursor.as_bytes())?;
            }
            // log_id 来自 genesis 响应：先钉住 relay 日志代，pull 再复核。
            set_sync_log_id(&vault, &receipt.log_id)?;
            // 初始 recovery 槽材料（密文）留给发布/Join 复用（同一 slot_id 幂等）。
            let slot_record = encode_initial_slot(&materials.recovery_slot)?;
            vault.set_meta(INITIAL_RECOVERY_SLOT_META, &slot_record)?;

            // 初始 recovery 槽必须**现在**上传（ADR 0009 切片 4c-4）：KEY_SLOT 对象的 MNE1
            // 密钥 `K_slot_mne = HKDF(恢复码种子)` 不可逆，而恢复码只在这一刻还在内存里、不落盘。
            // 这一步是初始化内部操作（`open_for_init` 跳过 pending 守卫）。
            Self::upload_initial_slot_during_init(
                &context,
                &vault,
                &report.identity,
                &materials.recovery_seed,
            )?;

            // 本地初始化完成：pending → Done → 清理（清理只在 Done）。
            // app 的延迟路径停在 PinCommitted，等身份 rename 成功后由调用方推进。
            if finish_identity {
                session.finish(&vault)?;
            }
            Ok((report.identity, materials.recovery_code.as_str().to_owned()))
        })();
        match created {
            Ok((identity, recovery_code)) => Ok(CreatedVault {
                vault,
                vault_id: identity.vault_id,
                recovery_code,
                identity,
                setup_secret: setup_secret.to_owned(),
            }),
            Err(error) => {
                drop(vault);
                remove_vault_files(vault_path);
                Err(error)
            }
        }
    }

    /// CLI 路径的发布初始基线：**用恢复码**确认管理身份，然后与身份路径共用同一条核心。
    ///
    /// 步骤（顺序不可交换）：
    /// 1. 恢复码必须与本地管理身份一致（`HKDF(R) == identity.manager_seed`）——错了在
    ///    任何上传之前失败，避免用一个错码把 KEY_SLOT 密文写坏；
    /// 2. 上传 genesis 的 recovery 槽（staging；`PUT key-slots` 对同 slot_id 同描述符幂等；
    ///    新实例在 Create 时已上传过一次，这里只是补齐旧实例或灾后重传）并落 durable 标记；
    /// 3. 核心：一轮完整 `SyncOnce`（覆盖证明只认 durable 事实）+ 连续两份同 epoch
    ///    checkpoint（见 [`Self::publish_baseline_core_with`]）。
    ///
    /// 这条入口保留给 CLI（`--dev-sync-publish`）、自动化与恢复场景；面板按钮走
    /// [`Self::publish_initial_pair_with_identity`]（不需要恢复码）。
    pub fn publish_initial_pair<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        recovery_code: &str,
    ) -> Result<CheckpointReport, SyncError> {
        let seed = RecoverySeed::from_code(recovery_code)?;
        verify_recovery_matches(identity, &seed)?;
        // 预算取历史默认（本入口没有 per-call 预算参数）；显式 context 用自己的预算。
        let context = target.relay_context(None)?;
        Self::upload_initial_slot(&context, vault, identity, &seed)?;
        Self::publish_baseline_core_with(&context, vault, identity, SyncOnceOptions::default())
    }

    /// **只凭受保护身份**发布初始基线（ADR 0009 切片 4c-4；面板「发布基线」的库入口）。
    ///
    /// 与 [`Self::publish_initial_pair`] 的差别只有"信任锚 + 初始槽"两步：
    ///
    /// - **不接收恢复码**：管理私钥（`identity.manager_seed`）必须存在，且派生出的管理公钥
    ///   必须等于身份里 pin 住的 `pinned_manager`。Join 设备没有管理私钥 → 明确错误，
    ///   绝不退化成"用设备 key 冒充管理"；
    /// - 初始 recovery 槽由 Create 上传（恢复码当时还在内存里），这里只做 fail-closed
    ///   校验：槽材料与身份/库自洽 + [`INITIAL_SLOT_UPLOADED_META`] 存在。旧实例（建库时
    ///   没传过槽）明确失败并指向 CLI 的恢复码路径——**绝不**静默发布一份 B 无法 Join 的
    ///   假基线；
    /// - 其余（sync + 两份 checkpoint + durable 基线事实）与 CLI 路径**逐字节同一条**
    ///   核心路径（[`Self::publish_baseline_core_with`]），不复制逻辑。
    ///
    /// 幂等语义同 4b：重跑会多一对真实 checkpoint，`/bootstrap` 只取最新两份 published。
    pub fn publish_initial_pair_with_identity<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<CheckpointReport, SyncError> {
        Self::publish_initial_pair_with_identity_with(
            target,
            vault,
            identity,
            SyncOnceOptions::default(),
        )
    }

    /// 同 [`Self::publish_initial_pair_with_identity`]，但接收**传输预算 + 集成前守卫**
    /// （ADR 0012 S1：发布内部也跑一轮同步并 `integrate`，所以与同步共用同一条安全纪律）。
    pub fn publish_initial_pair_with_identity_with<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        options: SyncOnceOptions<'_>,
    ) -> Result<CheckpointReport, SyncError> {
        // 先拒 Join 设备（真正原因），再看槽材料/上传标记（保持既有错误优先级：
        // 本地可判定原因先于 URL/网络配置错误）。
        require_manager_identity(identity)?;
        let slot = load_initial_slot(vault, &identity.vault_id)?;
        verify_identity_slot_consistency(identity, &slot)?;
        if !initial_slot_uploaded(vault, &slot)? {
            return Err(SyncError::Unsupported(
                "本实例建库时没有上传初始 recovery 槽（旧版本 Create）：面板无法补传\
                 （K_slot_mne 只能由恢复码派生）；请先用 --dev-sync-publish 与恢复码补一次，\
                 之后面板「发布基线」一直可用",
            ));
        }
        let context = target.relay_context(options.transport)?;
        Self::publish_baseline_core_with(&context, vault, identity, options)
    }

    /// 上传 genesis 的初始 recovery 槽（staging）并落 durable 标记。
    ///
    /// **只有恢复码在内存里的调用点能用**：`create`（genesis 刚返回）与
    /// [`Self::publish_initial_pair`]（CLI，恢复码来自环境变量）。KEY_SLOT 对象的 MNE1
    /// 密钥 `K_slot_mne = HKDF(恢复码种子)` 不可逆，恢复码又不落盘，所以事后（只有
    /// `manager_seed` 时）无法补传——这是 ADR 0009 切片 4c-4 把槽上传前移的根因。
    ///
    /// message id 确定性派生，整份 KEY_SLOT 对象逐字节可复现，重传 relay 幂等接受。
    fn upload_initial_slot(
        context: &RelayContext,
        vault: &Vault,
        identity: &LocalIdentityV1,
        seed: &RecoverySeed,
    ) -> Result<(), SyncError> {
        Self::upload_initial_slot_with(context, vault, identity, seed, false)
    }

    /// Create 内部的初始槽上传：pending 尚未收尾，走 `open_for_init`（跳过 pending 守卫）。
    pub(crate) fn upload_initial_slot_during_init(
        context: &RelayContext,
        vault: &Vault,
        identity: &LocalIdentityV1,
        seed: &RecoverySeed,
    ) -> Result<(), SyncError> {
        Self::upload_initial_slot_with(context, vault, identity, seed, true)
    }

    fn upload_initial_slot_with(
        context: &RelayContext,
        vault: &Vault,
        identity: &LocalIdentityV1,
        seed: &RecoverySeed,
        during_init: bool,
    ) -> Result<(), SyncError> {
        let slot = load_initial_slot(vault, &identity.vault_id)?;
        verify_identity_slot_consistency(identity, &slot)?;

        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let pinned = if during_init {
            context.open_for_init(vault, identity)?
        } else {
            context.open(vault, identity)?
        };
        let mut client = pinned.into_client();
        ensure_cursor(&mut client, vault)?;
        let message_id = key_slot_message_id(&slot)?;
        {
            let mut writer = CheckpointWriter::new(
                vault,
                &mut client,
                identity.vault_id,
                identity.device_id,
                signing_key,
                root,
                identity.loro_peer_id(),
            );
            writer.upload_recovery_slot(&slot, seed, message_id)?;
        }
        write_initial_slot_uploaded(vault, &slot.descriptor.slot_id, &message_id)
    }

    /// 发布核心：一轮完整 `SyncOnce` + 两份真实同 epoch checkpoint（两条入口共用）。
    ///
    /// - 一轮完整 `SyncOnce`：覆盖证明只认 durable 事实（integrated 前缀/skipped-asset
    ///   记录），所以本地刚建好的笔记必须先推送到 relay 并拉回 integrate；
    /// - 连续两份同 epoch checkpoint（`CheckpointWriter` 的 prepare→validate→promote）：
    ///   `/bootstrap` 只按 `published_at DESC` 取最新两份 published，所以**必须**成对发布。
    ///
    /// 资产源用 [`StorageAssets`]（真实 storage 资产；空库自然是 0 个资产），不用合成
    /// fixture。返回值是**较新那份** checkpoint 的报告（pair = 前一份 + 这一份）。
    fn publish_baseline_core_with(
        context: &RelayContext,
        vault: &Vault,
        identity: &LocalIdentityV1,
        mut options: SyncOnceOptions<'_>,
    ) -> Result<CheckpointReport, SyncError> {
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        // 缺 `sync.relay_cursor` 时用 `GET /state` 键 14 兜底（旧 relay 兼容）。
        ensure_cursor(&mut client, vault)?;

        // 1) 发布前把 durable 数据推/拉平（错误一律向上传播：暂停=不得发布基线）。
        //    集成前守卫与同步共用一条：取消/授权已失效时**不 append**。
        {
            let engine_identity = EngineIdentity {
                device_id: identity.device_id,
                signing_key: signing_key.clone(),
                peer_id: identity.loro_peer_id(),
            };
            let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root.clone());
            match options.before_integrate.as_mut() {
                Some(guard) => engine.sync_once_with_guard(&mut **guard)?,
                None => engine.sync_once()?,
            };
        }

        // 2) 两份真实 checkpoint（prepare→validate→promote 全部真实读回/导入）。
        let mut writer = CheckpointWriter::new(
            vault,
            &mut client,
            identity.vault_id,
            identity.device_id,
            signing_key,
            root,
            identity.loro_peer_id(),
        );
        let _first = writer.create_checkpoint(&StorageAssets)?;
        let second = writer.create_checkpoint(&StorageAssets)?;
        write_published_baseline(vault, &second)?;
        Ok(second)
    }

    /// Join：只给 `vault_id + 恢复码 + 新路径`，不读 A 的目录/identity/root。
    ///
    /// 初始化走 ADR 0013 S2b 的受限能力 [`InitSession::new_fresh_recovery`]：
    ///
    /// 1. 恢复码 → 恢复种子 →（会话内）管理私钥（管理信任锚）；
    /// 2. 新库 + durable pending（`Prepared`）→ 写前推进 `GenesisRequested` → `self_enroll`
    ///    自批新设备（唯一允许 unpinned 消费 `/control` 的路径，仍强制管理锚 + 逐条签名）；
    /// 3. 认证 `bootstrap::run_with_pins` 导入基线并单事务推进 pins/cursor；
    /// 4. 用**同一验证视图**提交 record pin + bootstrap 锚（`commit_recovery_pin`）；
    /// 5. pending → `Done` → 清理（清理只在 Done）；app 走
    ///    [`Self::join_deferring_identity`] 时停在 `PinCommitted`，身份落盘后再推进。
    ///
    /// 失败语义沿用 ADR 0009（app 验收依赖它）：**返回错误时清掉本次新建的本地文件**，
    /// 不写 cursor/identity（bootstrap 只在全部导入成功后推进水位）；pending 的用途是
    /// **崩溃**之后的启动对账（[`crate::init_session::reconcile`]）。
    pub fn join(
        relay_url: &str,
        vault_id: &VaultId,
        recovery_code: &str,
        vault_path: &Path,
    ) -> Result<JoinedVault, SyncError> {
        Self::join_inner(relay_url, vault_id, recovery_code, vault_path, true)
    }

    /// **app 专用**：与 [`Self::join`] 相同，但 pending 停在 `PinCommitted`。
    ///
    /// 与 [`Self::create_deferring_identity`] 对称：身份原子落盘后由调用方调用
    /// [`crate::init_session::mark_identity_written`] + `finish_identity_written`，
    /// 崩溃窗口由启动对账（[`crate::init_session::reconcile`]）补 finalize。
    pub fn join_deferring_identity(
        relay_url: &str,
        vault_id: &VaultId,
        recovery_code: &str,
        vault_path: &Path,
    ) -> Result<JoinedVault, SyncError> {
        Self::join_inner(relay_url, vault_id, recovery_code, vault_path, false)
    }

    fn join_inner(
        relay_url: &str,
        vault_id: &VaultId,
        recovery_code: &str,
        vault_path: &Path,
        finish_identity: bool,
    ) -> Result<JoinedVault, SyncError> {
        if vault_path.as_os_str().is_empty() {
            return Err(SyncError::LocalCorrupt("Join 需要明确的 vault 路径"));
        }
        if vault_path.exists() {
            return Err(SyncError::LocalCorrupt(
                "Join 只接受尚不存在的 vault 路径（已有库请按其身份使用）",
            ));
        }

        // 1. 恢复码 → 恢复种子；管理锚由 InitSession 从种子推导（不信任 relay 的公钥）。
        let seed = RecoverySeed::from_code(recovery_code)?;
        let context = RelayContext::local_dev(relay_url, DEFAULT_TRANSPORT_TIMEOUTS)?;
        let session = InitSession::new_fresh_recovery(context, *vault_id, &seed)?;

        // 2. 新库（pending 需要 SQLite；任何失败都清掉刚建的文件）。
        let vault = match Vault::open(vault_path) {
            Ok(vault) => vault,
            Err(error) => {
                remove_vault_files(vault_path);
                return Err(SyncError::Storage(error));
            }
        };
        let joined = (|| -> Result<LocalIdentityV1, SyncError> {
            session.prepare_pending(&vault, SystemClock.now_secs())?;
            let recovered = session.recover_identity(&vault, SystemClock.now_secs())?;
            session.commit_recovery_pin(&vault, &recovered.identity)?;
            if finish_identity {
                session.finish(&vault)?;
            }
            Ok(recovered.identity)
        })();

        match joined {
            Ok(identity) => Ok(JoinedVault {
                vault,
                identity,
                vault_id: *vault_id,
            }),
            Err(error) => {
                // 失败不留下半初始化库（cursor/identity 都没写；文件也清掉，重试从头来）。
                drop(vault);
                remove_vault_files(vault_path);
                Err(error)
            }
        }
    }

    /// SyncOnce：一轮完整同步（`encrypt_pending → push → pull → integrate → ack`）。
    ///
    /// [`SyncEngine`] 是借用型的，每次调用内构造；游标/水位由 engine 自己按 durable 事实推进。
    ///
    /// 这是**向后兼容入口**（CLI/测试/旧调用）：传输预算与集成前守卫都用历史默认
    /// （ADR 0012 S1）。需要可配预算或取消复核时用 [`Self::sync_once_with`]。
    pub fn sync_once<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<SyncReport, SyncError> {
        Self::sync_once_with(target, vault, identity, SyncOnceOptions::default())
    }

    /// 同 [`Self::sync_once`]，但由调用方注入**传输预算**与**集成前守卫**（ADR 0012 S1）。
    ///
    /// `options.before_integrate` 在 `integrate` 之前调用一次：返回 `Err` 时本轮
    /// **不会追加任何本地行**（取消/授权已失效），已推送的对象与已拉取的 inbox 保持
    /// durable 事实（重试可复用）。进入 integrate 后事务必须跑完，不再中断。
    pub fn sync_once_with<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        mut options: SyncOnceOptions<'_>,
    ) -> Result<SyncReport, SyncError> {
        let context = target.relay_context(options.transport)?;
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        // pull 必须有游标：B 由 bootstrap 落库，A 由 genesis 落库；旧 relay 两者都没有时
        // 用 `GET /state` 键 14 兜底（绝不把"缺失"当成 0 号游标硬编）。
        ensure_cursor(&mut client, vault)?;
        let engine_identity = EngineIdentity {
            device_id: identity.device_id,
            signing_key,
            peer_id: identity.loro_peer_id(),
        };
        let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root);
        match options.before_integrate.as_mut() {
            Some(guard) => engine.sync_once_with_guard(&mut **guard),
            None => engine.sync_once(),
        }
    }

    // ------------------------------------------------------------------
    // ADR 0012 S2a：可暂停的两阶段同步（Prepare / Integrate / Finish）
    // ------------------------------------------------------------------
    //
    // 三个入口各自**重新构造** client/engine：真正的事实只存在 DB（outbox/inbox/
    // cursor/pins），不依赖进程内 engine。调用方（app 的 worker）负责在阶段之间等待
    // 授权/交付回执；本模块只保证每一步"只做这一阶段的事"：
    //
    // - `sync_prepare`：encrypt → push → pull（**不** integrate、**不** ack）；
    // - `sync_integrate`：integrate（外加授权 head 复核），**不** ack；
    // - `sync_finish`：push tail（若有）→ ack。

    /// 两阶段同步的 **Prepare** 阶段：加密待发送意图 → 推送 → 拉取一页。
    ///
    /// 明确**不**做 `integrate`/`ack`：本阶段只把远端变化落成 durable inbox 并推进
    /// pull 游标，正文/结构 head 一律不动。因此 UI 在网络阶段不需要持闸、可以继续编辑。
    pub fn sync_prepare<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
    ) -> Result<PrepareReport, SyncError> {
        let context = target.relay_context(transport)?;
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        ensure_cursor(&mut client, vault)?;
        let engine_identity = EngineIdentity {
            device_id: identity.device_id,
            signing_key,
            peer_id: identity.loro_peer_id(),
        };
        let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root);
        let config = EngineConfig::default();
        let encrypted = engine.encrypt_pending(config.send_batch)?;
        let push = engine.push()?;
        // ADR 0012 两阶段路径的资产上传点：与完整 `sync_once` 同一阶段（正文之后、
        // pull 之前），保证自动同步（S3）也会增量上传新资产。
        let assets = engine.upload_pending_assets()?;
        let pull = engine.pull()?;
        Ok(PrepareReport {
            encrypted,
            push,
            assets,
            pull,
        })
    }

    /// 两阶段同步的 **Integrate** 阶段：把 durable verified inbox 集成进本地库。
    ///
    /// `expect_head` 是调用方（UI 持闸）给出的授权基准：`(doc_id, durable_head_seq)`。
    /// 任一条不满足就返回 [`SyncError::CancelledBeforeIntegrate`]，**不 append**：
    ///
    /// - 库中该文档 head 已被移动（授权失效 / 本地还有未持闸写入）；
    /// - 调用方在进入本阶段前已复核取消标志（见 app worker：先查 `OpControl`，再调本函数）。
    ///
    /// `watch_doc` 是 UI 准备 adopt 的活动文档：报告里会给出它的 head 是否在本阶段前进了，
    /// 供 UI 判定"是否需要交付"。**不** ack（ack 属于 Finish，等 UI adopt/end 之后）。
    ///
    /// 集成本身按 ADR 0012 S2b 的**依赖顺序**收敛（结构 → 只投影 → 正文公平轮转），
    /// 因此调用方不需要"再联网补一轮"来修结构/正文依赖。
    pub fn sync_integrate<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
        expect_head: Option<(&str, u64)>,
        watch_doc: &str,
    ) -> Result<IntegratePhaseReport, SyncError> {
        Self::sync_integrate_with_config(
            target,
            vault,
            identity,
            transport,
            expect_head,
            watch_doc,
            IntegrateConfig::default(),
        )
    }

    /// 同 [`Self::sync_integrate`]，但显式指定集成批次预算（测试/诊断用）。
    #[allow(clippy::too_many_arguments)]
    pub fn sync_integrate_with_config<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
        expect_head: Option<(&str, u64)>,
        watch_doc: &str,
        integrate_config: IntegrateConfig,
    ) -> Result<IntegratePhaseReport, SyncError> {
        // Integrate 前复核授权基准（ADR 0012 S2b）：head 被推进 = 授权已失效，
        // 一律不 append；inbox/outbox 原样保留，调用方可按新授权重试。
        if let Some((doc_id, expected)) = expect_head {
            let head = load_head(vault, doc_id)?;
            if head.head_seq != expected {
                return Err(SyncError::CancelledBeforeIntegrate);
            }
        }
        let before = load_head(vault, watch_doc)?.head_seq;
        let context = target.relay_context(transport)?;
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        ensure_cursor(&mut client, vault)?;
        let peer_id = identity.loro_peer_id();
        let engine_identity = EngineIdentity {
            device_id: identity.device_id,
            signing_key,
            peer_id,
        };
        let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root);
        let outcome = engine.integrate_dependency_ordered(integrate_config)?;
        drop(engine);
        // changed heads（正文）触发一次**现有接口**的物化：B 事务写 `notes.body_md`
        // 并发布 `BodyMaterialized` 事件（索引器据此增量更新）。结构投影的事件由
        // `StructureWriter::commit` 自己发布（`StructureProjected`）。
        let (materialized, materialize_failed) =
            materialize_changed_bodies(vault, peer_id, &outcome.changed_heads);
        let after = load_head(vault, watch_doc)?.head_seq;
        let watch_doc_changed = after != before;
        Ok(IntegratePhaseReport {
            integrate: outcome.report,
            changed_heads: outcome.changed_heads,
            watch_doc_changed,
            materialized,
            materialize_failed,
        })
    }

    /// 两阶段同步的 **Finish** 阶段：推送本阶段新落库的本地意图（若有）→ 上报 ACK。
    ///
    /// 调用方契约：UI 已经把 integrate 的 durable 变化 adopt 进 live、并 `end` 释放闸门。
    /// 本阶段不再 `pull`/`integrate`：它只做 "让 relay 知道本机已经收到/整合到哪" 以及
    /// 清掉本轮 Held 期间新产生的本地意图。
    pub fn sync_finish<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
    ) -> Result<FinishReport, SyncError> {
        let context = target.relay_context(transport)?;
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        ensure_cursor(&mut client, vault)?;
        let engine_identity = EngineIdentity {
            device_id: identity.device_id,
            signing_key,
            peer_id: identity.loro_peer_id(),
        };
        let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root);
        let config = EngineConfig::default();
        // 先推 tail（Held 窗口内新落库的本地意图），再上报 received/integrated 游标。
        let encrypted = engine.encrypt_pending(config.send_batch)?;
        let push = engine.push()?;
        let ack = engine.ack()?;
        Ok(FinishReport {
            ack,
            encrypted,
            push,
        })
    }

    /// ADR 0010 切片 2a-2：读一次**已验证控制链的只读成员视图**（设备列表 UI 的数据源）。
    ///
    /// 只构造一个**临时** `HttpRelayClient`（用完即丢，不缓存 token/控制链）：调用方拿到的
    /// 只有 [`MemberView`]——它不含 token/密钥，`Debug` 也只打印 id/status/fingerprint。
    /// 缓存失效语义由 2a-1 的 [`HttpRelayClient::members`] 负责（先比对 `/state.auth_rev`
    /// 再决定是否重新拉控制链）；每次新客户端天然读到最新的 `auth_rev`。
    ///
    /// 失败即真实失败（relay 不可达 / 被吊销 `REVOKED` / 控制链 pin 不符），不返回旧快照。
    pub fn members<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<Vec<MemberView>, SyncError> {
        let context = target.relay_context(None)?;
        let mut client = context.open(vault, identity)?.into_client();
        client.members()
    }

    /// ADR 0010 切片 1b + 2a-2：**移除一台设备对 relay 的访问**（不轮换密钥）。
    ///
    /// 语义与安全边界严格照 ADR 0010：
    ///
    /// - 只是诚实 relay 上的**访问撤销**（`auth_rev` 前进、目标 `status=revoked`），
    ///   不是签名成员移除、也不是密码学撤销：被移除设备仍可能用它已获得的旧 root 解密
    ///   旧/同 epoch 密文；管理材料（`manager_seed`/恢复码）**不会被撤销**；
    /// - **禁止移除本机**（这里在任何网络动作之前拒绝；helper 里还有第二道）；
    /// - **必须有管理密钥**：只有 `manager_seed` 在身份里的设备（Create/恢复码路径）能管理；
    ///   Join 设备明确失败，绝不退化成"用设备 key 冒充管理"；
    /// - 不删本地身份 / `manager_seed` / 恢复材料 / 笔记，不自动轮换、不把 revoked 翻回 active；
    /// - `request_id` 每次调用随机 16B（`OsRandom`）：响应丢失就整次重试（新 id），
    ///   不拿旧 id 去撞 relay；`expected_control_rev` 取刚读到的 `/state.control_rev`
    ///   （独立吊销**不推进** `control_rev`，CAS 冲突时应刷新后重试）。
    ///
    /// 成功后**重新拉一次成员视图**再返回，调用方（worker）可直接把最新列表投影到面板，
    /// 不需要另发一条列表命令。
    ///
    /// ## 这是**默认**移除路径，且**永不**轮换（task-123）
    ///
    /// 本调用只产生"设备访问已吊销"这一件事：返回的 [`RevokedDevice::outcome`] 里
    /// `changed`/`auth_rev` 可能变化，`control_rev` 与 vault 的 `key_epoch` **保持不变**
    /// （测试断言 relay `/state`、DB 与两台设备的本地身份 epoch）。它**不会**、也**不得**
    /// 被改成隐式触发轮换：需要新 root / 重加密历史时必须由用户显式走
    /// [`crate::dev_local::DevLocal::rotate`]（opt-in），两者的 wire 与错误语义互不复用。
    pub fn revoke_device<T: RelayTarget + ?Sized>(
        relay: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        target: DeviceId,
    ) -> Result<RevokedDevice, SyncError> {
        // ① 本机守卫：先于任何 IO（打开库/读身份以外）拒绝。
        if target == identity.device_id {
            return Err(SyncError::Unsupported(
                "禁止移除本机设备：会立即自断同步与管理访问（ADR 0010）；请用另一台管理设备或恢复码",
            ));
        }
        // ② 管理密钥必须来自受保护身份（Join 设备没有）：缺它就直接失败。
        let Some(seed) = identity.manager_seed.clone() else {
            return Err(SyncError::Unsupported(
                "本机没有管理密钥（Join 设备）：无法移除设备；请用 Create 设备或最新恢复码操作",
            ));
        };
        let manager = ManagementKey::from_seed(seed);

        // ③ CAS 基准取当前 `/state.control_rev`（独立吊销不推进它；不盲用 identity 里的旧值）。
        let context = relay.relay_context(None)?;
        let mut client = context.open(vault, identity)?.into_client();
        let state = client.state()?;
        if state.instance_id != identity.pinned_instance {
            return Err(SyncError::RelayWire("relay instance 与本地 pin 不符"));
        }

        // ④ 每次点击一个新 request_id（去重 scope 是固定 `management/device-revoke`，
        //    target 在 canonical body 里，跨设备不会碰撞）。
        let request_id: [u8; 16] = array(&mut OsRandom)?;
        let ctx = RevokeContext::from_identity(
            context.origin_str().as_str(),
            identity,
            state.control_rev,
            SystemClock.now_secs(),
        )
        .with_api_version(context.api_version());
        let mut transport = context.transport()?;
        let outcome = crate::device_management::revoke_device(
            &mut transport,
            &ctx,
            &manager,
            target,
            request_id,
        )?;

        // ⑤ 只读刷新：`auth_rev` 已前进，新客户端自然读到吊销后的成员集合。
        let members = Self::members(&context, vault, identity)?;
        Ok(RevokedDevice { outcome, members })
    }

    /// 读 dev-local 实例标签（[`VAULT_LABEL_META`]；Join 的库没有，返回 `None`）。
    pub fn vault_label(vault: &Vault) -> Result<Option<String>, SyncError> {
        match vault.get_meta(VAULT_LABEL_META)? {
            None => Ok(None),
            Some(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| SyncError::LocalCorrupt("vault 标签不是 UTF-8")),
        }
    }

    /// 读最近一次发布基线的 durable 事实（[`PUBLISHED_BASELINE_META`]）。
    pub fn published_baseline(vault: &Vault) -> Result<Option<PublishedBaseline>, SyncError> {
        let Some(bytes) = vault.get_meta(PUBLISHED_BASELINE_META)? else {
            return Ok(None);
        };
        decode_published_baseline(&bytes).map(Some)
    }
}

// ---------------------------------------------------------------- 内部助手

/// genesis 的 password 槽材料只用于凑齐 `GenesisMaterials`（本模块不上传口令槽）：
/// 用 32B 随机数的十六进制当一次性口令，构造完立即丢弃。
pub(crate) fn throwaway_password() -> Result<String, SyncError> {
    let bytes: [u8; 32] = array(&mut OsRandom)?;
    Ok(ids::hex_of(&bytes))
}

/// 身份的当前 epoch 根。
pub(crate) fn root_of(identity: &LocalIdentityV1) -> RootSecretV1 {
    RootSecretV1::new(
        identity.vault_id,
        identity.current_epoch,
        identity.current_root.clone(),
    )
}

/// 对 changed heads 里的**正文**文档触发一次现有接口的物化（`projection_outbox` →
/// `notes.body_md`，成功时发布 `BodyMaterialized` → 索引增量）。
///
/// 只处理派生数据：失败计入返回值并打日志，不改 CRDT 事实、也不改变集成结论
/// （搜索索引还有周期对账兜底）。结构文档的投影事件由 `StructureWriter::commit`
/// 自己发布。
fn materialize_changed_bodies(
    vault: &Vault,
    peer_id: u64,
    changed_heads: &[String],
) -> (usize, usize) {
    if changed_heads.is_empty() {
        return (0, 0);
    }
    let pending = match pending_outbox(vault) {
        Ok(pending) => pending,
        Err(error) => {
            eprintln!("[dev-sync] 读取 projection_outbox 失败（跳过本轮物化）：{error}");
            return (0, changed_heads.len());
        }
    };
    let mut materialized = 0usize;
    let mut failed = 0usize;
    for item in pending {
        if item.doc_id == STRUCTURE_DOC_ID || !changed_heads.iter().any(|head| head == &item.doc_id)
        {
            continue;
        }
        let Some(note_id) = item.note_id else {
            // 结构投影还没建立 notes 行：不是失败，等下一轮结构/索引对账。
            continue;
        };
        match materialize_body(vault, &note_id, &item.doc_id, item.target_seq, peer_id) {
            Ok(_) => materialized += 1,
            Err(error) => {
                failed += 1;
                eprintln!(
                    "[dev-sync] 正文物化失败（{}，目标 seq {}）：{error}",
                    item.doc_id, item.target_seq
                );
            }
        }
    }
    (materialized, failed)
}

/// 把 `PeerId` 的 u64 映射回 16B（`identity.peer_id` 的存储形态）。
///
/// 受限初始化入口（[`crate::init_session`]）也用同一条映射：Join 的 peer 口径只能有
/// 一处，不允许各写一套。
pub(crate) fn peer_bytes_of(peer_id: u64) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&peer_id.to_be_bytes());
    bytes
}

/// 缺 `sync.relay_cursor` 时用 relay `GET /state` 键 14 兜底（genesis 键 5 的旧 relay 兼容）。
fn ensure_cursor<T: HttpTransport>(
    client: &mut HttpRelayClient<T>,
    vault: &Vault,
) -> Result<(), SyncError> {
    if vault.get_meta(RELAY_CURSOR_META)?.is_none() {
        let cursor = client.initial_cursor()?;
        vault.set_meta(RELAY_CURSOR_META, cursor.as_bytes())?;
    }
    Ok(())
}

/// 恢复码必须与本地管理身份一致：`HKDF(R, vault_id, "MN/v1/management-ed25519\0")`
/// 逐字节等于 identity 里的 `manager_seed`。错码在这里失败，**不必先上传任何对象**。
fn verify_recovery_matches(
    identity: &LocalIdentityV1,
    seed: &RecoverySeed,
) -> Result<(), SyncError> {
    let Some(stored) = &identity.manager_seed else {
        return Err(SyncError::Unsupported(
            "发布初始基线需要 genesis 身份（含管理私钥）：Join 设备不能发布",
        ));
    };
    let derived = management_seed(seed, &identity.vault_id)?;
    if !derived.ct_eq(stored) {
        return Err(SyncError::RecoveryCode("恢复码与本地管理身份不匹配"));
    }
    Ok(())
}

/// 身份 ↔ 初始 recovery 槽的自洽校验（不联网、不碰秘密输出）。
///
/// 三条都对应真实故障模式，缺一不可：
/// 1. 槽归属 = 身份 vault（加载时已校验，这里再兜一次防"换库/换身份"的错配）；
/// 2. 槽 epoch = 身份当前 epoch：轮换过 epoch 的实例不能再用"初始基线"发布（否则会拿
///    旧 epoch 的槽配新 root，Join 端解出的根对不上）；
/// 3. 管理私钥必须存在，且派生公钥 == `pinned_manager`（管理 pin 自洽）：Join 设备没有
///    管理私钥 → 明确拒绝，绝不退化成"用设备 key 冒充管理"。
fn verify_identity_slot_consistency(
    identity: &LocalIdentityV1,
    slot: &KeySlotV1,
) -> Result<(), SyncError> {
    if slot.descriptor.vault_id != identity.vault_id {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽的 vault 归属与身份不一致",
        ));
    }
    if slot.descriptor.key_epoch != identity.current_epoch {
        return Err(SyncError::Unsupported(
            "初始基线只能在 genesis epoch 发布：初始槽的 epoch 与身份当前 epoch 不一致\
             （轮换之后不能用初始基线发布）",
        ));
    }
    require_manager_identity(identity)
}

/// 管理密钥（信任锚）必须来自受保护身份，且与 `pinned_manager` 自洽。
///
/// 这条校验**先于**读槽/任何上传：Join 设备（`manager_seed = None`）必须在拿到"缺初始槽"
/// 之类无关错误之前就看到真正原因，也绝不退化成"用设备 key 冒充管理"。
fn require_manager_identity(identity: &LocalIdentityV1) -> Result<(), SyncError> {
    let Some(seed) = identity.manager_seed.clone() else {
        return Err(SyncError::Unsupported(
            "发布初始基线需要 genesis 身份（含管理私钥）：Join 设备没有管理密钥，不能发布",
        ));
    };
    if ManagementKey::from_seed(seed).public_key() != identity.pinned_manager {
        return Err(SyncError::LocalCorrupt(
            "身份的管理私钥与 pinned_manager 不一致（身份文件被换过或损坏）",
        ));
    }
    Ok(())
}

/// 初始 recovery 槽（KEY_SLOT 对象）的确定性 `message_id`。
///
/// `message_id` 进 MNE1 header/AAD；relay 对同 `object_id` 换了 `message_id`（=不同信封
/// 字节）会返回 409 ID_CONFLICT，而"上传初始槽 / 发布基线"都是**可重跑**的动作（Create
/// 传过一次、CLI 发布补传、checkpoint 失败后重试）。所以这里只用槽材料（slot_id + 描述符
/// CDE，都是非秘密）派生一个稳定 id：重试/重启/跨入口都得到逐字节相同的 KEY_SLOT 对象，
/// relay 按幂等接受。
fn key_slot_message_id(slot: &KeySlotV1) -> Result<[u8; 16], SyncError> {
    let descriptor = slot.descriptor.encode()?;
    let mut hasher = Sha256::new();
    hasher.update(KEY_SLOT_MESSAGE_DOMAIN);
    hasher.update(slot.descriptor.slot_id);
    hasher.update(&descriptor);
    let digest = hasher.finalize();
    let mut message_id = [0u8; 16];
    message_id.copy_from_slice(&digest[..16]);
    Ok(message_id)
}

/// 从 `meta` 读回 genesis 的 recovery 槽（描述符 + 包裹根），并校验归属。
fn load_initial_slot(vault: &Vault, vault_id: &VaultId) -> Result<KeySlotV1, SyncError> {
    let bytes = vault
        .get_meta(INITIAL_RECOVERY_SLOT_META)?
        .ok_or(SyncError::LocalCorrupt(
            "vault 缺 genesis 初始 recovery 槽记录（只有 DevLocal::create 的库能发布基线）",
        ))?;
    decode_initial_slot(&bytes, vault_id)
}

/// `magic || u32 BE(descriptor_len) || CDE(descriptor) || nonce||ciphertext`。
pub(crate) fn encode_initial_slot(slot: &KeySlotV1) -> Result<Vec<u8>, SyncError> {
    let descriptor = slot.descriptor.encode()?;
    let descriptor_len = u32::try_from(descriptor.len())
        .map_err(|_| SyncError::LocalCorrupt("初始槽描述符长度超出 u32"))?;
    let wrapped = slot.wrapped_bytes();
    let mut out =
        Vec::with_capacity(INITIAL_SLOT_MAGIC.len() + 4 + descriptor.len() + wrapped.len());
    out.extend_from_slice(INITIAL_SLOT_MAGIC);
    out.extend_from_slice(&descriptor_len.to_be_bytes());
    out.extend_from_slice(&descriptor);
    out.extend_from_slice(&wrapped);
    Ok(out)
}

/// 解析 [`encode_initial_slot`] 的记录；任何不自洽都按本地损坏处理（不猜、不修复）。
fn decode_initial_slot(bytes: &[u8], vault_id: &VaultId) -> Result<KeySlotV1, SyncError> {
    let header_len = INITIAL_SLOT_MAGIC.len() + 4;
    if bytes.len() < header_len || !bytes.starts_with(INITIAL_SLOT_MAGIC) {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽记录 magic/长度不符",
        ));
    }
    let mut raw_len = [0u8; 4];
    raw_len.copy_from_slice(&bytes[INITIAL_SLOT_MAGIC.len()..header_len]);
    let descriptor_len = usize::try_from(u32::from_be_bytes(raw_len))
        .map_err(|_| SyncError::LocalCorrupt("初始 recovery 槽描述符长度非法"))?;
    let descriptor_end = header_len
        .checked_add(descriptor_len)
        .ok_or(SyncError::LocalCorrupt("初始 recovery 槽描述符长度溢出"))?;
    if descriptor_len == 0
        || descriptor_len > MAX_SLOT_DESCRIPTOR_LEN
        || descriptor_end >= bytes.len()
    {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽记录的分段长度非法",
        ));
    }
    let descriptor = SlotDescriptorV1::decode(&bytes[header_len..descriptor_end])?;
    if descriptor.vault_id != *vault_id {
        return Err(SyncError::LocalCorrupt("初始 recovery 槽的 vault 归属不符"));
    }
    if descriptor.kind != SlotKind::Recovery {
        return Err(SyncError::LocalCorrupt("初始槽记录不是 recovery 槽"));
    }
    let wrapped = WrappedRoot::from_bytes(&bytes[descriptor_end..])
        .map_err(|_| SyncError::LocalCorrupt("初始 recovery 槽的槽体损坏"))?;
    Ok(KeySlotV1 {
        descriptor,
        wrapped,
    })
}

/// `magic || slot_id(32) || message_id(16)`（初始 recovery 槽「已上传」记录）。
fn encode_initial_slot_uploaded(slot_id: &[u8; 32], message_id: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(INITIAL_SLOT_UPLOADED_MAGIC.len() + 32 + 16);
    out.extend_from_slice(INITIAL_SLOT_UPLOADED_MAGIC);
    out.extend_from_slice(slot_id);
    out.extend_from_slice(message_id);
    out
}

/// 解析 [`encode_initial_slot_uploaded`] 的记录（长度/magic 不符按本地损坏处理）。
fn decode_initial_slot_uploaded(bytes: &[u8]) -> Result<([u8; 32], [u8; 16]), SyncError> {
    let expected = INITIAL_SLOT_UPLOADED_MAGIC.len() + 32 + 16;
    if bytes.len() != expected || !bytes.starts_with(INITIAL_SLOT_UPLOADED_MAGIC) {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽上传记录 magic/长度不符",
        ));
    }
    let mut slot_id = [0u8; 32];
    let mut message_id = [0u8; 16];
    slot_id.copy_from_slice(&bytes[INITIAL_SLOT_UPLOADED_MAGIC.len()..][..32]);
    message_id.copy_from_slice(&bytes[INITIAL_SLOT_UPLOADED_MAGIC.len() + 32..]);
    Ok((slot_id, message_id))
}

/// 上传成功后写 durable 标记（`create` / CLI 发布都会走这里；重传幂等）。
fn write_initial_slot_uploaded(
    vault: &Vault,
    slot_id: &[u8; 32],
    message_id: &[u8; 16],
) -> Result<(), SyncError> {
    vault.set_meta(
        INITIAL_SLOT_UPLOADED_META,
        &encode_initial_slot_uploaded(slot_id, message_id),
    )?;
    Ok(())
}

/// 初始 recovery 槽是否**当前这份槽材料**已经上传过（`false` = 旧版本 Create 的实例）。
///
/// 记录存在但 `slot_id` 对不上 → 本地事实自相矛盾（换过库/槽材料），按损坏 fail-closed，
/// 不猜、不重写。
fn initial_slot_uploaded(vault: &Vault, slot: &KeySlotV1) -> Result<bool, SyncError> {
    let Some(bytes) = vault.get_meta(INITIAL_SLOT_UPLOADED_META)? else {
        return Ok(false);
    };
    let (slot_id, _message_id) = decode_initial_slot_uploaded(&bytes)?;
    if slot_id != slot.descriptor.slot_id {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽上传记录与本库槽材料不一致（不重写、不猜）",
        ));
    }
    Ok(true)
}

/// 把"最新一份已发布 checkpoint"落成本地 durable 事实（不覆盖更旧的两份，只记较新的）。
fn write_published_baseline(vault: &Vault, report: &CheckpointReport) -> Result<(), SyncError> {
    let published_at = report
        .published_at
        .ok_or(SyncError::LocalCorrupt("checkpoint report 缺发布时间"))?;
    let mut out = Vec::with_capacity(PUBLISHED_BASELINE_MAGIC.len() + 16 + 24);
    out.extend_from_slice(PUBLISHED_BASELINE_MAGIC);
    out.extend_from_slice(&report.checkpoint_id);
    out.extend_from_slice(&report.epoch.to_be_bytes());
    out.extend_from_slice(&report.covered_seq.to_be_bytes());
    out.extend_from_slice(&published_at.to_be_bytes());
    vault.set_meta(PUBLISHED_BASELINE_META, &out)?;
    Ok(())
}

/// 解析 [`write_published_baseline`] 的记录。
fn decode_published_baseline(bytes: &[u8]) -> Result<PublishedBaseline, SyncError> {
    let expected = PUBLISHED_BASELINE_MAGIC.len() + 16 + 8 * 3;
    if bytes.len() != expected || !bytes.starts_with(PUBLISHED_BASELINE_MAGIC) {
        return Err(SyncError::LocalCorrupt("已发布基线记录 magic/长度不符"));
    }
    let mut checkpoint_id = [0u8; 16];
    let mut offset = PUBLISHED_BASELINE_MAGIC.len();
    checkpoint_id.copy_from_slice(&bytes[offset..offset + 16]);
    offset += 16;
    let mut field = |name: &'static str| -> Result<u64, SyncError> {
        let end = offset
            .checked_add(8)
            .ok_or(SyncError::LocalCorrupt("已发布基线记录越界"))?;
        let raw = bytes
            .get(offset..end)
            .ok_or(SyncError::LocalCorrupt(name))?;
        offset = end;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(raw);
        Ok(u64::from_be_bytes(buf))
    };
    let epoch = field("已发布基线记录缺 epoch")?;
    let covered_seq = field("已发布基线记录缺 covered_seq")?;
    let published_at = field("已发布基线记录缺 published_at")?;
    Ok(PublishedBaseline {
        checkpoint_id,
        epoch,
        covered_seq,
        published_at,
    })
}

/// 清掉"本次调用刚建出来"的 vault 文件（主文件 + WAL 边车）与**空**实例目录。
///
/// 只允许在调用方已确认路径原本不存在时使用；失败路径尽力而为，不掩盖原始错误。
///
/// 目录只在"刚被本次初始化创建且为空"时删除（`remove_dir` 对非空目录失败，因此用户
/// 已有内容的目录不会被删）：Create 现在必须**先**建库写 pending、再联网（ADR 0013
/// S2b 的写前记录），所以"联网失败不该留下实例目录"这条既有验收要靠这里兜住。
pub(crate) fn remove_vault_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(sidecar));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

`````



@@SECTION code/023 `crates/sync/src/dev_local_rotation.rs`


`````rust
//! ADR 0009 **Phase 2** 的库级轮换编排（切片 5 的 sync 侧一半）。
//!
//! 本模块只做"把 `crates/sync` 已有的真实轮换步骤按 ADR 0006 §27–§35 / ADR 0009 切片 5
//! 的顺序串成两个可测、无 GPUI 的库入口"，**不新增协议、不改 wire/DDL、不碰 UI/worker**：
//!
//! 1. [`DevLocal::rotate`]（A 侧发起设备）：先同步到最新 → 冻结管理链 + `begin`（revoke
//!    列表进入签名 body）→ 旧根 integrate 到 S → 冻结资产重加密 → 两份独立候选基线 →
//!    候选槽 → 全 active 设备 HPKE grant → 双签 activation T + `activate` → 读回
//!    T/commit/grant → 本地原子推广 → 旧 outbox 重包。发起设备之外无人参与，B 全程可离线；
//! 2. [`DevLocal::apply_latest_epoch`]（B 侧任意设备）：从 `/state`+`/control` 认证 activation
//!    链 → 用**本机 HPKE 私钥**解开自己的 latest grant 得到候选根 → 选中 `/bootstrap` 基线 →
//!    走生产入口 `apply_epoch_change_with_client` 完成认证 merge/推广 → 旧 outbox 重包。
//!
//! ## 与 example 编排的关系
//!
//! 编排顺序照 `crates/sync/examples/e2e_rotation_rounds.rs`（`run_round_body` /
//! `apply_round_for_device`），但**不复制** example 的断言/日志/root oracle：候选根、T、
//! grant、基线全部经真实 relay 读回，apply 侧只看本机 grant 解出的根。
//!
//! ## 本片的明确取舍（不是遗漏）
//!
//! - **不轮换管理 key/恢复码**：推进管理 `rev`（relay v1 要求每次轮换都有管理权转换），
//!   但新旧管理 key 是**同一把**（`identity.manager_seed` 不变，双签由同一 key 各签一次），
//!   因此旧恢复码 R 仍能派生出当前管理 key；恢复码**重发**（新 `R'` 展示/备份）属于后续
//!   切片。候选 recovery 槽必须存在（relay §28.1 / §8 步 5 的硬要求），本片用一次性随机
//!   `R_slot` 包裹候选根：它让 relay 的槽集合完整，但**旧恢复码打不开它**——轮换之后
//!   `DevLocal::join` 仍要靠后续切片的恢复码重发补齐。管理设备（A）在"激活成功、本地
//!   推广失败"后可以用本机 grant 走 [`DevLocal::apply_latest_epoch`] 自愈：只有新管理 key
//!   与本地逐字节相等时才把本地 seed 交给 promotion（promotion 会再验），否则 fail-closed；
//! - **身份推广必须带 identity 路径**：原子推广（临时文件 → fsync → rename）是既有生产
//!   入口 `promote_identity_with_material` / `apply_epoch_change_with_client` 的唯一提交点，
//!   没有路径就只能"半推广"。所以两个入口都要求调用方给出受保护身份 BLOB 的路径
//!   （与 `e2e_common::write_identity` 同一格式：`LocalIdentityV1::protect()` 的原字节）；
//!   app 侧切片 6 接线时需要把 `DevIdentityStore` 的身份文件适配成这条写回路径；
//! - **不新建 resume 机制**：失败一律保留 journal/lease 并返回明确错误；重试/restart 复用
//!   库既有能力（`RotationJournal` 状态机、`RotationApplyPhase` 冻结屏障、repack 屏障）。
//!
//! ## 失败语义
//!
//! - `rotate` 任何阶段失败：journal 留在最后完成的 phase（不静默清除），身份不动
//!   （`promote_identity_with_material` 的 rename 是唯一提交点）；repack 失败时身份**已**
//!   完整推广（不是半推广），`sync.repack_required` 保持为 1 → 引擎继续冻结旧写，重跑
//!   `repack_outbox_after_rotation` 即可（幂等）；
//! - `apply_latest_epoch` 失败：内存/磁盘身份都不动（旧 epoch 可原样重试），
//!   `RotationApplyPhase` 停在 paused/applying（冻结屏障），下一次 apply 会先做认证收尾。

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::checkpoint::StaticAssets;
use crate::dev_local::root_of;
use crate::device::{DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use crate::engine::{EngineIdentity, SyncEngine};
use crate::error::SyncError;
use crate::hpke::open_epoch_grant;
use crate::http_relay::{Clock, ControlPin, HttpRelayClient, StdTcpTransport, SystemClock};
use crate::identity::{IdentityRotationStateV1, LocalIdentityV1};
use crate::keys::RootSecretV1;
use crate::random::{array, OsRandom};
use crate::recovery::RecoverySeed;
use crate::relay::{PauseReason, RelayClient};
use crate::relay_context::{RelayContext, RelayTarget};
use crate::rotation::verify_management_chain_and_pin;
use crate::rotation_activate::{
    activate_http, finish_promotion, verify_latest_grant_root, ActivationInput,
    AuthenticatedActivation, PromotionMaterial,
};
use crate::rotation_apply::{
    activation_commit_materials, apply_epoch_change_with_client,
    apply_epoch_change_with_client_and_manager_seed, verify_activation_commit_matches_t,
    ApplyOutcome as EpochApplyOutcome,
};
use crate::rotation_journal::RotationApplyPhase;
use crate::rotation_journal::{RotationJournal, RotationPhase};
use crate::rotation_repack::{repack_outbox_after_rotation, RepackReport};
use crate::rotation_slots::{
    build_manager_transition, candidate_asset_set_digest, checkpoint_commit_digest,
    frozen_asset_set_digest, generate_candidate_slots_with_recovery_seed, grants_digest,
    plan_all_active_grants, upload_candidate_slots, ActivationTMaterial,
};
use crate::rotation_stage::{
    assert_candidate_ready, assert_independent_baselines, attach_grants, begin_rotation_http,
    capture_baseline_evidence, fetch_frozen_assets,
    prepare_new_epoch_baseline_with_role_and_coverage_root, reencrypt_frozen_assets, stage_grants,
};
use crate::upload::fetch_object;
use crate::DeviceId;
use menghunote_storage::{set_repack_required, sync_cursor_state, Vault};
use menghunote_sync_proto::{ActivationTV1, GrantAadV1, NewManagerV1, ObjectType};

/// `rotate` 的成功事实（全部是公开标识/摘要，**不含**根/管理 seed/恢复码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationOutcome {
    /// 发起设备。
    pub device_id: DeviceId,
    /// 轮换前/后的 epoch（`to_epoch > from_epoch`）。
    pub from_epoch: u64,
    pub to_epoch: u64,
    /// relay 分配的 rotation id。
    pub rotation_id: [u8; 16],
    /// 本次 begin 的幂等 request_id（丢响应时按它查询，绝不盲 begin）。
    pub request_id: [u8; 16],
    /// 冻结水位 S（= begin 返回的 `freeze_seq`）。
    pub freeze_seq: u64,
    /// begin 的 CAS 基准：`/state.control_rev`（= journal.control_rev）。测试/调用方可以
    /// 用它复算被管理签名的 canonical begin body（含 `revoke[]`）。
    pub expected_control_rev: u64,
    /// 激活后的控制链 rev（**不是**管理 key rev）。
    pub control_rev: u64,
    /// 推进入的管理 key rev（管理 key 本身不变）。
    pub manager_rev: u64,
    /// 两份独立候选基线（primary/fallback）。
    pub checkpoint_ids: [[u8; 16]; 2],
    pub covered_seqs: [u64; 2],
    /// staged 到本次轮换的 grant 数（含本机；被吊销设备不在其中）。
    pub grants: usize,
    /// 本次 signed begin body 里的 revoke[]（顺序与调用方一致）。
    pub revoked: Vec<DeviceId>,
    /// `SHA256(CDE(begin canonical body))`：调用方可复算"签名覆盖了哪些设备"。
    pub begin_body_digest: [u8; 32],
    /// relay 读回的 `SHA256(CDE(ActivationCommitV1))`。
    pub activation_commit_hash: [u8; 32],
    /// 本机旧 epoch outbox 的重包结果（正常为 0/0/0）。
    pub repack: RepackReport,
}

/// `apply_latest_epoch` 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// 已完成认证 merge + 身份推广 + 旧 outbox 重包。
    Applied {
        from_epoch: u64,
        to_epoch: u64,
        /// 选中并 merge 的基线。
        checkpoint_id: [u8; 16],
        baseline_s: u64,
        baseline_cursor: String,
        control_rev: u64,
        manager_rev: u64,
        repack: RepackReport,
    },
    /// 本机已在 relay 的最新 epoch（幂等成功；若有中断的 apply/repack 会先认证收尾）。
    AlreadyCurrent { epoch: u64 },
    /// 本机不在目标 epoch 的 grant 列表（被吊销/未授权）：**不回退**旧 epoch、不猜根。
    NotGranted {
        device_id: DeviceId,
        /// 本机身份 epoch（relay 拒绝认证时看不到 active epoch）。
        local_epoch: u64,
    },
    /// 认证/基线/合并失败：保持旧身份与冻结屏障，可原样重试。
    Paused { reason: String },
}

impl crate::dev_local::DevLocal {
    /// A 侧：用本机管理私钥发起一次完整轮换，成功后把身份推广到新 epoch 并重包旧 outbox。
    ///
    /// 前置条件：`identity.manager_seed` 存在（Create/恢复码设备；Join 设备明确拒绝），
    /// 且 `identity` 与 `identity_path` 描述同一份身份。
    ///
    /// > **显式 opt-in（task-123 / ADR 0010）**：这是唯一分配新 `key_epoch`、重写旧 epoch
    /// > 密文并冻结 vault 的入口。**移除设备默认不走这里**——默认移除是
    /// > [`crate::dev_local::DevLocal::revoke_device`]（只删访问凭证，不换 epoch、不重加密），
    /// > 任何"移除失败/成功后自动轮换"的串联都是错误语义。`revoke[]` 里可以（也应当）
    /// > 包含此前已被独立吊销的设备，让 signed revoke 覆盖 status-only 撤销；这类目标不会
    /// > 产生第二次 status 迁移（relay `revoked_count=0`），`revoke[]` 仍是永久签名事实。
    ///
    /// > **签名与任务书的唯一差异**：多了 `identity_path`。没有它就只能做"内存推广"，
    /// > 而原子 rename 是既有生产入口的唯一提交点（半推广是不可接受的）。
    ///
    /// 顺序（任一步失败都**不会**推广身份，journal 留在最后完成的 phase）：
    ///
    /// 1. `sync_once` 到最新（把 outbox 推平，候选覆盖证明只认 durable 事实；遇到
    ///    checkpoint/control 日志事件暂停时继续，与 example 同口径）；
    /// 2. 校验管理身份 + relay 仍在旧 epoch；`RotationJournal::freeze` + `/control` 链尾 pin
    ///    + `POST /rotations`（`revoke` 进入被管理签名的 canonical body）；
    /// 3. `integrate_to_s`：旧根 pull+integrate 到 S（真实阶段 C 门禁，不是跳步）；
    /// 4. 冻结资产全量下载/验真实 SHA/新根重加密；两份互不复用 cid/manifest/snapshot 的
    ///    full-history 候选基线（primary/fallback）并各自读回验证；
    /// 5. 候选 password/recovery 槽 staging + 全 active 设备 HPKE grant（含本机；被吊销
    ///    设备既不在 `/control` active 集合里，也不会收到 grant）；
    /// 6. 旧/新管理 key 双签完整 activation T（本片同一把 key）→ `activate`；
    /// 7. 读回 `/control` 的 T/双签、`ActivationCommitV1`（与 relay 公开 hash 逐字节比对）、
    ///    以及本机 latest grant 解出的根；
    /// 8. `promote_identity_with_material`（rename 是唯一提交点）→ journal Completed+clear；
    /// 9. 用旧根重包本机旧 epoch outbox，成功后清 `sync.repack_required`。
    ///
    /// ## 恢复码（`recovery_code`，ADR 0009 切片 6a）
    ///
    /// 本片**不更换管理 key**（双签用同一把），所以 relay/客户端的管理信任锚仍是原管理
    /// 公钥；能通过管理 realm 认证的恢复码也只有**当前**那一张。`Some(code)` 时这里先校验
    /// `ManagementKey::from_recovery(code) == pinned_manager`，再用该种子去包候选 epoch 的
    /// recovery 槽——轮换后新设备才能用同一恢复码解出新根并加入。`None` 时沿用一次性随机
    /// `R'`：槽集合仍是 relay 契约要求的完整形态，但**没有任何恢复码能打开新槽**（轮换后
    /// 新设备无法用恢复码加入），调用方必须如实告知用户。
    pub fn rotate<T: RelayTarget + ?Sized>(
        relay: &T,
        vault: &Vault,
        identity_path: &Path,
        identity: &mut LocalIdentityV1,
        revoke: &[DeviceId],
        recovery_code: Option<&str>,
    ) -> Result<RotationOutcome, SyncError> {
        // 整段轮换共用一个 context：传输策略/预算/pin 决议不因阶段不同而漂移
        // （ADR 0013 §6「sync/rotate/apply/... 全部接受同一 context」）。
        let context: RelayContext = relay.relay_context(None)?;
        let vault_id = identity.vault_id;
        let device_id = identity.device_id;
        let peer_id = identity.loro_peer_id();
        let manager = require_manager_key(identity)?;
        let old_epoch = identity.current_epoch;
        let old_root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let now = SystemClock.now_secs();
        // 恢复码必须在**任何网络动作/本地写**之前校验：错码/换管理 key 的码一律拒绝。
        let recovery_seed = match recovery_code {
            Some(code) => {
                let seed = RecoverySeed::from_code(code)?;
                if ManagementKey::from_recovery(&seed, &vault_id)?.public_key()
                    != identity.pinned_manager
                {
                    return Err(SyncError::Unsupported(
                        "轮换不更换管理密钥：提供的恢复码与当前管理身份不符（请用当前恢复码）",
                    ));
                }
                Some(seed)
            }
            None => None,
        };

        // 1) 先把本地 durable 状态与 relay 拉平：候选基线的覆盖证明只认 durable 事实。
        match Self::sync_once(&context, vault, identity) {
            Ok(_) => {}
            // 与 example 同口径：日志里出现本任务范围外的事件时暂停是可接受的
            // （asset/cursor 事实已 durable 落盘），覆盖由阶段 C 门禁再验一次。
            Err(SyncError::Paused(PauseReason::UnsupportedEvent)) => {}
            Err(error) => return Err(error),
        }

        let mut client = context.open(vault, identity)?.into_client();
        let state = client.state()?;
        if state.vault_id != vault_id {
            return Err(SyncError::RelayWire("relay /state 的 vault 与本地身份不符"));
        }
        if state.instance_id != identity.pinned_instance {
            return Err(SyncError::RelayWire("relay instance 与本地 pin 不符"));
        }
        if state.epoch != old_epoch {
            return Err(SyncError::Unsupported(
                "relay 已不在本地 epoch：请先 apply 到 relay 最新 epoch，再发起下一次轮换",
            ));
        }
        let local_manager_rev = local_manager_rev(vault, identity)?;
        let new_manager_rev = local_manager_rev
            .checked_add(1)
            .ok_or(SyncError::Object("管理 rev 溢出"))?;

        // 2) 冻结 + 阶段 A（管理链逐环复算 + pin 已验证链尾）。
        let mut journal = RotationJournal::freeze(
            vault,
            old_epoch,
            array(&mut OsRandom)?,
            state.control_rev,
            [0u8; 32],
        )?;
        // begin 的硬门槛是 `r_prime_backed_up`：`Some(code)` 时 R' 就是用户手上已有的
        // 恢复码（已备份）；`None` 时 R' 是一次性随机种子（没人持有），这里只为满足
        // relay 契约置位——调用方必须在报告里明确警告"轮换后新设备无法用恢复码加入"。
        journal.confirm_r_prime_backed_up(vault)?;
        let begin_request_id = journal.request_id;
        let _ = client.control_head()?;
        let control_pin: ControlPin = client.control_head_pin().ok_or(SyncError::RelayWire(
            "控制链链尾 pin 缺失（/control 未验证）",
        ))?;
        verify_management_chain_and_pin(
            vault,
            &mut client,
            &mut journal,
            vault_id,
            &manager,
            // 本片永不轮换管理 key：身份 pin 的管理公钥就是 genesis 信任锚。
            Some(identity.pinned_manager),
            control_pin,
        )?;

        // 3) 阶段 B：管理 realm begin（revoke[] 进入被签名 body；auth_rev 前进）。
        let revoke_bytes: Vec<[u8; 16]> = revoke.to_vec();
        let api = context.api();
        let begin = begin_rotation_http(
            &mut context.transport()?,
            &api,
            vault,
            &mut journal,
            vault_id,
            &state.instance_id,
            state.log_id,
            &manager,
            &revoke_bytes,
            now,
        )?;
        client.clear_auth_cache();
        if begin.from_epoch != old_epoch || begin.to_epoch <= old_epoch {
            return Err(SyncError::RelayWire("begin 返回的 epoch 与本地不符"));
        }

        // 4) 阶段 C：旧根 integrate 到 S（门禁只拦 ≤S 的缺口与已上传未 ACK）。
        {
            let engine_identity = EngineIdentity {
                device_id,
                signing_key: signing_key.clone(),
                peer_id,
            };
            let local_cut = sync_cursor_state(vault)?.integrated_seq;
            let mut engine = SyncEngine::new(vault, &mut client, engine_identity, old_root.clone());
            crate::rotation::integrate_to_s(
                vault,
                &mut engine,
                &mut journal,
                begin.freeze_seq,
                local_cut,
            )?;
        }

        // 5) 阶段 D：冻结资产（含墓碑/无引用 finalized）全部取回（旧根）→ 新根重加密。
        let (frozen, source_digest, _total) =
            fetch_frozen_assets(&mut client, begin.rotation_id, 1)?;
        let mut all_assets = Vec::with_capacity(frozen.len());
        for asset in &frozen {
            let (_, payload) = fetch_object(
                &mut client,
                &old_root,
                ObjectType::Asset,
                &asset.object_id,
                asset.part_count,
                asset.total_plain_len,
                &asset.object_digest,
                &[],
            )?;
            let (_real, _byte_len, media_type) =
                crate::rotation_stage::decode_asset_plaintext(&payload)?;
            all_assets.push((
                media_type,
                crate::rotation_stage::payload_asset_data(&payload)?.to_vec(),
            ));
        }
        all_assets.sort_by_key(|(_, data)| {
            let digest: [u8; 32] = Sha256::digest(data).into();
            digest
        });
        let assets = StaticAssets::new(all_assets)?;
        let new_root = RootSecretV1::generate(vault_id, begin.to_epoch, &mut OsRandom)?;
        // 候选根/管理 rev 必须在**第一次上传候选对象之前**落进 journal（DPAPI 保护）：
        // 崩在阶段 E/F 之间时这是唯一的恢复线索，也保证重启后重试不会换一个候选根。
        journal.candidate_root = Some(new_root.clone());
        journal.new_manager_public = Some(manager.public_key());
        journal.new_manager_rev = new_manager_rev;
        journal.save(vault)?;

        // 6) 阶段 E/F：两份独立候选基线 + 候选槽 + 全设备 grants。
        let first = prepare_new_epoch_baseline_with_role_and_coverage_root(
            vault,
            &mut client,
            vault_id,
            device_id,
            signing_key.clone(),
            new_root.clone(),
            peer_id,
            &assets,
            Some("primary"),
            &old_root,
        )?;
        let reservation =
            client
                .rotation(begin.rotation_id)?
                .reservation_id
                .ok_or(SyncError::RelayWire(
                    "候选 checkpoint 未返回 maintenance reservation",
                ))?;
        // dev-local 没有用户口令：候选 password 槽用一次性口令（不出进程、不落盘），
        // 与 `create` 的 setup 口令同口径；恢复槽用调用方给的恢复种子（`None` = 一次性
        // 随机，轮换后没人能用恢复码打开它，见函数文档）。
        let candidate_password = one_shot_password()?;
        let slots = generate_candidate_slots_with_recovery_seed(
            vault_id,
            begin.to_epoch,
            &candidate_password,
            new_manager_rev,
            now,
            device_id,
            &new_root,
            recovery_seed.as_ref(),
            &mut OsRandom,
        )?;
        // 自洽断言：指定恢复码时必须仍能派生当前管理 key（否则上面已拒绝）。
        if recovery_seed.is_some() && slots.new_manager.public_key() != manager.public_key() {
            return Err(SyncError::Object(
                "候选恢复槽派生的管理 key 与当前管理身份不一致",
            ));
        }
        let slot_digest = upload_candidate_slots(
            &mut client,
            vault_id,
            device_id,
            &signing_key,
            &new_root,
            &slots,
            &candidate_password,
            &reservation,
        )?;
        let (reencrypted, _, _) = reencrypt_frozen_assets(
            vault,
            &mut client,
            vault_id,
            device_id,
            &signing_key,
            &old_root,
            &new_root,
            begin.rotation_id,
            1,
            Some(&reservation),
        )?;
        journal.advance(RotationPhase::Reencrypted)?;
        journal.save(vault)?;
        let second = prepare_new_epoch_baseline_with_role_and_coverage_root(
            vault,
            &mut client,
            vault_id,
            device_id,
            signing_key.clone(),
            new_root.clone(),
            peer_id,
            &assets,
            Some("fallback"),
            &old_root,
        )?;
        let frozen_hashes: Vec<[u8; 32]> =
            reencrypted.iter().map(|asset| asset.real_sha256).collect();
        let evidence1 = capture_baseline_evidence(&mut client, &new_root, &first, &frozen_hashes)?;
        let evidence2 = capture_baseline_evidence(&mut client, &new_root, &second, &frozen_hashes)?;
        assert_independent_baselines(&evidence1, &evidence2, begin.freeze_seq)?;
        assert_candidate_ready(
            &mut client,
            first.report.checkpoint_id,
            "primary",
            begin.freeze_seq,
        )?;
        assert_candidate_ready(
            &mut client,
            second.report.checkpoint_id,
            "fallback",
            begin.freeze_seq,
        )?;
        journal.advance(RotationPhase::BaselinesReady)?;
        journal.save(vault)?;

        let grants = plan_all_active_grants(&mut client, &new_root, &manager, local_manager_rev)?;
        if grants.is_empty() {
            return Err(SyncError::Unsupported("控制链没有 active 设备可发 grant"));
        }
        attach_grants(&mut journal, vault, &grants)?;
        stage_grants(
            &mut context.transport()?,
            &context.api(),
            vault_id,
            begin.rotation_id,
            &manager,
            &grants,
            now,
        )?;
        journal.advance(RotationPhase::GrantsReady)?;
        journal.save(vault)?;

        // 7) 阶段 G：完整 activation T（20 字段）+ 旧/新管理双签 + activate。
        let control_rev = client.rotation(begin.rotation_id)?.control_rev;
        let member_digest = client.active_member_cert_digest()?;
        let grant_digest = grants_digest(&grants)?;
        let old_asset_set_digest = frozen_asset_set_digest(&frozen)?;
        if Some(old_asset_set_digest) != source_digest {
            return Err(SyncError::Object(
                "冻结资产集合本地复算与 relay source_digest 不一致",
            ));
        }
        let new_asset_set_digest = candidate_asset_set_digest(begin.to_epoch, &reencrypted)?;
        // 激活前把四个摘要/成员/grants 记进 journal（恢复对账用，公开事实）。
        journal.member_set_digest = Some(member_digest);
        journal.grants_digest = Some(grant_digest);
        journal.slot_set_digest = Some(slot_digest);
        journal.asset_set_digest = Some(old_asset_set_digest);
        journal.save(vault)?;
        let (head_rev, prev_control_hash) = client.control_head()?;
        if head_rev != control_rev {
            return Err(SyncError::RelayWire(
                "控制链链尾 rev 与 rotation view 的 control_rev 不一致",
            ));
        }
        let activation_material = ActivationTMaterial {
            vault_id,
            instance_id: state.instance_id,
            rotation_id: begin.rotation_id,
            from_epoch: begin.from_epoch,
            to_epoch: begin.to_epoch,
            freeze_seq: begin.freeze_seq,
            old_manager_rev: local_manager_rev,
            new_manager_rev,
            new_manager_public: manager.public_key(),
            prev_control_hash,
            expected_control_rev: control_rev + 1,
            primary_checkpoint: first.report.checkpoint_id,
            primary_commit_digest: checkpoint_commit_digest(&first.report)?,
            fallback_checkpoint: second.report.checkpoint_id,
            fallback_commit_digest: checkpoint_commit_digest(&second.report)?,
            slot_set_digest: slot_digest,
            member_cert_digest: member_digest,
            grants_digest: grant_digest,
            old_asset_set_digest,
            new_asset_set_digest,
        };
        // 管理 key 不变：transition 的新公钥 = 旧公钥，双签由同一把 key 各签一次。
        let transition = build_manager_transition(&activation_material, &manager, &manager)?;
        let input = ActivationInput {
            rotation_id: begin.rotation_id,
            request_id: array(&mut OsRandom)?,
            expected_control_rev: control_rev,
            primary_checkpoint: first.report.checkpoint_id,
            fallback_checkpoint: second.report.checkpoint_id,
            member_cert_digest: member_digest,
            slot_set_digest: slot_digest,
            grants_digest: grant_digest,
            asset_set_digest: old_asset_set_digest,
            new_manager: Some(transition.clone()),
            candidate_root: new_root.clone(),
            new_manager_public: manager.public_key(),
            new_manager_rev,
            pv: 1,
        };
        let activated = activate_http(
            &mut context.transport()?,
            &context.api(),
            vault_id,
            &state.instance_id,
            &manager,
            &input,
            now,
        )?;
        if activated.epoch != begin.to_epoch {
            return Err(SyncError::RelayWire("activate 返回的 epoch 与 begin 不符"));
        }
        journal.advance(RotationPhase::Activated)?;
        journal.save(vault)?;

        // 8) 读回授权材料：T/双签、ActivationCommit（对 relay 公开 hash）、本机 grant 解根。
        let commit_hash = verify_activation_readback(
            &mut client,
            vault_id,
            state.instance_id,
            begin.rotation_id,
            &transition,
            activated.control_rev,
        )?;
        let hpke_key = DeviceHpkeKey::from_secret(identity.x25519_secret.clone());
        verify_latest_grant_root(
            &mut context.transport()?,
            &context.api(),
            vault_id,
            device_id,
            &signing_key,
            &hpke_key,
            &new_root,
            now,
        )?;

        // 9) 阶段 H：本地原子推广（rename 是唯一提交点）+ journal 收尾 + 阶段 J 重包。
        let old_roots = old_roots_for_repack(vault, identity)?;
        let material = PromotionMaterial {
            candidate_root: new_root.clone(),
            // 管理 key 不变：沿用同一 seed，pin 的公钥与 rev 一起前进一格。
            new_manager_seed: identity.manager_seed.clone(),
            new_manager_public: manager.public_key(),
            new_manager_rev,
            control_rev: activated.control_rev,
            authentication: Some(AuthenticatedActivation {
                instance_id: state.instance_id,
                control_rev: activated.control_rev,
                activation_hash: commit_hash,
            }),
        };
        let promoted = crate::rotation_activate::promote_identity_with_material(
            vault,
            identity_path,
            identity,
            &material,
        )?;
        if promoted.current_epoch != begin.to_epoch
            || !promoted.current_root.ct_eq(&new_root.root)
            || promoted.pinned_manager != manager.public_key()
        {
            return Err(SyncError::Object("推广后的身份 epoch/root/管理 pin 不符"));
        }
        // 身份推广的 durable 提交点（rename）已经发生：内存身份必须**立刻**与磁盘一致，
        // 否则后续重包失败时调用方会拿旧 epoch 的内存身份去重试（磁盘却已是新 epoch）。
        *identity = promoted;
        finish_promotion(&mut journal, vault)?;
        RotationJournal::clear(vault)?;

        let repack = repack_outbox_after_rotation(vault, &old_roots, &new_root, device_id)?;
        if repack.failed != 0 || repack.conflicts != 0 {
            return Err(SyncError::LocalCorrupt(
                "轮换后本机 outbox 重包存在失败/冲突行：身份已完整推广，repack 屏障保持为 1，\
                 请重跑 repack 后再同步",
            ));
        }
        set_repack_required(vault, false)?;

        // 签名事实（调用方/测试可复算；不含秘密）。
        let begin_body = crate::rotation::begin_body(
            activation_material.from_epoch,
            state.control_rev,
            &begin_request_id,
            &revoke_bytes,
        )?;
        let begin_body_digest: [u8; 32] = Sha256::digest(&begin_body).into();

        Ok(RotationOutcome {
            device_id,
            from_epoch: begin.from_epoch,
            to_epoch: begin.to_epoch,
            rotation_id: begin.rotation_id,
            request_id: begin_request_id,
            freeze_seq: begin.freeze_seq,
            expected_control_rev: state.control_rev,
            control_rev: activated.control_rev,
            manager_rev: new_manager_rev,
            checkpoint_ids: [first.report.checkpoint_id, second.report.checkpoint_id],
            covered_seqs: [first.report.covered_seq, second.report.covered_seq],
            grants: grants.len(),
            revoked: revoke.to_vec(),
            begin_body_digest,
            activation_commit_hash: commit_hash,
            repack,
        })
    }

    /// B 侧：把本机身份推进到 relay 的最新 epoch（认证 apply + 推广 + 重包）。
    ///
    /// 顺序：
    ///
    /// 1. `/state` 认证读取；`epoch < local` 明确拒绝（不静默回退），`= local` 幂等成功
    ///    （若上次 apply/repack 中断，先做认证收尾，**不**重复推广）；
    /// 2. 从 `/control` 复算 latest activation T（旧/新管理双签），得到授权的新管理 key/rev；
    ///    链/T/commit 的逐项绑定由生产入口 `apply_epoch_change_with_client` 再验一遍；
    /// 3. `GET /devices/self/grants/latest`：grant 必须属于目标 epoch、recipient 是本机
    ///    X25519 公钥，并用**本机 HPKE 私钥**解出候选根（不接收 A 的 root oracle）；
    ///    不在 grant 列表/被吊销 → [`ApplyOutcome::NotGranted`]，绝不回退旧 epoch；
    /// 4. 选中 `/bootstrap` 主基线（认证 commit/生产者证书/manifest 由生产入口完成）→
    ///    `apply_epoch_change_with_client`：认证链 + baseline merge + typed receipt +
    ///    atomic promotion + `applied`；
    /// 5. 用旧根重包旧 epoch outbox，成功后清 repack 屏障；`*identity` 用磁盘身份刷新。
    ///
    /// > **签名与任务书的唯一差异**：多了 `identity_path`（原子推广的唯一提交点）。
    pub fn apply_latest_epoch<T: RelayTarget + ?Sized>(
        relay: &T,
        vault: &Vault,
        identity_path: &Path,
        identity: &mut LocalIdentityV1,
    ) -> Result<ApplyOutcome, SyncError> {
        let context: RelayContext = relay.relay_context(None)?;
        let vault_id = identity.vault_id;
        let device_id = identity.device_id;
        let old_epoch = identity.current_epoch;
        let old_root = root_of(identity);
        let mut client = context.open(vault, identity)?.into_client();
        let state = match client.state() {
            Ok(state) => state,
            Err(error) if is_revoked(&error) => {
                return Ok(ApplyOutcome::NotGranted {
                    device_id,
                    local_epoch: old_epoch,
                })
            }
            Err(error) => return Err(error),
        };
        if state.vault_id != vault_id {
            return Err(SyncError::RelayWire("relay /state 的 vault 与本地身份不符"));
        }
        if state.instance_id != identity.pinned_instance {
            return Err(SyncError::RelayWire("relay instance 与本地 pin 不符"));
        }
        if state.epoch < old_epoch {
            return Err(SyncError::Unsupported(
                "relay epoch 低于本地身份：拒绝回退旧 epoch（需人工排查/重建）",
            ));
        }
        if state.epoch == old_epoch {
            // 幂等：先做中断 apply 的认证收尾（receipt 与磁盘身份逐字段比对），再幂等重包。
            if let Some(outcome) =
                crate::rotation_apply::reconcile_same_epoch_apply(vault, identity_path, identity)?
            {
                match outcome {
                    EpochApplyOutcome::Applied { .. } => {}
                    EpochApplyOutcome::Paused { reason } => {
                        return Ok(ApplyOutcome::Paused { reason })
                    }
                    other => {
                        return Ok(ApplyOutcome::Paused {
                            reason: format!("apply 收尾未完成：{other:?}"),
                        })
                    }
                }
            }
            finish_pending_repack(vault, identity, &old_root)?;
            return Ok(ApplyOutcome::AlreadyCurrent { epoch: state.epoch });
        }

        // 1) 认证的 latest activation T（链尾）：授权的新管理 key/rev。
        let view = match state.current_rotation_id {
            Some(rotation_id) => client.rotation(rotation_id)?,
            None => client.current_rotation()?.ok_or(SyncError::Unsupported(
                "relay epoch 已前进但找不到 rotation 视图（无法认证 apply）",
            ))?,
        };
        let evidence =
            client
                .verified_activation_t(&view.rotation_id)?
                .ok_or(SyncError::Unsupported(
                    "缺少可复算的 activation T（/control 无 T 或双签未通过）",
                ))?;
        let activation_t = evidence.activation_t.clone();
        if activation_t.to_epoch != state.epoch {
            return Err(SyncError::RelayWire(
                "链尾 activation T 的 to_epoch 与 relay /state 不符",
            ));
        }

        // 2) 本机 grant：不在列表/被吊销 → NotGranted（不回退、不猜根）。
        let candidate_root = match open_self_grant_root(&mut client, identity, state.epoch) {
            Ok(root) => root,
            Err(GrantFailure::Revoked) => {
                return Ok(ApplyOutcome::NotGranted {
                    device_id,
                    local_epoch: old_epoch,
                })
            }
            Err(GrantFailure::NotForEpoch { epoch }) => {
                return Ok(ApplyOutcome::NotGranted {
                    device_id,
                    local_epoch: epoch,
                })
            }
            Err(GrantFailure::Error(error)) => return Err(error),
        };

        // 3) 选中基线：relay 只把最新两份 published 作为 bootstrap 候选。
        let bundle = match client.bootstrap() {
            Ok(bundle) => bundle,
            Err(error @ SyncError::Transport(_)) => return Err(error),
            Err(error) => {
                return Ok(ApplyOutcome::Paused {
                    reason: format!("无法取得 relay /bootstrap 主/备基线：{error}"),
                })
            }
        };
        if bundle.epoch != state.epoch || bundle.primary.epoch != state.epoch {
            return Ok(ApplyOutcome::Paused {
                reason: "bootstrap 的 epoch 与 activation 目标 epoch 不符".to_owned(),
            });
        }
        let checkpoint = bundle.primary.clone();

        // 4) 生产入口：认证 activation 链 + 合并基线 + 推广（候选根只用本机解出的）。
        //
        // 管理设备（A）在"激活已成功、本地推广失败"之后也必须能自愈，否则它永久停在旧
        // epoch（journal 只能清除、不能重激活）。**只**在新管理 key 与本地管理 key 逐字节
        // 相等时（本片管理 key 不变）才把本地 seed 交给 promotion；否则保持 `None`
        // fail-closed（不许拿旧 seed 冒充新管理权，promotion 会再验一次）。
        let manager_seed = identity.manager_seed.clone().filter(|seed| {
            ManagementKey::from_seed(seed.clone()).public_key() == activation_t.new_manager_pubkey
        });
        let applied = match manager_seed {
            Some(seed) => apply_epoch_change_with_client_and_manager_seed(
                &mut client,
                vault,
                identity_path,
                identity,
                checkpoint.checkpoint_id,
                checkpoint.covered_seq,
                checkpoint.cursor.as_str(),
                &candidate_root,
                identity.pinned_manager,
                activation_t.new_manager_pubkey,
                u64::from(activation_t.new_manager_rev),
                None,
                seed,
            )?,
            None => apply_epoch_change_with_client(
                &mut client,
                vault,
                identity_path,
                identity,
                checkpoint.checkpoint_id,
                checkpoint.covered_seq,
                checkpoint.cursor.as_str(),
                &candidate_root,
                identity.pinned_manager,
                activation_t.new_manager_pubkey,
                u64::from(activation_t.new_manager_rev),
                None,
            )?,
        };
        let baseline_s = match applied {
            EpochApplyOutcome::Applied {
                baseline_s,
                promoted,
            } => {
                if !promoted {
                    return Ok(ApplyOutcome::Paused {
                        reason: "apply 合并成功但未推广身份".to_owned(),
                    });
                }
                baseline_s
            }
            EpochApplyOutcome::Paused { reason } => return Ok(ApplyOutcome::Paused { reason }),
            EpochApplyOutcome::Conflict => {
                return Ok(ApplyOutcome::Paused {
                    reason: "baseline cursor CAS 冲突（保持冻结，可重试）".to_owned(),
                })
            }
            EpochApplyOutcome::NeedsRestart { to_epoch, .. } => {
                return Ok(ApplyOutcome::Paused {
                    reason: format!("需要走 restart 到更高 epoch {to_epoch}"),
                })
            }
        };
        if baseline_s != checkpoint.covered_seq {
            return Err(SyncError::RelayWire("applied S 与所选基线 S 不一致"));
        }

        // 5) 磁盘身份才是 durable 真源：重新解封核对，再重包/替换内存身份。
        let promoted = load_identity(identity_path, vault_id, device_id)?;
        if promoted.current_epoch != state.epoch
            || !promoted.current_root.ct_eq(&candidate_root.root)
            || promoted.pinned_manager != activation_t.new_manager_pubkey
            || promoted.control_rev != activation_t.expected_control_rev
            || promoted.pending_rotation.is_some()
        {
            return Err(SyncError::Object(
                "apply 推广后的磁盘身份 epoch/root/pin 与授权材料不符",
            ));
        }
        // 推广已由 rename 提交：先让内存身份与磁盘一致，再做（可失败的）重包。
        let repack_outcome =
            repack_after_promotion(vault, identity, &promoted, &candidate_root, device_id);
        let control_rev = promoted.control_rev;
        let manager_rev = u64::from(activation_t.new_manager_rev);
        *identity = promoted;
        let repack = repack_outcome?;
        Ok(ApplyOutcome::Applied {
            from_epoch: old_epoch,
            to_epoch: state.epoch,
            checkpoint_id: checkpoint.checkpoint_id,
            baseline_s,
            baseline_cursor: checkpoint.cursor.clone(),
            control_rev,
            manager_rev,
            repack,
        })
    }
}

// ---------------------------------------------------------------- 内部助手

/// 一次 apply 里"本机 grant 不可用"的原因（区分 Revoked/错 epoch 与普通错误）。
enum GrantFailure {
    Revoked,
    NotForEpoch { epoch: u64 },
    Error(SyncError),
}

fn is_revoked(error: &SyncError) -> bool {
    matches!(error, SyncError::RelayApi { code, .. } if code == "REVOKED")
}

/// 管理设备才能发起轮换：`manager_seed` 必须存在且与 `pinned_manager` 自洽。
fn require_manager_key(identity: &LocalIdentityV1) -> Result<ManagementKey, SyncError> {
    let Some(seed) = identity.manager_seed.clone() else {
        return Err(SyncError::Unsupported(
            "轮换需要管理私钥（Create/恢复码设备）：Join 设备不能发起轮换",
        ));
    };
    let manager = ManagementKey::from_seed(seed);
    if manager.public_key() != identity.pinned_manager {
        return Err(SyncError::LocalCorrupt(
            "身份的管理私钥与 pinned_manager 不一致（身份文件被换过或损坏）",
        ));
    }
    Ok(manager)
}

/// 本地管理 rev：与 apply 侧锚定 activation 链首的口径一致（旁路状态 → genesis）。
pub(crate) fn local_manager_rev(
    vault: &Vault,
    identity: &LocalIdentityV1,
) -> Result<u64, SyncError> {
    match IdentityRotationStateV1::load_current(vault, identity)? {
        Some(state) => Ok(state.manager_rev),
        None if identity.current_epoch == crate::genesis::GENESIS_EPOCH => {
            Ok(crate::genesis::GENESIS_MANAGER_REV)
        }
        None => Err(SyncError::LocalCorrupt(
            "缺少身份旁路状态，无法确定管理 rev（epoch 已不在 genesis）",
        )),
    }
}

/// dev-local 的一次性候选口令（不上传用户口令，也不落盘）。
fn one_shot_password() -> Result<String, SyncError> {
    let bytes: [u8; 32] = array(&mut OsRandom)?;
    Ok(crate::ids::hex_of(&bytes))
}

/// 重包旧 outbox 需要的全部旧根（更早的历史根 + 当前根）。
fn old_roots_for_repack(
    vault: &Vault,
    identity: &LocalIdentityV1,
) -> Result<Vec<RootSecretV1>, SyncError> {
    let mut roots = Vec::new();
    if let Some(state) = IdentityRotationStateV1::load_current(vault, identity)? {
        for entry in &state.previous_roots {
            roots.push(RootSecretV1::new(
                identity.vault_id,
                entry.epoch,
                entry.root.clone(),
            ));
        }
    }
    roots.push(root_of(identity));
    roots.sort_by_key(|root| root.key_epoch);
    roots.dedup_by_key(|root| root.key_epoch);
    Ok(roots)
}

/// 阶段 G 读回：`/control` 的 activation T/双签必须与本地发出一致，`ActivationCommitV1`
/// 必须与 relay 公开的 32B hash 逐字节相等（不把 hash 当 CDE 解码）。
fn verify_activation_readback(
    client: &mut HttpRelayClient<StdTcpTransport>,
    vault_id: crate::VaultId,
    instance_id: [u8; 16],
    rotation_id: [u8; 16],
    transition: &NewManagerV1,
    activated_control_rev: u64,
) -> Result<[u8; 32], SyncError> {
    client.clear_auth_cache();
    let local_t = ActivationTV1::decode(
        transition
            .activation_t
            .as_deref()
            .ok_or(SyncError::LocalCorrupt("本地 NewManagerV1 缺 activation T"))?,
    )?;
    let evidence = client
        .verified_activation_t(&rotation_id)?
        .ok_or(SyncError::RelayWire(
            "relay /control 未携带可复算的 activation T",
        ))?;
    if evidence.activation_t != local_t {
        return Err(SyncError::RelayWire(
            "/control 的 activation T 与本地发出的不一致",
        ));
    }
    if evidence.activation_t.expected_control_rev != activated_control_rev {
        return Err(SyncError::RelayWire(
            "activation T 的 expected_control_rev 与 activate 回执不符",
        ));
    }
    let view = client.rotation(rotation_id)?;
    let primary = client.checkpoint_status(view.primary_checkpoint.ok_or(
        SyncError::RelayWire("激活后的 rotation view 缺 primary cid"),
    )?)?;
    let fallback = client.checkpoint_status(view.fallback_checkpoint.ok_or(
        SyncError::RelayWire("激活后的 rotation view 缺 fallback cid"),
    )?)?;
    let commit = activation_commit_materials(vault_id, instance_id, &view, &primary, &fallback)?;
    let commit_hash: [u8; 32] = Sha256::digest(commit.encode()?).into();
    if view.activation_commit.as_deref() != Some(commit_hash.as_slice()) {
        return Err(SyncError::RelayWire(
            "重建的 ActivationCommitV1 与 relay 公开 hash 不等",
        ));
    }
    verify_activation_commit_matches_t(&commit, &evidence.activation_t)?;
    Ok(commit_hash)
}

/// 用本机 HPKE 私钥解开目标 epoch 的 latest grant，得到候选根。
///
/// 只信 relay 的 grant 密文 + 本机 X25519 私钥；AAD 的 recipient/epoch 任一不符都按
/// "本机不在本次 grant 列表"处理（`NotGranted`），绝不用 relay/A 自报的根兜底。
fn open_self_grant_root<C: RelayClient>(
    client: &mut C,
    identity: &LocalIdentityV1,
    to_epoch: u64,
) -> Result<RootSecretV1, GrantFailure> {
    let hpke_key = DeviceHpkeKey::from_secret(identity.x25519_secret.clone());
    let grant = match client.self_grant_evidence() {
        Ok(grant) => grant,
        Err(error) if is_revoked(&error) => return Err(GrantFailure::Revoked),
        Err(error) => return Err(GrantFailure::Error(error)),
    };
    if grant.device_id != identity.device_id || grant.epoch != to_epoch {
        return Err(GrantFailure::NotForEpoch { epoch: grant.epoch });
    }
    if grant.signed_header.len() < 64 {
        return Err(GrantFailure::Error(SyncError::RelayWire(
            "latest grant 的 signed_header 过短",
        )));
    }
    let aad = GrantAadV1::decode(&grant.signed_header[..grant.signed_header.len() - 64])
        .map_err(|error| GrantFailure::Error(SyncError::Proto(error)))?;
    if aad.recipient_device_id != identity.device_id
        || aad.recipient_x25519_pub != hpke_key.public_key()
        || aad.epoch != to_epoch
    {
        return Err(GrantFailure::NotForEpoch { epoch: aad.epoch });
    }
    let opened = open_epoch_grant(hpke_key.secret(), &grant.hpke_enc, &aad, &grant.ciphertext)
        .map_err(GrantFailure::Error)?;
    if opened.vault_id != identity.vault_id || opened.key_epoch != to_epoch {
        return Err(GrantFailure::Error(SyncError::Object(
            "本机 grant 解出的根归属/epoch 与目标不符",
        )));
    }
    Ok(opened)
}

/// 推广后重包（旧根集合来自**推广后**的旁路状态；失败保留旧行与 repack 屏障）。
fn repack_after_promotion(
    vault: &Vault,
    pre_apply: &LocalIdentityV1,
    promoted: &LocalIdentityV1,
    new_root: &RootSecretV1,
    device_id: DeviceId,
) -> Result<RepackReport, SyncError> {
    let mut roots = Vec::new();
    if let Some(state) = IdentityRotationStateV1::load_current(vault, promoted)? {
        for entry in &state.previous_roots {
            roots.push(RootSecretV1::new(
                promoted.vault_id,
                entry.epoch,
                entry.root.clone(),
            ));
        }
    }
    if roots.is_empty() {
        roots.push(root_of(pre_apply));
    }
    roots.sort_by_key(|root| root.key_epoch);
    roots.dedup_by_key(|root| root.key_epoch);
    let repack = repack_outbox_after_rotation(vault, &roots, new_root, device_id)?;
    if repack.failed != 0 || repack.conflicts != 0 {
        return Err(SyncError::LocalCorrupt(
            "apply 后旧 outbox 重包存在失败/冲突行：身份已完整推广，repack 屏障保持为 1，\
             请重跑 repack 后再同步",
        ));
    }
    set_repack_required(vault, false)?;
    Ok(repack)
}

/// `AlreadyCurrent` 的幂等收尾：若 repack 屏障仍在，用现有能力把它跑完（没有旧行时是空操作）。
fn finish_pending_repack(
    vault: &Vault,
    identity: &LocalIdentityV1,
    current_root: &RootSecretV1,
) -> Result<(), SyncError> {
    if !menghunote_storage::repack_required(vault)? {
        return Ok(());
    }
    let roots = old_roots_for_repack(vault, identity)?;
    let repack = repack_outbox_after_rotation(vault, &roots, current_root, identity.device_id)?;
    if repack.failed != 0 || repack.conflicts != 0 {
        return Err(SyncError::LocalCorrupt(
            "中断的轮换重包仍存在失败/冲突行（repack 屏障保持为 1）",
        ));
    }
    set_repack_required(vault, false)?;
    Ok(())
}

/// 磁盘身份读回（apply 推广后 `*identity` 必须与磁盘一致；磁盘是 durable 真源）。
fn load_identity(
    identity_path: &Path,
    vault_id: crate::VaultId,
    device_id: DeviceId,
) -> Result<LocalIdentityV1, SyncError> {
    let blob = std::fs::read(identity_path)
        .map_err(|error| SyncError::Io(format!("读取受保护身份失败: {error}")))?;
    LocalIdentityV1::unprotect(&blob, &vault_id, &device_id)
}

/// apply 的 durable phase（`None` = 从未 apply；供调用方/测试判定是否半途）。
pub fn apply_phase(vault: &Vault) -> Result<Option<RotationApplyPhase>, SyncError> {
    RotationApplyPhase::load(vault)
}

`````



@@SECTION code/024 `crates/sync/src/device.rs`


`````rust
//! 设备身份与证书（ADR 0006 §4）。
//!
//! - **签名身份**：Ed25519（RFC 8032），种子 32B，私钥不 Debug/不日志；
//! - **HPKE 身份**：X25519，32B 私钥材料经 RFC 9180 `DeriveKeyPair` 得到 keypair；
//! - **`DeviceCertV1`**：wire 类型来自 `menghunote-sync-proto`，由**管理 key** 签名
//!   （不是 vault root 派生——ADR §4：管理权不是 rights 里的 flag）；
//! - **管理 key 独立**：`management_ed25519_seed = HKDF(R, vault_id, b"MN/v1/management-ed25519\0")`，
//!   只有恢复码持有者能派生；普通根持有者不能据此注册/吊销设备；
//! - **邀请令牌**：`T = random32`，relay 只存 `SHA256(T)`，24h 一次性；`redeem` 是纯函数
//!   （不改 IO，只返回新状态）；
//! - **设备指纹**：`SHA256(vault_id || device_id || sign_pk || hpke_pk)` 前 128 bit，
//!   双端显示为 32 位小写 hex，用于人工比对（ADR §4）。
//!
//! 证书签名输入的域前缀见 [`crate::domain::DEVICE_CERT`]（ADR 未冻结，已在实现回报标注）。

use crate::error::SyncError;
use crate::hpke;
use crate::ids::hex_of;
use crate::keys::Secret32;
use crate::random::RandomSource;
use crate::recovery::{self, RecoverySeed};
use crate::{DeviceId, VaultId};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use menghunote_sync_proto::{DeviceCertV1, Ed25519PublicKey, Rights, X25519PublicKey};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Ed25519 签名私钥（32 字节种子）。
#[derive(Clone)]
pub struct DeviceSigningKey(Secret32);

impl DeviceSigningKey {
    /// 生成新的随机签名密钥。
    pub fn generate(rng: &mut impl RandomSource) -> Result<Self, SyncError> {
        Ok(Self(Secret32::generate(rng)?))
    }

    /// 从已有种子构造。
    #[must_use]
    pub fn from_seed(seed: Secret32) -> Self {
        Self(seed)
    }

    /// 取种子（DPAPI 身份 BLOB 需要持久化它）。
    #[must_use]
    pub fn seed(&self) -> &Secret32 {
        &self.0
    }

    /// Ed25519 公钥（32B）。
    #[must_use]
    pub fn public_key(&self) -> Ed25519PublicKey {
        signing_key(&self.0).verifying_key().to_bytes()
    }

    /// 签名（64B）。
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        signing_key(&self.0).sign(message).to_bytes()
    }
}

fn signing_key(seed: &Secret32) -> SigningKey {
    SigningKey::from_bytes(seed.expose())
}

/// 验证 Ed25519 签名；成功返回 `Ok(())`。
pub fn verify_signature(
    public_key: &[u8; 32],
    message: &[u8],
    signature: &[u8; 64],
) -> Result<(), SyncError> {
    let vk = VerifyingKey::from_bytes(public_key).map_err(|_| SyncError::Signature)?;
    let sig = Signature::from_bytes(signature);
    // verify_strict：拒绝非规范/小阶公钥等边界情况（ADR 要求"篡改全拒绝"）。
    vk.verify_strict(message, &sig)
        .map_err(|_| SyncError::Signature)
}

/// 管理私钥（独立于 vault root，由恢复码派生）。
#[derive(Clone)]
pub struct ManagementKey(Secret32);

impl ManagementKey {
    /// 从已有种子构造（DPAPI 身份里的 `manager_seed`）。
    #[must_use]
    pub fn from_seed(seed: Secret32) -> Self {
        Self(seed)
    }

    /// 从恢复码派生（ADR §3：`management_ed25519_seed = HKDF(R, vault_id, ...)`）。
    pub fn from_recovery(seed: &RecoverySeed, vault_id: &VaultId) -> Result<Self, SyncError> {
        Ok(Self(recovery::management_seed(seed, vault_id)?))
    }

    /// 管理公钥（32B）。服务端只存这个。
    #[must_use]
    pub fn public_key(&self) -> Ed25519PublicKey {
        signing_key(&self.0).verifying_key().to_bytes()
    }

    /// 管理签名（64B）。
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        signing_key(&self.0).sign(message).to_bytes()
    }
}

/// X25519 HPKE 私钥（32 字节种子，经 RFC 9180 DeriveKeyPair 使用）。
#[derive(Clone)]
pub struct DeviceHpkeKey(Secret32);

impl DeviceHpkeKey {
    /// 生成新的随机 HPKE 身份。
    pub fn generate(rng: &mut impl RandomSource) -> Result<Self, SyncError> {
        Ok(Self(Secret32::generate(rng)?))
    }

    /// 从已有私钥材料构造。
    #[must_use]
    pub fn from_secret(secret: Secret32) -> Self {
        Self(secret)
    }

    /// 取私钥材料（DPAPI 身份 BLOB 需要持久化它）。
    #[must_use]
    pub fn secret(&self) -> &Secret32 {
        &self.0
    }

    /// X25519 公钥（HPKE `pkR`）。
    #[must_use]
    pub fn public_key(&self) -> X25519PublicKey {
        hpke::x25519_public_key(&self.0)
    }

    /// 解封给自己的 epoch grant（错 recipient 会返回 [`SyncError::RecipientMismatch`]）。
    pub fn open_grant(
        &self,
        enc: &[u8; 32],
        aad: &menghunote_sync_proto::GrantAadV1,
        ciphertext: &[u8],
    ) -> Result<crate::keys::RootSecretV1, SyncError> {
        hpke::open_epoch_grant(&self.0, enc, aad, ciphertext)
    }
}

/// 设备指纹（128 bit，显示为 32 位小写 hex）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DeviceFingerprint([u8; 16]);

impl DeviceFingerprint {
    /// 原始字节。
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// 32 位小写十六进制（双端人工比对用）。
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex_of(&self.0)
    }
}

impl core::fmt::Debug for DeviceFingerprint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// `SHA256(vault_id || device_id || sign_pk || hpke_pk)` 前 128 bit（ADR §4）。
#[must_use]
pub fn device_fingerprint(
    vault_id: &VaultId,
    device_id: &DeviceId,
    sign_pub: &Ed25519PublicKey,
    hpke_pub: &X25519PublicKey,
) -> DeviceFingerprint {
    let mut hasher = Sha256::new();
    hasher.update(vault_id);
    hasher.update(device_id);
    hasher.update(sign_pub);
    hasher.update(hpke_pub);
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    DeviceFingerprint(out)
}

/// 带签名的设备证书（relay `devices.certificate` 的载荷）。
#[derive(Clone, PartialEq, Eq)]
pub struct SignedDeviceCert {
    /// 证书本体（proto 的 `DeviceCertV1`）。
    pub cert: DeviceCertV1,
    /// 管理 key 的 Ed25519 签名（64B）。
    pub signature: [u8; 64],
}

impl SignedDeviceCert {
    /// 用管理公钥验签（签名输入 = `b"MN/v1/device-cert\0" || CDE(cert)`）。
    pub fn verify(&self, manager_pub: &Ed25519PublicKey) -> Result<(), SyncError> {
        let input = device_cert_signing_input(&self.cert)?;
        verify_signature(manager_pub, &input, &self.signature)
    }

    /// 证书 CDE（不含签名）。
    pub fn cert_bytes(&self) -> Result<Vec<u8>, SyncError> {
        Ok(self.cert.encode()?)
    }

    /// 本证书绑定的设备指纹。
    #[must_use]
    pub fn fingerprint(&self) -> DeviceFingerprint {
        device_fingerprint(
            &self.cert.vault_id,
            &self.cert.device_id,
            &self.cert.sign_ed25519_pub,
            &self.cert.hpke_x25519_pub,
        )
    }
}

/// 证书的管理签名输入：`b"MN/v1/device-cert\0" || CDE(DeviceCertV1)`（ADR §17 冻结）。
///
/// 只做转发：真正的字节由 `sync-proto` 生成，relay 与客户端因此共用同一套。
pub fn device_cert_signing_input(cert: &DeviceCertV1) -> Result<Vec<u8>, SyncError> {
    Ok(menghunote_sync_proto::device_cert_signed_bytes(cert)?)
}

/// 由管理 key 签发设备证书。
pub fn sign_device_cert(
    cert: &DeviceCertV1,
    manager: &ManagementKey,
) -> Result<SignedDeviceCert, SyncError> {
    let input = device_cert_signing_input(cert)?;
    Ok(SignedDeviceCert {
        cert: cert.clone(),
        signature: manager.sign(&input),
    })
}

/// 从恢复码出发验证整条证书链：
///
/// `R` → `management_ed25519_seed` → 管理公钥 → 验证证书签名，并校验
/// vault 归属、`issued_manager_rev`（管理权版本）与 rights 合法。
///
/// 这条链表达的是 ADR §4 的"恢复码具备独立管理能力"：没有 R 的一方（即使持有 vault
/// root）无法构造出通过校验的证书。
pub fn verify_cert_chain(
    signed: &SignedDeviceCert,
    recovery_seed: &RecoverySeed,
    expected_vault: &VaultId,
    expected_manager_rev: u64,
) -> Result<(), SyncError> {
    if signed.cert.vault_id != *expected_vault {
        return Err(SyncError::Certificate("证书 vault 归属不符"));
    }
    if signed.cert.issued_manager_rev != expected_manager_rev {
        return Err(SyncError::Certificate("证书管理权版本不符"));
    }
    // proto 的 validate() 会检查 cert_serial>0 与 rights 合法。
    signed.cert.validate()?;
    let manager = ManagementKey::from_recovery(recovery_seed, expected_vault)?;
    signed.verify(&manager.public_key())
}

/// 设备默认权限：READ|WRITE（ADR §4）。
#[must_use]
pub fn default_rights() -> Rights {
    Rights(Rights::ALL)
}

/// 邀请令牌 `T`（32B 随机）。**不 Debug/不 Display**。
#[derive(Clone)]
pub struct InviteToken(Secret32);

impl InviteToken {
    /// 生成 `T = random32`（ADR §4）。
    pub fn generate(rng: &mut impl RandomSource) -> Result<Self, SyncError> {
        Ok(Self(Secret32::generate(rng)?))
    }

    /// 从已有字节构造（配对 URI 里携带的就是它）。
    #[must_use]
    pub fn from_array(bytes: [u8; 32]) -> Self {
        Self(Secret32::from_array(bytes))
    }

    /// 取明文引用。
    #[must_use]
    pub fn expose(&self) -> &[u8; 32] {
        self.0.expose()
    }

    /// 常量时间相等。
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0)
    }
}

/// 邀请令牌的哈希：`SHA256(T)`（ADR §4：relay 只存这个，无额外域前缀）。
#[must_use]
pub fn invite_token_hash(token: &InviteToken) -> [u8; 32] {
    let digest = Sha256::digest(token.expose());
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// 邀请有效期：24h（ADR §4）。
pub const INVITE_TTL_SECS: u64 = 24 * 60 * 60;

/// 邀请记录（relay `invites` 行的客户端侧对应物）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InviteRecord {
    /// `SHA256(T)`。
    pub token_hash: [u8; 32],
    /// 过期时刻（Unix 秒）。
    pub expires_at: u64,
    /// 消费时刻；`Some` 后不可再用（一次性）。
    pub used_at: Option<u64>,
    /// 消费后绑定到的 PENDING 设备。
    pub pending_device: Option<DeviceId>,
}

impl InviteRecord {
    /// 由令牌与当前时刻创建（有效期 [`INVITE_TTL_SECS`]）。
    #[must_use]
    pub fn new(token: &InviteToken, now: u64) -> Self {
        Self {
            token_hash: invite_token_hash(token),
            expires_at: now.saturating_add(INVITE_TTL_SECS),
            used_at: None,
            pending_device: None,
        }
    }

    /// 纯函数消费：校验"未用过 + 未过期 + 哈希匹配"，返回新状态与 PENDING 设备。
    ///
    /// 不做任何 IO；持久化/CAS 由调用方（relay 或 storage）负责。哈希比较是常量时间。
    pub fn redeem(
        self,
        offered: &InviteToken,
        now: u64,
        pending_device: DeviceId,
    ) -> Result<(Self, DeviceId), SyncError> {
        if self.used_at.is_some() {
            return Err(SyncError::Invite("令牌已被消费"));
        }
        if now > self.expires_at {
            return Err(SyncError::Invite("令牌已过期"));
        }
        let offered_hash = invite_token_hash(offered);
        if !bool::from(offered_hash.ct_eq(&self.token_hash)) {
            return Err(SyncError::Invite("令牌哈希不匹配"));
        }
        let record = Self {
            used_at: Some(now),
            pending_device: Some(pending_device),
            ..self
        };
        Ok((record, pending_device))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRng;

    fn fixture(
        seed: u64,
    ) -> (
        TestRng,
        VaultId,
        DeviceId,
        DeviceSigningKey,
        DeviceHpkeKey,
        ManagementKey,
    ) {
        let mut rng = TestRng::new(seed);
        let vault_id: VaultId = [0x11; 16];
        let device_id: DeviceId = [0x22; 16];
        let sign = DeviceSigningKey::generate(&mut rng).unwrap();
        let hpke_key = DeviceHpkeKey::generate(&mut rng).unwrap();
        // 管理 key 从恢复码派生（与设备 key 完全独立）
        let recovery = RecoverySeed::generate(&mut rng).unwrap();
        let manager = ManagementKey::from_recovery(&recovery, &vault_id).unwrap();
        (rng, vault_id, device_id, sign, hpke_key, manager)
    }

    fn cert(
        vault_id: VaultId,
        device_id: DeviceId,
        sign: &DeviceSigningKey,
        hpke_key: &DeviceHpkeKey,
    ) -> DeviceCertV1 {
        DeviceCertV1 {
            vault_id,
            device_id,
            sign_ed25519_pub: sign.public_key(),
            hpke_x25519_pub: hpke_key.public_key(),
            cert_serial: 7,
            issued_manager_rev: 3,
            rights: default_rights(),
        }
    }

    #[test]
    fn sign_verify_roundtrip_and_tamper() {
        let (_rng, _v, _d, sign, _hpke_key, _manager) = fixture(31);
        let msg = b"MN/v1/device-cert\0payload";
        let sig = sign.sign(msg);
        verify_signature(&sign.public_key(), msg, &sig).unwrap();

        // 改消息 / 改签名 / 改公钥都失败
        assert!(verify_signature(&sign.public_key(), b"other", &sig).is_err());
        let mut bad_sig = sig;
        bad_sig[0] ^= 0x01;
        assert!(verify_signature(&sign.public_key(), msg, &bad_sig).is_err());
        let mut bad_pub = sign.public_key();
        bad_pub[0] ^= 0x01;
        assert!(verify_signature(&bad_pub, msg, &sig).is_err());
    }

    #[test]
    fn cert_cde_roundtrip_and_tamper_rejected() {
        let (_rng, vault_id, device_id, sign, hpke_key, manager) = fixture(32);
        let cert = cert(vault_id, device_id, &sign, &hpke_key);
        let bytes = cert.encode().unwrap();
        let parsed = DeviceCertV1::decode(&bytes).unwrap();
        assert_eq!(cert, parsed);
        assert_eq!(bytes, parsed.encode().unwrap());
        assert_eq!(bytes[0], 0xa7, "7 项 map 头");

        let signed = sign_device_cert(&cert, &manager).unwrap();
        signed.verify(&manager.public_key()).unwrap();
        assert_eq!(signed.cert_bytes().unwrap(), bytes);

        // 每个字段篡改都必须失败
        let manager_pub = manager.public_key();
        let mut tampered_sig = signed.clone();
        tampered_sig.cert.cert_serial += 1;
        assert!(tampered_sig.verify(&manager_pub).is_err());
        let mut tampered_sig = signed.clone();
        tampered_sig.cert.rights = Rights(Rights::READ);
        assert!(tampered_sig.verify(&manager_pub).is_err());
        let mut tampered_sig = signed.clone();
        tampered_sig.cert.hpke_x25519_pub[0] ^= 1;
        assert!(tampered_sig.verify(&manager_pub).is_err());
        let mut tampered_sig = signed.clone();
        tampered_sig.cert.device_id = [0x33; 16];
        assert!(tampered_sig.verify(&manager_pub).is_err());
        let mut tampered_sig = signed;
        tampered_sig.signature[63] ^= 1;
        assert!(tampered_sig.verify(&manager_pub).is_err());

        // 换一个管理 key 验签失败（"管理权不能来自 root"）
        let mut other_rng = TestRng::new(99);
        let other_manager = ManagementKey::from_recovery(
            &RecoverySeed::generate(&mut other_rng).unwrap(),
            &vault_id,
        )
        .unwrap();
        assert!(tampered_sig.verify(&other_manager.public_key()).is_err());
    }

    #[test]
    fn cert_chain_from_recovery_seed() {
        let mut rng = TestRng::new(33);
        let vault_id: VaultId = [0x44; 16];
        let device_id: DeviceId = [0x55; 16];
        let sign = DeviceSigningKey::generate(&mut rng).unwrap();
        let hpke_key = DeviceHpkeKey::generate(&mut rng).unwrap();
        let recovery = RecoverySeed::generate(&mut rng).unwrap();
        let manager = ManagementKey::from_recovery(&recovery, &vault_id).unwrap();
        let signed =
            sign_device_cert(&cert(vault_id, device_id, &sign, &hpke_key), &manager).unwrap();

        verify_cert_chain(&signed, &recovery, &vault_id, 3).unwrap();
        // 管理权版本不符
        assert!(verify_cert_chain(&signed, &recovery, &vault_id, 4).is_err());
        // 另一个 vault
        assert!(verify_cert_chain(&signed, &recovery, &[0x46; 16], 3).is_err());
        // 另一个恢复码（不是签发者）
        let other_recovery = RecoverySeed::generate(&mut rng).unwrap();
        assert!(verify_cert_chain(&signed, &other_recovery, &vault_id, 3).is_err());
        // 篡改证书后链校验失败
        let mut tampered = signed.clone();
        tampered.cert.sign_ed25519_pub[0] ^= 1;
        assert!(verify_cert_chain(&tampered, &recovery, &vault_id, 3).is_err());
    }

    #[test]
    fn management_key_is_independent_from_device_keys() {
        let (_rng, _vault_id, _device_id, sign, _hpke_key, manager) = fixture(34);
        assert_ne!(manager.public_key(), sign.public_key());
        // 设备 key 签的东西不能当管理签名通过
        let msg = b"management-only";
        assert!(verify_signature(&manager.public_key(), msg, &sign.sign(msg)).is_err());
    }

    #[test]
    fn device_fingerprint_golden_and_hex() {
        let fp = device_fingerprint(&[0x11; 16], &[0x22; 16], &[0x33; 32], &[0x44; 32]);
        // 独立实现（Python hashlib）算出的 golden：
        // SHA256(0x11*16 || 0x22*16 || 0x33*32 || 0x44*32) 前 16 字节
        assert_eq!(fp.to_hex(), "ed6a59d351a1502d49c04a3cb2838540");
        assert_eq!(fp.to_hex().len(), 32);
        assert_eq!(fp.as_bytes().len(), 16);
        // 任一输入不同 → 指纹不同
        assert_ne!(
            fp.to_hex(),
            device_fingerprint(&[0x11; 16], &[0x23; 16], &[0x33; 32], &[0x44; 32]).to_hex()
        );
        // 证书自带同一指纹
        let (_rng, vault_id, device_id, sign, hpke_key, _manager) = fixture(35);
        let c = cert(vault_id, device_id, &sign, &hpke_key);
        let signed = sign_device_cert(&c, &_manager).unwrap();
        assert_eq!(
            signed.fingerprint(),
            device_fingerprint(
                &vault_id,
                &device_id,
                &c.sign_ed25519_pub,
                &c.hpke_x25519_pub
            )
        );
    }

    #[test]
    fn rights_are_strict() {
        assert_eq!(Rights(Rights::ALL).bits(), 0b11);
        assert!(Rights(Rights::ALL).can_read());
        assert!(Rights(Rights::ALL).can_write());
        assert!(!Rights(Rights::READ).can_write());
        assert!(Rights::new(0b11).is_ok());
        assert!(Rights::new(0b01).is_ok());
        assert!(Rights::new(0).is_err());
        assert!(Rights::new(0b100).is_err());
        assert!(Rights::new(0xff).is_err());
        // proto 的 DeviceCertV1::validate 要求 cert_serial > 0
        let (_rng, vault_id, device_id, sign, hpke_key, _manager) = fixture(36);
        let mut c = cert(vault_id, device_id, &sign, &hpke_key);
        c.cert_serial = 0;
        assert!(c.encode().is_err());
    }

    #[test]
    fn invite_token_golden_and_single_use() {
        // T = 0x00..0x1f 的 SHA256（独立 Python hashlib 计算）
        let token = InviteToken::from_array([
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ]);
        let hash = invite_token_hash(&token);
        assert_eq!(
            hex_of(&hash),
            "630dcd2966c4336691125448bbb25b4ff412a49c732db2c8abc1b8581bd710dd"
        );
        assert_ne!(&hash, token.expose());

        let device: DeviceId = [0x77; 16];
        let record = InviteRecord::new(&token, 1_000);
        assert_eq!(record.expires_at, 1_000 + INVITE_TTL_SECS);
        assert_eq!(record.token_hash, hash);

        // 成功消费一次
        let (record, pending) = record.redeem(&token, 1_100, device).unwrap();
        assert_eq!(pending, device);
        assert_eq!(record.used_at, Some(1_100));
        assert_eq!(record.pending_device, Some(device));
        // 第二次消费失败（一次性）
        assert!(matches!(
            record.clone().redeem(&token, 1_200, device),
            Err(SyncError::Invite(_))
        ));
        // 过期
        let record = InviteRecord::new(&token, 1_000);
        assert!(record
            .clone()
            .redeem(&token, 1_000 + INVITE_TTL_SECS + 1, device)
            .is_err());
        // 边界：恰好在过期时刻仍可用（now == expires_at）
        assert!(record
            .clone()
            .redeem(&token, 1_000 + INVITE_TTL_SECS, device)
            .is_ok());
        // 错令牌
        let record = InviteRecord::new(&token, 1_000);
        let mut other = [0u8; 32];
        other[0] = 1;
        assert!(record
            .clone()
            .redeem(&InviteToken::from_array(other), 1_000, device)
            .is_err());
    }

    #[test]
    fn cert_unknown_field_rejected() {
        let (_rng, vault_id, device_id, sign, hpke_key, _manager) = fixture(37);
        let cert = cert(vault_id, device_id, &sign, &hpke_key);
        let bytes = cert.encode().unwrap();
        // 7 项 map 头改成 8 项 → 字段数不符
        let mut bad = bytes.clone();
        bad[0] = 0xa8;
        assert!(DeviceCertV1::decode(&bad).is_err());
        // rights 位非法（最后一项值改为 4）
        let mut bad = bytes;
        let last = bad.len() - 1;
        bad[last] = 0x04;
        assert!(DeviceCertV1::decode(&bad).is_err());
    }
}

`````



@@SECTION code/025 `crates/sync/src/device_management.rs`


`````rust
//! ADR 0010 切片 1b：管理端**独立**吊销设备访问的客户端 helper。
//!
//! 定位：同步是端到端加密，relay 只存密文，删设备凭证只能阻断**访问**，不能让已持有
//! 旧 root 的设备失去解密能力。本模块只发这一个管理端请求并把 relay 的事实返回给调用方，
//! **不删除**本地身份 / `manager_seed` / 恢复材料 / 笔记，也不自动轮换密钥
//! （轮换是显式 opt-in，ADR 0010 §安全边界）。
//!
//! wire（与 `crates/relay/src/api.rs` 的 `revoke_device` 严格一致）：
//!
//! ```text
//! POST /v1/vaults/{vault}/devices/{device}/revoke
//! B    = CDE{0: target_device(bytes16), 1: request_id(bytes16), 2: expected_control_rev(uint)}
//! wire = {0: {0: bytes(B), 1: signature64}, 1: request_id, 2: expected_control_rev}
//! ```
//!
//! - 管理 bearer：复用 `crate::recovery_enroll::management_token`（管理 realm challenge）；
//! - 管理签名：复用 `crate::rotation_stage::sign_management`（`POST=2` + 完整 path +
//!   instance/vault/request_id/expected_control_rev + `SHA256(CDE{method,path,B})`），
//!   **不复制**签名逻辑；
//! - 响应：`200 CDE{0: target, 1: "revoked", 2: changed, 3: auth_rev, 4: control_rev}`。
//!
//! 三条客户端硬约束：
//!
//! 1. **禁止移除本设备**：helper 层面直接拒绝（本地错误，**不发任何请求**）；标准 UI 也
//!    不得提供该入口（ADR 0010）。误删本设备只能靠另一台管理设备或恢复码重新入网；
//! 2. **管理密钥必须与本地身份 pin 一致**：helper 要求调用方传入 `ManagementKey`
//!    （类型上保证"有 key"），但仍校验其公钥等于身份里的 `pinned_manager`；不匹配说明
//!    恢复码/身份对应的是另一个管理代，必须在本地拒绝，不能拿错 key 去撞 relay；
//! 3. **只返回事实**：`control_rev` 不变（独立吊销不是签名控制事实），`auth_rev` 前进
//!    使全 vault 的旧 token 立即失效；缓存失效由 [`crate::http_relay`] 的 `state()` /
//!    REVOKED 处理负责，本模块不碰任何缓存。

use crate::api_version::{RelayApi, RelayApiVersion};
use crate::cbor;
use crate::device::ManagementKey;
use crate::error::SyncError;
use crate::http_relay::HttpTransport;
use crate::identity::LocalIdentityV1;
use crate::{DeviceId, VaultId};

/// 吊销请求的本地上下文（`base_url` / vault / instance pin / 本机设备 / CAS rev）。
///
/// 字段都不含秘密：`manager_pub` 是公开管理公钥（本地信任锚），`self_device_id` 是设备
/// 公开 ID。管理私钥只经 [`revoke_device`] 的 `manager` 参数传入。
pub struct RevokeContext {
    /// relay 基址（`http://127.0.0.1:port` 或 `https://...`，末尾 `/` 会被忽略）。
    pub base_url: String,
    /// relay API 面（ADR 0014 S3a；缺省 v1，v2 profile 由调用方显式设置）。
    pub api_version: RelayApiVersion,
    /// vault 身份。
    pub vault_id: VaultId,
    /// 已 pin 的 relay instance（签名绑定它；换实例即拒绝）。
    pub instance_id: [u8; 16],
    /// 本地身份 pin 住的管理公钥（不匹配 → 拒绝，不发请求）。
    pub manager_pub: [u8; 32],
    /// 本机设备 ID（**禁止**作为 target；`target == self` 直接拒绝）。
    pub self_device_id: DeviceId,
    /// 期望的 `control_rev`（CAS；吊销**不会**推进它）。
    pub expected_control_rev: u64,
    /// 当前 Unix 秒（管理 challenge/token 的有效期判断）。
    pub now: u64,
}

impl RevokeContext {
    /// 用本地身份构造上下文：instance/管理公钥/本机设备都取自受保护身份，
    /// 避免调用方各自拼错 pin。
    ///
    /// `expected_control_rev` 应取最近一次已验证的控制链 head（`control_head().0` 或
    /// `/state` 的 `control_rev`）；CAS 冲突时刷新列表后换新 `request_id` 重试，
    /// **不要**盲目改 rev 重放同一个请求。
    #[must_use]
    pub fn from_identity(
        base_url: impl Into<String>,
        identity: &LocalIdentityV1,
        expected_control_rev: u64,
        now: u64,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_version: RelayApiVersion::V1,
            vault_id: identity.vault_id,
            instance_id: identity.pinned_instance,
            manager_pub: identity.pinned_manager,
            self_device_id: identity.device_id,
            expected_control_rev,
            now,
        }
    }
}

impl RevokeContext {
    /// 显式选择 API 面（v2 profile 用；不改变其他字段）。
    #[must_use]
    pub fn with_api_version(mut self, api_version: RelayApiVersion) -> Self {
        self.api_version = api_version;
        self
    }
}

/// 吊销成功后的 relay 事实（`control_rev` 不变，`auth_rev` 已前进）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevokeOutcome {
    /// 被吊销的目标设备。
    pub target: DeviceId,
    /// 本次请求是否真的改变了状态；已 revoked 的重复请求 → `false`（`auth_rev` 不二次推进）。
    pub changed: bool,
    /// 吊销后的 `vault.auth_rev`（全 vault 旧 token 已失效）。
    pub auth_rev: u64,
    /// `vault.control_rev`（独立吊销**不改变**它，也不追加占位控制记录）。
    pub control_rev: u64,
}

/// 管理端吊销一台设备的数据访问权（ADR 0010）。
///
/// 顺序是语义相关的：本地守卫（本设备 / 管理公钥 pin）→ 管理认证 → 签名请求 →
/// 响应逐字段校验。**任何失败都不产生本地副作用**：不删身份、不删密钥、不改笔记。
///
/// 错误映射沿用既有 [`SyncError`]：relay 的业务错误（`NOT_FOUND` / `INVALID_STATE` /
/// `ROTATING` / `CAS_CONFLICT` / `ID_CONFLICT` / `REVOKED` / `MANAGEMENT_REQUIRED` …）
/// 经 `error_from_response` 变成 [`SyncError::RelayApi`]；本设备/错管理 key 是本地
/// [`SyncError::Unsupported`]，此时**不会发任何 HTTP 请求**。
pub fn revoke_device<T: HttpTransport>(
    transport: &mut T,
    ctx: &RevokeContext,
    manager: &ManagementKey,
    target: DeviceId,
    request_id: [u8; 16],
) -> Result<RevokeOutcome, SyncError> {
    // ① 禁止 self-remove：先于任何网络动作拒绝（ADR 0010 客户端硬约束）。
    if target == ctx.self_device_id {
        return Err(SyncError::Unsupported(
            "禁止吊销本设备：会立即自断同步/管理访问（ADR 0010）；请用另一台管理设备或恢复码",
        ));
    }
    // ② 管理 key 必须与身份 pin 一致。类型上已保证"有 key"，这里拒绝"错 key"：
    //    用错管理私钥去撞 relay 只会得到 MANAGEMENT_REQUIRED，还会把恢复码/身份
    //    对不上的问题掩盖成 relay 拒绝。
    if manager.public_key() != ctx.manager_pub {
        return Err(SyncError::Unsupported(
            "管理私钥与本地身份 pin 的管理公钥不符（恢复码/身份不匹配）；拒绝发送吊销请求",
        ));
    }

    let api = RelayApi::new(ctx.base_url.trim_end_matches('/'), ctx.api_version);
    // ③ 管理 realm bearer。challenge 响应里的 instance 必须等于本地 pin。
    let (token, instance_id) =
        crate::recovery_enroll::management_token(transport, &api, ctx.vault_id, manager, ctx.now)?;
    if instance_id != ctx.instance_id {
        return Err(SyncError::RelayWire("relay instance 与本地 pin 不符"));
    }

    // ④ canonical body `B`（业务字段只在这份被签 body 里；外层 1/2 是同一份事实的镜像）。
    let payload = cbor::encode(vec![
        (0, cbor::bytes(target.to_vec())),
        (1, cbor::bytes(request_id.to_vec())),
        (2, cbor::uint(ctx.expected_control_rev)),
    ])?;
    let path = api.revoke_device_path(&ctx.vault_id, &target);
    // ⑤ 签名绑定 POST=2/完整 path/instance/vault/request_id/expected_control_rev。
    //    复用管理写签名函数，绝不在这里复制一份签名输入拼接。
    let (signed, _signature) = crate::rotation_stage::sign_management(
        manager,
        &ctx.instance_id,
        ctx.vault_id,
        request_id,
        ctx.expected_control_rev,
        &path,
        &payload,
    )?;
    let body = cbor::encode(vec![
        (0, signed),
        (1, cbor::bytes(request_id.to_vec())),
        (2, cbor::uint(ctx.expected_control_rev)),
    ])?;
    let response =
        crate::recovery_enroll::send(transport, "POST", api.url(&path), Some(&token), body)?;

    // ⑥ 响应逐字段校验：target 必须与请求一致；`control_rev` 必须没变（CAS 的另一半）；
    //    业务状态字面量按 ADR 固定为 `"revoked"`。
    let fields = crate::recovery_enroll::decode(&response)?;
    let echoed_target: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
    if echoed_target != target {
        return Err(SyncError::RelayWire("吊销响应的 target 与请求不一致"));
    }
    let status = cbor::text_field(&fields, 1)?;
    if status != "revoked" {
        return Err(SyncError::RelayWire("吊销响应的状态字面量不是 revoked"));
    }
    let changed = cbor::bool_field(&fields, 2)?;
    let auth_rev = cbor::uint_field(&fields, 3)?;
    let control_rev = cbor::uint_field(&fields, 4)?;
    if control_rev != ctx.expected_control_rev {
        return Err(SyncError::RelayWire(
            "吊销响应的 control_rev 与请求不一致（控制链被并发推进？）",
        ));
    }
    Ok(RevokeOutcome {
        target,
        changed,
        auth_rev,
        control_rev,
    })
}

`````



@@SECTION code/026 `crates/sync/src/domain.rs`


`````rust
//! 域分隔符（ADR 0006 §14）。
//!
//! **真源是 `menghunote-sync-proto::constants`**：wire 契约只有一份，本 crate 只做
//! re-export + 一个 ADR 缺口的补充域。每个域 = `UTF8("MN/v1/<domain>")` 后接**一个 0x00
//! 字节**（源码里写作 `\0`，编译后是一个 NUL）。测试用十六进制断言这一点，任何把它改成
//! 反斜杠+`0` 两个字符的改动都会失败。

pub use menghunote_sync_proto::{
    dpapi_entropy, hkdf_data_info, password_wrap_info, recovery_wrap_info, signature_input,
    DPAPI_ENTROPY_PREFIX, HKDF_ASSET_ID_INFO, HKDF_DATA_INFO_PREFIX, HKDF_MANAGEMENT_ED25519_INFO,
    HKDF_PASSWORD_WRAP_PREFIX, HKDF_RECOVERY_WRAP_PREFIX, HPKE_INFO, SIGN_AUTH_DOMAIN,
    SIGN_CHECKPOINT_DOMAIN, SIGN_MANAGE_DOMAIN, SIGN_OBJECT_COMMIT_DOMAIN,
};

/// MNE1 信封 AAD 前缀（ADR §2：`b"MN/e2e/v1\0" || header[0..196] || ...`）。
pub use menghunote_sync_proto::AAD_DOMAIN as ENVELOPE_AAD;

/// 设备证书签名域（ADR §17 冻结：`MN/v1/device-cert\0`）。
pub use menghunote_sync_proto::SIGN_DEVICE_CERT_DOMAIN as DEVICE_CERT;

/// ASSET 确定性 nonce 的 HKDF info 前缀（ADR §19：`MN/v1/asset-nonce\\0`）。
pub const HKDF_ASSET_NONCE_INFO: &[u8] = b"MN/v1/asset-nonce\0";

/// KEY_SLOT MNE1 密钥的 HKDF info 前缀（ADR §20：`MN/v1/key-slot\\0`）。
pub const HKDF_KEY_SLOT_INFO: &[u8] = b"MN/v1/key-slot\0";

/// checkpoint 本地回执 HMAC 的 HKDF info（task-122）。
///
/// `K_receipt = HKDF(K_epoch, salt=vault_id, info=MN/v1/checkpoint-receipt-key\0, 32)`；
/// 只用于 coverage/verified 两类**本地 durable receipt** 的认证，不做对象加密、不进 MNE1
/// 信封、不复用 `K_object`/`K_asset_id`/槽密钥/cursor/签名域。
pub const HKDF_CHECKPOINT_RECEIPT_INFO: &[u8] = b"MN/v1/checkpoint-receipt-key\0";

/// checkpoint 本地回执 MAC 前缀（HMAC 输入 = `本域 || kind_u8 || body`）。
pub const CHECKPOINT_RECEIPT_MAC_DOMAIN: &[u8] = b"MN/v1/checkpoint-receipt-mac\0";

/// coverage pending receipt 的 MAC kind。
pub const CHECKPOINT_RECEIPT_KIND_COVERAGE: u8 = 1;
/// verified（promote 成功）receipt 的 MAC kind。
pub const CHECKPOINT_RECEIPT_KIND_VERIFIED: u8 = 2;

/// Linux 本地身份封装外层 AAD 域（ADR 0008：`MN/local-wrap/v1\0`）。
///
/// AAD = 本域 || 完整 30B 外层头 || 原 entropy。域体是 ASCII，末尾是一个真正的
/// 0x00 字节（ADR 0006 §14 口径）。
pub const LOCAL_WRAP_AAD_DOMAIN: &[u8] = b"MN/local-wrap/v1\0";

#[cfg(test)]
mod tests {
    use super::*;
    use menghunote_sync_proto::domain_ends_with_nul;

    /// ADR §14：域分隔符必须是真正的 NUL（0x00），不是反斜杠+`0`，也不是两个 ASCII 字节。
    #[test]
    fn domains_end_with_nul() {
        let all: &[(&str, &[u8])] = &[
            ("envelope_aad", ENVELOPE_AAD),
            ("password_wrap", HKDF_PASSWORD_WRAP_PREFIX),
            ("recovery_wrap", HKDF_RECOVERY_WRAP_PREFIX),
            ("management_ed25519", HKDF_MANAGEMENT_ED25519_INFO),
            ("data_key", HKDF_DATA_INFO_PREFIX),
            ("asset_id", HKDF_ASSET_ID_INFO),
            ("epoch_grant", HPKE_INFO),
            ("object_commit", SIGN_OBJECT_COMMIT_DOMAIN),
            ("checkpoint", SIGN_CHECKPOINT_DOMAIN),
            ("auth", SIGN_AUTH_DOMAIN),
            ("manage", SIGN_MANAGE_DOMAIN),
            ("dpapi_entropy", DPAPI_ENTROPY_PREFIX),
            ("device_cert", DEVICE_CERT),
            ("asset_nonce", HKDF_ASSET_NONCE_INFO),
            ("key_slot", HKDF_KEY_SLOT_INFO),
            ("local_wrap_aad", LOCAL_WRAP_AAD_DOMAIN),
            ("checkpoint_receipt_key", HKDF_CHECKPOINT_RECEIPT_INFO),
            ("checkpoint_receipt_mac", CHECKPOINT_RECEIPT_MAC_DOMAIN),
        ];
        for (name, domain) in all {
            assert_eq!(
                domain.last().copied(),
                Some(0x00),
                "{name} 必须以单个 0x00 结尾（ADR 0006 §14）"
            );
            assert!(
                domain_ends_with_nul(domain),
                "{name} 必须恰好一个结尾 NUL（proto 的口径）"
            );
            // 末尾不能是 "\\0" 这两个 ASCII 字符（反斜杠 0x5c + '0' 0x30）
            assert!(!domain.ends_with(b"\\0"), "{name} 不能以反斜杠+0 结尾");
            // 域体是 ASCII，且不含除结尾 NUL 之外的 NUL
            let body = &domain[..domain.len() - 1];
            assert!(body.iter().all(|b| b.is_ascii_graphic() || *b == b'/'));
            assert_eq!(body.iter().filter(|b| **b == 0x00).count(), 0);
        }
    }

    /// ADR §14 的 golden hex：`MN/e2e/v1\0` = 4d4e 2f65 3265 2f76 3100。
    #[test]
    fn aad_prefix_golden_hex() {
        assert_eq!(ENVELOPE_AAD, b"MN/e2e/v1\0");
        assert_eq!(
            ENVELOPE_AAD,
            &[0x4d, 0x4e, 0x2f, 0x65, 0x32, 0x65, 0x2f, 0x76, 0x31, 0x00]
        );
        assert_eq!(ENVELOPE_AAD.len(), 10);
        assert_eq!(HPKE_INFO, b"MN/v1/epoch-grant\0");
        assert_eq!(HPKE_INFO.len(), 18);
        assert_eq!(HPKE_INFO[17], 0x00);
        assert_eq!(DPAPI_ENTROPY_PREFIX, b"MN/dpapi/v1\0");
        assert_eq!(DPAPI_ENTROPY_PREFIX.len(), 12);
        // ADR 0008 的 Linux 本地封装 AAD 域：`MN/local-wrap/v1\0` =
        // 4d4e 2f6c 6f63 616c 2d77 7261 702f 7631 00。
        assert_eq!(LOCAL_WRAP_AAD_DOMAIN, b"MN/local-wrap/v1\0");
        assert_eq!(
            LOCAL_WRAP_AAD_DOMAIN,
            &[
                0x4d, 0x4e, 0x2f, 0x6c, 0x6f, 0x63, 0x61, 0x6c, 0x2d, 0x77, 0x72, 0x61, 0x70, 0x2f,
                0x76, 0x31, 0x00,
            ]
        );
        assert_eq!(LOCAL_WRAP_AAD_DOMAIN.len(), 17);
    }

    #[test]
    fn slot_info_is_domain_then_slot_id() {
        let slot_id = [0xab; 32];
        let info = password_wrap_info(&slot_id);
        assert_eq!(
            &info[..HKDF_PASSWORD_WRAP_PREFIX.len()],
            HKDF_PASSWORD_WRAP_PREFIX
        );
        assert_eq!(info[HKDF_PASSWORD_WRAP_PREFIX.len() - 1], 0x00);
        assert_eq!(&info[HKDF_PASSWORD_WRAP_PREFIX.len()..], &slot_id);
        assert_eq!(recovery_wrap_info(&slot_id).len(), info.len());
        assert_ne!(
            recovery_wrap_info(&slot_id)[..HKDF_RECOVERY_WRAP_PREFIX.len()],
            password_wrap_info(&slot_id)[..HKDF_PASSWORD_WRAP_PREFIX.len()]
        );
    }

    #[test]
    fn dpapi_entropy_layout() {
        let entropy = dpapi_entropy(&[1u8; 16], &[2u8; 16]);
        assert_eq!(&entropy[..DPAPI_ENTROPY_PREFIX.len()], DPAPI_ENTROPY_PREFIX);
        assert_eq!(
            &entropy[DPAPI_ENTROPY_PREFIX.len()..DPAPI_ENTROPY_PREFIX.len() + 16],
            &[1u8; 16]
        );
        assert_eq!(&entropy[DPAPI_ENTROPY_PREFIX.len() + 16..], &[2u8; 16]);
    }
}

`````



@@SECTION code/027 `crates/sync/src/dpapi.rs`


`````rust
//! DPAPI 本地身份包裹（ADR 0006 §3，仅 Windows）。
//!
//! v1 用 **`CryptProtectData` / `CryptUnprotectData`（CurrentUser 作用域）** 保护一份
//! 本地身份 BLOB（[`crate::identity::LocalIdentityV1`]）：
//!
//! - 可选 entropy = `b"MN/dpapi/v1\0" || vault_id || device_id`（见 [`entropy_for`]）；
//! - **不用** `CRYPTPROTECT_LOCAL_MACHINE`（ADR §3 明确要求 CurrentUser）；
//! - 密文存 `meta` 受保护 BLOB；本地正文/索引/备份仍是明文（ADR §10 风险项）。
//!
//! ## 平台语义（不假装）
//!
//! 非 Windows 平台本模块编译通过，但 [`is_available`] 返回 `false`，[`protect`] /
//! [`unprotect`] 一律返回 [`SyncError::DpapiUnavailable`]——**没有**降级为"明文存盘"或
//! 自造加密的路径。
//!
//! ## 跨 SID 失败（不可自动化验证）
//!
//! DPAPI CurrentUser 的密钥绑定在**用户 SID + 用户 profile** 上。把 vault/DB 复制到另一
//! 个 Windows 用户、重建 profile、或重置用户密码后（旧 DPAPI master key 没被保留），
//! `CryptUnprotectData` 会失败（典型是 `ERROR_INVALID_DATA`/`ERROR_ACCESS_DENIED`）。
//! 这是**预期行为**：本地身份 BLOB 不能跨用户解密，用户必须用口令槽/恢复码重新解锁并
//! 生成新的设备身份。该场景无法在本机自动化覆盖，属于"未验证项"。

use crate::error::SyncError;
use crate::{DeviceId, VaultId};
use zeroize::Zeroizing;

/// DPAPI entropy 前缀（ADR §3/§14：末尾是一个 0x00 字节；真源在 `sync-proto`）。
pub const ENTROPY_PREFIX: &[u8] = crate::domain::DPAPI_ENTROPY_PREFIX;

/// 计算 DPAPI 可选 entropy：`b"MN/dpapi/v1\0" || vault_id || device_id`。
#[must_use]
pub fn entropy_for(vault_id: &VaultId, device_id: &DeviceId) -> Vec<u8> {
    crate::domain::dpapi_entropy(vault_id, device_id)
}

/// 本平台是否具备 DPAPI 实现。
#[must_use]
pub const fn is_available() -> bool {
    cfg!(windows)
}

/// 用 DPAPI（CurrentUser）保护明文，返回密文 BLOB。
///
/// 非 Windows 返回 [`SyncError::DpapiUnavailable`]。
#[cfg(windows)]
pub fn protect(plaintext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, SyncError> {
    imp::protect(plaintext, entropy)
}

/// 用 DPAPI（CurrentUser）解封 BLOB，返回**零化**的明文。
///
/// 非 Windows 返回 [`SyncError::DpapiUnavailable`]。
#[cfg(windows)]
pub fn unprotect(blob: &[u8], entropy: &[u8]) -> Result<Zeroizing<Vec<u8>>, SyncError> {
    imp::unprotect(blob, entropy)
}

/// 非 Windows：明确不可用（不假装成功、不降级明文）。
#[cfg(not(windows))]
pub fn protect(_plaintext: &[u8], _entropy: &[u8]) -> Result<Vec<u8>, SyncError> {
    Err(SyncError::DpapiUnavailable)
}

/// 非 Windows：明确不可用。
#[cfg(not(windows))]
pub fn unprotect(_blob: &[u8], _entropy: &[u8]) -> Result<Zeroizing<Vec<u8>>, SyncError> {
    Err(SyncError::DpapiUnavailable)
}

/// Windows 上的 Win32 FFI 实现（唯一允许 unsafe 的地方）。
#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use super::SyncError;
    use core::ffi::c_void;
    use core::ptr;
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    use zeroize::Zeroizing;

    fn blob(data: &[u8]) -> Result<CRYPT_INTEGER_BLOB, SyncError> {
        let len = u32::try_from(data.len()).map_err(|_| SyncError::DpapiUnavailable)?;
        Ok(CRYPT_INTEGER_BLOB {
            cbData: len,
            pbData: data.as_ptr() as *mut u8,
        })
    }

    /// 把 DPAPI 输出拷出，并**先擦除再释放**系统缓冲区（解封时里面是明文）。
    fn take_output(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let data = unsafe {
            // SAFETY: CryptProtectData/CryptUnprotectData 成功时保证 pbData/cbData 有效，
            // 缓冲区由 LocalAlloc 分配，必须用 LocalFree 释放。
            ptr::slice_from_raw_parts(out.pbData, out.cbData as usize).as_ref()
        };
        let copied = match data {
            Some(slice) => slice.to_vec(),
            None => Vec::new(),
        };
        if !out.pbData.is_null() {
            unsafe {
                // 先清零（解封出来的明文不留在系统堆里），再释放。
                ptr::write_bytes(out.pbData, 0, out.cbData as usize);
                LocalFree(out.pbData.cast::<c_void>());
            }
        }
        copied
    }

    pub fn protect(plaintext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, SyncError> {
        let input = blob(plaintext)?;
        let entropy = blob(entropy)?;
        let mut out = CRYPT_INTEGER_BLOB::default();
        let ok = unsafe {
            CryptProtectData(
                &input,
                ptr::null(),
                &entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(SyncError::Dpapi(unsafe { GetLastError() }));
        }
        Ok(take_output(out))
    }

    pub fn unprotect(blob_bytes: &[u8], entropy: &[u8]) -> Result<Zeroizing<Vec<u8>>, SyncError> {
        let input = blob(blob_bytes)?;
        let entropy = blob(entropy)?;
        let mut out = CRYPT_INTEGER_BLOB::default();
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                &entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(SyncError::Dpapi(unsafe { GetLastError() }));
        }
        Ok(Zeroizing::new(take_output(out)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entropy_layout() {
        let entropy = entropy_for(&[1u8; 16], &[2u8; 16]);
        assert_eq!(&entropy[..ENTROPY_PREFIX.len()], ENTROPY_PREFIX);
        assert_eq!(entropy[ENTROPY_PREFIX.len() - 1], 0x00);
        assert_eq!(entropy.len(), ENTROPY_PREFIX.len() + 32);
        assert_eq!(
            &entropy[ENTROPY_PREFIX.len()..ENTROPY_PREFIX.len() + 16],
            &[1u8; 16]
        );
        assert_eq!(&entropy[ENTROPY_PREFIX.len() + 16..], &[2u8; 16]);
    }

    #[test]
    fn platform_flag_matches_cfg() {
        assert_eq!(is_available(), cfg!(windows));
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_explicitly_unavailable() {
        assert!(!is_available());
        assert!(matches!(
            protect(b"secret", b"entropy"),
            Err(SyncError::DpapiUnavailable)
        ));
        assert!(matches!(
            unprotect(b"blob", b"entropy"),
            Err(SyncError::DpapiUnavailable)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_roundtrip_and_entropy_binding() {
        let plaintext = b"local identity canary 0001";
        let entropy = entropy_for(&[7u8; 16], &[8u8; 16]);
        let blob = protect(plaintext, &entropy).unwrap();
        assert_ne!(blob, plaintext, "DPAPI 密文不能等于明文");
        assert!(!blob.windows(plaintext.len()).any(|w| w == plaintext));

        let opened = unprotect(&blob, &entropy).unwrap();
        assert_eq!(&**opened, plaintext);

        // 换 entropy（模拟另一个 vault/device）必须解不开
        let other = entropy_for(&[7u8; 16], &[9u8; 16]);
        assert!(unprotect(&blob, &other).is_err());

        // 篡改密文必须解不开
        let mut tampered = blob.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(unprotect(&tampered, &entropy).is_err());

        // 截断必须解不开（且不 panic）
        assert!(unprotect(&blob[..blob.len() / 2], &entropy).is_err());
        assert!(unprotect(&[], &entropy).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_protect_same_bytes_twice_produces_different_blobs() {
        // DPAPI 每次加密带随机 salt；同明文两次密文不同（不构成确定性承诺，只是健全性检查）。
        let entropy = entropy_for(&[1u8; 16], &[1u8; 16]);
        let a = protect(b"same", &entropy).unwrap();
        let b = protect(b"same", &entropy).unwrap();
        assert_ne!(a, b);
    }
}

`````



@@SECTION code/029 `crates/sync/src/error.rs`


`````rust
//! 统一错误类型。
//!
//! **秘密纪律**：任何变体都不得携带口令、根密钥、恢复种子、私钥或它们的派生值；
//! 只允许定位信息（字段名、长度、期望/实际的小整数、Win32 错误码）。
//! 这样即使错误被 `{:?}`/`{}` 打进日志，也不会泄露可用秘密。

use thiserror::Error;

/// `crates/sync` 的错误集合。
#[derive(Debug, Error)]
pub enum SyncError {
    /// `sync-proto` 的 wire 编解码失败（字段越界/非 canonical/未知字段等）。
    ///
    /// `ProtoError` 只带结构性描述，不含秘密。
    #[error("wire 契约失败：{0}")]
    Proto(#[from] menghunote_sync_proto::ProtoError),

    /// 本地文件操作失败（身份原子推广等）。
    #[error("本地 IO 失败：{0}")]
    Io(String),

    /// 口令长度越界（ADR 0006 §3：原样 UTF-8、不 trim、不规范化，1..=1024 字节）。
    #[error("口令长度必须在 {min}..={max} 字节之间（实际 {actual}）")]
    PasswordLength {
        /// 实际字节数。
        actual: usize,
        /// 允许下界（含）。
        min: usize,
        /// 允许上界（含）。
        max: usize,
    },

    /// Argon2id 参数越界（必须在调用 Argon 之前拒绝，ADR 0006 §3）。
    #[error("Argon2id 参数越界：{field}={value}")]
    Argon2Param {
        /// 字段名。
        field: &'static str,
        /// 实际取值。
        value: u64,
    },

    /// Argon2id 计算本身失败（参数已通过校验后仍失败，通常是实现/内存问题）。
    #[error("Argon2id 计算失败：{0}")]
    Argon2(&'static str),

    /// HKDF 派生失败（只可能是输出长度超过 255×HashLen）。
    #[error("HKDF-SHA256 派生失败（输出长度非法）")]
    Hkdf,

    /// 槽类型与 KDF/操作不匹配。
    #[error("密钥槽类型不匹配：期望 {expected}，实际 {actual}")]
    SlotKind {
        /// 期望类型。
        expected: &'static str,
        /// 实际类型。
        actual: &'static str,
    },

    /// `SlotDescriptorV1` 校验失败。
    #[error("密钥槽描述无效：{0}")]
    Descriptor(&'static str),

    /// 解封失败（口令/恢复码错误，或槽体被篡改/换错槽）。
    #[error("解封失败：口令/恢复码不匹配或槽体损坏")]
    Unwrap,

    /// 恢复码编解码失败。
    #[error("恢复码无效：{0}")]
    RecoveryCode(&'static str),

    /// HPKE 失败。
    #[error("HPKE 失败：{0}")]
    Hpke(&'static str),

    /// HPKE 收件人公钥与本机身份不一致（错 recipient）。
    #[error("HPKE 收件人不匹配")]
    RecipientMismatch,

    /// Ed25519 签名验证失败。
    #[error("签名验证失败")]
    Signature,

    /// `DeviceCertV1` 校验失败。
    #[error("设备证书无效：{0}")]
    Certificate(&'static str),

    /// 邀请令牌无效（已用/过期/哈希不匹配）。
    #[error("邀请令牌无效：{0}")]
    Invite(&'static str),

    /// 槽集合 CAS 冲突。
    #[error("槽集合 CAS 冲突：期望 rev={expected}，实际 rev={actual}")]
    Cas {
        /// 期望的槽集合版本。
        expected: u64,
        /// 存储侧当前版本。
        actual: u64,
    },

    /// relay 配置/URL/模式非法（ADR 0013 §4 `ConfigInvalid`）。
    ///
    /// 只含公开定位信息（scheme/host/模式/字段名），不含任何凭据。
    #[error("同步配置无效：{0}")]
    ConfigInvalid(String),

    /// 需要重新建立信任（ADR 0013 §3/§4 `NeedsTrust`）。
    ///
    /// 出现在"本地没有可证明的信任锚"的路径上：缺失 durable pin、旧 pin 只有
    /// `(instance, rev)` 下界（同 rev 无 hash）、高 rev 前进缺少旧前缀 anchor。
    /// **不得**在调用方降级成 unpinned/bootstrap-only 继续。
    #[error("需要建立信任（不盲信继续）：{0}")]
    NeedsTrust(&'static str),

    /// durable 控制 pin 损坏/版本不支持/vault 绑定不符（ADR 0013 §2/§4 `PinCorrupt`）。
    ///
    /// **不得**当成"没有 pin"：`None` 只表示从未建立过。
    #[error("控制 pin 损坏：{0}")]
    PinCorrupt(&'static str),

    /// durable 控制 pin 保存失败（写事务失败或 CAS 未命中后的明确失败）。
    #[error("控制 pin 保存失败：{0}")]
    PinSaveFailed(String),

    /// relay instance 与本地 pin/身份不一致（换实例/静默回滚）。
    #[error("relay 实例与本地 pin 不一致（疑似换实例或静默回滚）")]
    InstanceMismatch,

    /// 控制链回退：新 head 的 rev 低于已提交 pin（ADR 0006 §35）。
    #[error("控制链回退：本地 pin rev={expected}，收到 rev={actual}")]
    ControlRollback {
        /// 已提交 pin 的 rev。
        expected: u64,
        /// 新 head 的 rev。
        actual: u64,
    },

    /// 控制链内容不一致（equivocation：换 genesis/重写链/同 rev 换 bootstrap 视图/
    /// 旧锚未被当前链覆盖）。消息只含公开定位信息。
    #[error("控制链内容不一致（equivocation）：{0}")]
    Equivocation(&'static str),

    /// durable pin 的并发 CAS 冲突：重新验证后重试，**不覆盖**赢家。
    #[error("控制 pin CAS 冲突：已被并发修改，不覆盖赢家")]
    PinConflict,

    /// 改口令流程失败。
    #[error("改口令失败：{0}")]
    PasswordChange(&'static str),

    /// 确定性 CBOR 编解码失败。
    #[error("CDE 编解码失败：{0}")]
    Cde(&'static str),

    /// 本地身份 BLOB 结构无效。
    #[error("本地身份结构无效：{0}")]
    Identity(&'static str),

    /// DPAPI 不可用（非 Windows 平台明确拒绝，不假装可用）。
    #[error("DPAPI 不可用：仅 Windows CurrentUser 实现")]
    DpapiUnavailable,

    /// DPAPI 调用失败，携带 Win32 错误码（不含明文）。
    #[error("DPAPI 调用失败：Win32 错误码 {0}")]
    Dpapi(u32),

    /// 当前平台没有本地身份封装实现（Windows/Linux 之外明确拒绝，不回退明文）。
    #[error("本地身份封装在当前平台不可用")]
    LocalWrapUnsupportedPlatform,

    /// Linux 本地封装 BLOB 结构非法（magic/长度/编码不符）。
    #[error("本地封装格式非法：{0}")]
    LocalWrapFormat(&'static str),

    /// 未知的本地封装外层版本号。
    #[error("本地封装版本未知：{0}")]
    LocalWrapVersion(u8),

    /// 未知的本地封装外层 kind。
    #[error("本地封装类型未知：{0}")]
    LocalWrapKind(u8),

    /// 本地封装认证失败（头/nonce/密文被篡改，或 AAD 绑定的 vault/device/记录不符）。
    #[error("本地封装认证失败：密文损坏或 entropy 不匹配")]
    LocalWrapAuth,

    /// 本地身份封装路径无法解析（环境变量缺失/相对路径/非目录等）。
    #[error("本地主密钥路径无效：{0}")]
    LocalKeyPath(&'static str),

    /// 主密钥或其私有目录不存在（保护/解封只加载，不创建）。
    #[error("本地主密钥缺失：{0}")]
    LocalKeyMissing(&'static str),

    /// 主密钥内容损坏（长度/尾随字节/非 32B 主钥等）。
    #[error("本地主密钥损坏：{0}")]
    LocalKeyCorrupt(&'static str),

    /// 主密钥或目录的权限/属主/类型不安全（**不自动修复**）。
    #[error("本地主密钥权限不安全：{0}")]
    LocalKeyPermissions(&'static str),

    /// 主密钥文件 IO 失败（创建/打开/读写/fsync/原子发布）。
    #[error("本地主密钥 IO 失败：{0}")]
    LocalKeyIo(String),

    /// 系统随机源失败。
    #[error("系统随机源失败")]
    Random,

    /// 本地存储错误（经 `menghunote-storage` 暴露）。
    #[error("本地存储失败：{0}")]
    Storage(#[from] menghunote_storage::StorageError),

    /// 本地存储里的同步相关数据不一致（schema 不认、outbox 缺字段等）。
    #[error("本地同步数据异常：{0}")]
    LocalCorrupt(&'static str),

    /// relay 传输层失败（连接断开、响应被截断等）。
    #[error("relay 传输失败：{0}")]
    Transport(&'static str),

    /// TLS 校验/握手失败（证书链、主机名、有效期、信任根装配）。**不可重试**：
    /// 重试不会让一张过期或错配的证书变合法。
    #[error("TLS 校验失败：{0}")]
    Tls(String),

    /// 传输安全策略拒绝（明文 http、跨 origin 重定向、https→http 降级、非法 base_url）。
    /// **不可重试**：策略是确定性的，重试只会重复被拒。
    #[error("传输策略拒绝：{0}")]
    TransportPolicy(String),

    /// relay 返回业务错误（CBOR 错误体 `{0:code,1:retryable,2:epoch?,3:rev?,4:hint?}`）。
    #[error("relay 拒绝：{code}")]
    RelayApi {
        /// 统一错误码（wire 字符串）。
        code: String,
        /// 是否可重试。
        retryable: bool,
        /// relay 当前 epoch（`EPOCH` 类错误携带）。
        current_epoch: Option<u64>,
        /// relay 当前 control_rev（`CAS_CONFLICT` 类错误携带）。
        current_control_rev: Option<u64>,
        /// 需要重新 bootstrap 时的提示（`RELAY_RESET`/`CURSOR_EXPIRED`）。
        bootstrap_hint: Option<String>,
    },

    /// 邀请链接非法/不可用（ADR 0014 S1）。
    ///
    /// 只含**脱敏**定位信息：不含 token、不含完整 URL；对端可据此区分格式/TTL/pin/状态。
    #[error("邀请链接无效：{0}")]
    InviteLink(String),

    /// relay 响应格式非法（缺字段/ID 不是 16/32 字节等）。
    #[error("relay 响应非法：{0}")]
    RelayWire(&'static str),

    /// `ApprovalEnvelopeV1`（ADR 0016 S4d1b）编解码/导入验证失败。
    ///
    /// 只含**公开定位信息**（字段/槽位/角色/次数）：包内证据本就是公开的
    /// cap/控制链事实，本变体**不**携带私钥、root、seed、邀请 token。
    #[error("审批包无效：{0}")]
    Approval(String),

    /// HTTP 状态码非预期。
    #[error("relay HTTP 状态 {0}")]
    HttpStatus(u16),

    /// 端点/能力尚未实现（例如真实 relay 还没有设备证书查询）。
    #[error("尚未实现：{0}")]
    Unsupported(&'static str),

    /// 对象级校验失败（digest/签名/解密）。
    #[error("同步对象无效：{0}")]
    Object(&'static str),

    /// 同步必须暂停（游标回滚/epoch 变化等）；暂停时**不得**推进游标或 ACK。
    #[error("同步暂停：{0}")]
    Paused(crate::relay::PauseReason),

    /// 取消/授权复核在**首次外部集成前**赢得竞争：本轮**没有**追加任何外部行（ADR 0012 S1）。
    ///
    /// 只在 `integrate` 之前的那道门返回；一旦进入 integrate 就不会再用它中断事务
    /// （已开始的真实事务必须跑完并交付）。**不可重试**：重试是用户/调度器的决定，
    /// 不能让自动路径悄悄再来一遍。
    #[error("同步已取消：在外部集成前停止（未追加任何本地行）")]
    CancelledBeforeIntegrate,

    /// 结构投影失败（ADR 0012 S2b）。
    ///
    /// 结构 CRDT 与 inbox 水位都已 durable；失败的是本地 `tree/notes` 投影。调用方必须
    /// **明确报错并保持写禁用**（不得假装侧栏已刷新），并保留重试入口。
    #[error("结构投影失败：{0}")]
    StructureProjection(String),

    /// head CAS 冲突（并发本地写入把 head 推进了）。**可重试**：重读 head 后重放，
    /// 不得把它当成"数据丢了"而丢弃 outbox/inbox 行。
    #[error("head CAS 冲突：{doc_id} 期望 {expected}，实际 {actual}")]
    HeadConflict {
        /// 文档 ID。
        doc_id: String,
        /// 调用方用作 CAS 基准的 seq。
        expected: u64,
        /// 库里实际的 seq。
        actual: u64,
    },
}

impl SyncError {
    /// 是否可安全重试（不丢数据、可重复提交）。
    ///
    /// 只把明确的"并发/瞬时"错误标为可重试；校验/解密/签名失败一律不可重试，
    /// 以免把损坏数据当成可恢复状态反复重放。
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::HeadConflict { .. } | Self::Transport(_) | Self::Random => true,
            Self::RelayApi { retryable, .. } => *retryable,
            Self::Storage(menghunote_storage::StorageError::SnapshotBusy { .. }) => true,
            _ => false,
        }
    }
}

`````



@@SECTION code/030 `crates/sync/src/genesis.rs`


`````rust
//! genesis 客户端（S7-11）：本地造钥 → `POST /v1/vaults` → 落本地身份。
//!
//! 顺序（ADR §3/§4/§8/§17）：
//!
//! 1. 随机 `vault_id` / `device_id`，生成设备 Ed25519 + X25519 身份；
//! 2. 随机根 `K_epoch(1)`；随机恢复种子 `R` → Bech32m 恢复码 `mnrec1...`；
//! 3. 管理私钥 = `HKDF(R, vault_id, "MN/v1/management-ed25519\0")`，用它对
//!    `DeviceCertV1` 做 §17.1 管理签名；
//! 4. 初始 password 槽（Argon2id）与 recovery 槽（HKDF）封装根；
//! 5. 组 CBOR `POST /v1/vaults`（字段以 relay `api.rs` 为准），成功后把
//!    instance/log_id/epoch/control_rev/auth_rev 与初始游标（relay 键 5）带回。
//!
//! **初始游标**：relay 的 `GET /log` 需要 HMAC 游标，客户端无法自造。genesis 响应
//! 键 5 应带 `cursor_at(0)`；若 relay 版本较旧，调用方可用
//! [`crate::http_relay::HttpRelayClient::initial_cursor`] 兜底读 `GET /state` 键 14。
//! 两者都没有时返回明确错误，调用方必须走 bootstrap（`gc_floor > 0` 时同样必须
//! bootstrap，relay 对 `cursor_at(0)` 会返回 410 CURSOR_EXPIRED）。

use sha2::{Digest, Sha256};

use crate::cbor;
use crate::device::{
    default_rights, sign_device_cert, DeviceHpkeKey, DeviceSigningKey, ManagementKey,
    SignedDeviceCert,
};
use crate::error::SyncError;
use crate::http_relay::{
    error_from_response, HttpRequest, HttpResponse, HttpTransport, CONTENT_TYPE_CBOR,
};
use crate::ids::PeerId;
use crate::keys::RootSecretV1;
use crate::random::RandomSource;
use crate::recovery::{management_seed, RecoverySeed};
use crate::slot::{context_hash, KeySlotV1, WRAP_SUITE_XCHACHA20_POLY1305};
use crate::{DeviceId, VaultId};
use menghunote_sync_proto::{
    DeviceCertV1, EnvelopeHeader, Kdf, ObjectType, SlotDescriptorV1, SlotKind, PROTOCOL_VERSION,
};
use zeroize::Zeroizing;

/// 初始身份代。
pub const GENESIS_EPOCH: u64 = 1;
/// 初始管理权版本。
pub const GENESIS_MANAGER_REV: u64 = 1;

/// genesis 生成的全部本地材料。
pub struct GenesisMaterials {
    /// vault 身份。
    pub vault_id: VaultId,
    /// 设备身份。
    pub device_id: DeviceId,
    /// Loro peer id。
    pub peer_id: PeerId,
    /// 设备签名私钥。
    pub signing_key: DeviceSigningKey,
    /// 设备 HPKE 私钥。
    pub hpke_key: DeviceHpkeKey,
    /// epoch 1 的根。
    pub root: RootSecretV1,
    /// 恢复码派生的管理私钥。
    pub manager: ManagementKey,
    /// 恢复种子（用于恢复槽；不打印）。
    pub recovery_seed: RecoverySeed,
    /// 展示给用户的恢复码。
    pub recovery_code: Zeroizing<String>,
    /// genesis 设备证书（含 §17.1 管理签名）。
    pub signed_cert: SignedDeviceCert,
    /// 初始 password 槽。
    pub password_slot: KeySlotV1,
    /// 初始 recovery 槽。
    pub recovery_slot: KeySlotV1,
}

impl GenesisMaterials {
    /// 设备证书（不含签名）。
    #[must_use]
    pub fn certificate(&self) -> &DeviceCertV1 {
        &self.signed_cert.cert
    }

    /// 管理公钥（控制链信任锚）。
    #[must_use]
    pub fn manager_pub(&self) -> [u8; 32] {
        self.manager.public_key()
    }

    /// 设备指纹（真机展示用）。
    #[must_use]
    pub fn fingerprint(&self) -> crate::device::DeviceFingerprint {
        self.signed_cert.fingerprint()
    }

    /// 用于 Loro 的 peer id（u64；high bit 清零避免超出 Loro 的 i64 计数器域）。
    ///
    /// 规则与 [`crate::identity::LocalIdentityV1::loro_peer_id`] **同一条**：唯一实现是
    /// [`PeerId::loro_u64`]。app 的 dev-sync 身份与真实 genesis 身份因此不会给出两套
    /// peer 口径（ADR 0009 切片 2）。
    #[must_use]
    pub fn peer_id_u64(&self) -> u64 {
        self.peer_id.loro_u64()
    }

    /// 构造本地身份（instance_id/control_rev 由 genesis 响应填入）。
    pub fn identity(
        &self,
        instance_id: [u8; 16],
        control_rev: u64,
    ) -> Result<crate::identity::LocalIdentityV1, SyncError> {
        Ok(crate::identity::LocalIdentityV1 {
            vault_id: self.vault_id,
            device_id: self.device_id,
            peer_id: self.peer_id,
            ed25519_seed: self.signing_key.seed().clone(),
            x25519_secret: self.hpke_key.secret().clone(),
            current_epoch: self.root.key_epoch,
            current_root: self.root.root.clone(),
            manager_seed: Some(management_seed(&self.recovery_seed, &self.vault_id)?),
            pinned_instance: instance_id,
            pinned_manager: self.manager_pub(),
            control_rev,
            pending_rotation: None,
        })
    }
}

impl core::fmt::Debug for GenesisMaterials {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GenesisMaterials")
            .field("vault_id", &crate::ids::hex_of(&self.vault_id))
            .field("device_id", &crate::ids::hex_of(&self.device_id))
            .field("peer_id", &self.peer_id)
            .field("epoch", &self.root.key_epoch)
            .field("managers", &"<redacted>")
            .finish()
    }
}

/// genesis 响应里的 relay 事实（**不含**本地秘密材料）。
///
/// 拆出来的理由（ADR 0013 S2b）：初始化必须先 durable 记录 pending 材料，再发网络请求；
/// 请求只依赖本地材料，回执只需要这些公开事实，因此"造材料 / 提交请求 / 收事实"可以
/// 分三步走，pending 与身份落盘的先后关系才可对账。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenesisReceipt {
    /// relay 实例 id（pin 用）。
    pub instance_id: [u8; 16],
    /// relay 日志 id。
    pub log_id: [u8; 16],
    /// 活动 epoch。
    pub epoch: u64,
    /// 控制链版本。
    pub control_rev: u64,
    /// 鉴权版本。
    pub auth_rev: u64,
    /// 初始游标（relay 响应键 5；旧版 relay 可能没有）。
    pub initial_cursor: Option<String>,
}

/// genesis 成功后的结果。
pub struct GenesisOutcome {
    /// 本地材料。
    pub materials: GenesisMaterials,
    /// relay 实例 id（pin 用）。
    pub instance_id: [u8; 16],
    /// relay 日志 id。
    pub log_id: [u8; 16],
    /// 活动 epoch。
    pub epoch: u64,
    /// 控制链版本。
    pub control_rev: u64,
    /// 鉴权版本。
    pub auth_rev: u64,
    /// 初始游标（relay 响应键 5；旧版 relay 可能没有）。
    pub initial_cursor: Option<String>,
    /// 已落盘的本地身份。
    pub identity: crate::identity::LocalIdentityV1,
}

impl core::fmt::Debug for GenesisOutcome {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GenesisOutcome")
            .field("materials", &self.materials)
            .field("instance_id", &crate::ids::hex_of(&self.instance_id))
            .field("log_id", &crate::ids::hex_of(&self.log_id))
            .field("epoch", &self.epoch)
            .field("control_rev", &self.control_rev)
            .field("auth_rev", &self.auth_rev)
            .field("has_cursor", &self.initial_cursor.is_some())
            .finish()
    }
}

/// 生成 genesis 材料（不联网；测试/调用方注入 `now` 与随机源）。
pub fn build_materials(
    vault_id: VaultId,
    device_id: DeviceId,
    password: &str,
    now: u64,
    rng: &mut impl RandomSource,
) -> Result<GenesisMaterials, SyncError> {
    let signing_key = DeviceSigningKey::generate(rng)?;
    let hpke_key = DeviceHpkeKey::generate(rng)?;
    let root = RootSecretV1::generate(vault_id, GENESIS_EPOCH, rng)?;
    let recovery_seed = RecoverySeed::generate(rng)?;
    let recovery_code = recovery_seed.to_code()?;
    let manager = ManagementKey::from_recovery(&recovery_seed, &vault_id)?;

    let cert = DeviceCertV1 {
        vault_id,
        device_id,
        sign_ed25519_pub: signing_key.public_key(),
        hpke_x25519_pub: hpke_key.public_key(),
        cert_serial: 1,
        issued_manager_rev: GENESIS_MANAGER_REV,
        rights: default_rights(),
    };
    let signed_cert = sign_device_cert(&cert, &manager)?;

    let peer_hex: [u8; 16] = crate::random::array::<16>(rng)?;
    let peer_id = PeerId::from_bytes(peer_hex);

    let password_slot_id: [u8; 32] = crate::random::array::<32>(rng)?;
    let recovery_slot_id: [u8; 32] = crate::random::array::<32>(rng)?;
    let params = crate::kdf::new_writer_params(rng)?;

    let password_descriptor = SlotDescriptorV1 {
        protocol_version: PROTOCOL_VERSION,
        slot_id: password_slot_id,
        vault_id,
        key_epoch: GENESIS_EPOCH,
        kind: SlotKind::Password,
        wrap_suite: WRAP_SUITE_XCHACHA20_POLY1305,
        kdf: Kdf::Argon2id(params),
        created_at: now,
        manager_rev: GENESIS_MANAGER_REV,
    };
    let recovery_descriptor = SlotDescriptorV1 {
        protocol_version: PROTOCOL_VERSION,
        slot_id: recovery_slot_id,
        vault_id,
        key_epoch: GENESIS_EPOCH,
        kind: SlotKind::Recovery,
        wrap_suite: WRAP_SUITE_XCHACHA20_POLY1305,
        kdf: Kdf::RecoveryHkdf,
        created_at: now,
        manager_rev: GENESIS_MANAGER_REV,
    };

    let slot_aad = |descriptor: &SlotDescriptorV1,
                    nonce: &[u8; 24],
                    plaintext_len: usize|
     -> Result<Vec<u8>, SyncError> {
        let mut message_id = [0u8; 16];
        message_id.copy_from_slice(&descriptor.slot_id[..16]);
        let header = EnvelopeHeader {
            protocol_version: PROTOCOL_VERSION,
            header_len: 196,
            object_type: ObjectType::KeySlot,
            cipher_suite: 1,
            flags: 0,
            vault_id: descriptor.vault_id,
            object_id: descriptor.slot_id,
            message_id,
            producer_device_id: device_id,
            generation: 0,
            key_epoch: descriptor.key_epoch,
            sender_seq: 0,
            part_index: 0,
            part_count: 1,
            total_plaintext_len: plaintext_len as u64,
            this_plaintext_len: u32::try_from(plaintext_len)
                .map_err(|_| SyncError::Descriptor("根明文长度超出 u32"))?,
            ciphertext_len: u32::try_from(plaintext_len + 16)
                .map_err(|_| SyncError::Descriptor("槽密文长度超出 u32"))?,
            context_hash: context_hash(descriptor)?,
            nonce: *nonce,
        };
        Ok(header.aad(&descriptor.encode()?)?)
    };

    let password_slot = KeySlotV1::wrap_password(
        &password_descriptor,
        password.as_bytes(),
        &root,
        &slot_aad,
        rng,
    )?;
    let recovery_slot =
        KeySlotV1::wrap_recovery(&recovery_descriptor, &recovery_seed, &root, &slot_aad, rng)?;

    Ok(GenesisMaterials {
        vault_id,
        device_id,
        peer_id,
        signing_key,
        hpke_key,
        root,
        manager,
        recovery_seed,
        recovery_code,
        signed_cert,
        password_slot,
        recovery_slot,
    })
}

/// genesis 请求的 CBOR 字段：`{0:vault_id,1:device_id,2:sign_pk,3:hpke_pk,4:manager_pub,
/// 5:setup_secret,6:certificate,7:certificate_signature}`（relay `api.rs` create_vault）。
pub fn genesis_fields(
    materials: &GenesisMaterials,
    setup_secret: &str,
) -> Result<cbor::Fields, SyncError> {
    Ok(vec![
        (0, cbor::bytes(materials.vault_id)),
        (1, cbor::bytes(materials.device_id)),
        (2, cbor::bytes(materials.certificate().sign_ed25519_pub)),
        (3, cbor::bytes(materials.certificate().hpke_x25519_pub)),
        (4, cbor::bytes(materials.manager_pub())),
        (5, cbor::text(setup_secret)),
        (6, cbor::bytes(materials.signed_cert.cert_bytes()?)),
        (7, cbor::bytes(materials.signed_cert.signature)),
    ])
}

/// genesis 请求体（CBOR；测试 golden 用）。
pub fn genesis_request_bytes(
    materials: &GenesisMaterials,
    setup_secret: &str,
) -> Result<Vec<u8>, SyncError> {
    cbor::encode(genesis_fields(materials, setup_secret)?)
}

/// rev1 控制记录摘要：`record_hash = SHA256(零32 || 设备证书)`（ADR 0013 §3 / §35）。
///
/// 与 relay `vault::init` 里插入 rev1 控制记录时用的公式**逐字节相同**：第一条控制记录的
/// `prev_hash` 是 32 字节全零，`signed_record` 就是 genesis 设备证书的 CDE 字节。客户端在
/// **发请求之前**就能用本地提交的证书复算它，因此 Create 可以把"rev1 强 pin"作为
/// 第一次联网的信任起点，而不需要任何 unpinned 消费。
///
/// 证书编码失败（CDE 契约）返回 [`SyncError::Proto`]，不做任何降级。
pub fn rev1_record_hash(
    certificate: &menghunote_sync_proto::DeviceCertV1,
) -> Result<[u8; 32], SyncError> {
    let cert_bytes = certificate.encode()?;
    let mut input = Vec::with_capacity(32 + cert_bytes.len());
    input.extend_from_slice(&[0u8; 32]);
    input.extend_from_slice(&cert_bytes);
    Ok(Sha256::digest(&input).into())
}

/// v2 创建票据请求的 CBOR 字段：`{0:ticket, 1..7: 与 v1 genesis 同序}`。
///
/// 与 v1 的唯一差别是**授权字段**：v2 没有 setup secret，键 0 是 32B 创建票据，
/// 原有字段整体后移一位（relay `api::create_vault_v2` 逐字段对齐）。
pub fn ticket_genesis_fields(
    materials: &GenesisMaterials,
    ticket: &[u8; 32],
) -> Result<cbor::Fields, SyncError> {
    Ok(vec![
        (0, cbor::bytes(*ticket)),
        (1, cbor::bytes(materials.vault_id)),
        (2, cbor::bytes(materials.device_id)),
        (3, cbor::bytes(materials.certificate().sign_ed25519_pub)),
        (4, cbor::bytes(materials.certificate().hpke_x25519_pub)),
        (5, cbor::bytes(materials.manager_pub())),
        (6, cbor::bytes(materials.signed_cert.cert_bytes()?)),
        (7, cbor::bytes(materials.signed_cert.signature)),
    ])
}

/// v2 创建票据请求体（CBOR；测试 golden 用）。
pub fn ticket_genesis_request_bytes(
    materials: &GenesisMaterials,
    ticket: &[u8; 32],
) -> Result<Vec<u8>, SyncError> {
    cbor::encode(ticket_genesis_fields(materials, ticket)?)
}

/// 对 relay 执行 genesis：`POST /v1/vaults`，并返回回执（不含本地材料）。
///
/// S2b 的受限初始化入口用它把"本地材料已 durable 记录"与"远端已创建"分成两步；
/// 传统入口 [`run`] 只是"造材料 + 调本函数"。
pub fn submit<T: HttpTransport>(
    transport: &mut T,
    materials: &GenesisMaterials,
    setup_secret: &str,
) -> Result<GenesisReceipt, SyncError> {
    let body = genesis_request_bytes(materials, setup_secret)?;
    let path = crate::api_version::RelayApi::new("", crate::api_version::RelayApiVersion::V1)
        .create_vault_path();
    submit_request(transport, path, body)
}

/// 对 relay 执行 **v2** 票据创建：`POST /v2/vaults`（ADR 0014 S3a）。
///
/// 授权来自 32B 一次性票据（只在内存里；不落库/日志/配置）。响应与 v1 create 同形，
/// 因此回执解析复用 [`submit_request`]；路径来自统一 [`RelayApi`] 生成器。
pub fn submit_with_ticket<T: HttpTransport>(
    transport: &mut T,
    materials: &GenesisMaterials,
    ticket: &[u8; 32],
) -> Result<GenesisReceipt, SyncError> {
    let body = ticket_genesis_request_bytes(materials, ticket)?;
    let path = crate::api_version::RelayApi::new("", crate::api_version::RelayApiVersion::V2)
        .create_vault_path();
    submit_request(transport, path, body)
}

/// 发送创建请求并解析同形回执（v1/v2 共用，避免两份解析）。
fn submit_request<T: HttpTransport>(
    transport: &mut T,
    path: String,
    body: Vec<u8>,
) -> Result<GenesisReceipt, SyncError> {
    let response: HttpResponse = transport.send(&HttpRequest {
        method: "POST",
        path,
        headers: vec![("content-type".to_owned(), CONTENT_TYPE_CBOR.to_owned())],
        body,
    })?;
    if !(200..300).contains(&response.status) {
        return Err(error_from_response(&response));
    }
    let fields = cbor::decode(&response.body)?;
    Ok(GenesisReceipt {
        instance_id: cbor::bytes_fixed(&fields, 0)?,
        log_id: cbor::bytes_fixed(&fields, 1)?,
        epoch: cbor::uint_field(&fields, 2)?,
        control_rev: cbor::uint_field(&fields, 3)?,
        auth_rev: cbor::uint_field(&fields, 4)?,
        initial_cursor: cbor::opt_text_field(&fields, 5)?,
    })
}

/// 对 relay 执行 genesis：`POST /v1/vaults`。
pub fn run<T: HttpTransport>(
    transport: &mut T,
    setup_secret: &str,
    password: &str,
    now: u64,
    rng: &mut impl RandomSource,
) -> Result<GenesisOutcome, SyncError> {
    let vault_id: VaultId = crate::random::array::<16>(rng)?;
    let device_id: DeviceId = crate::random::array::<16>(rng)?;
    let materials = build_materials(vault_id, device_id, password, now, rng)?;
    let receipt = submit(transport, &materials, setup_secret)?;
    let identity = materials.identity(receipt.instance_id, receipt.control_rev)?;
    Ok(GenesisOutcome {
        materials,
        instance_id: receipt.instance_id,
        log_id: receipt.log_id,
        epoch: receipt.epoch,
        control_rev: receipt.control_rev,
        auth_rev: receipt.auth_rev,
        initial_cursor: receipt.initial_cursor,
        identity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRng;
    use menghunote_sync_proto::device_cert_signed_bytes;

    fn materials(seed: u64) -> GenesisMaterials {
        let mut rng = TestRng::new(seed);
        build_materials(
            [0x5a; 16],
            [0x04; 16],
            "correct horse",
            1_700_000_000,
            &mut rng,
        )
        .expect("genesis 材料")
    }

    #[test]
    fn genesis_request_layout_matches_relay_fields() {
        let materials = materials(11);
        let bytes = genesis_request_bytes(&materials, "setup-secret").expect("请求");
        let fields = cbor::decode(&bytes).expect("解码");
        // 键必须 0..=7 连续（可选字段在 genesis 里都是必填）。
        assert_eq!(fields.len(), 8);
        for (index, (key, _)) in fields.iter().enumerate() {
            assert_eq!(*key, index as u64);
        }
        assert_eq!(cbor::bytes_fixed::<16>(&fields, 0).unwrap(), [0x5a; 16]);
        assert_eq!(cbor::bytes_fixed::<16>(&fields, 1).unwrap(), [0x04; 16]);
        assert_eq!(
            cbor::bytes_fixed::<32>(&fields, 2).unwrap(),
            materials.certificate().sign_ed25519_pub
        );
        assert_eq!(
            cbor::bytes_fixed::<32>(&fields, 4).unwrap(),
            materials.manager_pub()
        );
        assert_eq!(cbor::text_field(&fields, 5).unwrap(), "setup-secret");
        assert_eq!(
            cbor::bytes_field(&fields, 6).unwrap(),
            materials.signed_cert.cert_bytes().unwrap()
        );
        assert_eq!(
            cbor::bytes_fixed::<64>(&fields, 7).unwrap(),
            materials.signed_cert.signature
        );
    }

    #[test]
    fn certificate_is_verifiable_by_recovery_chain() {
        let materials = materials(12);
        let signed_input = device_cert_signed_bytes(materials.certificate()).expect("待签");
        crate::device::verify_signature(
            &materials.manager_pub(),
            &signed_input,
            &materials.signed_cert.signature,
        )
        .expect("管理签名");
        // 恢复码 → 管理 key 的链路也必须自洽。
        let parsed = RecoverySeed::from_code(&materials.recovery_code).expect("恢复码");
        let manager = ManagementKey::from_recovery(&parsed, &materials.vault_id).expect("管理密钥");
        assert_eq!(manager.public_key(), materials.manager_pub());
        materials
            .signed_cert
            .verify(&manager.public_key())
            .expect("证书链");
    }

    #[test]
    fn initial_slots_unwrap_to_the_same_root() {
        let materials = materials(13);
        let password = materials
            .password_slot
            .unwrap_password(b"correct horse", &slot_aad_for(&materials))
            .expect("口令槽");
        assert!(password.ct_eq(&materials.root));
        let recovery = materials
            .recovery_slot
            .unwrap_recovery(&materials.recovery_seed, &slot_aad_for(&materials))
            .expect("恢复槽");
        assert!(recovery.ct_eq(&materials.root));
        // 错口令必须失败（Argon2id 派生后 AEAD 拒绝）。
        assert!(materials
            .password_slot
            .unwrap_password(b"wrong", &slot_aad_for(&materials))
            .is_err());
    }

    fn slot_aad_for(
        materials: &GenesisMaterials,
    ) -> impl Fn(&SlotDescriptorV1, &[u8; 24], usize) -> Result<Vec<u8>, SyncError> {
        let device_id = materials.device_id;
        move |descriptor: &SlotDescriptorV1, nonce: &[u8; 24], plaintext_len: usize| {
            let mut message_id = [0u8; 16];
            message_id.copy_from_slice(&descriptor.slot_id[..16]);
            let header = EnvelopeHeader {
                protocol_version: PROTOCOL_VERSION,
                header_len: 196,
                object_type: ObjectType::KeySlot,
                cipher_suite: 1,
                flags: 0,
                vault_id: descriptor.vault_id,
                object_id: descriptor.slot_id,
                message_id,
                producer_device_id: device_id,
                generation: 0,
                key_epoch: descriptor.key_epoch,
                sender_seq: 0,
                part_index: 0,
                part_count: 1,
                total_plaintext_len: plaintext_len as u64,
                this_plaintext_len: u32::try_from(plaintext_len).expect("长度"),
                ciphertext_len: u32::try_from(plaintext_len + 16).expect("长度"),
                context_hash: context_hash(descriptor)?,
                nonce: *nonce,
            };
            Ok(header.aad(&descriptor.encode()?)?)
        }
    }

    /// rev1 控制记录摘要：**已知证书向量**（与 relay `vault::init` 的公式逐字节一致）。
    ///
    /// 固定一份证书的 CDE 编码 → `SHA256(零32 || cert)`；期望值是常量，因此任何对证书
    /// 编码/公式的静默修改（哪怕只改一个字段编号）都会让本测试失败。
    #[test]
    fn rev1_record_hash_has_a_known_answer() {
        let cert = menghunote_sync_proto::DeviceCertV1 {
            vault_id: [0x11; 16],
            device_id: [0x22; 16],
            sign_ed25519_pub: [0x33; 32],
            hpke_x25519_pub: [0x44; 32],
            cert_serial: 7,
            issued_manager_rev: 1,
            rights: default_rights(),
        };
        let hash = rev1_record_hash(&cert).expect("rev1 摘要");
        assert_eq!(
            crate::ids::hex_of(&hash),
            "c0cfd56ee351ad4cb96d9070734f63d04e8e27539cbf3dd551b9f4981eaec8e4",
            "已知证书向量的 rev1 record_hash 变了：公式/证书编码被改动"
        );
        // 公式本身就是 `SHA256(零32 || CDE(cert))`：用独立实现复算一遍。
        let mut input = vec![0u8; 32];
        input.extend_from_slice(&cert.encode().expect("证书编码"));
        let expected: [u8; 32] = Sha256::digest(&input).into();
        assert_eq!(hash, expected);
        // 换任何一个字段都必须变（防止常量折叠式的假实现）。
        let mut other = cert.clone();
        other.cert_serial = 8;
        assert_ne!(rev1_record_hash(&other).expect("摘要"), hash);
    }

    #[test]
    fn identity_roundtrips_and_pins_manager() {
        let materials = materials(14);
        let identity = materials.identity([0x22; 16], 1).expect("身份");
        assert_eq!(identity.pinned_manager, materials.manager_pub());
        assert_eq!(identity.pinned_instance, [0x22; 16]);
        let encoded = identity.encode().expect("编码");
        let decoded = crate::identity::LocalIdentityV1::decode(&encoded).expect("解码");
        assert_eq!(decoded.vault_id, identity.vault_id);
        assert_eq!(decoded.device_id, identity.device_id);
        assert!(decoded.current_root.ct_eq(&identity.current_root));
    }

    #[test]
    fn secret_types_do_not_leak_in_debug() {
        let materials = materials(15);
        let text = format!("{materials:?}");
        assert!(text.contains("<redacted>"));
        assert!(!text.contains(&materials.recovery_code.to_string().replace("mnrec1", "")));
    }
}

`````



@@SECTION code/031 `crates/sync/src/hpke.rs`


`````rust
//! HPKE（RFC 9180）base mode：把 epoch 根封装给指定设备的 X25519 公钥（ADR 0006 §4/§17）。
//!
//! 冻结参数：
//!
//! | 项 | 值 |
//! | --- | --- |
//! | mode | base (0x00) |
//! | KEM | DHKEM(X25519, HKDF-SHA256) = 0x0020 |
//! | KDF | HKDF-SHA256 = 0x0001 |
//! | AEAD | ChaCha20Poly1305 = 0x0003 |
//! | info | `b"MN/v1/epoch-grant\0"`（`sync-proto::HPKE_INFO`，末尾真正的 0x00） |
//! | AAD | `CDE(GrantAadV1)`（`sync-proto`，§17 冻结） |
//!
//! **不自造 nonce**：AEAD nonce 由 HPKE 按 RFC 9180 §5.2 从 `base_nonce` 与序列号派生，
//! 本文件不接触 nonce 字节。实现用成熟 crate `hpke` 0.15.1（RustCrypto），并附 RFC 9180
//! 附录 A.2 的 known-answer 向量测试。
//!
//! 外层管理签名覆盖 `AAD || enc32 || SHA256(hpke_ciphertext)`（ADR §17），待签字节由
//! `sync-proto` 的 `GrantAadV1::grant_signed_bytes` 生成；签名本身由 [`crate::device`] 做。

use crate::domain;
use crate::error::SyncError;
use crate::keys::{RootSecretV1, Secret32};
use hpke::kem::X25519HkdfSha256;
use hpke::{
    aead::ChaCha20Poly1305, kdf::HkdfSha256, Deserializable, Kem as KemTrait, OpModeR, OpModeS,
    Serializable,
};
use menghunote_sync_proto::GrantAadV1;
use sha2::{Digest, Sha256};

/// HPKE KEM：DHKEM(X25519, HKDF-SHA256)（RFC 9180 值 0x0020）。
pub type GrantKem = X25519HkdfSha256;
/// HPKE KDF：HKDF-SHA256（0x0001）。
pub type GrantKdf = HkdfSha256;
/// HPKE AEAD：ChaCha20Poly1305（0x0003）。
pub type GrantAead = ChaCha20Poly1305;

/// HPKE info 域（ADR §4；`sync-proto` 的常量，末尾是一个 0x00 字节）。
pub const HPKE_INFO: &[u8] = domain::HPKE_INFO;

/// 一次封装的结果：`enc`（32B 临时公钥）+ HPKE 密文。
///
/// `enc` 与密文都**不是秘密**（密文只有收件设备能解）。
#[derive(Clone, PartialEq, Eq)]
pub struct SealedEpochGrant {
    /// HPKE 封装出的临时 X25519 公钥（RFC 9180 `enc`）。
    pub enc: [u8; 32],
    /// HPKE 密文（含 AEAD tag）。
    pub ciphertext: Vec<u8>,
}

impl SealedEpochGrant {
    /// 管理签名的待签字节：`AAD || enc32 || SHA256(hpke_ciphertext)`（ADR §17）。
    pub fn signature_input(&self, aad: &GrantAadV1) -> Result<Vec<u8>, SyncError> {
        grant_signature_input(aad, &self.enc, &self.ciphertext)
    }
}

/// ADR §17 的管理签名覆盖字节：`AAD || enc32 || SHA256(hpke_ciphertext)`。
///
/// AAD 是 `CDE(GrantAadV1)`；签名算法（Ed25519）与调用点由 [`crate::device`] 负责。
pub fn grant_signature_input(
    aad: &GrantAadV1,
    enc: &[u8; 32],
    ciphertext: &[u8],
) -> Result<Vec<u8>, SyncError> {
    let digest: [u8; 32] = Sha256::digest(ciphertext).into();
    Ok(aad.grant_signed_bytes(enc, &digest)?)
}

/// 由 32 字节私钥材料派生 X25519 公钥（RFC 9180 §7.1 `DeriveKeyPair`）。
#[must_use]
pub fn x25519_public_key(secret: &Secret32) -> [u8; 32] {
    let (_sk, pk) = GrantKem::derive_keypair(secret.expose_slice());
    let bytes = pk.to_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(bytes.as_slice());
    out
}

fn hpke_public_key(bytes: &[u8; 32]) -> Result<<GrantKem as KemTrait>::PublicKey, SyncError> {
    <GrantKem as KemTrait>::PublicKey::from_bytes(bytes)
        .map_err(|_| SyncError::Hpke("收件人 X25519 公钥无效"))
}

fn hpke_encapped_key(bytes: &[u8; 32]) -> Result<<GrantKem as KemTrait>::EncappedKey, SyncError> {
    <GrantKem as KemTrait>::EncappedKey::from_bytes(bytes)
        .map_err(|_| SyncError::Hpke("HPKE enc 长度/格式无效"))
}

fn hpke_private_key(secret: &Secret32) -> <GrantKem as KemTrait>::PrivateKey {
    let (sk, _pk) = GrantKem::derive_keypair(secret.expose_slice());
    sk
}

/// 内部封装：`info` 可注入，只用于 RFC 9180 known-answer 测试。
fn seal_raw(
    recipient_x25519_pub: &[u8; 32],
    info: &[u8],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<SealedEpochGrant, SyncError> {
    let pk = hpke_public_key(recipient_x25519_pub)?;
    let (enc, ciphertext) = hpke::single_shot_seal::<GrantAead, GrantKdf, GrantKem>(
        &OpModeS::Base,
        &pk,
        info,
        plaintext,
        aad,
    )
    .map_err(|_| SyncError::Hpke("HPKE 封装失败"))?;
    let enc_bytes = enc.to_bytes();
    let mut enc_out = [0u8; 32];
    enc_out.copy_from_slice(enc_bytes.as_slice());
    Ok(SealedEpochGrant {
        enc: enc_out,
        ciphertext,
    })
}

/// 内部解封：`info` 可注入，只用于 RFC 9180 known-answer 测试。
fn open_raw(
    recipient_secret: &Secret32,
    enc: &[u8; 32],
    info: &[u8],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, SyncError> {
    let sk = hpke_private_key(recipient_secret);
    let encapped = hpke_encapped_key(enc)?;
    hpke::single_shot_open::<GrantAead, GrantKdf, GrantKem>(
        &OpModeR::Base,
        &sk,
        &encapped,
        info,
        ciphertext,
        aad,
    )
    .map_err(|_| SyncError::Hpke("HPKE 解封失败"))
}

/// 把 epoch 根封装给收件设备（ADR §4/§17）。
///
/// 明文是 `RootSecretV1` 的 CDE；AAD 必须与收件设备公钥一致，否则拒绝。
pub fn seal_epoch_grant(
    recipient_x25519_pub: &[u8; 32],
    root: &RootSecretV1,
    aad: &GrantAadV1,
) -> Result<SealedEpochGrant, SyncError> {
    aad.validate()?;
    if &aad.recipient_x25519_pub != recipient_x25519_pub {
        return Err(SyncError::RecipientMismatch);
    }
    if aad.vault_id != root.vault_id || aad.epoch != root.key_epoch {
        return Err(SyncError::Hpke("grant AAD 与根归属不一致"));
    }
    let aad_bytes = aad.aad_bytes()?;
    // 明文根只在这层存在一瞬间：封装后立即零化。
    let plaintext = zeroize::Zeroizing::new(root.encode()?);
    seal_raw(recipient_x25519_pub, HPKE_INFO, &plaintext, &aad_bytes)
}

/// 解封 epoch grant（收件设备侧）。
///
/// 校验顺序：AAD 的收件人公钥 == 本机派生公钥（错 recipient 立即失败）→ HPKE 解封 →
/// 解析 `RootSecretV1` → 根归属必须与 AAD 一致。
pub fn open_epoch_grant(
    recipient_secret: &Secret32,
    enc: &[u8; 32],
    aad: &GrantAadV1,
    ciphertext: &[u8],
) -> Result<RootSecretV1, SyncError> {
    aad.validate()?;
    if x25519_public_key(recipient_secret) != aad.recipient_x25519_pub {
        return Err(SyncError::RecipientMismatch);
    }
    let aad_bytes = aad.aad_bytes()?;
    let plaintext = zeroize::Zeroizing::new(open_raw(
        recipient_secret,
        enc,
        HPKE_INFO,
        &aad_bytes,
        ciphertext,
    )?);
    let root = RootSecretV1::decode(&plaintext)?;
    if root.vault_id != aad.vault_id || root.key_epoch != aad.epoch {
        return Err(SyncError::Hpke("grant 载荷与 AAD 归属不一致"));
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRng;

    fn hex<const N: usize>(s: &str) -> [u8; N] {
        crate::ids::parse_hex::<N>(s).unwrap()
    }

    /// RFC 9180 附录 A.2：DHKEM(X25519, HKDF-SHA256) + HKDF-SHA256 + ChaCha20Poly1305，
    /// base mode，sequence number 0。
    ///
    /// 这证明我们选用的 ciphersuite（0x0020/0x0001/0x0003）与 AAD/明文处理是 RFC 一致的；
    /// 我们自己的 `info` 域不同，所以这里走 `open_raw`。
    ///
    /// 注意用 RFC 的 **ikmR**（而不是 skRm）作为输入：生产的 X25519 私钥就是从 32B ikm
    /// 经 `DeriveKeyPair` 得到的，这样 KAT 覆盖的正是真实代码路径（pk 必须等于 RFC 的 pkRm）。
    #[test]
    fn rfc9180_a2_known_answer() {
        let ikm_r = hex::<32>("1ac01f181fdf9f352797655161c58b75c656a6cc2716dcb66372da835542e1df");
        let enc = hex::<32>("1afa08d3dec047a643885163f1180476fa7ddb54c6a8029ea33f95796bf2ac4a");
        let info = hex::<20>("4f6465206f6e2061204772656369616e2055726e");
        let ct = hex::<45>(
            "1c5250d8034ec2b784ba2cfd69dbdb8af406cfe3ff938e131f0def8c8b60b4db21993c62ce81883d2dd1b51a28",
        );
        let secret = Secret32::from_array(ikm_r);
        let opened = open_raw(&secret, &enc, &info, b"Count-0", &ct).unwrap();
        assert_eq!(
            opened, b"Beauty is truth, truth beauty",
            "必须与 RFC 9180 A.2.1.1 的明文一致"
        );
    }

    fn sample_aad(rng: &mut TestRng) -> (Secret32, GrantAadV1) {
        let secret = Secret32::generate(rng).unwrap();
        let pubkey = x25519_public_key(&secret);
        (
            secret,
            GrantAadV1 {
                pv: 1,
                vault_id: [1u8; 16],
                epoch: 7,
                recipient_device_id: [2u8; 16],
                recipient_x25519_pub: pubkey,
                manager_rev: 3,
                grant_id: [9u8; 16],
            },
        )
    }

    #[test]
    fn seal_open_roundtrip() {
        let mut rng = TestRng::new(21);
        let (secret, aad) = sample_aad(&mut rng);
        let root = RootSecretV1::generate(aad.vault_id, aad.epoch, &mut rng).unwrap();

        let sealed = seal_epoch_grant(&aad.recipient_x25519_pub, &root, &aad).unwrap();
        assert_eq!(sealed.enc.len(), 32);
        assert!(sealed.ciphertext.len() > 32);
        assert_ne!(sealed.ciphertext, root.root.expose_slice());

        let opened = open_epoch_grant(&secret, &sealed.enc, &aad, &sealed.ciphertext).unwrap();
        assert!(root.ct_eq(&opened));
    }

    #[test]
    fn tampered_ciphertext_or_aad_or_enc_rejected() {
        let mut rng = TestRng::new(22);
        let (secret, aad) = sample_aad(&mut rng);
        let root = RootSecretV1::generate(aad.vault_id, aad.epoch, &mut rng).unwrap();
        let sealed = seal_epoch_grant(&aad.recipient_x25519_pub, &root, &aad).unwrap();

        // 密文任意一位翻转
        let mut ct = sealed.ciphertext.clone();
        ct[0] ^= 0x01;
        assert!(open_epoch_grant(&secret, &sealed.enc, &aad, &ct).is_err());

        // enc 翻转
        let mut enc = sealed.enc;
        enc[0] ^= 0x01;
        assert!(open_epoch_grant(&secret, &enc, &aad, &sealed.ciphertext).is_err());

        // AAD 任一字段变化
        let mut bad = aad.clone();
        bad.manager_rev += 1;
        assert!(open_epoch_grant(&secret, &sealed.enc, &bad, &sealed.ciphertext).is_err());
        let mut bad = aad.clone();
        bad.grant_id[0] ^= 0x01;
        assert!(open_epoch_grant(&secret, &sealed.enc, &bad, &sealed.ciphertext).is_err());
        let mut bad = aad.clone();
        bad.epoch += 1;
        assert!(open_epoch_grant(&secret, &sealed.enc, &bad, &sealed.ciphertext).is_err());
        let mut bad = aad.clone();
        bad.recipient_device_id = [3u8; 16];
        assert!(open_epoch_grant(&secret, &sealed.enc, &bad, &sealed.ciphertext).is_err());
        let mut bad = aad.clone();
        bad.pv = 2;
        assert!(open_epoch_grant(&secret, &sealed.enc, &bad, &sealed.ciphertext).is_err());

        // 截断
        assert!(open_epoch_grant(&secret, &sealed.enc, &aad, &sealed.ciphertext[..10]).is_err());
    }

    #[test]
    fn wrong_recipient_rejected() {
        let mut rng = TestRng::new(23);
        let (_secret, aad) = sample_aad(&mut rng);
        let root = RootSecretV1::generate(aad.vault_id, aad.epoch, &mut rng).unwrap();
        let sealed = seal_epoch_grant(&aad.recipient_x25519_pub, &root, &aad).unwrap();

        // 另一台设备的私钥：AAD 里的公钥对不上 → RecipientMismatch
        let other_secret = Secret32::generate(&mut rng).unwrap();
        assert!(matches!(
            open_epoch_grant(&other_secret, &sealed.enc, &aad, &sealed.ciphertext),
            Err(SyncError::RecipientMismatch)
        ));

        // 如果 AAD 被改成另一台设备的公钥，HPKE 解封本身失败
        let other_pub = x25519_public_key(&other_secret);
        let mut forged = aad.clone();
        forged.recipient_x25519_pub = other_pub;
        assert!(open_epoch_grant(&other_secret, &sealed.enc, &forged, &sealed.ciphertext).is_err());
    }

    #[test]
    fn seal_rejects_inconsistent_aad() {
        let mut rng = TestRng::new(24);
        let (_secret, aad) = sample_aad(&mut rng);
        let root = RootSecretV1::generate(aad.vault_id, aad.epoch, &mut rng).unwrap();
        // 收件人公钥与 AAD 不一致
        let mut wrong_aad = aad.clone();
        wrong_aad.recipient_x25519_pub = [0x77; 32];
        assert!(matches!(
            seal_epoch_grant(&aad.recipient_x25519_pub, &root, &wrong_aad),
            Err(SyncError::RecipientMismatch)
        ));
        // 根归属与 AAD 不一致
        let other_root = RootSecretV1::generate([8u8; 16], 7, &mut rng).unwrap();
        assert!(seal_epoch_grant(&aad.recipient_x25519_pub, &other_root, &aad).is_err());
    }

    #[test]
    fn aad_cde_roundtrip_and_field_bound() {
        let mut rng = TestRng::new(25);
        let (_secret, aad) = sample_aad(&mut rng);
        let bytes = aad.encode().unwrap();
        let back = GrantAadV1::decode(&bytes).unwrap();
        assert_eq!(aad, back);
        assert_eq!(bytes, back.encode().unwrap());
        assert_eq!(bytes[0], 0xa7, "7 项 map 头（0xa7）");
        // 末尾是 grant_id（16B）
        assert_eq!(&bytes[bytes.len() - 16..], &[9u8; 16]);
        // 尾部多余
        let mut long = bytes;
        long.push(0);
        assert!(GrantAadV1::decode(&long).is_err());
    }

    #[test]
    fn aad_unknown_field_rejected() {
        // 8 项 map → 字段数不符
        let value = menghunote_sync_proto::CdeValue::Map(vec![
            (0, menghunote_sync_proto::CdeValue::UInt(1)),
            (1, menghunote_sync_proto::CdeValue::Bytes(vec![0; 16])),
            (2, menghunote_sync_proto::CdeValue::UInt(7)),
            (3, menghunote_sync_proto::CdeValue::Bytes(vec![0; 16])),
            (4, menghunote_sync_proto::CdeValue::Bytes(vec![0; 32])),
            (5, menghunote_sync_proto::CdeValue::UInt(3)),
            (6, menghunote_sync_proto::CdeValue::Bytes(vec![0; 16])),
            (7, menghunote_sync_proto::CdeValue::UInt(0)),
        ]);
        let bytes = menghunote_sync_proto::encode_cde(&value).unwrap();
        assert!(GrantAadV1::decode(&bytes).is_err());
    }

    #[test]
    fn signature_input_matches_adr_coverage() {
        let enc = [0x11u8; 32];
        let ct = b"ciphertext";
        let (_secret, aad) = sample_aad(&mut TestRng::new(1));
        let input = grant_signature_input(&aad, &enc, ct).unwrap();
        let aad_bytes = aad.aad_bytes().unwrap();
        assert_eq!(&input[..aad_bytes.len()], aad_bytes.as_slice());
        assert_eq!(&input[aad_bytes.len()..aad_bytes.len() + 32], &enc);
        assert_eq!(
            &input[aad_bytes.len() + 32..],
            Sha256::digest(ct).as_slice()
        );
        // 覆盖范围变化必须改变待签字节
        let mut ct2 = ct.to_vec();
        ct2.push(0);
        assert_ne!(input, grant_signature_input(&aad, &enc, &ct2).unwrap());

        // `SealedEpochGrant::signature_input` 与自由函数一致
        let sealed = SealedEpochGrant {
            enc,
            ciphertext: ct.to_vec(),
        };
        assert_eq!(
            sealed.signature_input(&aad).unwrap(),
            grant_signature_input(&aad, &enc, ct).unwrap()
        );
    }

    #[test]
    fn x25519_public_key_is_derived_deterministically() {
        let mut rng = TestRng::new(26);
        let secret = Secret32::generate(&mut rng).unwrap();
        let pub_a = x25519_public_key(&secret);
        let pub_b = x25519_public_key(&Secret32::from_array(*secret.expose()));
        assert_eq!(pub_a, pub_b);
        assert_ne!(pub_a, [0u8; 32]);
    }
}

`````



@@SECTION code/032 `crates/sync/src/identity.rs`


`````rust
//! 本地身份 BLOB（ADR 0006 §3），只经 [`crate::identity_wrap`] 封装后落盘。
//!
//! Windows 为 DPAPI（CurrentUser）；Linux 为用户主密钥 + XChaCha20-Poly1305
//! （外层格式见 ADR 0008）。两种平台的 entropy 口径相同：
//! `b"MN/dpapi/v1\0" || vault_id || device_id`。
//!
//! 结构（整数键 0..11，optional 省略，编码用 `sync-proto` 的 CDE）：
//!
//! ```text
//! 0 vault_id           16B
//! 1 device_id          16B
//! 2 peer_id            16B   （Loro peer；克隆/恢复入口必须换新）
//! 3 ed25519_seed       32B   （设备签名私钥种子）
//! 4 x25519_secret      32B   （HPKE 私钥材料）
//! 5 current_epoch      UInt
//! 6 current_root       32B   （当前 epoch 根）
//! 7 manager_seed?      32B   （仅管理设备）
//! 8 pinned_instance    16B   （relay instance id，防换实例/回滚）
//! 9 pinned_manager     32B   （管理公钥）
//! 10 control_rev       UInt
//! 11 pending_rotation? map   （轮换中的候选根材料，ADR §8 步 1）
//! ```
//!
//! 轮换后的**历史根 / 管理 rev** 不在上面这份固定清单里：它们单独存
//! [`IDENTITY_ROTATION_STATE_META`] 的同 entropy DPAPI 记录（[`IdentityRotationStateV1`]）。
//! 原因有二：
//!
//! - 身份 BLOB 的字段号是 `0..11` 的严格清单，新增字段会破坏仓内其它结构体字面量构造点；
//! - 历史根只服务于"旧 epoch outbox 重包"这一跨重启需求，生命周期比身份 BLOB 短
//!   （repack 完成后可清），独立记录更容易做崩溃一致性（见 promotion 的写入顺序）。
//!
//! 这是**本地**格式（DPAPI 内层，不上 wire），但沿用 CDE 编码规则：确定性、整数键升序、
//! 定长字节用 byte string。秘密字段在编码后立即擦除中间 `CdeValue` 副本；比 wire 记录
//! 多一道零化，因为这里装的都是长期私钥。
//!
//! 跨用户/跨 SID 的 DPAPI 失败是预期行为，见 [`crate::dpapi`] 模块文档；
//! Linux 主钥文件被删除/权限被改坏时同样显式失败（`LocalKey*`），不回退 DPAPI/明文。

use crate::dpapi;
use crate::error::SyncError;
use crate::identity_wrap;
use crate::ids::PeerId;
use crate::keys::{RootSecretV1, Secret32};
use crate::{DeviceId, VaultId};
use core::iter::Peekable;
use menghunote_storage::Vault;
use menghunote_sync_proto::CdeValue;
use std::vec::IntoIter;
use zeroize::Zeroizing;

/// 轮换中的候选材料（ADR §8 步 1：先 durable 保存 pending root/槽/grant 材料）。
///
/// 本地结构，字段可扩展；`candidate_root` 与正式根一样按秘密处理。
#[derive(Clone)]
pub struct PendingRotationV1 {
    /// 目标 epoch（单调递增，废弃 epoch 永不复用）。
    pub to_epoch: u64,
    /// relay 侧的 rotation ID（16B）。
    pub rotation_id: [u8; 16],
    /// 候选 epoch 的随机根（**绝不能用旧根/旧 password/旧 R 封装**）。
    pub candidate_root: Secret32,
    /// 落盘时刻（Unix 秒），用于过期回收。
    pub staged_at: u64,
}

impl PendingRotationV1 {
    fn to_cde(&self) -> CdeValue {
        CdeValue::Map(vec![
            (0, CdeValue::UInt(self.to_epoch)),
            (1, CdeValue::Bytes(self.rotation_id.to_vec())),
            (
                2,
                CdeValue::Bytes(self.candidate_root.expose_slice().to_vec()),
            ),
            (3, CdeValue::UInt(self.staged_at)),
        ])
    }

    fn from_cde(value: CdeValue) -> Result<Self, SyncError> {
        let mut fields = crate::wire::expect_exact::<4>(crate::wire::entries_of(value)?)?;
        Ok(Self {
            to_epoch: crate::wire::expect_uint(fields.remove(0), "pending.to_epoch")?,
            rotation_id: crate::wire::expect_bytes::<16>(fields.remove(0), "pending.rotation_id")?,
            candidate_root: crate::wire::expect_secret32(fields.remove(0), "pending.root")?,
            staged_at: crate::wire::expect_uint(fields.remove(0), "pending.staged_at")?,
        })
    }
}

/// 轮换退役的历史 epoch 根（按 epoch 索引，秘密）。
///
/// 推广只把新根写进 [`LocalIdentityV1::current_root`]；旧根必须保留到本地旧 outbox
/// 重包完成（阶段 J）为止，否则 rename/重启后无法再解开旧 epoch 的 parts。
#[derive(Clone)]
pub struct HistoricalRootV1 {
    /// 该根对应的 `key_epoch`（与对象 header 的 epoch 一致）。
    pub epoch: u64,
    /// 已退役的根材料。
    pub root: Secret32,
}

impl HistoricalRootV1 {
    /// 构造一条历史根记录。
    #[must_use]
    pub fn new(epoch: u64, root: Secret32) -> Self {
        Self { epoch, root }
    }

    fn to_cde(&self) -> CdeValue {
        CdeValue::Map(vec![
            (0, CdeValue::UInt(self.epoch)),
            (1, CdeValue::Bytes(self.root.expose_slice().to_vec())),
        ])
    }

    fn from_cde(value: CdeValue) -> Result<Self, SyncError> {
        let mut fields = crate::wire::expect_exact::<2>(crate::wire::entries_of(value)?)?;
        Ok(Self {
            epoch: crate::wire::expect_uint(fields.remove(0), "identity.previous_root.epoch")?,
            root: crate::wire::expect_secret32(fields.remove(0), "identity.previous_root.root")?,
        })
    }
}

/// 身份随轮换演进的旁路状态（经 `identity_wrap` 封装，存 `meta`）。
///
/// 语义上与身份同源（entropy 相同），但独立编码；`current_epoch` 必须与该身份一致，
/// 否则视为崩溃残留的"未来"记录而被忽略（见 promotion 的写入顺序）。
pub struct IdentityRotationStateV1 {
    /// 与 [`LocalIdentityV1::current_epoch`] 对齐；不一致即过期。
    pub current_epoch: u64,
    /// 控制链 rev（**不是**管理 key rev）。
    pub control_rev: u64,
    /// 管理 key rev（独立单调计数，不能写进 `control_rev`）。
    pub manager_rev: u64,
    /// 已退役但仍需用于解旧 outbox 的根（至少含最近一个旧根）。
    pub previous_roots: Vec<HistoricalRootV1>,
}

impl IdentityRotationStateV1 {
    /// 读旁路状态；不存在返回 `None`。
    pub fn load(
        vault: &Vault,
        vault_id: &VaultId,
        device_id: &DeviceId,
    ) -> Result<Option<Self>, SyncError> {
        let Some(blob) = vault.get_meta(IDENTITY_ROTATION_STATE_META)? else {
            return Ok(None);
        };
        let plaintext = identity_wrap::unprotect(&blob, &dpapi::entropy_for(vault_id, device_id))?;
        Ok(Some(Self::decode(&plaintext)?))
    }

    /// 读"当前身份的"旁路状态；`current_epoch` 不匹配时返回 `None`（崩溃残留）。
    pub fn load_current(
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<Option<Self>, SyncError> {
        Ok(Self::load(vault, &identity.vault_id, &identity.device_id)?
            .filter(|state| state.current_epoch == identity.current_epoch))
    }

    /// 经本地封装（Windows DPAPI / Linux 用户主钥）保护后写回旁路状态。
    pub fn save(
        &self,
        vault: &Vault,
        vault_id: &VaultId,
        device_id: &DeviceId,
    ) -> Result<(), SyncError> {
        let plaintext = Zeroizing::new(self.encode()?);
        let blob = identity_wrap::protect(&plaintext, &dpapi::entropy_for(vault_id, device_id))?;
        vault.set_meta(IDENTITY_ROTATION_STATE_META, &blob)?;
        Ok(())
    }

    /// 按 epoch 取历史根。
    #[must_use]
    pub fn root_for_epoch(&self, epoch: u64) -> Option<&Secret32> {
        self.previous_roots
            .iter()
            .find(|entry| entry.epoch == epoch)
            .map(|entry| &entry.root)
    }

    fn encode(&self) -> Result<Vec<u8>, SyncError> {
        let mut value = CdeValue::Map(vec![
            (0, CdeValue::UInt(self.current_epoch)),
            (1, CdeValue::UInt(self.control_rev)),
            (2, CdeValue::UInt(self.manager_rev)),
            (
                3,
                CdeValue::Array(
                    self.previous_roots
                        .iter()
                        .map(HistoricalRootV1::to_cde)
                        .collect(),
                ),
            ),
        ]);
        let encoded = crate::wire::encode(&value)?;
        // 中间 CdeValue 里有根材料：编码完立刻擦除。
        crate::wire::zeroize_value(&mut value);
        Ok(encoded)
    }

    fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        let fields = crate::wire::expect_exact::<4>(crate::wire::decode_entries(bytes)?)?;
        let CdeValue::Array(items) = &fields[3] else {
            return Err(SyncError::Cde("identity.rotation previous_roots 不是数组"));
        };
        let mut previous_roots = Vec::with_capacity(items.len());
        for item in items {
            previous_roots.push(HistoricalRootV1::from_cde(item.clone())?);
        }
        Ok(Self {
            current_epoch: crate::wire::expect_uint(
                fields[0].clone(),
                "identity.rotation.current_epoch",
            )?,
            control_rev: crate::wire::expect_uint(
                fields[1].clone(),
                "identity.rotation.control_rev",
            )?,
            manager_rev: crate::wire::expect_uint(
                fields[2].clone(),
                "identity.rotation.manager_rev",
            )?,
            previous_roots,
        })
    }
}

/// 旁路状态的 `meta` 键。
pub const IDENTITY_ROTATION_STATE_META: &str = "sync.identity.rotation_state";

/// 取指定 epoch 的根（当前 epoch 用身份里的根，其余查旁路历史）。
///
/// 阶段 J 重包旧 outbox 时，调用方必须先用它拿到**旧 epoch** 的 [`RootSecretV1`]，
/// 不能用 `new_root` 猜。
pub fn root_secret_for_epoch(
    vault: &Vault,
    identity: &LocalIdentityV1,
    epoch: u64,
) -> Result<Option<RootSecretV1>, SyncError> {
    if epoch == identity.current_epoch {
        return Ok(Some(RootSecretV1::new(
            identity.vault_id,
            epoch,
            identity.current_root.clone(),
        )));
    }
    let Some(state) = IdentityRotationStateV1::load_current(vault, identity)? else {
        return Ok(None);
    };
    Ok(state
        .root_for_epoch(epoch)
        .map(|root| RootSecretV1::new(identity.vault_id, epoch, root.clone())))
}

/// 本地封装的本地身份（ADR §3 的字段清单；平台分派见 [`crate::identity_wrap`]）。
///
/// 不含 `Debug` 派生：手写 `Debug` 只打印非秘密字段，秘密一律 `<redacted>`。
pub struct LocalIdentityV1 {
    /// vault 身份。
    pub vault_id: VaultId,
    /// 设备身份。
    pub device_id: DeviceId,
    /// Loro peer id。
    pub peer_id: PeerId,
    /// Ed25519 私钥种子。
    pub ed25519_seed: Secret32,
    /// X25519 HPKE 私钥材料。
    pub x25519_secret: Secret32,
    /// 当前密钥代。
    pub current_epoch: u64,
    /// 当前 epoch 根。
    pub current_root: Secret32,
    /// 管理私钥种子（仅管理设备有）。
    pub manager_seed: Option<Secret32>,
    /// pin 住的 relay instance（防换实例/静默回滚）。
    pub pinned_instance: [u8; 16],
    /// pin 住的管理公钥。
    pub pinned_manager: [u8; 32],
    /// 管理控制链版本。
    pub control_rev: u64,
    /// 轮换进行中的候选材料。
    pub pending_rotation: Option<PendingRotationV1>,
}

impl LocalIdentityV1 {
    /// DPAPI 可选 entropy：`b"MN/dpapi/v1\0" || vault_id || device_id`。
    #[must_use]
    pub fn entropy(&self) -> Vec<u8> {
        dpapi::entropy_for(&self.vault_id, &self.device_id)
    }

    /// 编码为 CDE；编码后用 [`crate::wire::zeroize_value`] 擦掉中间副本。
    pub fn encode(&self) -> Result<Vec<u8>, SyncError> {
        let mut entries: Vec<(u64, CdeValue)> = vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.device_id.to_vec())),
            (2, CdeValue::Bytes(self.peer_id.as_bytes().to_vec())),
            (
                3,
                CdeValue::Bytes(self.ed25519_seed.expose_slice().to_vec()),
            ),
            (
                4,
                CdeValue::Bytes(self.x25519_secret.expose_slice().to_vec()),
            ),
            (5, CdeValue::UInt(self.current_epoch)),
            (
                6,
                CdeValue::Bytes(self.current_root.expose_slice().to_vec()),
            ),
        ];
        if let Some(manager_seed) = &self.manager_seed {
            entries.push((7, CdeValue::Bytes(manager_seed.expose_slice().to_vec())));
        }
        entries.push((8, CdeValue::Bytes(self.pinned_instance.to_vec())));
        entries.push((9, CdeValue::Bytes(self.pinned_manager.to_vec())));
        entries.push((10, CdeValue::UInt(self.control_rev)));
        if let Some(pending) = &self.pending_rotation {
            entries.push((11, pending.to_cde()));
        }
        let mut value = CdeValue::Map(entries);
        let encoded = crate::wire::encode(&value)?;
        // 中间 CdeValue 里是长期私钥：编码完立刻擦除。
        crate::wire::zeroize_value(&mut value);
        Ok(encoded)
    }

    /// 解析 CDE；未知键/缺字段/归属不符一律拒绝。
    pub fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        let entries = crate::wire::decode_entries(bytes)?;
        let mut it: Peekable<IntoIter<(u64, CdeValue)>> = entries.into_iter().peekable();

        let vault_id =
            crate::wire::expect_bytes::<16>(take_required(&mut it, 0)?, "identity.vault_id")?;
        let device_id =
            crate::wire::expect_bytes::<16>(take_required(&mut it, 1)?, "identity.device_id")?;
        let peer_id = PeerId::from_bytes(crate::wire::expect_bytes::<16>(
            take_required(&mut it, 2)?,
            "identity.peer_id",
        )?);
        let ed25519_seed =
            crate::wire::expect_secret32(take_required(&mut it, 3)?, "identity.ed25519_seed")?;
        let x25519_secret =
            crate::wire::expect_secret32(take_required(&mut it, 4)?, "identity.x25519_secret")?;
        let current_epoch =
            crate::wire::expect_uint(take_required(&mut it, 5)?, "identity.current_epoch")?;
        let current_root =
            crate::wire::expect_secret32(take_required(&mut it, 6)?, "identity.current_root")?;
        let manager_seed = match take_optional(&mut it, 7)? {
            Some(value) => Some(crate::wire::expect_secret32(
                value,
                "identity.manager_seed",
            )?),
            None => None,
        };
        let pinned_instance = crate::wire::expect_bytes::<16>(
            take_required(&mut it, 8)?,
            "identity.pinned_instance",
        )?;
        let pinned_manager =
            crate::wire::expect_bytes::<32>(take_required(&mut it, 9)?, "identity.pinned_manager")?;
        let control_rev =
            crate::wire::expect_uint(take_required(&mut it, 10)?, "identity.control_rev")?;
        let pending_rotation = match take_optional(&mut it, 11)? {
            Some(value) => Some(PendingRotationV1::from_cde(value)?),
            None => None,
        };
        if let Some((key, _)) = it.next() {
            return Err(SyncError::Cde(match key {
                7 => "可选字段 7 位置错误",
                11 => "可选字段 11 位置错误",
                _ => "身份 BLOB 含未知字段",
            }));
        }
        Ok(Self {
            vault_id,
            device_id,
            peer_id,
            ed25519_seed,
            x25519_secret,
            current_epoch,
            current_root,
            manager_seed,
            pinned_instance,
            pinned_manager,
            control_rev,
            pending_rotation,
        })
    }

    /// 经平台本地封装保护整份身份（Windows DPAPI / Linux 用户主钥）。
    ///
    /// 明文序列化缓冲包在 `Zeroizing` 里，加密后立即清零。
    pub fn protect(&self) -> Result<Vec<u8>, SyncError> {
        let plaintext = Zeroizing::new(self.encode()?);
        identity_wrap::protect(&plaintext, &self.entropy())
    }

    /// 解封平台本地封装 BLOB 并解析；`vault_id`/`device_id` 必须与 BLOB 内容一致
    /// （entropy 已经绑定它们，这里再校验一次结构，防错配）。
    pub fn unprotect(
        blob: &[u8],
        vault_id: &VaultId,
        device_id: &DeviceId,
    ) -> Result<Self, SyncError> {
        let plaintext = identity_wrap::unprotect(blob, &dpapi::entropy_for(vault_id, device_id))?;
        let identity = Self::decode(&plaintext)?;
        if identity.vault_id != *vault_id || identity.device_id != *device_id {
            return Err(SyncError::Identity(
                "身份 BLOB 的 vault/device 与期望不一致",
            ));
        }
        Ok(identity)
    }

    /// Loro peer id：把 16B [`PeerId`] **确定性**映射到 `u64`（ADR 0009 切片 2a）。
    ///
    /// 这是 app 侧 dev-sync 取 peer 的**唯一入口**：不允许另造随机 peer，也不允许继续
    /// 用常量 `1`（两个实例会撞 CRDT peer）。映射规则（前 8 字节大端、清最高位、
    /// `0 → 1`、碰撞边界）全部写在 [`PeerId::loro_u64`] 的文档里；映射本身随身份
    /// 持久化，重启不变。
    #[must_use]
    pub fn loro_peer_id(&self) -> u64 {
        self.peer_id.loro_u64()
    }

    /// dev-only：生成一份**自包含**的本地身份（不连 relay、不含管理信任锚）。
    ///
    /// 仅供 app 的 `DevIdentityStore` 在 dev-sync 首次启动时造身份用（ADR 0009
    /// 「dev-only，无恢复码备份」）。字段语义：
    ///
    /// - `peer_id` / `ed25519_seed` / `x25519_secret` / `current_root`：OS 随机；
    /// - `current_epoch` = [`DEV_EPOCH`]（与 genesis 初始代一致）；
    /// - `manager_seed` = `None`、`pinned_instance`/`pinned_manager` 全 0、
    ///   `control_rev` = 0：**没有**接触过任何 relay，pin 与信任锚必须为空，
    ///   等到切片 4 的 Create 才由 genesis/Join 正式填充。
    ///
    /// 不复制任何 crypto 逻辑：随机数与密钥材料全部走 [`crate::random`] /
    /// [`Secret32::generate`]；真实身份仍必须走 [`crate::genesis`] / 恢复码路径。
    pub fn generate_dev(vault_id: VaultId, device_id: DeviceId) -> Result<Self, SyncError> {
        let mut rng = crate::random::OsRandom;
        Ok(Self {
            vault_id,
            device_id,
            peer_id: PeerId::from_bytes(crate::random::array::<16>(&mut rng)?),
            ed25519_seed: Secret32::generate(&mut rng)?,
            x25519_secret: Secret32::generate(&mut rng)?,
            current_epoch: DEV_EPOCH,
            current_root: Secret32::generate(&mut rng)?,
            manager_seed: None,
            pinned_instance: [0u8; 16],
            pinned_manager: [0u8; 32],
            control_rev: 0,
            pending_rotation: None,
        })
    }
}

/// dev 身份的占位 epoch（与 [`crate::genesis::GENESIS_EPOCH`] 一致）。
///
/// dev 身份没有 key slot / 管理信任锚，这个代只用于让字段自洽；切片 4 建真实 vault 时
/// 整个身份会被 genesis/Join 的结果替换。
pub const DEV_EPOCH: u64 = 1;

impl core::fmt::Debug for LocalIdentityV1 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LocalIdentityV1")
            .field("vault_id", &crate::ids::hex_of(&self.vault_id))
            .field("device_id", &crate::ids::hex_of(&self.device_id))
            .field("peer_id", &self.peer_id)
            .field("ed25519_seed", &crate::keys::redacted())
            .field("x25519_secret", &crate::keys::redacted())
            .field("current_epoch", &self.current_epoch)
            .field("current_root", &crate::keys::redacted())
            .field(
                "manager_seed",
                &self.manager_seed.as_ref().map(|_| crate::keys::redacted()),
            )
            .field(
                "pinned_instance",
                &crate::ids::hex_of(&self.pinned_instance),
            )
            .field("pinned_manager", &crate::ids::hex_of(&self.pinned_manager))
            .field("control_rev", &self.control_rev)
            .field(
                "pending_rotation",
                &self.pending_rotation.as_ref().map(|p| p.to_epoch),
            )
            .finish()
    }
}

fn take_required(
    it: &mut Peekable<IntoIter<(u64, CdeValue)>>,
    key: u64,
) -> Result<CdeValue, SyncError> {
    take_optional(it, key)?.ok_or(SyncError::Cde("身份 BLOB 缺少必需字段"))
}

fn take_optional(
    it: &mut Peekable<IntoIter<(u64, CdeValue)>>,
    key: u64,
) -> Result<Option<CdeValue>, SyncError> {
    match it.peek() {
        Some((k, _)) if *k == key => Ok(it.next().map(|(_, value)| value)),
        Some((k, _)) if *k < key => Err(SyncError::Cde("身份 BLOB 字段乱序")),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRng;

    fn fixture(with_manager: bool, with_pending: bool, seed: u64) -> LocalIdentityV1 {
        let mut rng = TestRng::new(seed);
        LocalIdentityV1 {
            vault_id: [0x10; 16],
            device_id: [0x20; 16],
            peer_id: PeerId::from_bytes([0x30; 16]),
            ed25519_seed: Secret32::generate(&mut rng).unwrap(),
            x25519_secret: Secret32::generate(&mut rng).unwrap(),
            current_epoch: 7,
            current_root: Secret32::generate(&mut rng).unwrap(),
            manager_seed: if with_manager {
                Some(Secret32::generate(&mut rng).unwrap())
            } else {
                None
            },
            pinned_instance: [0x40; 16],
            pinned_manager: [0x50; 32],
            control_rev: 11,
            pending_rotation: if with_pending {
                Some(PendingRotationV1 {
                    to_epoch: 8,
                    rotation_id: [0x60; 16],
                    candidate_root: Secret32::generate(&mut rng).unwrap(),
                    staged_at: 1_700_000_000,
                })
            } else {
                None
            },
        }
    }

    #[test]
    fn roundtrip_all_optional_shapes() {
        for (with_manager, with_pending) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let identity = fixture(with_manager, with_pending, 71);
            let bytes = identity.encode().unwrap();
            let back = LocalIdentityV1::decode(&bytes).unwrap();
            assert_eq!(back.vault_id, identity.vault_id);
            assert_eq!(back.device_id, identity.device_id);
            assert_eq!(back.peer_id, identity.peer_id);
            assert_eq!(back.current_epoch, 7);
            assert_eq!(back.control_rev, 11);
            assert!(back.ed25519_seed.ct_eq(&identity.ed25519_seed));
            assert!(back.x25519_secret.ct_eq(&identity.x25519_secret));
            assert!(back.current_root.ct_eq(&identity.current_root));
            assert_eq!(back.manager_seed.is_some(), with_manager);
            assert_eq!(back.pending_rotation.is_some(), with_pending);
            if let (Some(a), Some(b)) = (&back.pending_rotation, &identity.pending_rotation) {
                assert_eq!(a.to_epoch, b.to_epoch);
                assert_eq!(a.rotation_id, b.rotation_id);
                assert_eq!(a.staged_at, b.staged_at);
                assert!(a.candidate_root.ct_eq(&b.candidate_root));
            }
            // 编码确定性
            assert_eq!(bytes, back.encode().unwrap());
        }
    }

    #[test]
    fn field_count_matches_optionals() {
        // 10 项（无 optional）
        let bytes = fixture(false, false, 72).encode().unwrap();
        assert_eq!(bytes[0], 0xaa);
        // 11 项（一个 optional）
        let bytes = fixture(true, false, 73).encode().unwrap();
        assert_eq!(bytes[0], 0xab);
        let bytes = fixture(false, true, 74).encode().unwrap();
        assert_eq!(bytes[0], 0xab);
        // 12 项（两个 optional）
        let bytes = fixture(true, true, 75).encode().unwrap();
        assert_eq!(bytes[0], 0xac);
    }

    #[test]
    fn unknown_or_truncated_rejected() {
        let identity = fixture(true, true, 76);
        let bytes = identity.encode().unwrap();
        // 截断
        assert!(LocalIdentityV1::decode(&bytes[..bytes.len() - 1]).is_err());
        // 尾部多余字节
        let mut long = bytes.clone();
        long.push(0);
        assert!(LocalIdentityV1::decode(&long).is_err());
        // 未知字段：在合法记录末尾追加键 12，必须被拒绝（v1 严格解码）
        let mut entries = crate::wire::decode_entries(&bytes).unwrap();
        entries.push((12, CdeValue::UInt(0)));
        let bad = crate::wire::encode(&CdeValue::Map(entries)).unwrap();
        assert!(LocalIdentityV1::decode(&bad).is_err());
    }

    #[test]
    fn required_field_missing_rejected() {
        // 手工构造只有 6 项（缺 manager/pinned/control）的 map
        let value = CdeValue::Map(vec![
            (0, CdeValue::Bytes(vec![0x10; 16])),
            (1, CdeValue::Bytes(vec![0x20; 16])),
            (2, CdeValue::Bytes(vec![0x30; 16])),
            (3, CdeValue::Bytes(vec![0x40; 32])),
            (4, CdeValue::Bytes(vec![0x50; 32])),
            (5, CdeValue::UInt(7)),
            (6, CdeValue::Bytes(vec![0x60; 32])),
        ]);
        let bytes = crate::wire::encode(&value).unwrap();
        assert!(LocalIdentityV1::decode(&bytes).is_err());
    }

    #[test]
    fn debug_redacts_all_secrets() {
        let identity = fixture(true, true, 77);
        let text = format!("{identity:?}");
        assert!(text.contains("<redacted>"));
        // 任何秘密的十六进制都不能出现在 Debug 输出里
        for secret in [
            &identity.ed25519_seed,
            &identity.x25519_secret,
            &identity.current_root,
            identity.manager_seed.as_ref().unwrap(),
            &identity.pending_rotation.as_ref().unwrap().candidate_root,
        ] {
            assert!(
                !text.contains(&crate::ids::hex_of(secret.expose_slice())),
                "Debug 输出泄露了秘密: {text}"
            );
        }
        // 非秘密字段可见（便于排障）
        assert!(text.contains(&crate::ids::hex_of(&identity.device_id)));
        assert!(text.contains("current_epoch"));
    }

    #[test]
    fn rotation_state_roundtrips_previous_roots() {
        let old_a = Secret32::from_array([0x71; 32]);
        let old_b = Secret32::from_array([0x61; 32]);
        let state = IdentityRotationStateV1 {
            current_epoch: 8,
            control_rev: 12,
            manager_rev: 5,
            previous_roots: vec![
                HistoricalRootV1::new(7, old_a.clone()),
                HistoricalRootV1::new(6, old_b.clone()),
            ],
        };
        let bytes = state.encode().unwrap();
        let back = IdentityRotationStateV1::decode(&bytes).unwrap();
        assert_eq!(back.current_epoch, 8);
        assert_eq!(back.control_rev, 12);
        assert_eq!(back.manager_rev, 5);
        assert_eq!(back.previous_roots.len(), 2);
        assert!(back.root_for_epoch(7).unwrap().ct_eq(&old_a));
        assert!(back.root_for_epoch(6).unwrap().ct_eq(&old_b));
        assert!(back.root_for_epoch(8).is_none());
        // 编码确定性 + 截断/尾随/未知字段拒绝
        assert_eq!(bytes, back.encode().unwrap());
        assert!(IdentityRotationStateV1::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut long = bytes.clone();
        long.push(0);
        assert!(IdentityRotationStateV1::decode(&long).is_err());
        let mut entries = crate::wire::decode_entries(&bytes).unwrap();
        entries.push((4, CdeValue::UInt(0)));
        let bad = crate::wire::encode(&CdeValue::Map(entries)).unwrap();
        assert!(IdentityRotationStateV1::decode(&bad).is_err());
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn rotation_state_is_wrapped_and_epoch_scoped() {
        crate::test_support::ensure_local_wrap();
        let base = std::env::var("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/tmp"));
        std::fs::create_dir_all(&base).expect("target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("identity-rotation-")
            .tempdir_in(&base)
            .expect("临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
        let identity = fixture(false, false, 81);
        let old_root = Secret32::from_array([0x72; 32]);
        let state = IdentityRotationStateV1 {
            current_epoch: identity.current_epoch,
            control_rev: 9,
            manager_rev: 4,
            previous_roots: vec![HistoricalRootV1::new(
                identity.current_epoch - 1,
                old_root.clone(),
            )],
        };
        state
            .save(&vault, &identity.vault_id, &identity.device_id)
            .expect("save");
        // 本地封装密文里不能出现根的明文
        let blob = vault
            .get_meta(IDENTITY_ROTATION_STATE_META)
            .expect("meta")
            .expect("blob");
        assert!(
            !blob
                .windows(32)
                .any(|window| window == old_root.expose_slice()),
            "旁路状态必须以本地封装密文落盘"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(&blob[..4], b"MNKW", "Linux 旁路状态必须带 MNKW 外层头");
        let loaded = IdentityRotationStateV1::load_current(&vault, &identity)
            .expect("load")
            .expect("state");
        assert_eq!(loaded.control_rev, 9);
        assert_eq!(loaded.manager_rev, 4);
        // 当前 epoch 用身份里的根，旧 epoch 用历史根
        let current = root_secret_for_epoch(&vault, &identity, identity.current_epoch)
            .expect("current")
            .expect("current root");
        assert_eq!(current.key_epoch, identity.current_epoch);
        assert!(current.root.ct_eq(&identity.current_root));
        let old = root_secret_for_epoch(&vault, &identity, identity.current_epoch - 1)
            .expect("old")
            .expect("old root");
        assert!(old.root.ct_eq(&old_root));
        assert!(
            root_secret_for_epoch(&vault, &identity, identity.current_epoch + 1)
                .expect("miss")
                .is_none()
        );
        // 崩溃残留：旁路状态 epoch 领先身份时整条记录视为过期
        let future = IdentityRotationStateV1 {
            current_epoch: identity.current_epoch + 1,
            control_rev: 10,
            manager_rev: 5,
            previous_roots: vec![HistoricalRootV1::new(
                identity.current_epoch,
                old_root.clone(),
            )],
        };
        future
            .save(&vault, &identity.vault_id, &identity.device_id)
            .expect("save future");
        assert!(IdentityRotationStateV1::load_current(&vault, &identity)
            .expect("load")
            .is_none());
        assert!(
            root_secret_for_epoch(&vault, &identity, identity.current_epoch - 1)
                .expect("stale")
                .is_none()
        );
    }

    #[test]
    fn loro_peer_id_is_deterministic_and_persists_through_codec() {
        let identity = fixture(false, false, 82);
        // 同一身份恒定（多次调用/不同字段不影响 peer）。
        let peer = identity.loro_peer_id();
        assert_eq!(peer, identity.loro_peer_id());
        assert_eq!(peer, identity.peer_id.loro_u64());
        assert_ne!(peer, 0, "0 不是合法 Loro peer");
        // 落盘再读回（encode/decode 是 `protect`/`unprotect` 的内层）仍一致。
        let bytes = identity.encode().unwrap();
        let back = LocalIdentityV1::decode(&bytes).unwrap();
        assert_eq!(back.loro_peer_id(), peer);
        // 只改后 8 字节（映射不看后半段）：u64 相同但 16B 身份不同 → 碰撞边界的实证。
        let mut same_head = fixture(false, false, 82);
        same_head.peer_id = PeerId::from_bytes({
            let mut bytes = *identity.peer_id.as_bytes();
            bytes[8..].copy_from_slice(&[0xee; 8]);
            bytes
        });
        assert_eq!(same_head.loro_peer_id(), peer);
        assert_ne!(same_head.peer_id, identity.peer_id);
    }

    #[test]
    fn loro_peer_id_differs_across_random_identities() {
        // 至少两个**随机**身份（generate_dev 走 OS 随机）：peer 必须不同。
        let first = LocalIdentityV1::generate_dev([0x10; 16], [0x01; 16]).expect("身份 1");
        let second = LocalIdentityV1::generate_dev([0x10; 16], [0x02; 16]).expect("身份 2");
        assert_ne!(first.peer_id, second.peer_id);
        assert_ne!(first.loro_peer_id(), second.loro_peer_id());
        // 多于两个也两两不同（dev 场景的碰撞概率 ~2^-63，不靠运气）。
        let mut peers = vec![first.loro_peer_id(), second.loro_peer_id()];
        for device in 3..6u8 {
            let other = LocalIdentityV1::generate_dev([0x10; 16], [device; 16]).expect("身份 N");
            let peer = other.loro_peer_id();
            assert!(!peers.contains(&peer), "随机身份的 peer 撞了：{peer}");
            peers.push(peer);
        }
    }

    #[test]
    fn generate_dev_is_self_contained_and_varies() {
        let identity = LocalIdentityV1::generate_dev([0x11; 16], [0x22; 16]).expect("dev 身份");
        assert_eq!(identity.vault_id, [0x11; 16]);
        assert_eq!(identity.device_id, [0x22; 16]);
        assert_eq!(identity.current_epoch, DEV_EPOCH);
        // 没有接触 relay：无管理材料、无 pin、无控制链、无轮换残留。
        assert!(identity.manager_seed.is_none());
        assert_eq!(identity.pinned_instance, [0u8; 16]);
        assert_eq!(identity.pinned_manager, [0u8; 32]);
        assert_eq!(identity.control_rev, 0);
        assert!(identity.pending_rotation.is_none());
        assert_ne!(identity.loro_peer_id(), 0);
        // 两次生成除 vault/device 外全部不同（OS 随机，不复制固定夹具）。
        let other = LocalIdentityV1::generate_dev([0x11; 16], [0x22; 16]).expect("dev 身份 2");
        assert_ne!(other.peer_id, identity.peer_id);
        assert!(!other.ed25519_seed.ct_eq(&identity.ed25519_seed));
        assert!(!other.x25519_secret.ct_eq(&identity.x25519_secret));
        assert!(!other.current_root.ct_eq(&identity.current_root));
        // 落盘格式自洽（编码/解码不改 peer）。
        let decoded = LocalIdentityV1::decode(&identity.encode().unwrap()).unwrap();
        assert_eq!(decoded.loro_peer_id(), identity.loro_peer_id());
        // Debug 不得泄露任何私钥材料。
        let text = format!("{identity:?}");
        assert!(text.contains("<redacted>"));
        for secret in [
            &identity.ed25519_seed,
            &identity.x25519_secret,
            &identity.current_root,
        ] {
            assert!(!text.contains(&crate::ids::hex_of(secret.expose_slice())));
        }
    }

    #[test]
    fn entropy_binds_vault_and_device() {
        let identity = fixture(false, false, 78);
        let entropy = identity.entropy();
        assert_eq!(
            entropy,
            dpapi::entropy_for(&identity.vault_id, &identity.device_id)
        );
        assert_ne!(
            entropy,
            dpapi::entropy_for(&[0x11; 16], &identity.device_id)
        );
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn local_identity_roundtrip_is_platform_wrapped() {
        crate::test_support::ensure_local_wrap();
        let identity = fixture(true, true, 79);
        let blob = identity.protect().unwrap();
        assert!(!blob.is_empty());
        #[cfg(target_os = "linux")]
        {
            assert_eq!(&blob[..4], b"MNKW");
            assert_eq!(blob[4], 1, "外层 version");
            assert_eq!(blob[5], 1, "外层 kind=LinuxKeyFile");
        }
        // 无明文旁路：种子/根等长期私钥不得出现在落盘 BLOB 里。
        for secret in [
            identity.ed25519_seed.expose_slice(),
            identity.x25519_secret.expose_slice(),
            identity.current_root.expose_slice(),
            identity
                .pending_rotation
                .as_ref()
                .unwrap()
                .candidate_root
                .expose_slice(),
        ] {
            assert!(
                !blob.windows(secret.len()).any(|window| window == secret),
                "身份 BLOB 泄露了秘密明文"
            );
        }
        let opened =
            LocalIdentityV1::unprotect(&blob, &identity.vault_id, &identity.device_id).unwrap();
        assert_eq!(opened.device_id, identity.device_id);
        assert!(opened.current_root.ct_eq(&identity.current_root));
        assert!(opened
            .manager_seed
            .as_ref()
            .unwrap()
            .ct_eq(identity.manager_seed.as_ref().unwrap()));
        // 换 device_id（entropy 不同）必须失败
        assert!(LocalIdentityV1::unprotect(&blob, &identity.vault_id, &[0x99; 16]).is_err());
        // 换 vault_id 必须失败
        assert!(LocalIdentityV1::unprotect(&blob, &[0x98; 16], &identity.device_id).is_err());
        // 密文/标签被篡改必须失败
        let mut tampered = blob;
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(
            LocalIdentityV1::unprotect(&tampered, &identity.vault_id, &identity.device_id).is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_rejects_bare_windows_dpapi_blob() {
        crate::test_support::ensure_local_wrap();
        let identity = fixture(false, false, 80);
        // Windows DPAPI 裸包没有 MNKW 头：Linux 必须显式拒绝，绝不回退 DPAPI/明文。
        assert!(matches!(
            LocalIdentityV1::unprotect(&[0x01u8; 128], &identity.vault_id, &identity.device_id),
            Err(SyncError::LocalWrapFormat(_))
        ));
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    #[test]
    fn local_wrap_is_unavailable_off_supported_platforms() {
        let identity = fixture(false, false, 80);
        assert!(matches!(
            identity.protect(),
            Err(SyncError::LocalWrapUnsupportedPlatform)
        ));
        assert!(matches!(
            LocalIdentityV1::unprotect(b"blob", &identity.vault_id, &identity.device_id),
            Err(SyncError::LocalWrapUnsupportedPlatform)
        ));
    }
}

`````


