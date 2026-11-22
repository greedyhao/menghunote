//! 轮换阶段 D–F（S7-29；ADR §28.2）。
//!
//! - D：冻结资产全量重加密（新根 HMAC object_id + §19 确定性 nonce）；全 docs 走 checkpoint
//!   full-history snapshot；不改变 peer/device/doc generation。
//! - E：两份基线独立导出/读回/空上下文验证；对象身份独立、assets 可共享同 epoch 对象；
//!   两 commit 的 S 必须 ≥ freeze_seq。
//! - F：对每个未吊销设备封 HPKE root grant 并生成待 stage 的 leaf manifest。
//!
//! 本模块不写 relay 侧冻结资产分页（relay task-97）；资产集合由调用方从 frozen 登记表/
//! 源列表传入，保证墓碑与无引用资产也被覆盖。

use sha2::{Digest, Sha256};

use crate::api_version::RelayApi;
use crate::checkpoint::{asset_object_id, asset_plaintext, AssetSource, CheckpointWriter};
use crate::device::{DeviceSigningKey, ManagementKey};
use crate::error::SyncError;
use crate::hpke::seal_epoch_grant;
use crate::keys::RootSecretV1;
use crate::relay::RelayClient;
use crate::upload::{asset_message_id, fetch_object, upload_object};
use crate::{DeviceId, ObjectId, VaultId};
use menghunote_storage::Vault;
use menghunote_sync_proto::{GrantAadV1, ObjectType, SnapshotV1};

/// 阶段 D 产出的重加密资产。
#[derive(Debug, Clone)]
pub struct ReencryptedAsset {
    pub real_sha256: [u8; 32],
    pub byte_len: u64,
    pub media_type: String,
    pub object_id: ObjectId,
    pub object_digest: [u8; 32],
}

/// 阶段 D：冻结资产**全量**下载→验真实 SHA256→新根重加密上传。
///
/// `assets` 必须包含墓碑/不可见引用/无引用 finalized 资产（调用方从 frozen 登记表传入）。
#[allow(clippy::too_many_arguments)]
pub fn reencrypt_assets<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    old_root: &RootSecretV1,
    new_root: &RootSecretV1,
    assets: &dyn AssetSource,
    reservation: Option<&[u8; 16]>,
) -> Result<Vec<ReencryptedAsset>, SyncError> {
    if new_root.vault_id != vault_id || old_root.vault_id != vault_id {
        return Err(SyncError::Object("重加密 root 归属 vault 不符"));
    }
    if new_root.key_epoch <= old_root.key_epoch {
        return Err(SyncError::Object("重加密目标 epoch 必须更大"));
    }
    let mut out = Vec::new();
    for meta in assets.list(vault)? {
        let data = assets.read(vault, &meta)?;
        let real: [u8; 32] = Sha256::digest(&data).into();
        if real != meta.real_sha256 || data.len() as u64 != meta.byte_len {
            return Err(SyncError::Object("冻结资产字节与元数据不一致"));
        }
        let plaintext = asset_plaintext(&real, meta.byte_len, &meta.media_type, &data)?;
        let object_id = asset_object_id(new_root, &real, meta.byte_len, &meta.media_type)?;
        let message_id = asset_message_id(new_root, &object_id)?;
        let object = if let Some(reservation) = reservation {
            crate::upload::upload_object_with_reservation(
                client,
                vault_id,
                device_id,
                signing_key,
                new_root,
                ObjectType::Asset,
                object_id,
                message_id,
                0,
                0,
                &plaintext,
                &[],
                None,
                reservation,
            )?
        } else {
            upload_object(
                client,
                vault_id,
                device_id,
                signing_key,
                new_root,
                ObjectType::Asset,
                object_id,
                message_id,
                0,
                0,
                &plaintext,
                &[],
                None,
            )?
        };
        crate::asset_sync::mark_uploaded(
            vault,
            &crate::asset_sync::AssetUploadedFact {
                object_id: object.object_id,
                part_count: object.part_count,
                total_plain_len: object.total_plain_len,
                object_digest: object.object_digest,
                key_epoch: new_root.key_epoch,
            },
        )?;
        out.push(ReencryptedAsset {
            real_sha256: real,
            byte_len: meta.byte_len,
            media_type: meta.media_type,
            object_id: object.object_id,
            object_digest: object.object_digest,
        });
    }
    Ok(out)
}

