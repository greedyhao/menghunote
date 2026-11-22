//! 新设备 bootstrap（S7-18；ADR §3.7/§4/§7）。
//!
//! 本模块先落"数据恢复"这一段：给定已解出的 root（由槽解封或后续恢复流程提供），
//! `POST /bootstrap` 取两份基线 → 下 manifest/snapshot/asset → 全量校验 → 空库落库。
//!
//! 落库路径（不直连 SQL）：
//! - body：`create_body_doc` → `BodyDoc::restore` → `pending_update(empty)` →
//!   `append_updates`（1 条全量 update；**不产生 snapshot 行**，后续可优化）；
//! - structure：`StructureWriter::open` → `doc().import(snapshot)` → `commit("bootstrap")`
//!   （同一 A 事务写 loro_updates 并投影 notes/tree/tags）；
//! - asset：`insert_asset`（先按 root 重算 HMAC 对象名，再验明文 sha256/长度/MIME）。
//!
//! 写库发生在**全部下载/解密/摘要校验通过之后**，失败不会写半库。
//!
//! **待定协议点（Lead 决策中）**：KEY_SLOT 的 MNE1 密钥当前由 root 派生，
//! 恢复设备在拿到 root 之前无法解槽；因此"仅恢复码 + relay"的槽解封属于另一段协议修订，
//! 本模块只接收调用方已解出的 root。
//!
//! **已知边界**：relay `/bootstrap` 不返回 `CheckpointCommit`/签名；本模块能校验
//! manifest 内 checkpoint id/S/对象摘要，但无法验证发布签名（需 relay 补字段）。
//! 新设备无旧水位，无法检测"有效签名的旧基线回滚"（ADR §10 已记录）。

use loro::VersionVector;
use sha2::{Digest, Sha256};

use crate::checkpoint::{verify_asset_plaintext, AssetRef};
use crate::device::verify_signature;
use crate::engine::RELAY_CURSOR_META;
use crate::error::SyncError;
use crate::keys::RootSecretV1;
use crate::relay::{BootstrapCheckpoint, RelayClient};
use crate::upload::fetch_object;
use menghunote_storage::{
    append_updates, create_body_doc, doc_exists, ensure_structure_row, insert_asset, load_body_doc,
    set_confirmed_highwater, set_sync_log_id, BodyDoc, StructureWriter, UpdateRecord, Vault,
    META_SYNC_INTEGRATED_SEQ, META_SYNC_RECEIVED_SEQ,
};
use menghunote_sync_proto::{
    checkpoint_signing_input, refs_digest_input, CheckpointCommit, Envelope, EnvelopeHeader,
    ManifestAsset, ManifestDoc, ManifestV1, ObjectRefV1, ObjectType, SnapshotV1,
};

/// bootstrap 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapReport {
    /// 使用的 checkpoint id。
    pub checkpoint_id: [u8; 16],
    /// 覆盖水位 S。
    pub covered_seq: u64,
    /// 是否回退到 fallback。
    pub used_fallback: bool,
    /// 导入文档（structure 的文本为 None）。
    pub docs: Vec<(String, Option<String>)>,
    /// 导入资产数。
    pub assets: usize,
    /// S 对应的 relay 游标。
    pub cursor: String,
    /// 是否验证了 checkpoint 发布签名（relay 返回键 6/7 且验签通过）。
    pub signed_checkpoint: bool,
}

struct VerifiedDoc {
    doc_id: String,
    kind: u64,
    snapshot: SnapshotV1,
    text: Option<String>,
}

struct VerifiedAsset {
    real_sha256: [u8; 32],
    byte_len: u64,
    media_type: String,
    data: Vec<u8>,
}

/// `run` 选中的基线：manifest/docs/assets + 基线本身 + 是否 fallback。
type SelectedBaseline = (
    ManifestV1,
    Vec<VerifiedDoc>,
    Vec<VerifiedAsset>,
    BootstrapCheckpoint,
    bool,
);

