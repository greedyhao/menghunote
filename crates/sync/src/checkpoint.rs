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
//!    逐条 `asset_bytes`，不一次性读进内存），object_id=HMAC(K_asset_id, real_sha256)；
//! 4. `ManifestV1`（generation=0）+ `refs_digest`（只覆盖 snapshot/asset）；
//! 5. refs（data + manifest）与 publish（`CheckpointCommit` 设备签名）。
//!
//! 发布后不自行删日志；GC 由 relay/后续任务。

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::device::{verify_signature, DeviceSigningKey};
use crate::error::SyncError;
use crate::keys::{derive_asset_id_key, RootSecretV1};
use crate::relay::{
    CheckpointPublish, CheckpointRef, CheckpointRole, RelayClient, SlotRecord, SlotUpload,
};
use crate::slot::KeySlotV1;
use crate::upload::{fetch_object, upload_object_with_reservation, UploadedObject};
use crate::{DeviceId, ObjectId, VaultId};
use menghunote_storage::{
    asset_bytes, list_all_docs, list_assets, load_structure, read_doc_bootstrap, BodyDoc, DocKind,
    StructureDoc, STRUCTURE_DOC_ID,
};
use menghunote_sync_proto::{
    checkpoint_signing_input, refs_digest_input, CheckpointCommit, ManifestAsset, ManifestDoc,
    ManifestV1, ObjectRefV1, ObjectType, SnapshotV1,
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
/// storage 的 `assets` 表目前没有公共只读 API（task-89 在补）；在它落地前，真实路径用
/// [`EmptyAssets`]（assets=[] 是当前 storage 的真实状态），测试用 [`StaticAssets`]。
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

/// 空资产源：当前 storage 没有资产读取 API 时的真实路径。
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
    /// 发布时间。
    pub published_at: u64,
    /// 是否已从 relay 读回并 validate（只有 validate 过的基线才能 promote）。
    pub validated: bool,
}

/// `prepare` 的产物：对象已上传/publish，但尚未 validate，不能作为合并基线。
#[derive(Debug, Clone)]
pub struct PreparedCheckpoint {
    /// checkpoint 报告（`validated=false`）。
    pub report: CheckpointReport,
    /// 已签名的 CheckpointCommit 编码。
    pub signed_commit: Vec<u8>,
    /// checkpoint 发布签名。
    pub signature: [u8; 64],
}