/// 独立空上下文里的一份基线证据（不共享任何 Loro/Vault 状态）。
#[derive(Debug, Clone)]
pub struct BaselineEvidence {
    pub checkpoint_id: [u8; 16],
    pub covered_seq: u64,
    pub manifest_object_id: ObjectId,
    pub manifest_s: u64,
    pub docs: Vec<DocEvidence>,
    pub asset_count: usize,
    pub asset_set_digest: [u8; 32],
    /// 该基线实际引用的 snapshot object id（用于证明两份基线不复用对象）。
    pub snapshot_ids: Vec<ObjectId>,
}

/// 单个 doc 快照的公开字段。
#[derive(Debug, Clone)]
pub struct DocEvidence {
    pub doc_id: String,
    pub generation: u64,
    pub vv: Vec<u8>,
    pub frontier: Vec<u8>,
    pub min_reader: u32,
    pub full_history: bool,
}

/// 从 relay 全量读回一份 prepared baseline 并解码/验证（不导入任何 Vault）。
pub fn capture_baseline_evidence<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    prepared: &crate::checkpoint::PreparedCheckpoint,
    frozen_asset_hashes: &[[u8; 32]],
) -> Result<BaselineEvidence, SyncError> {
    let report = &prepared.report;
    let mut docs = Vec::with_capacity(report.docs.len());
    for doc in &report.docs {
        let (_, bytes) = fetch_object(
            client,
            root,
            ObjectType::Snapshot,
            &doc.object.object_id,
            doc.object.part_count,
            doc.object.total_plain_len,
            &doc.object.object_digest,
            &[],
        )?;
        let snapshot = SnapshotV1::decode(&bytes)?;
        verify_snapshot_integrity(&snapshot)?;
        if snapshot.doc_id != doc.doc_id
            || snapshot.generation != doc.generation
            || !snapshot.full_history
        {
            return Err(SyncError::Object("基线快照不是 full-history/字段不符"));
        }
        docs.push(DocEvidence {
            doc_id: snapshot.doc_id,
            generation: snapshot.generation,
            vv: snapshot.vv,
            frontier: snapshot.frontier,
            min_reader: snapshot.min_reader,
            full_history: snapshot.full_history,
        });
    }

    let mut asset_hashes = Vec::with_capacity(report.assets.len());
    for asset in &report.assets {
        let (_, payload) = fetch_object(
            client,
            root,
            ObjectType::Asset,
            &asset.object.object_id,
            asset.object.part_count,
            asset.object.total_plain_len,
            &asset.object.object_digest,
            &[],
        )?;
        let decoded = decode_asset_plaintext(&payload)?;
        if decoded.0 != asset.real_sha256
            || decoded.1 != asset.byte_len
            || decoded.2 != asset.media_type
        {
            return Err(SyncError::Object("基线资产明文与 manifest 不符"));
        }
        asset_hashes.push(asset.real_sha256);
    }
    asset_hashes.sort_unstable();
    let mut frozen = frozen_asset_hashes.to_vec();
    frozen.sort_unstable();
    if asset_hashes != frozen {
        return Err(SyncError::Object("基线资产集合不等于冻结资产集合"));
    }
    let mut input = Vec::with_capacity(4 + frozen.len() * 32);
    input.extend_from_slice(&(frozen.len() as u32).to_be_bytes());
    for hash in &frozen {
        input.extend_from_slice(hash);
    }
    Ok(BaselineEvidence {
        checkpoint_id: report.checkpoint_id,
        covered_seq: report.covered_seq,
        manifest_object_id: report.manifest.object_id,
        manifest_s: report.covered_seq,
        asset_count: frozen.len(),
        asset_set_digest: Sha256::digest(&input).into(),
        snapshot_ids: report.docs.iter().map(|doc| doc.object.object_id).collect(),
        docs,
    })
}

/// T18：full-history 且 VV 必须与快照内实际 Loro 历史一致。
pub fn verify_snapshot_integrity(snapshot: &SnapshotV1) -> Result<(), SyncError> {
    if !snapshot.full_history {
        return Err(SyncError::Object("基线快照不是 full-history（T04/T18）"));
    }
    let doc = loro::LoroDoc::new();
    doc.import(&snapshot.loro_snapshot_bytes)
        .map_err(|_| SyncError::Object("快照 Loro 数据非法"))?;
    // 标志位与 VV 都不是 full-history 证据：Loro shallow snapshot 必须单独拒绝。
    crate::checkpoint::ensure_snapshot_full_history(&doc)?;
    let actual = doc.oplog_vv();
    let claimed = if snapshot.vv.is_empty() {
        loro::VersionVector::new()
    } else {
        loro::VersionVector::decode(&snapshot.vv).map_err(|_| SyncError::Object("快照 VV 非法"))?
    };
    if actual != claimed {
        return Err(SyncError::Object(
            "快照宣称 VV 与实际 Loro 历史不一致（T18）",
        ));
    }
    Ok(())
}

