//! 轮换客户端阶段 B/C（S7-28；ADR §28.2）。
//!
//! - 阶段 B：管理签 canonical `POST begin`；journal 先持久化 request_id，响应丢失时**只按
//!   request_id 查询**，查询不可用/未找到时不盲 begin。
//! - 阶段 C：旧根 pull+integrate 到 S（必须 `integrated_seq >= S`，received 不够），
//!   固定 local cut L；pending outbox 由 [`RotationJournal::mark_integrated`] 拒绝。
//!
//! relay 的 request_id 查询端点由 relay task-101 提供；未提供时本模块返回原错误并保持
//! Frozen，调用方可安全重试查询。

use sha2::{Digest, Sha256};

use crate::cbor;
use crate::device::{verify_signature, ManagementKey};
use crate::error::SyncError;
use crate::relay::{RelayClient, RotationBegin, RotationView};
use crate::rotation_journal::{RotationJournal, RotationPhase};
use crate::VaultId;
use menghunote_storage::Vault;
use menghunote_sync_proto::{management_request_cde, management_signing_input, HttpMethod};

/// §27.2 管理转换 9 字段（本地解码/编码，不在 sync-proto 新增记录）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerTransitionV1 {
    pub vault_id: [u8; 16],
    pub instance_id: [u8; 16],
    pub rotation_id: [u8; 16],
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub old_rev: u64,
    pub new_rev: u64,
    pub new_pubkey: [u8; 32],
    pub covered_seq: u64,
}

impl ManagerTransitionV1 {
    /// CDE 编码（9 字段，键 0..8）。
    pub fn encode(&self) -> Result<Vec<u8>, SyncError> {
        cbor::encode(vec![
            (0, cbor::bytes(self.vault_id.to_vec())),
            (1, cbor::bytes(self.instance_id.to_vec())),
            (2, cbor::bytes(self.rotation_id.to_vec())),
            (3, cbor::uint(self.from_epoch)),
            (4, cbor::uint(self.to_epoch)),
            (5, cbor::uint(self.old_rev)),
            (6, cbor::uint(self.new_rev)),
            (7, cbor::bytes(self.new_pubkey.to_vec())),
            (8, cbor::uint(self.covered_seq)),
        ])
    }

    /// CDE 解码并校验 9 字段存在。
    pub fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        let map = cbor::decode(bytes)?;
        Ok(Self {
            vault_id: cbor::bytes_fixed(&map, 0)?,
            instance_id: cbor::bytes_fixed(&map, 1)?,
            rotation_id: cbor::bytes_fixed(&map, 2)?,
            from_epoch: cbor::uint_field(&map, 3)?,
            to_epoch: cbor::uint_field(&map, 4)?,
            old_rev: cbor::uint_field(&map, 5)?,
            new_rev: cbor::uint_field(&map, 6)?,
            new_pubkey: cbor::bytes_fixed(&map, 7)?,
            covered_seq: cbor::uint_field(&map, 8)?,
        })
    }
}

/// 阶段 A：校验 `/control` 管理链并 pin instance/log/control head。
///
/// 逐条验证 9 字段绑定、old/new 双签、rev 连续；`manager` 必须等于链尾公钥。
pub fn verify_management_chain_and_pin<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    journal: &mut RotationJournal,
    vault_id: VaultId,
    current_manager: &ManagementKey,
    first_old_public: Option<[u8; 32]>,
) -> Result<(), SyncError> {
    let state = client.state()?;
    let chain = client.management_chain()?;
    let mut prev_rev = 1u64;
    let mut prev_pub = first_old_public.unwrap_or_else(|| current_manager.public_key());
    let mut last_rev = 1u64;
    for entry in &chain {
        // rev 1 是 genesis 初始管理 key（无 transition/签名），从 rev 2 起才是管理转换。
        if entry.rev <= 1 {
            prev_rev = 1;
            prev_pub = entry.pubkey;
            last_rev = 1;
            continue;
        }
        let transition = ManagerTransitionV1::decode(&entry.transition)?;
        if transition.vault_id != vault_id
            || transition.instance_id != state.instance_id
            || transition.old_rev != prev_rev
            || transition.new_rev != entry.rev
            || transition.new_rev != transition.old_rev.saturating_add(1)
            || transition.to_epoch != transition.from_epoch.saturating_add(1)
            || transition.new_pubkey != entry.pubkey
            || entry.old_rev != prev_rev
        {
            return Err(SyncError::RelayWire("管理转换链字段/rev 不连续"));
        }
        let old_signature = entry
            .old_signature
            .ok_or(SyncError::RelayWire("管理转换链缺旧管理签名"))?;
        let new_signature = entry
            .new_signature
            .ok_or(SyncError::RelayWire("管理转换链缺新管理签名"))?;
        verify_signature(&prev_pub, &entry.transition, &old_signature)?;
        verify_signature(&entry.pubkey, &entry.transition, &new_signature)?;
        prev_rev = entry.rev;
        prev_pub = entry.pubkey;
        last_rev = entry.rev;
    }
    if let Some(last) = chain.last() {
        if last.pubkey != current_manager.public_key() {
            return Err(SyncError::RelayWire("管理链尾公钥与当前管理 key 不符"));
        }
    }
    journal.pinned_instance_id = Some(state.instance_id);
    journal.pinned_log_id = Some(state.log_id);
    journal.pinned_control_hash = Some(journal.control_hash);
    journal.control_chain_rev = Some(last_rev);
    journal.save(vault)?;
    Ok(())
}

