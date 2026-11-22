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