/// 用已解出的 root 执行数据 bootstrap。
pub fn run<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &RootSecretV1,
) -> Result<BootstrapReport, SyncError> {
    if root.vault_id != vault_id {
        return Err(SyncError::Object("bootstrap root 与 vault 不符"));
    }
    let bundle = client.bootstrap()?;
    // 选中哪个基线，就只用它的 S/cursor/manifest/docs；primary 任一阶段失败才整体切 fallback。
    let mut selected: Option<SelectedBaseline> = None;
    let mut last_error: Option<SyncError> = None;
    for (checkpoint, used_fallback) in [
        (bundle.primary.clone(), false),
        (bundle.fallback.clone(), true),
    ] {
        match load_baseline(client, root, vault_id, peer_id, &checkpoint, bundle.epoch) {
            Ok((manifest, docs, assets)) => {
                selected = Some((manifest, docs, assets, checkpoint, used_fallback));
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let (manifest, docs, assets, checkpoint, used_fallback) =
        selected.ok_or_else(|| last_error.expect("bootstrap 至少尝试过一个基线"))?;

    // 全部校验通过后才写库；body → structure → asset（structure 投影需要 body FK 已存在）。
    let mut preexisting = std::collections::HashSet::new();
    for doc in docs.iter().filter(|doc| doc.kind != 0) {
        if doc_exists(vault, &doc.doc_id)? {
            preexisting.insert(doc.doc_id.clone());
        }
        persist_body(vault, peer_id, doc)?;
    }
    let structure = docs
        .iter()
        .find(|doc| doc.doc_id == manifest.structure_doc_id)
        .ok_or(SyncError::Object("manifest 缺 structure 文档"))?;
    persist_structure(vault, peer_id, structure)?;
    for asset in &assets {
        insert_asset(
            vault,
            &asset.real_sha256,
            asset.byte_len,
            Some(&asset.media_type),
            &asset.data,
        )?;
    }

    let state = client.state()?;
    if state.epoch != manifest.key_epoch {
        return Err(SyncError::Object("bootstrap epoch 与本地根不一致"));
    }
    set_sync_log_id(vault, &state.log_id)?;
    set_confirmed_highwater(vault, checkpoint.covered_seq)?;
    vault.set_meta(RELAY_CURSOR_META, checkpoint.cursor.as_bytes())?;
    vault.set_meta(
        META_SYNC_RECEIVED_SEQ,
        &checkpoint.covered_seq.to_be_bytes(),
    )?;
    vault.set_meta(
        META_SYNC_INTEGRATED_SEQ,
        &checkpoint.covered_seq.to_be_bytes(),
    )?;

    let mut report_docs = Vec::with_capacity(docs.len());
    for doc in docs {
        let text = match doc.text {
            None => None,
            Some(expected) => {
                let (loaded, _) = load_body_doc(vault, &doc.doc_id, peer_id, None)?;
                // 本地已有 doc 会按 VV 合并，正文不再等于快照；只要合并没有丢内容即可。
                if !preexisting.contains(&doc.doc_id) && loaded.text() != expected {
                    return Err(SyncError::Object("bootstrap 后正文与快照不一致"));
                }
                Some(loaded.text())
            }
        };
        report_docs.push((doc.doc_id, text));
    }
    Ok(BootstrapReport {
        checkpoint_id: checkpoint.checkpoint_id,
        covered_seq: checkpoint.covered_seq,
        used_fallback,
        docs: report_docs,
        assets: assets.len(),
        cursor: checkpoint.cursor,
        signed_checkpoint: true,
    })
}

fn load_baseline<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    vault_id: [u8; 16],
    peer_id: u64,
    checkpoint: &BootstrapCheckpoint,
    epoch: u64,
) -> Result<(ManifestV1, Vec<VerifiedDoc>, Vec<VerifiedAsset>), SyncError> {
    let (manifest, manifest_bytes, manifest_digest) = fetch_manifest(client, root, checkpoint)?;
    verify_checkpoint_commit(
        client,
        checkpoint,
        &manifest,
        &manifest_bytes,
        &manifest_digest,
    )?;
    verify_manifest(&manifest, vault_id, checkpoint, epoch)?;
    let docs = download_docs(client, root, peer_id, &manifest)?;
    let assets = download_assets(client, root, &manifest)?;
    Ok((manifest, docs, assets))
}

fn verify_checkpoint_commit<C: RelayClient>(
    client: &mut C,
    checkpoint: &BootstrapCheckpoint,
    manifest: &ManifestV1,
    manifest_bytes: &[u8],
    manifest_digest: &[u8; 32],
) -> Result<(), SyncError> {
    let signed_commit = checkpoint
        .signed_commit
        .as_deref()
        .ok_or(SyncError::Object("bootstrap 缺 CheckpointCommit"))?;
    let signature = checkpoint
        .signature
        .ok_or(SyncError::Object("bootstrap 缺 checkpoint 签名"))?;
    let commit = CheckpointCommit::decode(signed_commit)?;
    let _ = manifest_bytes;
    if commit.vault_id != manifest.vault_id
        || commit.checkpoint_id != checkpoint.checkpoint_id
        || commit.epoch != checkpoint.epoch
        || commit.covered_relay_seq != checkpoint.covered_seq
        || commit.manifest_object_id != checkpoint.manifest_oid
        || commit.manifest_digest != *manifest_digest
    {
        return Err(SyncError::Object("bootstrap checkpoint commit 与响应不符"));
    }
    // ADR §21：manifest 条目已带 key_epoch/total_plain_len/object_digest，可只凭 manifest
    // 重算 refs_digest；不再依赖 GET objects。
    let refs = manifest_refs(manifest);
    let refs_digest: [u8; 32] = Sha256::digest(refs_digest_input(&refs)?).into();
    if commit.refs_digest != refs_digest {
        return Err(SyncError::Object("bootstrap refs_digest 与 manifest 不符"));
    }
    if let Some(refs_digest) = checkpoint.refs_digest {
        if commit.refs_digest != refs_digest {
            return Err(SyncError::Object("bootstrap refs_digest 与 commit 不符"));
        }
    }
    let certificate = client
        .device_cert(&commit.producer_device_id)?
        .ok_or(SyncError::Object("bootstrap 缺 checkpoint 生产者证书"))?;
    let input = checkpoint_signing_input(&commit)?;
    verify_signature(&certificate.sign_ed25519_pub, &input, &signature)?;
    Ok(())
}

fn fetch_manifest<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    checkpoint: &BootstrapCheckpoint,
) -> Result<(ManifestV1, Vec<u8>, [u8; 32]), SyncError> {
    let (header, bytes) = fetch_self_described(
        client,
        root,
        ObjectType::Manifest,
        &checkpoint.manifest_oid,
        None,
        &[],
    )?;
    let digest = object_digest_of(client, &checkpoint.manifest_oid, header.part_count)?;
    let manifest = ManifestV1::decode(&bytes)?;
    Ok((manifest, bytes, digest))
}

fn object_digest_of<C: RelayClient>(
    client: &mut C,
    object_id: &[u8; 32],
    part_count: u32,
) -> Result<[u8; 32], SyncError> {
    let mut digests = Vec::with_capacity(part_count as usize);
    for index in 0..part_count {
        let bytes = client.part(object_id, index)?;
        digests.push(Sha256::digest(&bytes).into());
    }
    let input = menghunote_sync_proto::object_digest_input(part_count, &digests)?;
    Ok(Sha256::digest(&input).into())
}

fn manifest_refs(manifest: &ManifestV1) -> Vec<ObjectRefV1> {
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

fn verify_manifest(
    manifest: &ManifestV1,
    vault_id: [u8; 16],
    checkpoint: &BootstrapCheckpoint,
    epoch: u64,
) -> Result<(), SyncError> {
    if manifest.vault_id != vault_id
        || manifest.checkpoint_id != checkpoint.checkpoint_id
        || manifest.covered_relay_seq != checkpoint.covered_seq
        || manifest.key_epoch != epoch
    {
        return Err(SyncError::Object("bootstrap manifest 与 checkpoint 不符"));
    }
    if manifest.docs.is_empty() {
        return Err(SyncError::Object("bootstrap manifest 没有文档"));
    }
    if !manifest
        .docs
        .iter()
        .any(|doc| doc.doc_id == manifest.structure_doc_id)
    {
        return Err(SyncError::Object("bootstrap manifest 缺 structure 文档"));
    }
    Ok(())
}

fn download_docs<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    peer_id: u64,
    manifest: &ManifestV1,
) -> Result<Vec<VerifiedDoc>, SyncError> {
    let mut docs = Vec::with_capacity(manifest.docs.len());
    for doc in &manifest.docs {
        let (header, bytes) = fetch_self_described(
            client,
            root,
            ObjectType::Snapshot,
            &doc.snapshot_object_id,
            Some(&doc.object_digest),
            &[],
        )?;
        if header.part_count != doc.part_count {
            return Err(SyncError::Object("bootstrap 快照片数与 manifest 不符"));
        }
        let snapshot = SnapshotV1::decode(&bytes)?;
        if !snapshot.full_history
            || snapshot.doc_id != doc.doc_id
            || snapshot.kind != doc.kind
            || snapshot.generation != doc.generation
            || snapshot.vv != doc.vv
            || snapshot.frontier != doc.frontier
        {
            return Err(SyncError::Object("bootstrap 快照与 manifest 不符"));
        }
        let text = if doc.kind == 0 {
            None
        } else {
            let body = BodyDoc::restore(
                doc.doc_id.clone(),
                peer_id,
                Some(&snapshot.loro_snapshot_bytes),
                &[],
            )?;
            Some(body.text())
        };
        docs.push(VerifiedDoc {
            doc_id: doc.doc_id.clone(),
            kind: doc.kind,
            snapshot,
            text,
        });
    }
    Ok(docs)
}

fn download_assets<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    manifest: &ManifestV1,
) -> Result<Vec<VerifiedAsset>, SyncError> {
    let mut assets = Vec::with_capacity(manifest.assets.len());
    for manifest_asset in &manifest.assets {
        let expected = crate::checkpoint::asset_object_id(root, &manifest_asset.real_sha256)?;
        if expected != manifest_asset.object_id {
            return Err(SyncError::Object("bootstrap 资产 object_id 与根不符"));
        }
        let (header, bytes) = fetch_self_described(
            client,
            root,
            ObjectType::Asset,
            &manifest_asset.object_id,
            Some(&manifest_asset.object_digest),
            &[],
        )?;
        if header.part_count != manifest_asset.part_count {
            return Err(SyncError::Object("bootstrap 资产片数与 manifest 不符"));
        }
        let reference = AssetRef {
            real_sha256: manifest_asset.real_sha256,
            byte_len: manifest_asset.byte_len,
            media_type: manifest_asset.media_type.clone(),
            object: crate::upload::UploadedObject {
                object_id: manifest_asset.object_id,
                part_count: manifest_asset.part_count,
                total_plain_len: header.total_plaintext_len,
                object_digest: manifest_asset.object_digest,
            },
        };
        verify_asset_plaintext(&bytes, &reference)?;
        let body_start = 42 + manifest_asset.media_type.len();
        let data = bytes
            .get(body_start..)
            .ok_or(SyncError::Object("bootstrap 资产明文越界"))?
            .to_vec();
        assets.push(VerifiedAsset {
            real_sha256: manifest_asset.real_sha256,
            byte_len: manifest_asset.byte_len,
            media_type: manifest_asset.media_type.clone(),
            data,
        });
    }
    Ok(assets)
}

