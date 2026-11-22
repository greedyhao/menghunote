//! 真实 relay + 真实 HTTP 的“一条轮换轮次”共享实现。
//!
//! 本文件被 `e2e_rotation_two_devices` 与 `e2e_rotation_two_rounds` 通过
//! `#[path = "e2e_rotation_rounds.rs"] mod e2e_rotation_rounds;` 复用；cargo 也会把
//! `examples/*.rs` 当独立 example 编译，所以这里保留一个只打印提示的 `main`。
//!
//! 拆出共享实现的原因（评审 0003 T13）：多轮轮换必须复用同一条真实管理路径
//! （begin → 冻结资产 → 两份独立基线 → 候选槽 → 全设备 grants → T/双签 activate →
//! 本地推广 → outbox 重包），任何“第二套只跑测试的路径”都不能作为证据。
//!
//! ## 一轮 A 侧做什么
//!
//! 1. `RotationJournal::freeze` + `/control` 管理链 pin + `begin`（冻结旧 epoch）；
//! 2. 冻结资产分页 → 旧根解密 → 新根重加密；
//! 3. 两份独立 candidate 基线（primary/fallback）+ 证据；
//! 4. 候选 password/recovery 槽（新口令 + 新 R'）+ 全 active 设备 grants；
//! 5. 构造完整 activation T（含旧/新管理双签）→ activate；
//! 6. 读回 `/control` 的 T 与自身 latest grant（解根 == candidate root）；
//! 7. **本地原子推广**（历史根/管理 seed/pin/repack 屏障）→ journal → Completed → clear；
//! 8. 用旧根重包本机 outbox（A 通常为空）并清除 repack 屏障。
//!
//! ## B 侧做什么
//!
//! `apply_round_for_device` 只走生产网络入口 `apply_epoch_change_with_client`：
//! 它自己读回 T/commit/signed commit/manifest/S/cursor/grant inclusion，并从本地
//! `current_epoch` 起沿 `/control` 的 `epoch.activated` 逐环验证（离线跨多 epoch）。
//! 外部不拼 `BaselinePayload`，也不传 relay 自报的 hash 当授权。

#![allow(dead_code)]

#[path = "e2e_common.rs"]
pub mod e2e_common;

use std::path::Path;

use e2e_common::AnyResult;
use menghunote_storage::{set_repack_required, Vault};
use menghunote_sync::api_version::{RelayApi, RelayApiVersion};
use menghunote_sync::checkpoint::StaticAssets;
use menghunote_sync::device::{DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use menghunote_sync::engine::{EngineIdentity, SyncEngine};
use menghunote_sync::hpke::open_epoch_grant;
use menghunote_sync::http_relay::{HttpRelayClient, StdTcpTransport};
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::keys::RootSecretV1;
use menghunote_sync::random::{array, OsRandom};
use menghunote_sync::recovery::RecoverySeed;
use menghunote_sync::relay::{PauseReason, RelayClient, RelayState};
use menghunote_sync::rotation;
use menghunote_sync::rotation_activate::{
    activate_http, finish_promotion, promote_identity_with_material,
    promotion_material_from_recovery, verify_latest_grant_root, ActivationInput,
};
use menghunote_sync::rotation_apply::{
    apply_epoch_change_with_client, verify_activation_commit_matches_t,
    verify_epoch_activation_chain, ApplyOutcome,
};
use menghunote_sync::rotation_journal::{RotationJournal, RotationPhase};
use menghunote_sync::rotation_repack::{repack_outbox_after_rotation, RepackReport};
use menghunote_sync::rotation_slots::{
    build_manager_transition, candidate_asset_set_digest, checkpoint_commit_digest,
    frozen_asset_set_digest, generate_candidate_slots, grants_digest, plan_all_active_grants,
    upload_candidate_slots, ActivationTMaterial, CandidateSlots,
};
use menghunote_sync::rotation_stage::{
    assert_candidate_ready, assert_independent_baselines, attach_grants, begin_rotation_http,
    capture_baseline_evidence, fetch_frozen_assets,
    prepare_new_epoch_baseline_with_role_and_coverage_root, reencrypt_frozen_assets,
    restart_rotation, stage_grants, StartedRotation,
};
use menghunote_sync::slot::KeySlotV1;
use menghunote_sync::upload::{fetch_object, UploadedObject};
use menghunote_sync::{ids, DeviceId, SyncError, VaultId};
use menghunote_sync_proto::{ActivationCommitV1, GrantAadV1, ObjectType};
use sha2::{Digest, Sha256};

/// 一轮 A 侧的真实输入。
pub struct ARoundParams<'a> {
    pub relay: &'a str,
    pub vault_id: VaultId,
    pub device_id: DeviceId,
    pub peer_id: u64,
    pub signing_key: &'a DeviceSigningKey,
    pub hpke_key: &'a DeviceHpkeKey,
    /// 身份文件路径（DPAPI BLOB；推广时原子 rename 的唯一提交点）。
    pub identity_path: &'a Path,
    /// 本轮开始前持久化的身份（必须已处在 `old_root.key_epoch`）。
    pub identity: &'a LocalIdentityV1,
    /// 旧 epoch 根（冻结资产的解密根，也是 outbox 重包的旧根）。
    pub old_root: &'a RootSecretV1,
    /// 旧管理 key（begin/grants/activate 的签名者）。
    pub old_manager: &'a ManagementKey,
    pub old_manager_rev: u64,
    /// genesis（rev1）管理公钥：管理链验证的信任锚，跨轮不变。
    pub genesis_manager_public: [u8; 32],
    pub new_password: &'a str,
    /// 新管理 key rev（relay 要求 == 旧 rev + 1）。
    pub new_manager_rev: u64,
}