/// 阶段 E：两份基线必须对象独立、S 覆盖冻结点，且各自证据可解密验证。
pub fn assert_independent_baselines(
    first: &BaselineEvidence,
    second: &BaselineEvidence,
    freeze_seq: u64,
) -> Result<(), SyncError> {
    if first.checkpoint_id == second.checkpoint_id
        || first.manifest_object_id == second.manifest_object_id
    {
        return Err(SyncError::Object("两份基线复用了同一 cid/manifest"));
    }
    if first.covered_seq < freeze_seq || second.covered_seq < freeze_seq {
        return Err(SyncError::Object("基线 S 未覆盖冻结点"));
    }
    if first.asset_set_digest != second.asset_set_digest {
        return Err(SyncError::Object("两份基线的资产集合不一致"));
    }
    if first
        .snapshot_ids
        .iter()
        .any(|id| second.snapshot_ids.contains(id))
    {
        return Err(SyncError::Object("两份基线复用了 snapshot 对象"));
    }
    Ok(())
}

/// 从两份 prepared 中提取 snapshot object id 集合（用于证明身份不重叠）。
pub fn snapshot_object_ids(prepared: &crate::checkpoint::PreparedCheckpoint) -> Vec<ObjectId> {
    prepared
        .report
        .docs
        .iter()
        .map(|doc| doc.object.object_id)
        .collect()
}

/// frozen 分页结果：资产列表、relay asset_set_digest、total。
pub type FrozenAssets = (Vec<crate::relay::FrozenAssetRef>, Option<[u8; 32]>, u64);
/// 重加密结果：新资产、源 asset_set_digest、源 total。
pub type ReencryptedAssets = (Vec<ReencryptedAsset>, Option<[u8; 32]>, u64);

/// 阶段 B 的真实管理 realm begin（device client 不能调用 `POST /rotations`）。
#[allow(clippy::too_many_arguments)]
pub fn begin_rotation_http<T: crate::http_relay::HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault: &Vault,
    journal: &mut crate::rotation_journal::RotationJournal,
    vault_id: VaultId,
    instance_id: &[u8; 16],
    log_id: [u8; 16],
    manager: &ManagementKey,
    revoke: &[[u8; 16]],
    now: u64,
) -> Result<StartedRotation, SyncError> {
    let (token, _) =
        crate::recovery_enroll::management_token(transport, api, vault_id, manager, now)?;
    let begin = crate::rotation::sign_begin(
        manager,
        instance_id,
        vault_id,
        journal.request_id,
        journal.from_epoch,
        journal.control_rev,
        revoke,
        api.version(),
    )?;
    let signed = crate::cbor::CdeValue::Map(vec![
        (0, crate::cbor::bytes(begin.signed_body.clone())),
        (1, crate::cbor::bytes(begin.signature.to_vec())),
    ]);
    let response = crate::recovery_enroll::send(
        transport,
        "POST",
        api.url(&api.vault_path(&vault_id, "/rotations")),
        Some(&token),
        crate::cbor::encode(vec![
            (0, signed),
            (1, crate::cbor::bytes(begin.request_id.to_vec())),
            (2, crate::cbor::uint(begin.expected_control_rev)),
            (3, crate::cbor::uint(begin.expected_epoch)),
            (
                4,
                crate::cbor::array(
                    begin
                        .revoke
                        .iter()
                        .map(|device| crate::cbor::bytes(device.to_vec()))
                        .collect(),
                ),
            ),
        ])?,
    )?;
    let fields = crate::recovery_enroll::decode(&response)?;
    let view = crate::cbor::value_map(crate::cbor::field(&fields, 0)?)?;
    let started = StartedRotation {
        rotation_id: crate::cbor::bytes_fixed(&view, 0)?,
        state: crate::cbor::text_field(&view, 1)?,
        from_epoch: crate::cbor::uint_field(&view, 2)?,
        to_epoch: crate::cbor::uint_field(&view, 3)?,
        freeze_seq: crate::cbor::uint_field(&view, 4)?,
    };
    if started.freeze_seq != journal.freeze_seq.unwrap_or(started.freeze_seq) {
        return Err(SyncError::RelayWire("begin S 与 journal 冻结点不符"));
    }
    journal.mark_begun(started.rotation_id, started.freeze_seq, log_id)?;
    journal.save(vault)?;
    Ok(started)
}

/// 管理 realm begin 的视图（不需要完整 RotationView）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedRotation {
    pub rotation_id: [u8; 16],
    pub state: String,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub freeze_seq: u64,
}