/// 读第 0 片拿 header，再按 header 的 part_count/total_len 读全对象并校验摘要/解密。
fn fetch_self_described<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: &[u8; 32],
    expected_digest: Option<&[u8; 32]>,
    descriptor: &[u8],
) -> Result<(EnvelopeHeader, Vec<u8>), SyncError> {
    let first = client.part(object_id, 0)?;
    let envelope = Envelope::decode(&first)?;
    if envelope.header.object_type != object_type
        || envelope.header.object_id != *object_id
        || envelope.header.part_count == 0
    {
        return Err(SyncError::Object("bootstrap 对象 header 不符"));
    }
    let digest = match expected_digest {
        Some(expected) => *expected,
        None => single_object_digest(&first)?,
    };
    fetch_object(
        client,
        root,
        object_type,
        object_id,
        envelope.header.part_count,
        envelope.header.total_plaintext_len,
        &digest,
        descriptor,
    )
}

fn persist_body(vault: &Vault, peer_id: u64, doc: &VerifiedDoc) -> Result<(), SyncError> {
    if doc_exists(vault, &doc.doc_id)? {
        // 本地已有：按 Loro VV 合并，保留本地离线编辑（不能直接跳过覆盖）。
        let (existing, current) = load_body_doc(vault, &doc.doc_id, peer_id, None)?;
        let before = existing.vv();
        existing.import(&doc.snapshot.loro_snapshot_bytes)?;
        let pending = existing.pending_update(&before)?;
        if !pending.payload.is_empty() {
            append_updates(
                vault,
                &doc.doc_id,
                current.head_seq,
                &[UpdateRecord::new("bootstrap", pending)],
            )?;
        }
        return Ok(());
    }
    create_body_doc(vault, &doc.doc_id)?;
    let body = BodyDoc::restore(
        doc.doc_id.clone(),
        peer_id,
        Some(&doc.snapshot.loro_snapshot_bytes),
        &[],
    )?;
    let update = body.pending_update(&VersionVector::default())?;
    append_updates(
        vault,
        &doc.doc_id,
        0,
        &[UpdateRecord::new("bootstrap", update)],
    )?;
    Ok(())
}