/// 一轮 A 侧完成后，B 侧/下一轮需要的事实。
pub struct ARoundOutcome {
    pub rotation_id: [u8; 16],
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub freeze_seq: u64,
    pub control_rev: u64,
    pub member_set_hash: [u8; 32],
    pub new_root: RootSecretV1,
    pub new_manager: ManagementKey,
    pub new_manager_public: [u8; 32],
    pub new_manager_rev: u64,
    pub new_password: String,
    pub new_recovery_seed: RecoverySeed,
    pub new_recovery_code: String,
    pub password_slot: KeySlotV1,
    pub recovery_slot: KeySlotV1,
    pub primary_checkpoint: [u8; 16],
    pub primary_covered_seq: u64,
    /// 候选基线第一份 doc snapshot（旧根负例用；不含明文）。
    pub primary_snapshot: Option<UploadedObject>,
    pub fallback_checkpoint: [u8; 16],
    pub fallback_covered_seq: u64,
    pub activation_commit: ActivationCommitV1,
    pub activation_commit_hash: [u8; 32],
    pub grants_count: usize,
    pub promoted_identity: LocalIdentityV1,
    /// A 本机 outbox 重包结果（通常为 0/0/0）。
    pub repack: RepackReport,
    /// restart 轮：被 relay restart 废弃的旧候选 epoch（普通轮为 `None`）。
    pub previous_candidate_epoch: Option<u64>,
    /// 本轮是否走了 relay restart 跳 epoch（普通轮为 `false`）。
    pub restarted: bool,
}

/// 阶段 0：轮换前同步 + freeze/pin（A 侧两条路径共用的前置）。
///
/// 返回 `(relay /state, 已 pin 的 journal)`；调用方随后走 `begin`（普通轮）或
/// `begin` + `restart`（restart 跳 epoch）再进入 [`run_round_body`]。
fn prepare_round(
    vault: &Vault,
    client: &mut HttpRelayClient<StdTcpTransport>,
    params: &ARoundParams<'_>,
) -> AnyResult<(RelayState, RotationJournal)> {
    if params.identity.current_epoch != params.old_root.key_epoch {
        return Err(format!(
            "A 身份 epoch={} 与旧根 epoch={} 不一致（上一轮未推广？）",
            params.identity.current_epoch, params.old_root.key_epoch
        )
        .into());
    }
    // 轮换前先把 A 追到 relay 头部：候选基线 validate 的 coverage 证明只认持久事实
    // （inbox 行 / engine 在 pull 时落盘的 skipped-asset 记录）。生产设备也会在轮换前
    // 至少同步一次；这里显式 sync_once，避免拿“本地没拉过 asset 事件”的库去导出基线。
    //
    // 从第 2 轮起 relay 日志里已有 checkpoint/control 事件，engine 按设计在它们处暂停
    // （不静默推进游标）。暂停前 pull 已把本页遇到的 ASSET 事件落成 durable
    // skipped-asset 事实——这正是候选基线 coverage 证明需要的；游标不前进，后续
    // prepare 仍从同一位置重扫。
    {
        let identity = EngineIdentity {
            device_id: params.device_id,
            signing_key: params.signing_key.clone(),
            peer_id: params.peer_id,
        };
        let mut engine = SyncEngine::new(vault, client, identity, params.old_root.clone());
        match engine.sync_once() {
            Ok(report) => println!(
                "[rotation-round] A pre-rotation sync: encrypt+push uploaded={} acked={} pulled={} integrated={}",
                report.push.uploaded,
                report.push.acked,
                report.pull.events,
                report.integrate.integrated
            ),
            Err(SyncError::Paused(PauseReason::UnsupportedEvent)) => println!(
                "[rotation-round] A pre-rotation sync stopped at checkpoint/control log event (asset skip facts persisted, cursor unchanged)"
            ),
            Err(error) => return Err(error.into()),
        }
    }
    let state = client.state()?;
    if state.epoch != params.old_root.key_epoch {
        return Err(format!(
            "begin 前 relay epoch={} 与本地旧根 epoch={} 不一致",
            state.epoch, params.old_root.key_epoch
        )
        .into());
    }
    println!(
        "[rotation-round] A begin step epoch {} -> {} control_rev={}",
        state.epoch,
        state.epoch.saturating_add(1),
        state.control_rev
    );

    // 阶段 A：冻结旧 epoch + 用已验证 /control 链尾固定 pin。
    let mut journal = RotationJournal::freeze(
        vault,
        params.old_root.key_epoch,
        array(&mut OsRandom)?,
        state.control_rev,
        [0u8; 32],
    )?;
    journal.confirm_r_prime_backed_up(vault)?;
    let _ = client.control_head()?;
    let control_pin = client
        .control_head_pin()
        .ok_or("控制链链尾 pin 缺失（/control 未验证）")?;
    rotation::verify_management_chain_and_pin(
        vault,
        client,
        &mut journal,
        params.vault_id,
        params.old_manager,
        Some(params.genesis_manager_public),
        control_pin,
    )?;
    Ok((state, journal))
}