/// 阶段 D：用 relay 的 frozen finalized 资产分页（不依赖 log）全量取回。
#[allow(unused_assignments)]
pub fn fetch_frozen_assets<C: RelayClient>(
    client: &mut C,
    rotation_id: [u8; 16],
    page_limit: u32,
) -> Result<FrozenAssets, SyncError> {
    if page_limit == 0 {
        return Err(SyncError::Unsupported("frozen asset 分页 limit 必须 >0"));
    }
    let mut out = Vec::new();
    let mut after = None;
    let mut digest = None;
    let mut total = None;
    loop {
        let page = client.rotation_assets(rotation_id, after, page_limit)?;
        total = Some(page.total);
        if let Some(page_digest) = page.asset_set_digest {
            if digest.is_some_and(|existing| existing != page_digest) {
                return Err(SyncError::RelayWire("frozen 资产分页 digest 变化"));
            }
            digest = Some(page_digest);
        }
        let last = page.assets.last().map(|asset| asset.object_id);
        out.extend(page.assets);
        if out.len() as u64 >= total.unwrap_or(0) || last.is_none() {
            break;
        }
        after = last;
    }
    let total = total.unwrap_or(0);
    if out.len() as u64 != total {
        return Err(SyncError::RelayWire("frozen 资产分页数量与 total 不符"));
    }
    Ok((out, digest, total))
}

/// 阶段 D：按 relay frozen 分页逐资产解密/验证/新根重加密。
#[allow(clippy::too_many_arguments)]
pub fn reencrypt_frozen_assets<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    old_root: &RootSecretV1,
    new_root: &RootSecretV1,
    rotation_id: [u8; 16],
    page_limit: u32,
    reservation: Option<&[u8; 16]>,
) -> Result<ReencryptedAssets, SyncError> {
    let (refs, digest, total) = fetch_frozen_assets(client, rotation_id, page_limit)?;
    let mut out = Vec::with_capacity(refs.len());
    for asset in refs {
        let (_, payload) = fetch_object(
            client,
            old_root,
            ObjectType::Asset,
            &asset.object_id,
            asset.part_count,
            asset.total_plain_len,
            &asset.object_digest,
            &[],
        )?;
        let (real, byte_len, media_type) = decode_asset_plaintext(&payload)?;
        let plaintext =
            asset_plaintext(&real, byte_len, &media_type, payload_asset_data(&payload)?)?;
        let object_id = asset_object_id(new_root, &real, byte_len, &media_type)?;
        let message_id = asset_message_id(new_root, &object_id)?;
        let object = if let Some(reservation) = reservation {
            crate::upload::upload_object_with_reservation(
                client,
                vault_id,
                device_id,
                signing_key,
                new_root,
                ObjectType::Asset,
                object_id,
                message_id,
                0,
                0,
                &plaintext,
                &[],
                None,
                reservation,
            )?
        } else {
            upload_object(
                client,
                vault_id,
                device_id,
                signing_key,
                new_root,
                ObjectType::Asset,
                object_id,
                message_id,
                0,
                0,
                &plaintext,
                &[],
                None,
            )?
        };
        crate::asset_sync::mark_uploaded(
            vault,
            &crate::asset_sync::AssetUploadedFact {
                object_id: object.object_id,
                part_count: object.part_count,
                total_plain_len: object.total_plain_len,
                object_digest: object.object_digest,
                key_epoch: new_root.key_epoch,
            },
        )?;
        out.push(ReencryptedAsset {
            real_sha256: real,
            byte_len,
            media_type,
            object_id: object.object_id,
            object_digest: object.object_digest,
        });
    }
    Ok((out, digest, total))
}

pub(crate) fn payload_asset_data(payload: &[u8]) -> Result<&[u8], SyncError> {
    if payload.len() < 42 {
        return Err(SyncError::Object("资产明文过短"));
    }
    let mime_len = usize::from(u16::from_be_bytes(
        payload[40..42]
            .try_into()
            .map_err(|_| SyncError::Object("资产 MIME 长度字段非法"))?,
    ));
    payload
        .get(42 + mime_len..)
        .ok_or(SyncError::Object("资产数据越界"))
}