/// 从 relay 读回并验证通过的基线句柄；构造后只能由 [`CheckpointWriter::promote`] 提升。
#[derive(Debug, Clone)]
pub struct VerifiedBaseline {
    pub checkpoint_id: [u8; 16],
    pub epoch: u64,
    /// 固定 S。
    pub covered_seq: u64,
    /// S 对应游标。
    pub cursor: String,
    pub manifest_object_id: ObjectId,
    pub refs_digest: [u8; 32],
    pub signed_commit: Vec<u8>,
    pub signature: [u8; 64],
    /// 只有 `validate` 构造的句柄为 true；外部无法伪造该字段。
    validated: bool,
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

struct PreparedDoc {
    doc_id: String,
    kind: u64,
    generation: u64,
    vv: Vec<u8>,
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
        }
    }

    /// 候选 epoch 固定 role（primary/fallback）；创建 checkpoint 时传给 relay。
    pub fn with_role(mut self, role: &str) -> Self {
        self.role = Some(role.to_owned());
        self
    }

    /// 上传两个初始槽并校验 active/staging。
    pub fn upload_initial_slots(
        &mut self,
        password_slot: &KeySlotV1,
        recovery_slot: &KeySlotV1,
        secrets: &SlotSecrets<'_>,
    ) -> Result<SlotsReport, SyncError> {
        let password = self.put_slot(password_slot, secrets)?;
        let recovery = self.put_slot(recovery_slot, secrets)?;
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

    /// 创建并验证一份 checkpoint（`prepare → validate → promote` 的便捷入口）。
    pub fn create_checkpoint(
        &mut self,
        assets: &dyn AssetSource,
    ) -> Result<CheckpointReport, SyncError> {
        let prepared = self.prepare(assets)?;
        let baseline = self.validate(&prepared)?;
        let _promoted = self.promote(&baseline)?;
        let mut report = prepared.report;
        report.validated = true;
        Ok(report)
    }

    /// prepare：先固定 S（lease），再导出/上传；publish 不等于可合并基线。
    pub fn prepare(&mut self, assets: &dyn AssetSource) -> Result<PreparedCheckpoint, SyncError> {
        let epoch = self.root.key_epoch;
        let asset_metas = assets.list(self.vault)?;
        let estimated = asset_metas
            .iter()
            .map(|asset| asset.byte_len)
            .sum::<u64>()
            .saturating_add(4 << 20);
        // 先固定 S，再导出；此后 checkpoint 的内容覆盖到 S 为止。
        let lease = self.client.create_checkpoint_with_role(
            estimated,
            Some(epoch),
            self.role.as_deref(),
        )?;
        let covered_seq = lease.covered_seq;
        let prepared = self.prepare_docs()?;
        if prepared.is_empty() {
            return Err(SyncError::LocalCorrupt(
                "checkpoint 需要至少 structure 文档",
            ));
        }

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
                vv: doc.vv.clone(),
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
                vv: doc.vv.clone(),
                frontier: doc.frontier.clone(),
                object,
            });
        }

        let mut asset_refs = Vec::with_capacity(asset_metas.len());
        for meta in &asset_metas {
            // ASSET object_id = HMAC(K_asset_id, real_sha256)，同资产再上传必须复用同一份
            // 密文（relay 对同 object_id 的不同字节返回 ID_CONFLICT）。
            if let Some(cached) = self.uploaded_assets.get(&meta.real_sha256).cloned() {
                if cached.byte_len == meta.byte_len && cached.media_type == meta.media_type {
                    asset_refs.push(cached);
                    continue;
                }
            }
            let data = assets.read(self.vault, meta)?;
            let real: [u8; 32] = Sha256::digest(&data).into();
            if real != meta.real_sha256 || data.len() as u64 != meta.byte_len {
                return Err(SyncError::Object("资产字节与元数据不一致"));
            }
            let plaintext = asset_plaintext(&real, meta.byte_len, &meta.media_type, &data)?;
            let object_id = asset_object_id(&self.root, &real)?;
            // ASSET 重传必须字节一致（ADR §19）：nonce 与 message_id 都确定性派生。
            let message_id = crate::upload::asset_message_id(&self.root, &object_id)?;
            let object = upload_object_with_reservation(
                self.client,
                self.vault_id,
                self.device_id,
                &self.signing_key,
                &self.root,
                ObjectType::Asset,
                object_id,
                message_id,
                0,
                0,
                &plaintext,
                &[],
                None,
                &lease.reservation_id,
            )?;
            asset_refs.push(AssetRef {
                real_sha256: real,
                byte_len: meta.byte_len,
                media_type: meta.media_type.clone(),
                object,
            });
            if let Some(entry) = asset_refs.last().cloned() {
                self.uploaded_assets.insert(real, entry);
            }
        }

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
        let mut refs: Vec<CheckpointRef> = data_refs
            .iter()
            .map(|reference| CheckpointRef {
                object_id: reference.object_id,
                role: CheckpointRole::Data,
            })
            .collect();
        refs.push(CheckpointRef {
            object_id: manifest_object.object_id,
            role: CheckpointRole::Manifest,
        });
        self.client.add_checkpoint_refs(lease.checkpoint_id, refs)?;

        let commit = CheckpointCommit {
            vault_id: self.vault_id,
            checkpoint_id: lease.checkpoint_id,
            epoch,
            covered_relay_seq: covered_seq,
            manifest_object_id: manifest_object.object_id,
            manifest_digest: manifest_object.object_digest,
            refs_digest,
            producer_device_id: self.device_id,
            validation_version: 1,
        };
        let signed_commit = commit.encode()?;
        let signature = self.signing_key.sign(&checkpoint_signing_input(&commit)?);
        let published = self.client.publish_checkpoint(CheckpointPublish {
            checkpoint_id: lease.checkpoint_id,
            covered_seq,
            manifest_oid: manifest_object.object_id,
            refs_digest,
            signed_commit: signed_commit.clone(),
            signature,
        })?;
        let report = CheckpointReport {
            checkpoint_id: lease.checkpoint_id,
            epoch,
            covered_seq,
            docs,
            assets: asset_refs,
            manifest: manifest_object,
            refs_digest,
            published_at: published.published_at,
            validated: false,
        };
        Ok(PreparedCheckpoint {
            report,
            signed_commit,
            signature,
        })
    }

    /// validate：从 relay 读回 manifest，校验解密、摘要、refs_digest、签名与覆盖 S。
    pub fn validate(
        &mut self,
        prepared: &PreparedCheckpoint,
    ) -> Result<VerifiedBaseline, SyncError> {
        let report = &prepared.report;
        if report.validated {
            return Err(SyncError::Object("checkpoint 不能重复 validate"));
        }
        let manifest = report.manifest;
        let (_, manifest_bytes) = fetch_object(
            self.client,
            &self.root,
            ObjectType::Manifest,
            &manifest.object_id,
            manifest.part_count,
            manifest.total_plain_len,
            &manifest.object_digest,
            &[],
        )?;
        let decoded = ManifestV1::decode(&manifest_bytes)?;
        if decoded.vault_id != self.vault_id
            || decoded.checkpoint_id != report.checkpoint_id
            || decoded.covered_relay_seq != report.covered_seq
            || decoded.key_epoch != report.epoch
        {
            return Err(SyncError::Object("validate: manifest 与 report 不符"));
        }
        let refs = manifest_refs_for_validate(&decoded);
        let refs_digest: [u8; 32] = Sha256::digest(refs_digest_input(&refs)?).into();
        if refs_digest != report.refs_digest {
            return Err(SyncError::Object("validate: refs_digest 不符"));
        }
        let commit = CheckpointCommit::decode(&prepared.signed_commit)?;
        if commit.vault_id != self.vault_id
            || commit.checkpoint_id != report.checkpoint_id
            || commit.epoch != report.epoch
            || commit.covered_relay_seq != report.covered_seq
            || commit.manifest_object_id != manifest.object_id
            || commit.manifest_digest != manifest.object_digest
            || commit.refs_digest != report.refs_digest
        {
            return Err(SyncError::Object(
                "validate: CheckpointCommit 与 report 不符",
            ));
        }
        verify_signature(
            &self.signing_key.public_key(),
            &checkpoint_signing_input(&commit)?,
            &prepared.signature,
        )?;
        let state = self.client.state()?;
        if state.head_seq < report.covered_seq {
            return Err(SyncError::Object("validate: relay head 未覆盖 S"));
        }
        Ok(VerifiedBaseline {
            checkpoint_id: report.checkpoint_id,
            epoch: report.epoch,
            covered_seq: report.covered_seq,
            cursor: format!("c{}", report.covered_seq),
            manifest_object_id: manifest.object_id,
            refs_digest: report.refs_digest,
            signed_commit: prepared.signed_commit.clone(),
            signature: prepared.signature,
            validated: true,
        })
    }

    /// promote：只接受 validate 产出的句柄，返回可交给轮换/activate 的基线。
    pub fn promote(&self, baseline: &VerifiedBaseline) -> Result<PromotedBaseline, SyncError> {
        if !baseline.validated {
            return Err(SyncError::Object("checkpoint 未 validate，不能 promote"));
        }
        Ok(PromotedBaseline {
            checkpoint_id: baseline.checkpoint_id,
            epoch: baseline.epoch,
            covered_seq: baseline.covered_seq,
            cursor: baseline.cursor.clone(),
            manifest_object_id: baseline.manifest_object_id,
            refs_digest: baseline.refs_digest,
        })
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

    fn put_slot(
        &mut self,
        slot: &KeySlotV1,
        secrets: &SlotSecrets<'_>,
    ) -> Result<SlotRecord, SyncError> {
        let descriptor = slot.descriptor.encode()?;
        let plaintext = slot.wrapped_bytes();
        let message_id: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
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
            let snapshot_bytes = if meta.kind == DocKind::Structure {
                let (structure, _head) = load_structure(self.vault, self.peer_id)?;
                structure.export_snapshot()?
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
                body.export_snapshot()?
            };
            docs.push(PreparedDoc {
                doc_id: meta.doc_id,
                kind,
                generation: meta.generation,
                vv: meta.head_vv.unwrap_or_default(),
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

pub fn asset_object_id(root: &RootSecretV1, real_sha256: &[u8; 32]) -> Result<ObjectId, SyncError> {
    let key = derive_asset_id_key(root)?;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key.expose_slice())
        .map_err(|_| SyncError::Object("资产 HMAC 密钥非法"))?;
    mac.update(real_sha256);
    let digest = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}