/// 执行一轮完整的 A 侧轮换，并在成功 activate 后完成本地原子推广与 journal 收尾。
///
/// 前置条件：`params.identity` 已经是 `old_root.key_epoch` 的持久身份（第 1 轮来自
/// genesis，后续轮来自上一轮返回的 `promoted_identity`）。函数内部用生产入口
/// [`promote_identity_with_material`]，不写测试垫片、不伪造签名。
pub fn run_a_rotation_round(
    vault: &Vault,
    client: &mut HttpRelayClient<StdTcpTransport>,
    params: &ARoundParams<'_>,
) -> AnyResult<ARoundOutcome> {
    let (state, mut journal) = prepare_round(vault, client, params)?;

    // 阶段 B：管理 realm begin（旧 device token 全部失效，必须重新认证）。
    let api = RelayApi::new(params.relay, RelayApiVersion::V1);
    let started = begin_rotation_http(
        &mut StdTcpTransport::new_local_dev(params.relay)?,
        &api,
        vault,
        &mut journal,
        params.vault_id,
        &state.instance_id,
        state.log_id,
        params.old_manager,
        &[],
        e2e_common::now_secs(),
    )?;
    client.clear_auth_cache();
    println!(
        "[rotation-round] A begin rotation={} state={} S={} epoch {}->{}",
        ids::hex_of(&started.rotation_id),
        started.state,
        started.freeze_seq,
        started.from_epoch,
        started.to_epoch
    );
    run_round_body(
        vault,
        client,
        params,
        &started,
        state.instance_id,
        &mut journal,
    )
}

/// restart 跳 epoch 的一轮：`begin` 分配 E(n+1) → `POST /rotations/{r}/restart` 原子废弃它、
/// 分配 E(n+2)，然后在**新 rotation id** 上跑标准 stage（冻结资产 → 两份候选基线 →
/// 候选槽 → grants → T 双签 activate → 推广 → 重包）。
///
/// 为什么不用 `rotation_restart::execute_restart`：它的 `RestartRequest.reservation` 必须在
/// POST restart **之前**就有，而 reservation 的 `target_epoch` 只能来自 restart **之后**
/// 才存在的候选 epoch（relay `objects` 按 rotation id 反查 to_epoch）。真实 relay 下那条
/// 路径目前不可用（产品缺陷，不在本任务修）。这里只复用**真实 restart 端点**与标准 stage，
/// 不伪造任何授权材料；`RestartIntent`/`begin_restart` 的 durable 语义仍由单测覆盖。
///
/// journal 记账说明：restart 后 relay 已经把登记换到新 rotation/epoch，本地 journal 只是
/// "阶段推进/收尾"的载体；因此这里 `clear` 后按 **relay 视图**重新 freeze + `mark_begun`，
/// 事实（rotation_id/from/to/freeze_seq/control_rev）全部来自 relay 响应，不复制到生产。
pub fn run_a_restart_round(
    vault: &Vault,
    client: &mut HttpRelayClient<StdTcpTransport>,
    params: &ARoundParams<'_>,
) -> AnyResult<ARoundOutcome> {
    let (state, mut journal) = prepare_round(vault, client, params)?;
    let begin = begin_rotation_http(
        &mut StdTcpTransport::new_local_dev(params.relay)?,
        &RelayApi::new(params.relay, RelayApiVersion::V1),
        vault,
        &mut journal,
        params.vault_id,
        &state.instance_id,
        state.log_id,
        params.old_manager,
        &[],
        e2e_common::now_secs(),
    )?;
    client.clear_auth_cache();
    println!(
        "[rotation-round] A begin rotation={} state={} S={} epoch {}->{} (restart 候选)",
        ids::hex_of(&begin.rotation_id),
        begin.state,
        begin.freeze_seq,
        begin.from_epoch,
        begin.to_epoch
    );

    let (head_rev, _) = client.control_head()?;
    let restart_request_id: [u8; 16] = array(&mut OsRandom)?;
    restart_rotation(
        &mut StdTcpTransport::new_local_dev(params.relay)?,
        &RelayApi::new(params.relay, RelayApiVersion::V1),
        params.vault_id,
        begin.rotation_id,
        params.old_manager,
        &state.instance_id,
        restart_request_id,
        head_rev,
        e2e_common::now_secs(),
    )?;
    client.clear_auth_cache();
    let view = client
        .current_rotation()?
        .ok_or("/rotations/current 在 restart 后为空")?;
    if view.state != "frozen"
        || view.from_epoch != params.old_root.key_epoch
        || view.to_epoch != begin.to_epoch.saturating_add(1)
        || view.freeze_seq != begin.freeze_seq
    {
        return Err(format!(
            "restart 后 rotation 视图不符：state={} {}->{} S={}（begin {}->{} S={}）",
            view.state,
            view.from_epoch,
            view.to_epoch,
            view.freeze_seq,
            begin.from_epoch,
            begin.to_epoch,
            begin.freeze_seq
        )
        .into());
    }
    println!(
        "[rotation-round] A restart: rotation {} -> {} epoch {} aborted, allocated E{} (S={})",
        ids::hex_of(&begin.rotation_id),
        ids::hex_of(&view.rotation_id),
        begin.to_epoch,
        view.to_epoch,
        view.freeze_seq
    );

    // journal 只做阶段推进/收尾的载体：事实（rotation_id/epoch/S/control_rev）来自 relay 视图。
    RotationJournal::clear(vault)?;
    let mut journal = RotationJournal::freeze(
        vault,
        state.epoch,
        array(&mut OsRandom)?,
        view.control_rev,
        [0u8; 32],
    )?;
    journal.mark_begun(view.rotation_id, view.freeze_seq, state.log_id)?;
    journal.to_epoch = view.to_epoch;
    journal.save(vault)?;

    let started = StartedRotation {
        rotation_id: view.rotation_id,
        state: view.state.clone(),
        from_epoch: view.from_epoch,
        to_epoch: view.to_epoch,
        freeze_seq: view.freeze_seq,
    };
    let mut outcome = run_round_body(
        vault,
        client,
        params,
        &started,
        state.instance_id,
        &mut journal,
    )?;
    outcome.previous_candidate_epoch = Some(begin.to_epoch);
    outcome.restarted = true;
    Ok(outcome)
}