/// 阶段 E：候选 checkpoint 必须 ready、role 固定且尚未被 activate 对外生效。
pub fn assert_candidate_ready<C: RelayClient>(
    client: &mut C,
    checkpoint_id: [u8; 16],
    expected_role: &str,
    freeze_seq: u64,
) -> Result<crate::relay::CheckpointStatus, SyncError> {
    let status = client.checkpoint_status(checkpoint_id)?;
    if status.state != "candidate" {
        return Err(SyncError::Object(
            "基线 checkpoint 不是 candidate（已对外生效？）",
        ));
    }
    if status.role.as_deref() != Some(expected_role) {
        return Err(SyncError::Object(
            "基线 role 不是 primary/fallback 固定角色",
        ));
    }
    if !status.candidate_ready {
        return Err(SyncError::Object("候选基线尚未 ready"));
    }
    if status.covered_seq < freeze_seq {
        return Err(SyncError::Object("候选基线未覆盖冻结点"));
    }
    Ok(status)
}

/// §30.3：restart（canonical body 空）；失败不要 abort，调用方先查 current。
#[allow(clippy::too_many_arguments)]
pub fn restart_rotation<T: crate::http_relay::HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: VaultId,
    rotation_id: [u8; 16],
    manager: &ManagementKey,
    instance_id: &[u8; 16],
    request_id: [u8; 16],
    expected_control_rev: u64,
    now: u64,
) -> Result<(), SyncError> {
    let (token, _) =
        crate::recovery_enroll::management_token(transport, api, vault_id, manager, now)?;
    let path = api.vault_path(
        &vault_id,
        &format!(
            "/rotations/{}/restart",
            crate::base64url::encode(&rotation_id)
        ),
    );
    let body = cbor_empty();
    let (signed, signature) = sign_management(
        manager,
        instance_id,
        vault_id,
        request_id,
        expected_control_rev,
        &path,
        &body,
    )?;
    let response = crate::recovery_enroll::send(
        transport,
        "POST",
        api.url(&path),
        Some(&token),
        crate::cbor::encode(vec![
            (0, signed),
            (1, crate::cbor::bytes(request_id.to_vec())),
            (2, crate::cbor::uint(expected_control_rev)),
        ])?,
    )?;
    let fields = crate::recovery_enroll::decode(&response)?;
    let _view = crate::cbor::value_map(crate::cbor::field(&fields, 0)?)?;
    let _ = signature;
    Ok(())
}

/// §30.3：revoke-more（canonical body `{0: revoke[]}`，外层键 3 必须逐字节镜像）。
#[allow(clippy::too_many_arguments)]
pub fn revoke_more<T: crate::http_relay::HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: VaultId,
    rotation_id: [u8; 16],
    manager: &ManagementKey,
    instance_id: &[u8; 16],
    request_id: [u8; 16],
    expected_control_rev: u64,
    revoke: &[[u8; 16]],
    now: u64,
) -> Result<u64, SyncError> {
    let (token, _) =
        crate::recovery_enroll::management_token(transport, api, vault_id, manager, now)?;
    let path = api.vault_path(
        &vault_id,
        &format!(
            "/rotations/{}/revoke-more",
            crate::base64url::encode(&rotation_id)
        ),
    );
    let body = crate::cbor::encode(vec![(
        0,
        crate::cbor::array(
            revoke
                .iter()
                .map(|device| crate::cbor::bytes(device.to_vec()))
                .collect(),
        ),
    )])?;
    let (signed, _) = sign_management(
        manager,
        instance_id,
        vault_id,
        request_id,
        expected_control_rev,
        &path,
        &body,
    )?;
    let response = crate::recovery_enroll::send(
        transport,
        "POST",
        api.url(&path),
        Some(&token),
        crate::cbor::encode(vec![
            (0, signed),
            (1, crate::cbor::bytes(request_id.to_vec())),
            (2, crate::cbor::uint(expected_control_rev)),
            (
                3,
                crate::cbor::array(
                    revoke
                        .iter()
                        .map(|device| crate::cbor::bytes(device.to_vec()))
                        .collect(),
                ),
            ),
        ])?,
    )?;
    let fields = crate::recovery_enroll::decode(&response)?;
    let _view = crate::cbor::value_map(crate::cbor::field(&fields, 0)?)?;
    crate::cbor::uint_field(&fields, 2)
}

/// staged grants 读回：leaf manifests、grants digest、count。
pub type StagedGrants = (Vec<Vec<u8>>, [u8; 32], u64);