/// 阶段 G：管理签的 activate canonical body（含四个 digest 键 3–6）。
#[derive(Debug, Clone)]
pub struct SignedActivation {
    /// `RotationActivateBodyV1` 的 canonical 字节。
    pub body: Vec<u8>,
    pub signature: [u8; 64],
}

/// 阶段 G：校验四个 digest 必填并管理签 activate body。
#[allow(clippy::too_many_arguments)]
pub fn sign_activate(
    manager: &ManagementKey,
    instance_id: &[u8; 16],
    vault_id: VaultId,
    rotation_id: [u8; 16],
    request_id: [u8; 16],
    expected_control_rev: u64,
    body: &menghunote_sync_proto::RotationActivateBodyV1,
) -> Result<SignedActivation, SyncError> {
    if body.member_cert_digest.is_none()
        || body.grants_digest.is_none()
        || body.slot_set_digest.is_none()
        || body.asset_set_digest.is_none()
    {
        return Err(SyncError::Unsupported(
            "activate body 必须携带 member/slot/grant/asset 四个 digest",
        ));
    }
    let payload = body.encode()?;
    let path = format!(
        "/v1/vaults/{}/rotations/{}/activate",
        crate::base64url::encode(&vault_id),
        crate::base64url::encode(&rotation_id)
    );
    let body_digest: [u8; 32] =
        Sha256::digest(management_request_cde(HttpMethod::Post, &path, &payload)?).into();
    let signature = manager.sign(&management_signing_input(
        instance_id,
        &vault_id,
        &request_id,
        expected_control_rev,
        &body_digest,
    )?);
    Ok(SignedActivation {
        body: payload,
        signature,
    })
}

/// 阶段 B 的唯一 canonical body（CDE；管理签名覆盖它的 request CDE）。
pub fn begin_body(
    expected_epoch: u64,
    expected_control_rev: u64,
    request_id: &[u8; 16],
    revoke: &[[u8; 16]],
) -> Result<Vec<u8>, SyncError> {
    let body = menghunote_sync_proto::RotationBeginBodyV1 {
        expected_epoch,
        expected_control_rev,
        request_id: *request_id,
        revoke: revoke.to_vec(),
    };
    Ok(body.encode()?)
}

/// 生成 begin wire（canonical body + 管理签名）。
#[allow(clippy::too_many_arguments)]
pub fn sign_begin(
    manager: &ManagementKey,
    instance_id: &[u8; 16],
    vault_id: VaultId,
    request_id: [u8; 16],
    expected_epoch: u64,
    expected_control_rev: u64,
    revoke: &[[u8; 16]],
) -> Result<RotationBegin, SyncError> {
    let body = begin_body(expected_epoch, expected_control_rev, &request_id, revoke)?;
    let path = format!(
        "/v1/vaults/{}/rotations",
        crate::base64url::encode(&vault_id)
    );
    let body_digest: [u8; 32] =
        Sha256::digest(management_request_cde(HttpMethod::Post, &path, &body)?).into();
    let signature = manager.sign(&management_signing_input(
        instance_id,
        &vault_id,
        &request_id,
        expected_control_rev,
        &body_digest,
    )?);
    Ok(RotationBegin {
        request_id,
        expected_epoch,
        expected_control_rev,
        revoke: revoke.to_vec(),
        signed_body: body,
        signature,
    })
}

