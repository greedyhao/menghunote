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