/// §30.3：management 读 staged grants（叶子 manifest + digest/count）。
pub fn list_staged_grants<T: crate::http_relay::HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: VaultId,
    rotation_id: [u8; 16],
    manager: &ManagementKey,
    now: u64,
) -> Result<StagedGrants, SyncError> {
    // 返回值即 relay `{0 leaves,1 grants_digest,2 count}`。
    let (token, _) =
        crate::recovery_enroll::management_token(transport, api, vault_id, manager, now)?;
    let response = crate::recovery_enroll::send(
        transport,
        "GET",
        api.vault_path(
            &vault_id,
            &format!(
                "/rotations/{}/grants",
                crate::base64url::encode(&rotation_id)
            ),
        ),
        Some(&token),
        Vec::new(),
    )?;
    let fields = crate::recovery_enroll::decode(&response)?;
    let leaves = crate::cbor::array_field(&fields, 0)?
        .iter()
        .map(crate::cbor::value_bytes)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((
        leaves,
        crate::cbor::bytes_fixed(&fields, 1)?,
        crate::cbor::uint_field(&fields, 2)?,
    ))
}

fn cbor_empty() -> Vec<u8> {
    let fields: crate::cbor::Fields = Vec::new();
    crate::cbor::encode(fields).unwrap_or_default()
}

/// 管理写签名（`POST=2` + 完整 path + instance/vault/request_id/expected_control_rev +
/// `SHA256(CDE{method,path,body})`）。
///
/// `pub(crate)`：ADR 0010 的 `device_management::revoke_device` 复用同一实现，避免复制
/// 签名输入拼接（wire 变更必须只有一处）。
pub(crate) fn sign_management(
    manager: &ManagementKey,
    instance_id: &[u8; 16],
    vault_id: VaultId,
    request_id: [u8; 16],
    expected_control_rev: u64,
    path: &str,
    body: &[u8],
) -> Result<(crate::cbor::CdeValue, [u8; 64]), SyncError> {
    let digest: [u8; 32] = Sha256::digest(menghunote_sync_proto::management_request_cde(
        menghunote_sync_proto::HttpMethod::Post,
        path,
        body,
    )?)
    .into();
    let signature = manager.sign(&menghunote_sync_proto::management_signing_input(
        instance_id,
        &vault_id,
        &request_id,
        expected_control_rev,
        &digest,
    )?);
    Ok((
        crate::cbor::CdeValue::Map(vec![
            (0, crate::cbor::bytes(body.to_vec())),
            (1, crate::cbor::bytes(signature.to_vec())),
        ]),
        signature,
    ))
}

/// 阶段 F：一个待 stage 的 grant leaf。
#[derive(Debug, Clone)]
pub struct GrantLeaf {
    pub device_id: DeviceId,
    pub epoch: u64,
    pub grant_id: [u8; 16],
    pub hpke_enc: [u8; 32],
    pub ciphertext: Vec<u8>,
    pub signed_header: Vec<u8>,
    pub signature: [u8; 64],
    pub manager_rev: u64,
    pub leaf_digest: [u8; 32],
}

impl GrantLeaf {
    /// leaf manifest 中用于 activate 锁定 digest 的规范字节。
    pub fn manifest_bytes(&self) -> Result<Vec<u8>, SyncError> {
        crate::cbor::encode(vec![
            (0, crate::cbor::bytes(self.device_id.to_vec())),
            (1, crate::cbor::uint(self.epoch)),
            (2, crate::cbor::bytes(self.grant_id.to_vec())),
            (3, crate::cbor::bytes(self.hpke_enc.to_vec())),
            (4, crate::cbor::bytes(self.ciphertext.clone())),
            (5, crate::cbor::bytes(self.signed_header.clone())),
            (6, crate::cbor::bytes(self.signature.to_vec())),
            (7, crate::cbor::uint(self.manager_rev)),
        ])
    }
}

/// 阶段 F：对一组（未吊销、含离线）设备封真实 root。
pub fn plan_grants(
    new_root: &RootSecretV1,
    manager: &ManagementKey,
    manager_rev: u64,
    recipients: &[(DeviceId, [u8; 32])],
) -> Result<Vec<GrantLeaf>, SyncError> {
    let mut leaves = Vec::with_capacity(recipients.len());
    for (device_id, hpke_pub) in recipients {
        let grant_id: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
        let aad = GrantAadV1 {
            pv: 1,
            vault_id: new_root.vault_id,
            epoch: new_root.key_epoch,
            recipient_device_id: *device_id,
            recipient_x25519_pub: *hpke_pub,
            manager_rev,
            grant_id,
        };
        let sealed = seal_epoch_grant(hpke_pub, new_root, &aad)?;
        let signed_header = sealed.signature_input(&aad)?;
        let signature = manager.sign(&signed_header);
        let mut leaf = GrantLeaf {
            device_id: *device_id,
            epoch: new_root.key_epoch,
            grant_id,
            hpke_enc: sealed.enc,
            ciphertext: sealed.ciphertext,
            signed_header,
            signature,
            manager_rev,
            leaf_digest: [0u8; 32],
        };
        let manifest = leaf.manifest_bytes()?;
        leaf.leaf_digest = Sha256::digest(&manifest).into();
        leaves.push(leaf);
    }
    Ok(leaves)
}

