//! 轮换客户端阶段 B/C（S7-28；ADR §28.2）。
//!
//! - 阶段 B：管理签 canonical `POST begin`；journal 先持久化 request_id，响应丢失时**只按
//!   request_id 查询**，查询不可用/未找到时不盲 begin。
//! - 阶段 C：旧根 pull+integrate 到 S（必须 `integrated_seq >= S`，received 不够），
//!   固定 local cut L；门禁只拦 **≤S 的入站缺口** 与 **已上传未 ACK** 的更新，
//!   冻结后继续产生的本地 `pending/encrypted` 编辑**不阻断**轮换（推广后用新 epoch
//!   重包/上传即可）。
//!
//! relay 的 request_id 查询端点由 relay task-101 提供；未提供时本模块返回原错误并保持
//! Frozen，调用方可安全重试查询。

use sha2::{Digest, Sha256};

use crate::api_version::{RelayApi, RelayApiVersion};
use crate::cbor;
use crate::device::{verify_signature, ManagementKey};
use crate::error::SyncError;
use crate::http_relay::ControlPin;
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
/// 安全契约（三条缺陷的修复口径）：
///
/// 1. **信任锚**：`first_old_public` 必须是调用方从**可信本地材料**（恢复码派生的 genesis
///    管理公钥、本地 identity/pin）拿到的 genesis 管理公钥。relay 返回的 rev1 记录只能
///    **校验相等**，绝不覆盖锚点；锚点缺失时 fail-closed（不允许退化成"信任 relay 给的
///    第一个公钥"）。
/// 2. **epoch 只要求前进**：`to_epoch > from_epoch` 即合法（restart 允许 1→3 跳 epoch），
///    rev 仍必须严格 `new_rev = old_rev + 1` 且链连续。
/// 3. **pin 的 control hash 只能来自已验证的 `/control` 链尾**：`control_pin` 必须由调用方用
///    `HttpRelayClient::control_head` / `HttpRelayClient::control_head_pin` 取得（含链尾
///    `record_hash`）。journal 自带的 `control_hash` **不参与** pin；control rev 的 pin 事实
///    是 `journal.control_rev`，必须与已验证链尾 rev 相等（低 pin / 控制链已前进一律拒绝）。
///    同一 rev 下链尾 `record_hash` 变化（换链尾/分叉）同样拒绝。
///
/// 逐条验证 9 字段绑定、old/new 双签、rev 连续；链尾公钥必须等于 `current_manager`。
#[allow(clippy::too_many_arguments)]
pub fn verify_management_chain_and_pin<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    journal: &mut RotationJournal,
    vault_id: VaultId,
    current_manager: &ManagementKey,
    first_old_public: Option<[u8; 32]>,
    control_pin: ControlPin,
) -> Result<(), SyncError> {
    // ① 信任锚：缺失即 fail-closed，绝不拿 relay 的 rev1 或"当前管理 key"兜底。
    let anchor = first_old_public.ok_or(SyncError::Unsupported(
        "阶段 A 缺少 genesis 管理 key 信任锚（first_old_public / 本地 identity pin）",
    ))?;
    let state = client.state()?;
    let chain = client.management_chain()?;

    // ③ pin 只接受"已验证 `/control` 链尾"这一事实来源。
    if control_pin.instance_id != state.instance_id {
        return Err(SyncError::RelayWire(
            "控制链 pin 的 relay instance 与 /state 不符",
        ));
    }
    let head_hash = control_pin.record_hash.ok_or(SyncError::RelayWire(
        "控制链 pin 缺链尾 record_hash（未经 /control 验证，不能用 journal 字段代替）",
    ))?;
    if control_pin.control_rev != journal.control_rev {
        return Err(SyncError::RelayWire(
            "已验证 /control 链尾 rev 与 rotation 冻结的 control_rev 不符（低 pin/控制链已前进）",
        ));
    }
    // 同一 rotation 内已有 pin 时，同 rev 只允许逐字节相等（换链尾/替换 genesis 后重写都拒绝）。
    if journal
        .pinned_control_hash
        .is_some_and(|pinned| pinned != head_hash)
    {
        return Err(SyncError::RelayWire(
            "控制链同 rev 的链尾 record_hash 与本地 pin 不符（换链尾）",
        ));
    }

    // rev1 是 genesis（无 transition/签名），rev≥2 才是管理转换；链必须按 rev 严格递增。
    let mut prev_rev = 1u64;
    let mut prev_pub = anchor;
    let mut prev_entry_rev = 0u64;
    let mut genesis_seen = false;
    let mut last_rev = 1u64;
    for entry in &chain {
        if entry.rev <= 1 {
            // relay 提供的 genesis 记录只允许出现一次、位于链首，且只能与信任锚相等。
            if entry.rev != 1 || genesis_seen || prev_entry_rev != 0 {
                return Err(SyncError::RelayWire(
                    "管理链 genesis 记录非法（重复/乱序/rev!=1）",
                ));
            }
            if entry.pubkey != anchor {
                return Err(SyncError::RelayWire(
                    "管理链 rev1 genesis 公钥与本地信任锚不符（relay 替换 genesis）",
                ));
            }
            genesis_seen = true;
            prev_entry_rev = 1;
            last_rev = 1;
            continue;
        }
        if entry.rev <= prev_entry_rev {
            return Err(SyncError::RelayWire("管理链 rev 不递增"));
        }
        let transition = ManagerTransitionV1::decode(&entry.transition)?;
        if transition.vault_id != vault_id
            || transition.instance_id != state.instance_id
            || transition.old_rev != prev_rev
            || transition.new_rev != entry.rev
            || transition.new_rev != transition.old_rev.saturating_add(1)
            // ② restart 允许跳 epoch：只要求前进，不要求 +1。
            || transition.to_epoch <= transition.from_epoch
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
        prev_entry_rev = entry.rev;
        last_rev = entry.rev;
    }
    // 链尾必须是当前管理 key；空链等价于"从未发生管理转换"，此时 genesis 必须就是当前 key。
    let head_pub = chain.last().map_or(anchor, |entry| entry.pubkey);
    if head_pub != current_manager.public_key() {
        return Err(SyncError::RelayWire("管理链尾公钥与当前管理 key 不符"));
    }

    // ③ 只 pin 已验证事实：instance/log 来自本次 `/state`，control hash 来自已验证链尾，
    // control rev 已在 freeze 时固定并与链尾 rev 核对（上面第 3 条）。
    journal.pinned_instance_id = Some(state.instance_id);
    journal.pinned_log_id = Some(state.log_id);
    journal.pinned_control_hash = Some(head_hash);
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
    api_version: RelayApiVersion,
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
    let path = RelayApi::new("", api_version).vault_path(
        &vault_id,
        &format!(
            "/rotations/{}/activate",
            crate::base64url::encode(&rotation_id)
        ),
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
    api_version: RelayApiVersion,
) -> Result<RotationBegin, SyncError> {
    let body = begin_body(expected_epoch, expected_control_rev, &request_id, revoke)?;
    let path = RelayApi::new("", api_version).vault_path(&vault_id, "/rotations");
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
#[allow(clippy::too_many_arguments)]
pub fn begin_rotation<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    journal: &mut RotationJournal,
    vault_id: VaultId,
    manager: &ManagementKey,
    instance_id: &[u8; 16],
    revoke: &[[u8; 16]],
    api_version: RelayApiVersion,
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
        api_version,
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

/// 阶段 C 的 ready 门禁（收敛到本模块，语义见下）。
///
/// 只允许下面两类情况阻断进入 ready：
///
/// 1. **≤S 的入站缺口**：`confirmed_highwater`/`integrated_seq` 未覆盖 S，或 inbox 里
///    `relay_seq <= S` 仍有非 `integrated` 行（quarantine/因果洞/AEAD 未验）；
/// 2. **已上传但未 ACK**：outbox `state='sent'` 的行。它们都在冻结前生成，对象依赖覆盖
///    ≤S 的本地状态，必须等 ACK 或按新 epoch 重包后才能继续。
///
/// 明确**不**检查 `pending_encryption` / `pending_send`：这些是冻结后继续产生的本地编辑，
/// 尚未上传、可以推广后重包上传，不能让轮换被持续编辑永久卡住（旧门禁的缺陷）。
pub fn ensure_c_phase_gate(vault: &Vault, target_s: u64) -> Result<(), SyncError> {
    let cursor = menghunote_storage::sync_cursor_state(vault)?;
    if cursor.confirmed_highwater.unwrap_or(0) < target_s {
        return Err(SyncError::Unsupported(
            "rotation integrate: confirmed highwater 未覆盖 S",
        ));
    }
    if cursor.integrated_seq < target_s {
        return Err(SyncError::Unsupported(
            "rotation integrate: integrated_seq 未到 S（received 不够）",
        ));
    }
    if inbox_gap_at_or_before(vault, &cursor, target_s)? {
        return Err(SyncError::Unsupported(
            "rotation integrate: inbox 在 ≤S 仍有 quarantine/因果洞",
        ));
    }
    if !menghunote_storage::outbox_rows(vault, menghunote_storage::SyncOutboxState::Sent, 1)?
        .is_empty()
    {
        return Err(SyncError::Unsupported(
            "rotation integrate: 存在已上传但未 ACK 的更新（≤S 未覆盖）",
        ));
    }
    Ok(())
}

/// inbox 里是否有 `relay_seq <= S` 且尚未 integrated 的行（quarantine/因果洞）。
///
/// 正常不变量下 `integrated_seq >= S` 已经蕴含"≤S 无洞"；这里仍显式查一遍，既为防御
/// meta 与 inbox 不一致（崩溃/手工修复），也让门禁失败原因可诊断。
pub(crate) fn inbox_gap_at_or_before(
    vault: &Vault,
    cursor: &menghunote_storage::SyncCursorState,
    target_s: u64,
) -> Result<bool, SyncError> {
    let Some(log_id) = cursor.log_id else {
        return Ok(false);
    };
    let target = i64::try_from(target_s)
        .map_err(|_| SyncError::LocalCorrupt("rotation integrate: S 超出 i64"))?;
    let rows = vault.query(
        "SELECT relay_seq FROM sync_inbox \
         WHERE log_id=?1 AND relay_seq<=?2 AND state<>'integrated' \
         ORDER BY relay_seq LIMIT 1",
        &[
            menghunote_storage::Value::Blob(log_id.to_vec()),
            menghunote_storage::Value::Integer(target),
        ],
    )?;
    Ok(!rows.is_empty())
}

/// 阶段 C：用旧根 pull+integrate 到 S，固定 local cut L。
///
/// `received_seq` 不能代替 `integrated_seq`；只有本地真的 integrate 到 S 才允许继续。
/// 通过 [`ensure_c_phase_gate`] 后按 journal 状态机推进 `Begun → Integrated`。
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
    // 门禁不再复用 `RotationJournal::mark_integrated`：那个实现把任何 pending/encrypted
    // 本地编辑都当成"≤S 未覆盖"而拒绝（文件不在本次作用域）。这里按同一状态机语义
    // （Begun → Integrated + local_cut）推进，门禁统一由 `ensure_c_phase_gate` 收口。
    ensure_c_phase_gate(vault, target_s)?;
    journal.advance(RotationPhase::Integrated)?;
    journal.local_cut = Some(local_cut);
    journal.save(vault)?;
    Ok(())
}

fn _journal_vault_hint(_vault_id: VaultId) {}