fn persist_structure(vault: &Vault, peer_id: u64, doc: &VerifiedDoc) -> Result<(), SyncError> {
    ensure_structure_row(vault)?;
    let mut writer = StructureWriter::open(vault, peer_id)?;
    writer.doc().import(&doc.snapshot.loro_snapshot_bytes)?;
    writer.commit("bootstrap")?;
    Ok(())
}

fn single_object_digest(envelope_bytes: &[u8]) -> Result<[u8; 32], SyncError> {
    let part = Sha256::digest(envelope_bytes);
    let mut input = Vec::with_capacity(4 + 4 + 32);
    input.extend_from_slice(&1u32.to_be_bytes());
    input.extend_from_slice(&0u32.to_be_bytes());
    input.extend_from_slice(&part);
    Ok(Sha256::digest(&input).into())
}

#[allow(dead_code)]
fn _manifest_types(_: &ManifestAsset, _: &ManifestDoc) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(key_epoch: u64, total_plain_len: u64) -> ManifestV1 {
        ManifestV1 {
            vault_id: [1; 16],
            key_epoch: 1,
            checkpoint_id: [2; 16],
            covered_relay_seq: 3,
            min_reader: 1,
            structure_doc_id: "structure".to_owned(),
            docs: vec![ManifestDoc {
                doc_id: "structure".to_owned(),
                kind: 0,
                generation: 1,
                key_epoch,
                vv: vec![1],
                frontier: vec![2],
                snapshot_object_id: [3; 32],
                part_count: 1,
                total_plain_len,
                object_digest: [4; 32],
            }],
            assets: vec![],
        }
    }

    fn digest(value: &ManifestV1) -> [u8; 32] {
        Sha256::digest(refs_digest_input(&manifest_refs(value)).expect("refs")).into()
    }

    #[test]
    fn manifest_key_epoch_or_total_len_tamper_changes_refs_digest() {
        let base = digest(&manifest(1, 100));
        assert_ne!(
            digest(&manifest(2, 100)),
            base,
            "key_epoch 篡改必须改变 refs_digest"
        );
        assert_ne!(
            digest(&manifest(1, 101)),
            base,
            "total_plain_len 篡改必须改变 refs_digest"
        );
    }
}