/// 解码 asset plaintext 前缀（32B real sha + BE64 len + BE16 mime len + mime + data）。
pub(crate) fn decode_asset_plaintext(bytes: &[u8]) -> Result<([u8; 32], u64, String), SyncError> {
    if bytes.len() < 42 {
        return Err(SyncError::Object("资产明文过短"));
    }
    let real = bytes[..32]
        .try_into()
        .map_err(|_| SyncError::Object("资产 real_sha256 长度非法"))?;
    let len = u64::from_be_bytes(
        bytes[32..40]
            .try_into()
            .map_err(|_| SyncError::Object("资产长度字段非法"))?,
    );
    let mime_len = usize::from(u16::from_be_bytes(
        bytes[40..42]
            .try_into()
            .map_err(|_| SyncError::Object("资产 MIME 长度字段非法"))?,
    ));
    let mime_end = 42 + mime_len;
    let mime = std::str::from_utf8(
        bytes
            .get(42..mime_end)
            .ok_or(SyncError::Object("资产 MIME 越界"))?,
    )
    .map_err(|_| SyncError::Object("资产 MIME 不是 UTF-8"))?
    .to_owned();
    let data = bytes
        .get(mime_end..)
        .ok_or(SyncError::Object("资产数据越界"))?;
    if data.len() as u64 != len {
        return Err(SyncError::Object("资产数据长度与明文字段不符"));
    }
    let hash: [u8; 32] = Sha256::digest(data).into();
    if hash != real {
        return Err(SyncError::Object("资产真实 SHA256 不符"));
    }
    Ok((real, len, mime))
}

/// 阶段 F：把 grant leaf stage 到 relay（management realm；§28.1 条目 5）。
pub fn stage_grants<T: crate::http_relay::HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: VaultId,
    rotation_id: [u8; 16],
    manager: &ManagementKey,
    leaves: &[GrantLeaf],
    now: u64,
) -> Result<(), SyncError> {
    let (token, _) =
        crate::recovery_enroll::management_token(transport, api, vault_id, manager, now)?;
    for leaf in leaves {
        // URL 里的 `{device_id}` 只是设备维度；body 键 0 是**真实 grant_id**（grant AAD
        // 里绑定的那个），relay 按 grant_id 验签/落库，不能拿 device_id 顶替。
        let body = menghunote_sync_proto::GrantStageBodyV1 {
            grant_id: leaf.grant_id,
            hpke_enc: leaf.hpke_enc,
            hpke_ciphertext: leaf.ciphertext.clone(),
            signed_header: leaf.signed_header.clone(),
            signature: leaf.signature.to_vec(),
            manager_rev: leaf.manager_rev,
        }
        .encode()?;
        let response = crate::recovery_enroll::send(
            transport,
            "PUT",
            api.url(&api.vault_path(
                &vault_id,
                &format!(
                    "/rotations/{}/grants/{}",
                    crate::base64url::encode(&rotation_id),
                    crate::base64url::encode(&leaf.device_id)
                ),
            )),
            Some(&token),
            body,
        )?;
        let fields = crate::recovery_enroll::decode(&response)?;
        let _view = crate::cbor::value_map(crate::cbor::field(&fields, 0)?)?;
    }
    Ok(())
}

/// 阶段 F：把 grant leaf manifest 持久化进 rotation journal（stage 网络调用等 relay 101）。
pub fn attach_grants(
    journal: &mut crate::rotation_journal::RotationJournal,
    vault: &Vault,
    leaves: &[GrantLeaf],
) -> Result<[u8; 32], SyncError> {
    let manifests = leaves
        .iter()
        .map(GrantLeaf::manifest_bytes)
        .collect::<Result<Vec<_>, _>>()?;
    journal.set_grants(vault, &manifests)
}

/// 便捷入口：用现有 `CheckpointWriter` 准备一份 full-history 基线（阶段 D/E）。
#[allow(clippy::too_many_arguments)]
pub fn prepare_new_epoch_baseline<'a, C: RelayClient>(
    vault: &'a Vault,
    client: &'a mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: DeviceSigningKey,
    new_root: RootSecretV1,
    peer_id: u64,
    assets: &'a dyn AssetSource,
) -> Result<crate::checkpoint::PreparedCheckpoint, SyncError> {
    prepare_new_epoch_baseline_with_role(
        vault,
        client,
        vault_id,
        device_id,
        signing_key,
        new_root,
        peer_id,
        assets,
        None,
    )
}

