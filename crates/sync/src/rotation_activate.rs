//! 轮换阶段 G/H：activate、grant 解根校验与本地原子推广。
//!
//! 只实现“发起设备”的路径；其它设备应用与 outbox 重打包见后续阶段 I/J。

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::device::{DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use crate::error::SyncError;
use crate::hpke::open_epoch_grant;
use crate::http_relay::HttpTransport;
use crate::identity::LocalIdentityV1;
use crate::keys::RootSecretV1;
use crate::recovery_enroll::{decode, device_token, management_token, send};
use crate::relay::RotationView;
use crate::rotation_journal::{RotationJournal, RotationPhase};
use crate::{DeviceId, VaultId};
use menghunote_sync_proto::{GrantAadV1, RotationActivateBodyV1};

/// 阶段 G 的 activate 材料。
#[derive(Clone)]
pub struct ActivationInput {
    pub rotation_id: [u8; 16],
    pub request_id: [u8; 16],
    pub expected_control_rev: u64,
    pub primary_checkpoint: [u8; 16],
    pub fallback_checkpoint: [u8; 16],
    pub member_cert_digest: [u8; 32],
    pub slot_set_digest: [u8; 32],
    pub grants_digest: [u8; 32],
    pub asset_set_digest: [u8; 32],
    pub new_manager: Option<menghunote_sync_proto::NewManagerV1>,
    /// 候选根（只用于推广与 grant 校验，不上 wire）。
    pub candidate_root: RootSecretV1,
    pub new_manager_public: [u8; 32],
    pub new_manager_rev: u64,
    /// 四个 digest 的 canonical 绑定版本。
    pub pv: u64,
}

impl ActivationInput {
    fn body(&self) -> RotationActivateBodyV1 {
        RotationActivateBodyV1 {
            new_manager: self.new_manager.clone(),
            primary_checkpoint: Some(self.primary_checkpoint),
            fallback_checkpoint: Some(self.fallback_checkpoint),
            member_cert_digest: Some(self.member_cert_digest),
            grants_digest: Some(self.grants_digest),
            slot_set_digest: Some(self.slot_set_digest),
            asset_set_digest: Some(self.asset_set_digest),
        }
    }
}

/// activate 成功后的 relay 结果（响应键 0–6，不是 rotation view）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivatedOutcome {
    pub epoch: u64,
    pub control_rev: u64,
    pub auth_rev: u64,
    pub member_set_hash: [u8; 32],
    pub primary_checkpoint: [u8; 16],
    pub fallback_checkpoint: [u8; 16],
    pub activation_commit: Vec<u8>,
}