/// 阶段 B：发起 begin；响应丢失时只按 request_id 查询。
///
/// 成功（含查询找回）后 journal 进入 `Begun` 并持久化 rotation_id/S/log_id。
pub fn begin_rotation<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    journal: &mut RotationJournal,
    vault_id: VaultId,
    manager: &ManagementKey,
    instance_id: &[u8; 16],
    revoke: &[[u8; 16]],
) -> Result<RotationView, SyncError> {
    if journal.phase != RotationPhase::Frozen {
        return Err(SyncError::Unsupported("rotation begin 需要 Frozen journal"));
    }
    if !journal.r_prime_backed_up {
        return Err(SyncError::Unsupported(
            "rotation begin 前必须确认新恢复码 R' 已备份",
        ));
    }
    let pinned_instance = journal.pinned_instance_id.ok_or(SyncError::Unsupported(
        "rotation begin 前必须完成阶段 A pin",
    ))?;
    if journal.pinned_log_id.is_none() || journal.pinned_control_hash.is_none() {
        return Err(SyncError::Unsupported(
            "rotation begin 前必须完成阶段 A pin",
        ));
    }
    if pinned_instance != *instance_id {
        return Err(SyncError::RelayWire(
            "rotation begin 的 instance 与 pin 不符",
        ));
    }
    let request = sign_begin(
        manager,
        instance_id,
        vault_id,
        journal.request_id,
        journal.from_epoch,
        journal.control_rev,
        revoke,
    )?;
    match client.begin_rotation(request) {
        Ok(view) => {
            mark_begun(vault, client, journal, view.clone())?;
            Ok(view)
        }
        Err(error) => {
            // 丢响应：只按 request_id 查询，绝不盲目重发 begin。
            match client.rotation_for_request(journal.request_id) {
                Ok(Some(view)) => {
                    mark_begun(vault, client, journal, view.clone())?;
                    Ok(view)
                }
                Ok(None) | Err(_) => Err(error),
            }
        }
    }
}

fn mark_begun<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    journal: &mut RotationJournal,
    view: RotationView,
) -> Result<(), SyncError> {
    if view.from_epoch != journal.from_epoch || view.to_epoch != journal.to_epoch {
        return Err(SyncError::RelayWire("rotation view epoch 与 journal 不符"));
    }
    let state = client.state()?;
    journal.mark_begun(view.rotation_id, view.freeze_seq, state.log_id)?;
    journal.save(vault)?;
    Ok(())
}

/// 阶段 C：用旧根 pull+integrate 到 S，固定 local cut L。
///
/// `received_seq` 不能代替 `integrated_seq`；只有本地真的 integrate 到 S 才允许继续。
pub fn integrate_to_s<C: RelayClient>(
    vault: &Vault,
    engine: &mut crate::engine::SyncEngine<'_, C>,
    journal: &mut RotationJournal,
    target_s: u64,
    local_cut: u64,
) -> Result<(), SyncError> {
    if journal.phase != RotationPhase::Begun {
        return Err(SyncError::Unsupported(
            "rotation integrate 需要 Begun journal",
        ));
    }
    loop {
        let cursor = menghunote_storage::sync_cursor_state(vault)?;
        if cursor.integrated_seq >= target_s {
            break;
        }
        let before = cursor.integrated_seq;
        let pulled = engine.pull()?;
        engine.integrate(usize::MAX)?;
        let after = menghunote_storage::sync_cursor_state(vault)?;
        if after.integrated_seq == before {
            return Err(SyncError::Unsupported(if pulled.events == 0 {
                "rotation integrate: relay 已无更多事件但未到 S"
            } else {
                "rotation integrate: 日志未推进到 S（存在因果洞/quarantine）"
            }));
        }
    }
    journal.mark_integrated(vault, target_s, local_cut)?;
    journal.save(vault)?;
    Ok(())
}

fn _journal_vault_hint(_vault_id: VaultId) {}