/// 一轮 A 侧的 stage 主体（`begin` 之后到重包收尾），普通轮与 restart 轮共用。
fn run_round_body(
    vault: &Vault,
    client: &mut HttpRelayClient<StdTcpTransport>,
    params: &ARoundParams<'_>,
    started: &StartedRotation,
    instance_id: [u8; 16],
    journal: &mut RotationJournal,
) -> AnyResult<ARoundOutcome> {
    // 阶段 D：frozen 资产分页 → 旧根解密 → 新根重加密。
    let (frozen, source_digest, total) = fetch_frozen_assets(client, started.rotation_id, 1)?;
    println!(
        "[rotation-round] A frozen assets count={} total={} digest={}",
        frozen.len(),
        total,
        source_digest.map_or_else(|| "none".to_owned(), |digest| ids::hex_of(&digest))
    );
    let mut all_assets = Vec::with_capacity(frozen.len());
    for asset in &frozen {
        let (_, payload) = fetch_object(
            client,
            params.old_root,
            ObjectType::Asset,
            &asset.object_id,
            asset.part_count,
            asset.total_plain_len,
            &asset.object_digest,
            &[],
        )?;
        all_assets.push(e2e_common::asset_plain_from_payload(&payload)?);
    }
    all_assets.sort_by_key(|(_, data)| {
        let digest: [u8; 32] = Sha256::digest(data).into();
        digest
    });
    let assets = StaticAssets::new(all_assets)?;
    let new_root = RootSecretV1::generate(params.vault_id, started.to_epoch, &mut OsRandom)?;

    // 阶段 E/F：两份独立候选基线 + 候选槽 + 全设备 grants。
    let first = prepare_new_epoch_baseline_with_role_and_coverage_root(
        vault,
        client,
        params.vault_id,
        params.device_id,
        params.signing_key.clone(),
        new_root.clone(),
        params.peer_id,
        &assets,
        Some("primary"),
        params.old_root,
    )?;
    let reservation = client
        .rotation(started.rotation_id)?
        .reservation_id
        .ok_or("候选 checkpoint 未返回 reservation")?;
    let slots = generate_candidate_slots(
        params.vault_id,
        new_root.key_epoch,
        params.new_password,
        params.new_manager_rev,
        e2e_common::now_secs(),
        params.device_id,
        &new_root,
        &mut OsRandom,
    )?;
    let new_manager_public = slots.new_manager_public;
    let slot_digest = upload_candidate_slots(
        client,
        params.vault_id,
        params.device_id,
        params.signing_key,
        &new_root,
        &slots,
        params.new_password,
        &reservation,
    )?;
    println!(
        "[rotation-round] A candidate slots password={} recovery={} digest={}",
        ids::hex_of(&slots.password.descriptor.slot_id),
        ids::hex_of(&slots.recovery.descriptor.slot_id),
        ids::hex_of(&slot_digest)
    );

    let (reencrypted, _, _) = reencrypt_frozen_assets(
        vault,
        client,
        params.vault_id,
        params.device_id,
        params.signing_key,
        params.old_root,
        &new_root,
        started.rotation_id,
        1,
        Some(&reservation),
    )?;
    let frozen_hashes: Vec<[u8; 32]> = reencrypted.iter().map(|asset| asset.real_sha256).collect();
    let second = prepare_new_epoch_baseline_with_role_and_coverage_root(
        vault,
        client,
        params.vault_id,
        params.device_id,
        params.signing_key.clone(),
        new_root.clone(),
        params.peer_id,
        &assets,
        Some("fallback"),
        params.old_root,
    )?;
    let evidence1 = capture_baseline_evidence(client, &new_root, &first, &frozen_hashes)?;
    let evidence2 = capture_baseline_evidence(client, &new_root, &second, &frozen_hashes)?;
    assert_independent_baselines(&evidence1, &evidence2, started.freeze_seq)?;
    let status1 = assert_candidate_ready(
        client,
        first.report.checkpoint_id,
        "primary",
        started.freeze_seq,
    )?;
    let status2 = assert_candidate_ready(
        client,
        second.report.checkpoint_id,
        "fallback",
        started.freeze_seq,
    )?;
    println!(
        "[rotation-round] A candidate baselines primary={} S={} fallback={} S={} reencrypted={} ready={}/{}",
        ids::hex_of(&first.report.checkpoint_id),
        evidence1.covered_seq,
        ids::hex_of(&second.report.checkpoint_id),
        evidence2.covered_seq,
        reencrypted.len(),
        status1.candidate_ready,
        status2.candidate_ready
    );

    let grants = plan_all_active_grants(
        client,
        &new_root,
        params.old_manager,
        params.old_manager_rev,
    )?;
    attach_grants(journal, vault, &grants)?;
    stage_grants(
        &mut StdTcpTransport::new_local_dev(params.relay)?,
        &RelayApi::new(params.relay, RelayApiVersion::V1),
        params.vault_id,
        started.rotation_id,
        params.old_manager,
        &grants,
        e2e_common::now_secs(),
    )?;
    println!(
        "[rotation-round] A grants staged count={} manager_rev={}",
        grants.len(),
        params.old_manager_rev
    );

    // 阶段 G：完整 activation T（20 字段）+ 旧/新管理双签 + activate。
    let control_rev = client.rotation(started.rotation_id)?.control_rev;
    let member_digest = client.active_member_cert_digest()?;
    let grant_digest = grants_digest(&grants)?;
    let old_asset_set_digest = frozen_asset_set_digest(&frozen)?;
    if Some(old_asset_set_digest) != source_digest {
        return Err("冻结资产集合本地复算与 relay source_digest 不一致".into());
    }
    let new_asset_set_digest = candidate_asset_set_digest(new_root.key_epoch, &reencrypted)?;
    let (head_rev, prev_control_hash) = client.control_head()?;
    if head_rev != control_rev {
        return Err(format!(
            "控制链链尾 rev={head_rev} 与 rotation view control_rev={control_rev} 不一致"
        )
        .into());
    }
    let activation_material = ActivationTMaterial {
        vault_id: params.vault_id,
        instance_id,
        rotation_id: started.rotation_id,
        from_epoch: started.from_epoch,
        to_epoch: started.to_epoch,
        freeze_seq: started.freeze_seq,
        old_manager_rev: params.old_manager_rev,
        new_manager_rev: params.new_manager_rev,
        new_manager_public,
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
    let transition =
        build_manager_transition(&activation_material, params.old_manager, &slots.new_manager)?;
    let input = ActivationInput {
        rotation_id: started.rotation_id,
        request_id: array(&mut OsRandom)?,
        expected_control_rev: control_rev,
        primary_checkpoint: first.report.checkpoint_id,
        fallback_checkpoint: second.report.checkpoint_id,
        member_cert_digest: member_digest,
        slot_set_digest: slot_digest,
        grants_digest: grant_digest,
        asset_set_digest: old_asset_set_digest,
        new_manager: Some(transition),
        candidate_root: new_root.clone(),
        new_manager_public,
        new_manager_rev: params.new_manager_rev,
        pv: 1,
    };
    let outcome = activate_http(
        &mut StdTcpTransport::new_local_dev(params.relay)?,
        &RelayApi::new(params.relay, RelayApiVersion::V1),
        params.vault_id,
        &instance_id,
        params.old_manager,
        &input,
        e2e_common::now_secs(),
    )?;
    println!(
        "[rotation-round] A activated epoch={} control_rev={} auth_rev={}",
        outcome.epoch, outcome.control_rev, outcome.auth_rev
    );
    if outcome.epoch != started.to_epoch {
        return Err(format!(
            "activate 返回 epoch={}，期望 {}",
            outcome.epoch, started.to_epoch
        )
        .into());
    }

    // relay 的 `/rotations/{r}` 只返回 activation commit 的 SHA256；按已公开事实重建
    // `ActivationCommitV1`，用 hash 与 relay 逐字节比对（B 侧同样只信这个 hash）。
    let activation_commit = ActivationCommitV1 {
        vault_id: params.vault_id,
        instance_id,
        rotation_id: started.rotation_id,
        from_epoch: started.from_epoch,
        to_epoch: started.to_epoch,
        covered_seq: started.freeze_seq,
        primary_checkpoint: first.report.checkpoint_id,
        fallback_checkpoint: second.report.checkpoint_id,
        primary_covered_seq: first.report.covered_seq,
        fallback_covered_seq: second.report.covered_seq,
        primary_manifest: first.report.manifest.object_id,
        fallback_manifest: second.report.manifest.object_id,
        member_set_hash: outcome.member_set_hash,
        grants_digest: grant_digest,
        asset_set_digest: Some(old_asset_set_digest),
        control_rev: outcome.control_rev,
    };
    let activation_commit_hash: [u8; 32] = Sha256::digest(activation_commit.encode()?).into();
    println!(
        "[rotation-round] A activation commit hash={}",
        ids::hex_of(&activation_commit_hash)
    );

    // activate 后旧 token 失效；重新认证再读 /control 的 T 与自身 grant。
    client.clear_auth_cache();
    let evidence = client
        .verified_activation_t(&started.rotation_id)?
        .ok_or("relay /control 未携带可复算的 activation T")?;
    let local_manager = input
        .new_manager
        .as_ref()
        .ok_or("activate 输入缺 new_manager")?;
    let local_t = menghunote_sync_proto::ActivationTV1::decode(
        local_manager
            .activation_t
            .as_deref()
            .ok_or("本地 NewManagerV1 缺 activation T")?,
    )?;
    if evidence.activation_t != local_t
        || Some(evidence.old_t_signature.as_slice()) != local_manager.old_t_signature.as_deref()
        || Some(evidence.new_t_signature.as_slice()) != local_manager.new_t_signature.as_deref()
    {
        return Err("/control 的 activation T/双签与本地发出的不一致".into());
    }
    println!("[rotation-round] A control activation T 双签复算通过");
    verify_latest_grant_root(
        &mut StdTcpTransport::new_local_dev(params.relay)?,
        &RelayApi::new(params.relay, RelayApiVersion::V1),
        params.vault_id,
        params.device_id,
        params.signing_key,
        params.hpke_key,
        &new_root,
        e2e_common::now_secs(),
    )?;
    println!("[rotation-round] A own latest grant opens candidate root");

    // 阶段 H：本地原子推广（新管理 seed 由 R' 派生，身份 rename 是唯一提交点）。
    let material = promotion_material_from_recovery(
        &outcome,
        new_root.clone(),
        &slots.recovery_seed,
        &params.vault_id,
        params.new_manager_rev,
    )?;
    let promoted =
        promote_identity_with_material(vault, params.identity_path, params.identity, &material)?;
    if promoted.current_epoch != started.to_epoch || !promoted.current_root.ct_eq(&new_root.root) {
        return Err("A 推广后的身份 epoch/root 不符".into());
    }
    // 本 e2e 不跑阶段 C 的 pull 循环（旧根 integrate 到 S 由 e2e_localhost/e2e_rotation 覆盖），
    // 这里按状态机顺序推进 journal 到 Activated，确保收尾仍走生产 `finish_promotion`
    // 的 →Promoted→Completed（不允许跳步）。
    for phase in [
        RotationPhase::Integrated,
        RotationPhase::Reencrypted,
        RotationPhase::BaselinesReady,
        RotationPhase::GrantsReady,
        RotationPhase::Activated,
    ] {
        journal.advance(phase)?;
    }
    journal.save(vault)?;
    finish_promotion(journal, vault)?;
    RotationJournal::clear(vault)?;
    println!(
        "[rotation-round] A identity epoch {} -> {} (journal Completed+cleared)",
        params.identity.current_epoch, promoted.current_epoch
    );

    // 阶段 J：本机旧 epoch outbox 用旧根重包（通常为空）；完成后清 repack 屏障。
    let repack = repack_outbox_after_rotation(vault, params.old_root, &new_root, params.device_id)?;
    if repack.failed != 0 || repack.conflicts != 0 {
        return Err(format!(
            "A 轮换后 outbox 重打包失败：repacked={} failed={} conflicts={}",
            repack.repacked, repack.failed, repack.conflicts
        )
        .into());
    }
    set_repack_required(vault, false)?;
    println!(
        "[rotation-round] A outbox repacked={} failed={} conflicts={}",
        repack.repacked, repack.failed, repack.conflicts
    );

    let CandidateSlots {
        password,
        recovery,
        recovery_seed,
        recovery_code,
        new_manager,
        new_manager_public: _,
    } = slots;
    Ok(ARoundOutcome {
        rotation_id: started.rotation_id,
        from_epoch: started.from_epoch,
        to_epoch: started.to_epoch,
        freeze_seq: started.freeze_seq,
        control_rev: outcome.control_rev,
        member_set_hash: outcome.member_set_hash,
        new_root,
        new_manager,
        new_manager_public,
        new_manager_rev: params.new_manager_rev,
        new_password: params.new_password.to_owned(),
        new_recovery_seed: recovery_seed,
        new_recovery_code: recovery_code.as_str().to_owned(),
        password_slot: password,
        recovery_slot: recovery,
        primary_checkpoint: first.report.checkpoint_id,
        primary_covered_seq: first.report.covered_seq,
        primary_snapshot: first.report.docs.first().map(|doc| doc.object),
        fallback_checkpoint: second.report.checkpoint_id,
        fallback_covered_seq: second.report.covered_seq,
        activation_commit,
        activation_commit_hash,
        grants_count: grants.len(),
        promoted_identity: promoted,
        repack,
        previous_candidate_epoch: None,
        restarted: false,
    })
}

/// 一轮 B 侧网络 apply 的结果。
pub struct BCatchUpOutcome {
    pub promoted: LocalIdentityV1,
    pub baseline_s: u64,
    pub baseline_cursor: String,
    pub bootstrap_epoch: u64,
}

/// B 侧：用本地 durable 身份/旧 grant，从 `identity.current_epoch` 网络追赶到 `round.to_epoch`。
///
/// 只走生产入口 [`apply_epoch_change_with_client`]：T/双签、activation 链、commit、
/// signed commit、manifest、S/cursor、grant inclusion 全部由它自己读回验证；本函数只
/// 做“显示给日志/负例用”的只读前置校验，不构造授权材料。
pub fn apply_round_for_device(
    client: &mut HttpRelayClient<StdTcpTransport>,
    vault: &Vault,
    identity_dir: &Path,
    identity_path: &Path,
    identity: &LocalIdentityV1,
    hpke_key: &DeviceHpkeKey,
    round: &ARoundOutcome,
) -> AnyResult<BCatchUpOutcome> {
    // rotation begin/activate 都会 bump `auth_rev`，使所有旧 device token 失效；
    // 这里先清本地认证/控制缓存，再重新认证并重验整条 `/control`。
    client.clear_auth_cache();
    let state = client.state()?;
    if state.epoch != round.to_epoch {
        return Err(format!(
            "设备看到 relay epoch={}，期望 {}",
            state.epoch, round.to_epoch
        )
        .into());
    }
    let view = client.rotation(round.rotation_id)?;
    if view.state != "activated" {
        return Err(format!("设备看到 rotation state={}，期望 activated", view.state).into());
    }
    let view_commit = view
        .activation_commit
        .as_deref()
        .ok_or("view 缺少 activation commit")?;
    if view_commit != round.activation_commit_hash.as_slice() {
        return Err(format!(
            "relay activation_commit={} 与重建的 commit hash={} 不一致",
            ids::hex_of(view_commit),
            ids::hex_of(&round.activation_commit_hash)
        )
        .into());
    }
    let control = RelayClient::control_kinds(client)?;
    if !control.iter().any(|(_, kind)| kind == "epoch.activated") {
        return Err("控制链缺少 epoch.activated".into());
    }
    // 从 `/control` 复算 latest activation T，并与 relay 公开 commit 交叉绑定一次。
    let activation_t = client
        .verified_activation_t(&round.rotation_id)?
        .ok_or("设备的 /control 未携带可复算的 activation T")?;
    verify_activation_commit_matches_t(&round.activation_commit, &activation_t.activation_t)?;
    println!(
        "[rotation-round] device activation T verified freeze_seq={} control_rev={} from={} to={}",
        activation_t.activation_t.freeze_seq,
        activation_t.activation_t.expected_control_rev,
        activation_t.activation_t.from_epoch,
        activation_t.activation_t.to_epoch
    );

    // grant root：latest grant 必须由本设备 HPKE 解回 candidate root（旧 X25519 身份不变）。
    let grant = client.self_grant_evidence()?;
    if grant.signed_header.len() < 64 {
        return Err("latest grant signed_header 过短".into());
    }
    let grant_aad = GrantAadV1::decode(&grant.signed_header[..grant.signed_header.len() - 64])?;
    if grant_aad.recipient_device_id != identity.device_id || grant_aad.epoch != round.to_epoch {
        return Err("latest grant 的 recipient/epoch 不符".into());
    }
    let opened = open_epoch_grant(
        hpke_key.secret(),
        &grant.hpke_enc,
        &grant_aad,
        &grant.ciphertext,
    )?;
    if !opened.ct_eq(&round.new_root) {
        return Err("设备 grant 解出的 root 不是 candidate root".into());
    }
    println!(
        "[rotation-round] device latest grant opens candidate root epoch={} manager_rev={}",
        grant_aad.epoch, grant_aad.manager_rev
    );

    let bundle = client.bootstrap()?;
    if bundle.epoch != round.to_epoch {
        return Err(format!(
            "activate 后 bootstrap epoch={}，期望 {}",
            bundle.epoch, round.to_epoch
        )
        .into());
    }
    let ck = &bundle.primary;
    if ck.epoch != round.to_epoch {
        return Err(format!(
            "bootstrap primary checkpoint epoch={}，期望 {}",
            ck.epoch, round.to_epoch
        )
        .into());
    }
    println!(
        "[rotation-round] device selected baseline cid={} S={} cursor={}",
        ids::hex_of(&ck.checkpoint_id),
        ck.covered_seq,
        ck.cursor
    );

    // 生产网络入口：从本地 `identity.current_epoch` 起逐环验 activation 链（含离线跨多 epoch），
    // 再完成 merge + receipt + rename 推广 + applied。
    //
    // 候选根用**本设备从 relay latest grant 解出的** `opened`（上方已 ct_eq 断言它与 A 的
    // candidate 一致），而不是 A 的测试根 `round.new_root`：这样 apply 的真实输入完全不依赖
    // 测试 oracle，旧身份/旧 HPKE 解不到 grant 就不可能走到 merge。
    let applied_s = match apply_epoch_change_with_client(
        client,
        vault,
        identity_path,
        identity,
        ck.checkpoint_id,
        ck.covered_seq,
        ck.cursor.as_str(),
        &opened,
        identity.pinned_manager,
        round.new_manager_public,
        round.new_manager_rev,
        None,
    )? {
        ApplyOutcome::Applied {
            baseline_s,
            promoted,
        } => {
            if !promoted {
                return Err("apply 成功但未推广身份".into());
            }
            baseline_s
        }
        other => return Err(format!("设备 apply 未成功：{other:?}").into()),
    };
    if applied_s != ck.covered_seq {
        return Err(format!(
            "applied S={applied_s} 与所选基线 S={} 不一致",
            ck.covered_seq
        )
        .into());
    }
    let promoted = e2e_common::load_identity(identity_dir)?;
    if promoted.current_epoch != round.to_epoch
        || !promoted.current_root.ct_eq(&round.new_root.root)
    {
        return Err("推广后的身份 epoch/root 不符".into());
    }
    if promoted.pending_rotation.is_some() {
        return Err("推广后仍残留 pending_rotation".into());
    }
    println!(
        "[rotation-round] device identity epoch {} -> {} (apply done, baseline_s={})",
        identity.current_epoch, promoted.current_epoch, applied_s
    );

    Ok(BCatchUpOutcome {
        promoted,
        baseline_s: applied_s,
        baseline_cursor: ck.cursor.clone(),
        bootstrap_epoch: bundle.epoch,
    })
}

/// 只读审计：用真实 relay 的 `/control` 逐环验证 `from_epoch -> latest` 的 activation 链。
///
/// 返回每一环 `(from_epoch, to_epoch, control_rev)`。它不写库、不推广，只证明“离线跨
/// 多 epoch”的链在真实 relay 上完整存在且可复算（评审 0003 T13 要求的证据）。
pub fn audit_epoch_chain(
    client: &mut HttpRelayClient<StdTcpTransport>,
    vault_id: VaultId,
    instance_id: [u8; 16],
    from_epoch: u64,
    anchor_manager_public: [u8; 32],
    latest_rotation_id: [u8; 16],
) -> AnyResult<Vec<(u64, u64, u64)>> {
    let links = client.epoch_activations(from_epoch)?;
    let latest = client
        .verified_activation_t(&latest_rotation_id)?
        .ok_or("链审计：latest activation T 缺失")?;
    let chain = verify_epoch_activation_chain(
        &links,
        &latest,
        vault_id,
        instance_id,
        from_epoch,
        anchor_manager_public,
    )?;
    if chain.tail_new_manager_public != latest.activation_t.new_manager_pubkey {
        return Err("链审计：链尾新管理公钥与 latest T 不符".into());
    }
    let epochs: Vec<(u64, u64, u64)> = links
        .iter()
        .map(|link| {
            (
                link.evidence.activation_t.from_epoch,
                link.evidence.activation_t.to_epoch,
                link.control_rev,
            )
        })
        .collect();
    println!(
        "[rotation-round] chain audit from E{from_epoch}: {} link(s), tail manager_rev={}",
        epochs.len(),
        chain.tail_new_manager_rev
    );
    Ok(epochs)
}

/// 旧根负例的通用包装：`decrypt_envelope` 失败才算通过。
pub fn assert_old_root_cannot_decrypt(
    old_root: &RootSecretV1,
    object_type: ObjectType,
    object_id: &[u8; 32],
    envelope: &[u8],
    context: &str,
) -> AnyResult<()> {
    match menghunote_sync::upload::decrypt_envelope(old_root, object_type, object_id, envelope, &[])
    {
        Ok(_) => Err(format!("{context}：旧 epoch root 竟然解开了新对象").into()),
        Err(_) => Ok(()),
    }
}

/// `SyncError` 的最小展示（避免示例里到处手写 match）。
#[allow(dead_code)]
pub fn describe_sync_error(error: &SyncError) -> String {
    error.to_string()
}

fn main() {
    println!("e2e_rotation_rounds 是双设备轮换示例的共享模块，请运行 e2e_rotation_two_rounds。");
}