/// 阶段 G：management activate；超时只查 `rotations/current`，不 abort。
#[allow(clippy::too_many_arguments)]
pub fn activate_http<T: HttpTransport>(
    transport: &mut T,
    base: &str,
    vault_id: VaultId,
    instance_id: &[u8; 16],
    manager: &ManagementKey,
    input: &ActivationInput,
    now: u64,
) -> Result<ActivatedOutcome, SyncError> {
    let body = input.body();
    let signed = crate::rotation::sign_activate(
        manager,
        instance_id,
        vault_id,
        input.rotation_id,
        input.request_id,
        input.expected_control_rev,
        &body,
    )?;
    let (token, _) = management_token(transport, base, vault_id, manager, now)?;
    let signed_value = crate::cbor::CdeValue::Map(vec![
        (0, crate::cbor::bytes(signed.body.clone())),
        (1, crate::cbor::bytes(signed.signature.to_vec())),
    ]);
    let path = format!(
        "{base}/v1/vaults/{}/rotations/{}/activate",
        crate::base64url::encode(&vault_id),
        crate::base64url::encode(&input.rotation_id)
    );
    let response = send(
        transport,
        "POST",
        path,
        Some(&token),
        crate::cbor::encode(vec![
            (0, signed_value),
            (1, crate::cbor::bytes(input.request_id.to_vec())),
            (2, crate::cbor::uint(input.expected_control_rev)),
        ])?,
    );
    match response {
        Ok(response) => {
            let fields = decode(&response)?;
            parse_activated_outcome(&fields)
        }
        Err(error @ SyncError::Transport(_)) => {
            let view = query_current_rotation(transport, base, vault_id, manager, now)?;
            if view.state == "activated" {
                Ok(ActivatedOutcome {
                    epoch: view.to_epoch,
                    control_rev: view.control_rev,
                    auth_rev: view.auth_rev,
                    member_set_hash: view.member_set_hash.unwrap_or_default(),
                    primary_checkpoint: view.primary_checkpoint.unwrap_or_default(),
                    fallback_checkpoint: view.fallback_checkpoint.unwrap_or_default(),
                    activation_commit: view.activation_commit.unwrap_or_default(),
                })
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

fn parse_activated_outcome(fields: &crate::cbor::Fields) -> Result<ActivatedOutcome, SyncError> {
    Ok(ActivatedOutcome {
        epoch: crate::cbor::uint_field(fields, 0)?,
        control_rev: crate::cbor::uint_field(fields, 1)?,
        auth_rev: crate::cbor::uint_field(fields, 2)?,
        member_set_hash: crate::cbor::bytes_fixed(fields, 3)?,
        primary_checkpoint: crate::cbor::bytes_fixed(fields, 4)?,
        fallback_checkpoint: crate::cbor::bytes_fixed(fields, 5)?,
        activation_commit: crate::cbor::bytes_field(fields, 6)?,
    })
}

fn query_current_rotation<T: HttpTransport>(
    transport: &mut T,
    base: &str,
    vault_id: VaultId,
    manager: &ManagementKey,
    now: u64,
) -> Result<RotationView, SyncError> {
    let (token, _) = management_token(transport, base, vault_id, manager, now)?;
    let response = send(
        transport,
        "GET",
        format!(
            "{base}/v1/vaults/{}/rotations/current",
            crate::base64url::encode(&vault_id)
        ),
        Some(&token),
        Vec::new(),
    )?;
    let fields = decode(&response)?;
    let view = crate::cbor::value_map(crate::cbor::field(&fields, 0)?)?;
    parse_activated_view(&view)
}

fn parse_activated_view(view: &crate::cbor::Fields) -> Result<RotationView, SyncError> {
    Ok(RotationView {
        rotation_id: crate::cbor::bytes_fixed(view, 0)?,
        state: crate::cbor::text_field(view, 1)?,
        from_epoch: crate::cbor::uint_field(view, 2)?,
        to_epoch: crate::cbor::uint_field(view, 3)?,
        freeze_seq: crate::cbor::uint_field(view, 4)?,
        revoked_count: crate::cbor::uint_field(view, 5)?,
        member_set_hash: None,
        grants_count: crate::cbor::uint_field(view, 7)?,
        grants_digest: None,
        published_checkpoints: crate::cbor::uint_field(view, 9)?,
        control_rev: crate::cbor::uint_field(view, 10)?,
        auth_rev: crate::cbor::uint_field(view, 11)?,
        manager_rev: crate::cbor::uint_field(view, 12)?,
        from_epoch_revoked_once: cbor_uint(view, 13) != 0,
        freeze_asset_count: cbor_uint(view, 15),
        freeze_asset_digest: None,
        member_cert_digest: None,
        primary_checkpoint: None,
        fallback_checkpoint: None,
        activation_commit: None,
        activated_at: cbor_opt_uint(view, 21),
        activated_control_rev: cbor_opt_uint(view, 22),
        candidate_count: 0,
        candidate_ready: false,
        reservation_id: None,
    })
}

fn cbor_uint(fields: &crate::cbor::Fields, key: u64) -> u64 {
    crate::cbor::uint_field(fields, key).unwrap_or(0)
}

fn cbor_opt_uint(fields: &crate::cbor::Fields, key: u64) -> Option<u64> {
    crate::cbor::opt_uint_field(fields, key).ok().flatten()
}

/// 阶段 H：读回自己的最新 grant，解封必须等于 candidate root。
#[allow(clippy::too_many_arguments)]
pub fn verify_latest_grant_root<T: HttpTransport>(
    transport: &mut T,
    base: &str,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    hpke_key: &DeviceHpkeKey,
    expected_root: &RootSecretV1,
    now: u64,
) -> Result<(), SyncError> {
    let token = device_token(transport, base, vault_id, device_id, signing_key, now)?;
    let response = send(
        transport,
        "GET",
        format!(
            "{base}/v1/vaults/{}/devices/self/grants/latest",
            crate::base64url::encode(&vault_id)
        ),
        Some(&token),
        Vec::new(),
    )?;
    let fields = decode(&response)?;
    let epoch = crate::cbor::uint_field(&fields, 0)?;
    let grant_device: [u8; 16] = crate::cbor::bytes_fixed(&fields, 1)?;
    let enc: [u8; 32] = crate::cbor::bytes_fixed(&fields, 3)?;
    let ciphertext = crate::cbor::bytes_field(&fields, 4)?;
    let signed_header = crate::cbor::bytes_field(&fields, 5)?;
    if grant_device != device_id || expected_root.key_epoch != epoch || signed_header.len() < 64 {
        return Err(SyncError::RelayWire("latest grant 与候选 epoch/设备不符"));
    }
    let aad_len = signed_header.len() - 64;
    let aad = GrantAadV1::decode(&signed_header[..aad_len])?;
    if aad.epoch != epoch
        || aad.recipient_device_id != device_id
        || aad.recipient_x25519_pub != hpke_key.public_key()
    {
        return Err(SyncError::RelayWire("latest grant AAD 与本地设备不符"));
    }
    let opened = open_epoch_grant(hpke_key.secret(), &enc, &aad, &ciphertext)?;
    if opened.encode()? != expected_root.encode()? {
        return Err(SyncError::Object(
            "latest grant 解出的 root 不是 candidate root，拒绝推广",
        ));
    }
    Ok(())
}

/// 阶段 H：原子推广 DPAPI 身份 + repack 屏障。
///
/// 写入顺序：临时身份 → repack fence → rename 原子替换；rename 失败时旧身份与 fence 保留，
/// 重启后只能按旧身份恢复并重试（不会出现半新半旧）。
pub fn promote_identity_atomic(
    vault: &menghunote_storage::Vault,
    identity_path: &Path,
    identity: &LocalIdentityV1,
    candidate_root: &RootSecretV1,
    new_manager_public: [u8; 32],
    new_manager_rev: u64,
) -> Result<LocalIdentityV1, SyncError> {
    if identity.current_epoch >= candidate_root.key_epoch {
        return Err(SyncError::Object("候选 epoch 必须高于当前身份 epoch"));
    }
    let promoted = LocalIdentityV1 {
        vault_id: identity.vault_id,
        device_id: identity.device_id,
        peer_id: identity.peer_id,
        ed25519_seed: identity.ed25519_seed.clone(),
        x25519_secret: identity.x25519_secret.clone(),
        current_epoch: candidate_root.key_epoch,
        current_root: candidate_root.root.clone(),
        manager_seed: identity.manager_seed.clone(),
        pinned_instance: identity.pinned_instance,
        pinned_manager: new_manager_public,
        control_rev: new_manager_rev,
        pending_rotation: None,
    };
    let blob = promoted.protect()?;
    let temp = identity_path.with_extension("promote.tmp");
    fs::write(&temp, &blob).map_err(|error| SyncError::Io(error.to_string()))?;
    vault.set_meta("sync.repack_required", b"1")?;
    fs::rename(&temp, identity_path).map_err(|error| SyncError::Io(error.to_string()))?;
    Ok(promoted)
}

/// 阶段 H：把 journal 标记为 Completed 并清除候选秘密。
pub fn finish_promotion(
    journal: &mut RotationJournal,
    vault: &menghunote_storage::Vault,
) -> Result<(), SyncError> {
    if journal.phase != RotationPhase::Activated {
        return Err(SyncError::Unsupported("promotion 需要 Activated journal"));
    }
    journal.advance(RotationPhase::Completed)?;
    journal.save(vault)
}

/// G 的 canonical body digest（供测试/日志，不参与签名）。
pub fn activate_body_digest(input: &ActivationInput) -> Result<[u8; 32], SyncError> {
    let body = input.body();
    Ok(Sha256::digest(body.encode()?).into())
}