/// 同上，但为候选 epoch 固定 primary/fallback role（relay §28.1）。
#[allow(clippy::too_many_arguments)]
pub fn prepare_new_epoch_baseline_with_role<'a, C: RelayClient>(
    vault: &'a Vault,
    client: &'a mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: DeviceSigningKey,
    new_root: RootSecretV1,
    peer_id: u64,
    assets: &'a dyn AssetSource,
    role: Option<&str>,
) -> Result<crate::checkpoint::PreparedCheckpoint, SyncError> {
    prepare_baseline_with_context(
        vault,
        client,
        vault_id,
        device_id,
        signing_key,
        new_root,
        peer_id,
        assets,
        role,
        None,
        None,
    )
}

/// 同上，但显式传入候选 epoch 的 coverage 验证根（**旧 active 根**）。
///
/// 候选基线在 `from_epoch` 冻结点导出：`<= S` 的 ASSET 日志事件用旧根加密，而
/// manifest refs 里的资产用新 epoch 根加密。缺旧根时 validate 必须暂停/报错，
/// 绝不拿新根去"猜"。
#[allow(clippy::too_many_arguments)]
pub fn prepare_new_epoch_baseline_with_role_and_coverage_root<'a, C: RelayClient>(
    vault: &'a Vault,
    client: &'a mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: DeviceSigningKey,
    new_root: RootSecretV1,
    peer_id: u64,
    assets: &'a dyn AssetSource,
    role: Option<&str>,
    coverage_root: &RootSecretV1,
) -> Result<crate::checkpoint::PreparedCheckpoint, SyncError> {
    prepare_baseline_with_context(
        vault,
        client,
        vault_id,
        device_id,
        signing_key,
        new_root,
        peer_id,
        assets,
        role,
        None,
        Some(coverage_root.clone()),
    )
}

/// restart 候选基线：额外把 rotation/CAS 交给 writer，使 create intent 与
/// upload/validate/promote 各阶段进度都 durable（评审 0003 P1-D）。
///
/// 与 [`prepare_new_epoch_baseline_with_role`] 不同，本入口要求 writer 在
/// create 之前持久 per-role intent；崩溃/重试按同一 request_id/cid 恢复，绝不 fresh create。
#[allow(clippy::too_many_arguments)]
pub fn prepare_restart_baseline_with_role<'a, C: RelayClient>(
    vault: &'a Vault,
    client: &'a mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: DeviceSigningKey,
    new_root: RootSecretV1,
    peer_id: u64,
    assets: &'a dyn AssetSource,
    role: &str,
    rotation_id: [u8; 16],
    expected_control_rev: u64,
    coverage_root: &RootSecretV1,
) -> Result<crate::checkpoint::PreparedCheckpoint, SyncError> {
    prepare_baseline_with_context(
        vault,
        client,
        vault_id,
        device_id,
        signing_key,
        new_root,
        peer_id,
        assets,
        Some(role),
        Some((rotation_id, expected_control_rev)),
        Some(coverage_root.clone()),
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_baseline_with_context<'a, C: RelayClient>(
    vault: &'a Vault,
    client: &'a mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: DeviceSigningKey,
    new_root: RootSecretV1,
    peer_id: u64,
    assets: &'a dyn AssetSource,
    role: Option<&str>,
    restart_baseline: Option<([u8; 16], u64)>,
    coverage_root: Option<RootSecretV1>,
) -> Result<crate::checkpoint::PreparedCheckpoint, SyncError> {
    let mut writer = CheckpointWriter::new(
        vault,
        client,
        vault_id,
        device_id,
        signing_key,
        new_root,
        peer_id,
    );
    if let Some(role) = role {
        writer = writer.with_role(role);
    }
    if let Some((rotation_id, expected_control_rev)) = restart_baseline {
        writer = writer.with_restart_baseline(rotation_id, expected_control_rev);
    }
    if let Some(coverage_root) = coverage_root {
        writer = writer.with_coverage_root(coverage_root);
    }
    // ADR §32：候选基线同样必须 prepare → validate（真读回/导入）→ promote（签名发布）。
    // 返回的 PreparedCheckpoint 已标记 validated，供阶段 E 继续采集独立证据。
    let mut prepared = writer.prepare(assets)?;
    let baseline = writer.validate(&prepared)?;
    let promoted = writer.promote(&baseline)?;
    prepared.report.validated = true;
    prepared.report.published_at = Some(promoted.published_at);
    prepared.report.commit_digest = Some(promoted.commit_digest);
    Ok(prepared)
}
