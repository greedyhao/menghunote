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
use crate::keys::{derive_asset_id_key, RootSecretV1};
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
    checkpoint_signing_input, part_count_for, refs_digest_input, CheckpointCommit, ManifestAsset,
    ManifestDoc, ManifestV1, ObjectCommit, ObjectRefV1, ObjectType, SnapshotV1, UpdateV1,
    MAX_ASSET_RAW_BYTES,
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
/// 私有验证事实（durable 资产数/实证候选数/证明摘要）。旧修订**拒绝**，禁止把旧
/// 字节改号升级。
const VERIFIED_DURABLE_VERSION: u64 = 3;

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
            },
        })
    }
}

/// 本地 durable "已发布基线验证回执" 的 meta 键前缀。
///
/// restart 快捷恢复（`rotation_restart::baseline_from_journal`）只认这里的新修订
/// 回执；旧 journal 的 `verified=true` 没有这份回执 → 拒绝，必须完整重验/重建。
pub(crate) const VERIFIED_RECEIPT_META_PREFIX: &str = "sync.checkpoint.verified_receipt.";
/// 回执格式修订号：旧开发 vault 的回执不存在/格式不同，一律不得消费。
const VERIFIED_RECEIPT_MAGIC: &[u8] = b"MN/cp-verified/1\0";

/// promote 成功后落盘的本地回执：把"这份基线的验证/发布事实"与 commit digest 绑定。
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
}

fn verified_receipt_key(checkpoint_id: &[u8; 16]) -> String {
    format!(
        "{VERIFIED_RECEIPT_META_PREFIX}{}",
        crate::base64url::encode(checkpoint_id)
    )
}

impl VerifiedReceipt {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(VERIFIED_RECEIPT_MAGIC.len() + 16 + 16 + 8 + 8 + 32 + 32);
        out.extend_from_slice(VERIFIED_RECEIPT_MAGIC);
        out.extend_from_slice(&self.vault_id);
        out.extend_from_slice(&self.checkpoint_id);
        out.extend_from_slice(&self.epoch.to_be_bytes());
        out.extend_from_slice(&self.covered_seq.to_be_bytes());
        out.extend_from_slice(&self.commit_digest);
        let digest: [u8; 32] = Sha256::digest(&out).into();
        out.extend_from_slice(&digest);
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        let fixed = VERIFIED_RECEIPT_MAGIC.len() + 16 + 16 + 8 + 8 + 32 + 32;
        if bytes.len() != fixed || !bytes.starts_with(VERIFIED_RECEIPT_MAGIC) {
            return Err(SyncError::LocalCorrupt(
                "verified receipt 版本不支持（旧修订必须完整重验）",
            ));
        }
        let (body, digest) = bytes.split_at(bytes.len() - 32);
        let expected: [u8; 32] = Sha256::digest(body).into();
        if digest != expected {
            return Err(SyncError::LocalCorrupt("verified receipt 摘要不符"));
        }
        let mut input = &body[VERIFIED_RECEIPT_MAGIC.len()..];
        Ok(Self {
            vault_id: take_array::<16>(&mut input)?,
            checkpoint_id: take_array::<16>(&mut input)?,
            epoch: take_u64(&mut input)?,
            covered_seq: take_u64(&mut input)?,
            commit_digest: take_array::<32>(&mut input)?,
        })
    }
}

/// 读取本地 durable 验证回执；缺失返回 `None`（调用方必须 fail-closed）。
pub(crate) fn load_verified_receipt(
    vault: &menghunote_storage::Vault,
    checkpoint_id: &[u8; 16],
) -> Result<Option<VerifiedReceipt>, SyncError> {
    let Some(bytes) = vault.get_meta(&verified_receipt_key(checkpoint_id))? else {
        return Ok(None);
    };
    let receipt = VerifiedReceipt::decode(&bytes)?;
    if receipt.checkpoint_id != *checkpoint_id {
        return Err(SyncError::LocalCorrupt(
            "verified receipt 与 checkpoint id 不一致",
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

/// 从不可变验证事实重算 `CheckpointCommit` 摘要（restart receipt 恢复用）。
fn checkpoint_commit_digest_from_facts(
    vault_id: VaultId,
    producer_device_id: DeviceId,
    facts: &BaselineFacts,
) -> Result<[u8; 32], SyncError> {
    let commit = CheckpointCommit {
        vault_id,
        checkpoint_id: facts.checkpoint_id,
        epoch: facts.epoch,
        covered_relay_seq: facts.covered_seq,
        manifest_object_id: facts.manifest_object_id,
        manifest_digest: facts.manifest_digest,
        refs_digest: facts.refs_digest,
        producer_device_id,
        validation_version: 1,
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
        Ok(VerifiedBaseline {
            facts: BaselineFacts {
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
            },
        })
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
        let receipt = VerifiedReceipt {
            checkpoint_id: facts.checkpoint_id,
            vault_id: facts.vault_id,
            epoch: facts.epoch,
            covered_seq: facts.covered_seq,
            commit_digest,
        };
        self.vault.set_meta(
            &verified_receipt_key(&facts.checkpoint_id),
            &receipt.encode(),
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
        let commit = CheckpointCommit {
            vault_id: self.vault_id,
            checkpoint_id: facts.checkpoint_id,
            epoch: facts.epoch,
            covered_relay_seq: facts.covered_seq,
            manifest_object_id: facts.manifest_object_id,
            manifest_digest: facts.manifest_digest,
            refs_digest: facts.refs_digest,
            producer_device_id: self.device_id,
            validation_version: 1,
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
        let receipt = CoverageReceipt::decode(&bytes)?;
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
        let bytes = receipt.encode()?;
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
/// pending 候选格式版本：v2 只记录完整 ObjectCommit 候选，**不含**任何成功覆盖声明。
/// 旧 v1（`MN/cp-coverage/1`）一律拒绝；必须重新 prepare。
const COVERAGE_RECEIPT_MAGIC: &[u8] = b"MN/cp-pending/2\0";
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
    fn encode(&self) -> Result<Vec<u8>, SyncError> {
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
        let digest: [u8; 32] = Sha256::digest(&out).into();
        out.extend_from_slice(&digest);
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        let fixed = COVERAGE_RECEIPT_MAGIC.len() + 16 + 16 + 16 + 8 * 4 + 4 + 4 + 32;
        if bytes.len() < fixed || !bytes.starts_with(COVERAGE_RECEIPT_MAGIC) {
            return Err(SyncError::Object("coverage receipt 结构非法"));
        }
        let (body, digest) = bytes.split_at(bytes.len() - 32);
        let expected: [u8; 32] = Sha256::digest(body).into();
        if digest != expected {
            return Err(SyncError::Object("coverage receipt 摘要不符"));
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
            },
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
        let error = CoverageReceipt::decode(&old).expect_err("旧 receipt 必须拒绝");
        assert!(format!("{error}").contains("结构非法"), "{error}");
    }

    #[test]
    fn verified_receipt_rejects_old_magic() {
        let mut bytes = sample_verified().encode_durable().expect("verified bytes");
        // 直接构造旧格式 verified receipt 头（不是 VerifiedBaseline 的 CBOR）。
        bytes[..4].copy_from_slice(b"MN/c");
        let error = VerifiedReceipt::decode(&bytes).expect_err("旧 receipt 头必须拒绝");
        assert!(format!("{error}").contains("版本不支持"), "{error}");
    }
}
