//! P3-J restart 执行链：relay 契约、durable journal、固定密文、备份确认与冻结语义。

mod support;

use support::*;

use sha2::{Digest, Sha256};

use std::collections::HashMap;

use menghunote_storage::{
    ensure_structure_row, pending_encryption, sync_cursor_state, Value, Vault,
};
use menghunote_sync::api_version::{RelayApi, RelayApiVersion};
use menghunote_sync::base64url;
use menghunote_sync::cbor;
use menghunote_sync::checkpoint::{
    baseline_create_request_id, RestartBaselineJournal, RestartBaselineStage, StaticAssets,
};
use menghunote_sync::device::{verify_signature, ManagementKey};
use menghunote_sync::engine::{EngineIdentity, SyncEngine, RELAY_CURSOR_META};
use menghunote_sync::genesis;
use menghunote_sync::hpke::open_epoch_grant;
use menghunote_sync::http_relay::{HttpRequest, HttpResponse, HttpTransport};
use menghunote_sync::keys::{RootSecretV1, Secret32};
use menghunote_sync::relay::{
    BootstrapBundle, BootstrapLease, CheckpointLease, CheckpointPublish, CheckpointRef,
    CheckpointStatus, FrozenAssetPage, FrozenAssetRef, ObjectSubmission, PublishedCheckpoint,
    RelayClient, RelayLogPage, RelayState, SlotRecord, SlotUpload, UpdateOutcome, UpdateSubmission,
};
use menghunote_sync::rotation::ManagerTransitionV1;
use menghunote_sync::rotation_activate::{ActivatedOutcome, ActivationInput};
use menghunote_sync::rotation_journal::{RotationJournal, RotationPhase};
use menghunote_sync::rotation_restart::{
    execute_restart, plan_restart, BaselineRole, HttpRestartManagement, ManagerKeyStage,
    RestartActivationFacts, RestartContract, RestartManagement, RestartRelayRequest,
    RestartRelayResponse, RestartRequest, RestartRotationView, SkipReason, StoredActivationT,
};
use menghunote_sync::rotation_slots::{
    candidate_asset_set_digest, checkpoint_commit_digest, slot_set_digest,
};
use menghunote_sync::rotation_stage::{prepare_new_epoch_baseline, snapshot_object_ids};
use menghunote_sync::slot::context_hash;
use menghunote_sync::upload::fetch_object;
use menghunote_sync::{DeviceId, SyncError};
use menghunote_sync_proto::{
    activation_t_signing_input, auth_signing_input, part_count_for, ActivationTV1, AuthRealm,
    Challenge, ControlKind, EnvelopeHeader, EpochActivated, GrantAadV1, HttpMethod,
    MemberCertDigestInputV1, ObjectRefV1, ObjectType, RotationActivateBodyV1, SignedControlRecord,
    SlotDescriptorV1, HEADER_LEN, PROTOCOL_VERSION,
};

const RESERVATION: [u8; 16] = [0xCE; 16];
const OLD_ROTATION: [u8; 16] = [0x90; 16];
const RESTART_ROTATION: [u8; 16] = [0x91; 16];
const SECOND_ROTATION: [u8; 16] = [0x93; 16];
const REQUEST_ID: [u8; 16] = [0x92; 16];
const CONTROL_REV: u64 = 7;
const INSTANCE_ID: [u8; 16] = [0x22; 16];
const OLD_CANDIDATE_EPOCH: u64 = EPOCH + 1;
const NEW_CANDIDATE_EPOCH: u64 = EPOCH + 2;
const ORPHAN_ASSET: &[u8] = b"orphan/tombstone asset";
const LIVE_ASSET: &[u8] = b"live asset";

fn assets() -> StaticAssets {
    StaticAssets::new(vec![
        ("text/plain".to_owned(), LIVE_ASSET.to_vec()),
        ("application/octet-stream".to_owned(), ORPHAN_ASSET.to_vec()),
    ])
    .expect("静态资产源")
}

fn setup() -> (
    tempfile::TempDir,
    Vault,
    genesis::GenesisMaterials,
    FakeRelay,
) {
    let (dir, vault) = fresh_vault();
    ensure_structure_row(&vault).expect("structure");
    let materials = genesis::build_materials(
        VAULT_ID,
        [0x44; 16],
        "pw",
        1_700_000_000,
        &mut TestRandom::new(0x71_17),
    )
    .expect("genesis");
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    (dir, vault, materials, relay)
}

/// 第二个 active 设备（离线也能收到 grant）。
fn second_device(seed: u8) -> Device {
    Device::new(seed)
}

/// 让旧 epoch 产生冻结资产分页：先按旧根上传资产并发布一版基线。
fn freeze_assets(
    vault: &Vault,
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
    assets: &StaticAssets,
) -> u64 {
    let doc = seed_doc(vault, "note-1", materials.peer_id_u64());
    doc.insert_scalars(0, "restart body").expect("正文");
    // 本地提交带同步意图（不加密/不推送）：restart 全程不得动它。
    let head = commit_with_intent(
        vault,
        &doc,
        &uuid_like(0x71),
        [0x61; 32],
        materials.root.key_epoch,
    );
    assert!(head > 0);
    // 有一条 relay 事件，冻结点 S 才有意义（真实旧根密文 ASSET，validate 需实证）。
    let _ = publish_real_asset(
        relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        b"frozen relay asset",
        "text/plain",
    );
    // 先按真实客户端顺序 pull 消费旧 epoch 事件（落 durable skipped-asset 记录），
    // 让后续 epoch 的 checkpoint 能在不持有旧根时证明 S 之前的缺口。
    // （pull 放在旧基线发布之后，确保基线自身上传的 ASSET 事件也被消费。）
    let baseline = prepare_new_epoch_baseline(
        vault,
        relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
        assets,
    )
    .expect("旧 epoch 基线");
    relay.frozen_assets = baseline
        .report
        .assets
        .iter()
        .map(|asset| FrozenAssetRef {
            object_id: asset.object.object_id,
            generation: 0,
            key_epoch: materials.root.key_epoch,
            part_count: asset.object.part_count,
            total_plain_len: asset.object.total_plain_len,
            object_digest: asset.object.object_digest,
        })
        .collect();
    relay.frozen_asset_digest = Some(frozen_set_digest(&relay.frozen_assets));
    // 真实客户端会 pull 消费日志（ASSET 落 durable skipped-asset 记录）。
    let identity = EngineIdentity {
        device_id: materials.device_id,
        signing_key: materials.signing_key.clone(),
        peer_id: materials.peer_id_u64(),
    };
    let mut sync = SyncEngine::new(vault, relay, identity, materials.root.clone());
    sync.pull().expect("engine pull 冻结资产证据");
    // 复位前先确认游标确实来自真实 pull：restart 期间 cursor 不推进的断言基线是
    // c0，但 durable skipped-asset 证据必须由真实消费落盘，不能手写成 c1。
    let consumed_cursor = vault
        .get_meta(RELAY_CURSOR_META)
        .expect("读游标")
        .expect("pull 必须写游标");
    assert_eq!(
        consumed_cursor,
        format!("c{}", relay.head_seq).into_bytes(),
        "复位前的游标必须由真实 pull 推进到 relay head"
    );
    // **仅测试夹具**：生产 restart 路径必须保持 cursor 不推进；这里把 pull 的游标
    // 复位成 c0，保持各测试对 cursor 的断言基线。durable skipped-asset 记录与游标
    // 分开持久化，复位不影响 coverage 证据。
    vault
        .set_meta(RELAY_CURSOR_META, b"c0")
        .expect("复位 relay 游标（仅测试夹具）");
    relay.head_seq
}

/// relay 口径的冻结资产集合 digest（测试侧独立复算）。
fn frozen_set_digest(refs: &[FrozenAssetRef]) -> [u8; 32] {
    let object_refs = refs
        .iter()
        .map(|reference| ObjectRefV1 {
            object_id: reference.object_id,
            object_type: ObjectType::Asset,
            generation: reference.generation,
            key_epoch: reference.key_epoch,
            part_count: reference.part_count,
            total_plain_len: reference.total_plain_len,
            object_digest: reference.object_digest,
        })
        .collect::<Vec<_>>();
    let input = menghunote_sync_proto::refs_digest_input(&object_refs).expect("refs 输入");
    Sha256::digest(&input).into()
}

/// 控制链 active 成员证书（不含 relay 已吊销与调用方显式排除的设备）。
fn member_certs(relay: &FakeRelay, excluded: &[DeviceId]) -> Vec<MemberCertDigestInputV1> {
    let mut certs = relay
        .certs
        .iter()
        .filter(|(device_id, _)| {
            !relay.revoked.contains(*device_id) && !excluded.contains(*device_id)
        })
        .map(|(device_id, cert)| MemberCertDigestInputV1 {
            device_id: *device_id,
            certificate: cert.encode().expect("证书编码"),
            certificate_signature: None,
        })
        .collect::<Vec<_>>();
    certs.sort_by_key(|input| input.device_id);
    certs
}

/// 一次 stage 调用的记录：rotation id + 每个 leaf 的 (device_id, epoch, grant_id)。
type StagedCall = ([u8; 16], Vec<([u8; 16], u64, [u8; 16])>);

fn relay_view(
    rotation_id: [u8; 16],
    state: &str,
    from_epoch: u64,
    to_epoch: u64,
    freeze_seq: u64,
    control_rev: u64,
    manager_rev: u64,
) -> RestartRotationView {
    RestartRotationView {
        rotation_id,
        state: state.to_owned(),
        from_epoch,
        to_epoch,
        freeze_seq,
        control_rev,
        manager_rev,
    }
}

#[allow(clippy::too_many_arguments)]
fn restart_response(
    old_rotation: [u8; 16],
    new_rotation: [u8; 16],
    active_epoch: u64,
    previous_candidate_epoch: u64,
    new_to_epoch: u64,
    freeze_seq: u64,
    control_rev: u64,
    manager_rev: u64,
) -> RestartRelayResponse {
    RestartRelayResponse {
        previous: relay_view(
            old_rotation,
            "frozen",
            active_epoch,
            previous_candidate_epoch,
            freeze_seq,
            control_rev,
            manager_rev,
        ),
        restarted: relay_view(
            new_rotation,
            "frozen",
            active_epoch,
            new_to_epoch,
            freeze_seq,
            control_rev + 1,
            manager_rev,
        ),
    }
}

fn valid_response(freeze_seq: u64) -> RestartRelayResponse {
    restart_response(
        OLD_ROTATION,
        RESTART_ROTATION,
        EPOCH,
        OLD_CANDIDATE_EPOCH,
        NEW_CANDIDATE_EPOCH,
        freeze_seq,
        CONTROL_REV,
        1,
    )
}

fn outcome_for(input: &ActivationInput, epoch: u64) -> ActivatedOutcome {
    ActivatedOutcome {
        epoch,
        control_rev: input.expected_control_rev + 1,
        auth_rev: 2,
        member_set_hash: [0x5A; 32],
        primary_checkpoint: input.primary_checkpoint,
        fallback_checkpoint: input.fallback_checkpoint,
        activation_commit: vec![0xA1, 0x02],
    }
}

/// 记录管理 realm 网络步骤的假实现（restart + stage grants + activate）。
#[derive(Default)]
struct RecordingManagement {
    response: Option<RestartRelayResponse>,
    restart_calls: Vec<RestartRelayRequest>,
    staged: Vec<StagedCall>,
    activations: Vec<ActivationInput>,
    attempted: Option<ActivationInput>,
    fail_restart: Option<&'static str>,
    fail_stage: Option<&'static str>,
    fail_activate: Option<&'static str>,
    reported_epoch: Option<u64>,
    /// activate 超时后 `/rotations/current` 明确返回 activated（用本次输入重建回执）。
    timeout_reports_activated: bool,
    activated_lookup: Option<ActivatedOutcome>,
    /// 模拟 relay `/control` 里已持久化的 activation T（跨摸拟实例续跑时显式传入）。
    stored_activation: Option<StoredActivationT>,
    /// 模拟 relay 按 exact request_id 能查回的 restart 结果（POST 已提交、响应丢失）。
    restart_replay: Option<RestartRelayResponse>,
    /// 每次 `restart_outcome` 的 `(request_id, previous_rotation_id)` 记录。
    restart_outcome_calls: Vec<([u8; 16], [u8; 16])>,
    /// `run_restart` 从 journal manager_seed 重建并注入的新管理 key（公钥）。
    adopted_manager_public: Option<[u8; 32]>,
}

impl RecordingManagement {
    fn with_response(response: RestartRelayResponse) -> Self {
        Self {
            response: Some(response),
            ..Self::default()
        }
    }

    /// 本次 mock 收到过的 activate 输入里携带的 T 与双签（模拟 relay 持久化）。
    fn attempted_stored(&self) -> Option<StoredActivationT> {
        let manager = self.attempted.as_ref()?.new_manager.as_ref()?;
        Some(StoredActivationT {
            activation_t: manager.activation_t.clone()?,
            old_t_signature: manager.old_t_signature.clone()?,
            new_t_signature: manager.new_t_signature.clone()?,
        })
    }
}

impl RestartManagement for RecordingManagement {
    fn previous_rotation(
        &mut self,
        rotation_id: [u8; 16],
    ) -> Result<RestartRotationView, menghunote_sync::SyncError> {
        let response = self
            .response
            .as_ref()
            .ok_or(menghunote_sync::SyncError::Transport("测试缺 restart 响应"))?;
        if response.previous.rotation_id != rotation_id {
            return Err(menghunote_sync::SyncError::RelayWire(
                "测试旧 rotation 查询 id 不符",
            ));
        }
        Ok(response.previous.clone())
    }

    fn restart(
        &mut self,
        request: &RestartRelayRequest,
    ) -> Result<RestartRelayResponse, menghunote_sync::SyncError> {
        self.restart_calls.push(*request);
        if let Some(reason) = self.fail_restart {
            return Err(menghunote_sync::SyncError::Transport(reason));
        }
        self.response
            .clone()
            .ok_or(menghunote_sync::SyncError::Transport("测试缺 restart 响应"))
    }

    fn restart_outcome(
        &mut self,
        request_id: [u8; 16],
        previous_rotation_id: [u8; 16],
    ) -> Result<Option<RestartRelayResponse>, menghunote_sync::SyncError> {
        self.restart_outcome_calls
            .push((request_id, previous_rotation_id));
        Ok(self.restart_replay.clone())
    }

    fn stage_grants(
        &mut self,
        rotation_id: [u8; 16],
        leaves: &[menghunote_sync::rotation_stage::GrantLeaf],
    ) -> Result<(), menghunote_sync::SyncError> {
        if let Some(reason) = self.fail_stage {
            return Err(menghunote_sync::SyncError::Transport(reason));
        }
        self.staged.push((
            rotation_id,
            leaves
                .iter()
                .map(|leaf| (leaf.device_id, leaf.epoch, leaf.grant_id))
                .collect(),
        ));
        Ok(())
    }

    fn activate(
        &mut self,
        input: &ActivationInput,
    ) -> Result<ActivatedOutcome, menghunote_sync::SyncError> {
        self.attempted = Some(input.clone());
        if let Some(reason) = self.fail_activate {
            return Err(menghunote_sync::SyncError::Transport(reason));
        }
        self.activations.push(input.clone());
        let epoch = self
            .reported_epoch
            .unwrap_or(input.candidate_root.key_epoch);
        Ok(outcome_for(input, epoch))
    }

    fn activated_outcome(
        &mut self,
        _rotation_id: [u8; 16],
    ) -> Result<Option<ActivatedOutcome>, menghunote_sync::SyncError> {
        if let Some(found) = &self.activated_lookup {
            return Ok(Some(found.clone()));
        }
        if self.timeout_reports_activated {
            return Ok(self
                .attempted
                .as_ref()
                .map(|input| outcome_for(input, input.candidate_root.key_epoch)));
        }
        Ok(None)
    }

    fn activation_facts(
        &mut self,
        rotation_id: [u8; 16],
        _stage: ManagerKeyStage,
    ) -> Result<RestartActivationFacts, menghunote_sync::SyncError> {
        let restarted = self
            .response
            .as_ref()
            .map(|response| &response.restarted)
            .ok_or(menghunote_sync::SyncError::Transport("测试缺 restart 响应"))?;
        if restarted.rotation_id != rotation_id {
            return Err(menghunote_sync::SyncError::RelayWire(
                "测试 activation facts rotation 不符",
            ));
        }
        let stored = self
            .stored_activation
            .clone()
            .or_else(|| self.attempted_stored());
        let mut facts = RestartActivationFacts {
            control_rev: restarted.control_rev,
            prev_control_hash: [0xC7; 32],
            stored: stored.clone(),
        };
        if let Some(stored) = &stored {
            let activation_t = menghunote_sync_proto::ActivationTV1::decode(&stored.activation_t)
                .expect("mock activation T decode");
            facts.prev_control_hash = activation_t.prev_control_hash;
        }
        Ok(facts)
    }

    fn adopt_new_manager(
        &mut self,
        manager: &ManagementKey,
    ) -> Result<(), menghunote_sync::SyncError> {
        self.adopted_manager_public = Some(manager.public_key());
        Ok(())
    }
}

/// 模拟 relay "每 epoch role 唯一 CAS" 的包装器，可注入一次 fallback 失败。
///
/// `FakeRelay` 默认忽略 role；这里把 `(epoch, primary)` 视为一次性资源：
/// 重跑若再次创建 primary，就会像真实 relay 一样返回 `ID_CONFLICT`。这样测试能证明
/// primary 基线确实在 fallback 之前 durable，而不是靠"新 primary 恰好也能成功"蒙混。
struct RoleGuardRelay<'a> {
    inner: &'a mut FakeRelay,
    fail_fallback_once: bool,
    /// 非 `None` role 的创建记录 `(epoch, role)`。
    role_calls: Vec<(u64, String)>,
    /// 已成功创建过 primary 的 epoch。
    primary_created: Option<u64>,
}

impl<'a> RoleGuardRelay<'a> {
    fn new(
        inner: &'a mut FakeRelay,
        fail_fallback_once: bool,
        primary_created: Option<u64>,
    ) -> Self {
        Self {
            inner,
            fail_fallback_once,
            role_calls: Vec::new(),
            primary_created,
        }
    }
}

impl RelayClient for RoleGuardRelay<'_> {
    fn state(&mut self) -> Result<RelayState, SyncError> {
        self.inner.state()
    }

    fn log(&mut self, cursor: Option<&str>, limit: u32) -> Result<RelayLogPage, SyncError> {
        self.inner.log(cursor, limit)
    }

    fn part(
        &mut self,
        object_id: &menghunote_sync::ObjectId,
        part_no: u32,
    ) -> Result<Vec<u8>, SyncError> {
        self.inner.part(object_id, part_no)
    }

    fn put_part(
        &mut self,
        object_id: &menghunote_sync::ObjectId,
        part_no: u32,
        envelope: &[u8],
    ) -> Result<(), SyncError> {
        self.inner.put_part(object_id, part_no, envelope)
    }

    fn submit_updates(
        &mut self,
        items: Vec<UpdateSubmission>,
    ) -> Result<Vec<UpdateOutcome>, SyncError> {
        self.inner.submit_updates(items)
    }

    fn submit_object(&mut self, item: ObjectSubmission) -> Result<UpdateOutcome, SyncError> {
        self.inner.submit_object(item)
    }

    fn ack(&mut self, received_seq: u64, integrated_seq: u64, epoch: u64) -> Result<(), SyncError> {
        self.inner.ack(received_seq, integrated_seq, epoch)
    }

    fn device_cert(
        &mut self,
        device_id: &DeviceId,
    ) -> Result<Option<menghunote_sync_proto::DeviceCertV1>, SyncError> {
        self.inner.device_cert(device_id)
    }

    fn put_key_slot(&mut self, upload: SlotUpload) -> Result<SlotRecord, SyncError> {
        self.inner.put_key_slot(upload)
    }

    fn list_key_slots(&mut self) -> Result<Vec<SlotRecord>, SyncError> {
        self.inner.list_key_slots()
    }

    fn activate_password_slot(
        &mut self,
        expected_slot_set_rev: u64,
        slot_id: menghunote_sync::ObjectId,
        signed_message: Vec<u8>,
        signature: [u8; 64],
    ) -> Result<u64, SyncError> {
        self.inner
            .activate_password_slot(expected_slot_set_rev, slot_id, signed_message, signature)
    }

    fn create_checkpoint(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
    ) -> Result<CheckpointLease, SyncError> {
        self.inner.create_checkpoint(estimated_bytes, epoch)
    }

    fn create_checkpoint_with_role(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
        role: Option<&str>,
    ) -> Result<CheckpointLease, SyncError> {
        let epoch = epoch.unwrap_or(self.inner.epoch);
        if let Some(role) = role {
            self.role_calls.push((epoch, role.to_owned()));
            if role == "primary" && self.primary_created.is_some() {
                // 模拟 relay 每 epoch role 唯一 CAS。
                return Err(SyncError::RelayApi {
                    code: "ID_CONFLICT".to_owned(),
                    retryable: false,
                    current_epoch: None,
                    current_control_rev: None,
                    bootstrap_hint: None,
                });
            }
            if role == "fallback" && self.fail_fallback_once {
                self.fail_fallback_once = false;
                return Err(SyncError::Transport("模拟 fallback 基线失败"));
            }
        }
        let lease = self
            .inner
            .create_checkpoint_with_role(estimated_bytes, Some(epoch), role)?;
        if role == Some("primary") {
            self.primary_created = Some(lease.epoch);
        }
        Ok(lease)
    }

    fn add_checkpoint_refs(
        &mut self,
        checkpoint_id: [u8; 16],
        refs: Vec<CheckpointRef>,
    ) -> Result<u64, SyncError> {
        self.inner.add_checkpoint_refs(checkpoint_id, refs)
    }

    fn publish_checkpoint(
        &mut self,
        publish: CheckpointPublish,
    ) -> Result<PublishedCheckpoint, SyncError> {
        self.inner.publish_checkpoint(publish)
    }

    fn bootstrap(&mut self) -> Result<BootstrapBundle, SyncError> {
        self.inner.bootstrap()
    }

    fn renew_bootstrap_lease(&mut self, lease_id: [u8; 16]) -> Result<BootstrapLease, SyncError> {
        self.inner.renew_bootstrap_lease(lease_id)
    }

    fn active_member_hpke_keys(&mut self) -> Result<Vec<(DeviceId, [u8; 32])>, SyncError> {
        self.inner.active_member_hpke_keys()
    }

    fn rotation_assets(
        &mut self,
        rotation_id: [u8; 16],
        after: Option<[u8; 32]>,
        limit: u32,
    ) -> Result<FrozenAssetPage, SyncError> {
        self.inner.rotation_assets(rotation_id, after, limit)
    }

    fn checkpoint_status(
        &mut self,
        checkpoint_id: [u8; 16],
    ) -> Result<CheckpointStatus, SyncError> {
        self.inner.checkpoint_status(checkpoint_id)
    }
}

/// 模拟进程在候选基线各阶段被杀；relay 侧状态跨"重启"保留。
#[derive(Default)]
struct CrashRelayState {
    /// request_id → lease（relay create 的幂等结果）。
    leases: HashMap<[u8; 16], CheckpointLease>,
    /// `(epoch, role)` → request_id（每 epoch role 唯一 CAS）。
    role_requests: HashMap<(u64, String), [u8; 16]>,
    /// cid → role。
    checkpoint_roles: HashMap<[u8; 16], String>,
    /// cid → (signed_commit, published)（publish 幂等）。
    published: HashMap<[u8; 16], (Vec<u8>, PublishedCheckpoint)>,
    /// 每次 create 的 `(request_id, epoch, role)`。
    create_calls: Vec<([u8; 16], u64, String)>,
    /// 成功新建（非 dedup）次数。
    new_creates: u32,
    /// request_id 命中 relay 幂等结果的次数。
    dedup_hits: u32,
    /// 同一 `(epoch, role)` 用不同 request_id 再次 create 的次数。
    role_conflicts: u32,
    /// 实际 publish 次数。
    publish_calls: u32,
    /// 已 published 后按同一 commit 重放 publish 的次数。
    republish_hits: u32,
}

/// 模拟进程死亡的点（relay 侧写入已落/未落）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillPoint {
    /// relay create 已提交，本地 lease 未落盘。
    Create,
    /// 全部对象/refs 已上传，本地 refs 阶段未落盘。
    Upload,
    /// validate 已完成、本地 receipt 已落盘，publish 尚未发生。
    Validate,
    /// relay publish 已提交，本地 commit receipt 未落盘。
    Promote,
}

/// 在指定 role/阶段注入一次传输错误（模拟进程被杀），其余行为像真实 relay：
/// create 按 request_id 幂等、每 epoch role 唯一、publish 对同 commit 幂等。
struct CrashRelay<'a> {
    inner: &'a mut FakeRelay,
    state: &'a mut CrashRelayState,
    kill: Option<(String, KillPoint)>,
}

impl<'a> CrashRelay<'a> {
    fn new(
        inner: &'a mut FakeRelay,
        state: &'a mut CrashRelayState,
        kill: Option<(String, KillPoint)>,
    ) -> Self {
        Self { inner, state, kill }
    }

    fn should_kill(&self, role: &str, point: KillPoint) -> bool {
        matches!(&self.kill, Some((target, expected)) if target == role && *expected == point)
    }

    fn role_of(&self, checkpoint_id: &[u8; 16]) -> Option<String> {
        self.state.checkpoint_roles.get(checkpoint_id).cloned()
    }
}

impl RelayClient for CrashRelay<'_> {
    fn state(&mut self) -> Result<RelayState, SyncError> {
        self.inner.state()
    }

    fn log(&mut self, cursor: Option<&str>, limit: u32) -> Result<RelayLogPage, SyncError> {
        self.inner.log(cursor, limit)
    }

    fn part(
        &mut self,
        object_id: &menghunote_sync::ObjectId,
        part_no: u32,
    ) -> Result<Vec<u8>, SyncError> {
        self.inner.part(object_id, part_no)
    }

    fn put_part(
        &mut self,
        object_id: &menghunote_sync::ObjectId,
        part_no: u32,
        envelope: &[u8],
    ) -> Result<(), SyncError> {
        self.inner.put_part(object_id, part_no, envelope)
    }

    fn submit_updates(
        &mut self,
        items: Vec<UpdateSubmission>,
    ) -> Result<Vec<UpdateOutcome>, SyncError> {
        self.inner.submit_updates(items)
    }

    fn submit_object(&mut self, item: ObjectSubmission) -> Result<UpdateOutcome, SyncError> {
        self.inner.submit_object(item)
    }

    fn ack(&mut self, received_seq: u64, integrated_seq: u64, epoch: u64) -> Result<(), SyncError> {
        self.inner.ack(received_seq, integrated_seq, epoch)
    }

    fn device_cert(
        &mut self,
        device_id: &DeviceId,
    ) -> Result<Option<menghunote_sync_proto::DeviceCertV1>, SyncError> {
        self.inner.device_cert(device_id)
    }

    fn put_key_slot(&mut self, upload: SlotUpload) -> Result<SlotRecord, SyncError> {
        self.inner.put_key_slot(upload)
    }

    fn list_key_slots(&mut self) -> Result<Vec<SlotRecord>, SyncError> {
        self.inner.list_key_slots()
    }

    fn activate_password_slot(
        &mut self,
        expected_slot_set_rev: u64,
        slot_id: menghunote_sync::ObjectId,
        signed_message: Vec<u8>,
        signature: [u8; 64],
    ) -> Result<u64, SyncError> {
        self.inner
            .activate_password_slot(expected_slot_set_rev, slot_id, signed_message, signature)
    }

    fn create_checkpoint(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
    ) -> Result<CheckpointLease, SyncError> {
        self.inner.create_checkpoint(estimated_bytes, epoch)
    }

    fn create_checkpoint_with_role(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
        role: Option<&str>,
    ) -> Result<CheckpointLease, SyncError> {
        let Some(role) = role else {
            return self
                .inner
                .create_checkpoint_with_role(estimated_bytes, epoch, None);
        };
        let epoch = epoch.unwrap_or(self.inner.epoch);
        // 与 HTTP 层/writer durable intent 完全同一派生入口。
        let request_id = baseline_create_request_id(&self.inner.vault_id, role, epoch);
        self.state
            .create_calls
            .push((request_id, epoch, role.to_owned()));
        if let Some(lease) = self.state.leases.get(&request_id) {
            self.state.dedup_hits += 1;
            return Ok(lease.clone());
        }
        if let Some(previous) = self.state.role_requests.get(&(epoch, role.to_owned())) {
            if *previous != request_id {
                self.state.role_conflicts += 1;
                return Err(SyncError::RelayApi {
                    code: "ID_CONFLICT".to_owned(),
                    retryable: false,
                    current_epoch: None,
                    current_control_rev: None,
                    bootstrap_hint: None,
                });
            }
        }
        let lease =
            self.inner
                .create_checkpoint_with_role(estimated_bytes, Some(epoch), Some(role))?;
        self.state.new_creates += 1;
        self.state.leases.insert(request_id, lease.clone());
        self.state
            .role_requests
            .insert((epoch, role.to_owned()), request_id);
        self.state
            .checkpoint_roles
            .insert(lease.checkpoint_id, role.to_owned());
        if self.should_kill(role, KillPoint::Create) {
            return Err(SyncError::Transport("模拟 create 已落、进程被杀"));
        }
        Ok(lease)
    }

    fn add_checkpoint_refs(
        &mut self,
        checkpoint_id: [u8; 16],
        refs: Vec<CheckpointRef>,
    ) -> Result<u64, SyncError> {
        let role = self.role_of(&checkpoint_id);
        let inserted = self.inner.add_checkpoint_refs(checkpoint_id, refs)?;
        if let Some(role) = role {
            if self.should_kill(&role, KillPoint::Upload) {
                return Err(SyncError::Transport("模拟 upload/refs 已落、进程被杀"));
            }
        }
        Ok(inserted)
    }

    fn publish_checkpoint(
        &mut self,
        publish: CheckpointPublish,
    ) -> Result<PublishedCheckpoint, SyncError> {
        let checkpoint_id = publish.checkpoint_id;
        let role = self.role_of(&checkpoint_id);
        // 真实 relay：已 published 且同 commit 字节 → 幂等返回原结果。
        if let Some((signed_commit, published)) = self.state.published.get(&checkpoint_id) {
            if signed_commit.as_slice() == publish.signed_commit.as_slice() {
                self.state.republish_hits += 1;
                return Ok(published.clone());
            }
            return Err(SyncError::RelayApi {
                code: "CAS_CONFLICT".to_owned(),
                retryable: false,
                current_epoch: None,
                current_control_rev: None,
                bootstrap_hint: None,
            });
        }
        if let Some(role) = role.as_deref() {
            if self.should_kill(role, KillPoint::Validate) {
                return Err(SyncError::Transport("模拟 validate 后、publish 前进程被杀"));
            }
        }
        let signed_commit = publish.signed_commit.clone();
        let published = self.inner.publish_checkpoint(publish)?;
        self.state.publish_calls += 1;
        self.state
            .published
            .insert(checkpoint_id, (signed_commit, published.clone()));
        if let Some(role) = role.as_deref() {
            if self.should_kill(role, KillPoint::Promote) {
                return Err(SyncError::Transport(
                    "模拟 publish 已落、receipt 未落时进程被杀",
                ));
            }
        }
        Ok(published)
    }

    fn bootstrap(&mut self) -> Result<BootstrapBundle, SyncError> {
        self.inner.bootstrap()
    }

    fn renew_bootstrap_lease(&mut self, lease_id: [u8; 16]) -> Result<BootstrapLease, SyncError> {
        self.inner.renew_bootstrap_lease(lease_id)
    }

    fn active_member_hpke_keys(&mut self) -> Result<Vec<(DeviceId, [u8; 32])>, SyncError> {
        self.inner.active_member_hpke_keys()
    }

    fn rotation_assets(
        &mut self,
        rotation_id: [u8; 16],
        after: Option<[u8; 32]>,
        limit: u32,
    ) -> Result<FrozenAssetPage, SyncError> {
        self.inner.rotation_assets(rotation_id, after, limit)
    }

    fn checkpoint_status(
        &mut self,
        checkpoint_id: [u8; 16],
    ) -> Result<CheckpointStatus, SyncError> {
        self.inner.checkpoint_status(checkpoint_id)
    }
}

fn standard_slot_aad(
    device_id: DeviceId,
) -> impl Fn(&SlotDescriptorV1, &[u8; 24], usize) -> Result<Vec<u8>, menghunote_sync::SyncError> {
    move |descriptor: &SlotDescriptorV1, nonce: &[u8; 24], plaintext_len: usize| {
        let mut message_id = [0u8; 16];
        message_id.copy_from_slice(&descriptor.slot_id[..16]);
        let header = EnvelopeHeader {
            protocol_version: PROTOCOL_VERSION,
            header_len: HEADER_LEN as u16,
            object_type: ObjectType::KeySlot,
            cipher_suite: 1,
            flags: 0,
            vault_id: descriptor.vault_id,
            object_id: descriptor.slot_id,
            message_id,
            producer_device_id: device_id,
            generation: 0,
            key_epoch: descriptor.key_epoch,
            sender_seq: 0,
            part_index: 0,
            part_count: 1,
            total_plaintext_len: plaintext_len as u64,
            this_plaintext_len: u32::try_from(plaintext_len).expect("槽明文长度"),
            ciphertext_len: u32::try_from(plaintext_len + 16).expect("槽密文长度"),
            context_hash: context_hash(descriptor)?,
            nonce: *nonce,
        };
        Ok(header.aad(&descriptor.encode()?)?)
    }
}

fn fixed64(bytes: &[u8]) -> [u8; 64] {
    bytes.try_into().expect("64 字节签名")
}

fn asset_part_count(byte_len: u64, media_type: &str) -> u32 {
    let total = byte_len + 42 + media_type.len() as u64;
    // ASSET 明文上限 4MiB；测试资产只有几十字节。
    u32::try_from(part_count_for(total, ObjectType::Asset)).expect("分片数")
}

#[allow(clippy::too_many_arguments)]
fn request<'a>(
    materials: &'a genesis::GenesisMaterials,
    assets: &'a dyn menghunote_sync::checkpoint::AssetSource,
    certs: &'a [MemberCertDigestInputV1],
    revoked: &'a [DeviceId],
    old_root: RootSecretV1,
    old_manager: &'a ManagementKey,
    manager_rev: u64,
    previous_rotation_id: [u8; 16],
    control_rev: u64,
    freeze_seq: u64,
    recovery_backup_confirmed: bool,
) -> RestartRequest<'a> {
    RestartRequest {
        vault_id: VAULT_ID,
        device_id: materials.device_id,
        peer_id: materials.peer_id_u64(),
        signing_key: materials.signing_key.clone(),
        old_root,
        old_manager,
        manager_rev,
        previous_rotation_id,
        instance_id: INSTANCE_ID,
        control_rev,
        freeze_seq,
        request_id: REQUEST_ID,
        reservation: RESERVATION,
        member_certs: certs,
        revoked,
        assets,
        local_hpke_public: materials.hpke_key.public_key(),
        page_limit: 32,
        created_at: 1_700_000_500,
        pv: 1,
        recovery_backup_confirmed,
    }
}

#[test]
fn restart_plan_uses_relay_allocated_epoch_and_fresh_secrets() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let contract = RestartContract::from_relay_response(&valid_response(10)).expect("relay 契约");
    let mut rng = TestRandom::new(0x91_11);
    let plan = plan_restart(
        &vault,
        VAULT_ID,
        &contract,
        2,
        [0x81; 16],
        1_700_000_000,
        &mut rng,
    )
    .expect("plan restart");
    assert_eq!(plan.from_epoch, EPOCH);
    assert_eq!(plan.previous_candidate_epoch, OLD_CANDIDATE_EPOCH);
    assert_eq!(plan.to_epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(plan.candidate_root.key_epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(
        plan.slots.password.descriptor.key_epoch,
        NEW_CANDIDATE_EPOCH
    );
    assert_eq!(
        plan.slots.recovery.descriptor.key_epoch,
        NEW_CANDIDATE_EPOCH
    );
    assert!(!plan.password.is_empty());
    assert_ne!(
        plan.slots.recovery_seed.to_code().expect("code").as_str(),
        ""
    );
    assert_eq!(plan.new_manager_rev, 3);
    assert_eq!(
        vault
            .get_meta("sync.repack_required")
            .expect("meta")
            .as_deref(),
        Some(b"1".as_slice()),
        "restart 必须置 repack 屏障"
    );

    // 第二次 restart 必须是 relay 分配的更高 epoch（不是本地 +1）。
    let second_contract = RestartContract::from_relay_response(&restart_response(
        RESTART_ROTATION,
        SECOND_ROTATION,
        NEW_CANDIDATE_EPOCH,
        NEW_CANDIDATE_EPOCH + 1,
        NEW_CANDIDATE_EPOCH + 3,
        10,
        11,
        3,
    ))
    .expect("second contract");
    let second = plan_restart(
        &vault,
        VAULT_ID,
        &second_contract,
        plan.new_manager_rev,
        [0x81; 16],
        1_700_000_001,
        &mut TestRandom::new(0x92_22),
    )
    .expect("second restart");
    assert_eq!(second.from_epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(second.previous_candidate_epoch, NEW_CANDIDATE_EPOCH + 1);
    assert_eq!(second.to_epoch, NEW_CANDIDATE_EPOCH + 3);
    assert_ne!(
        second.candidate_root.root.expose(),
        plan.candidate_root.root.expose()
    );
    assert_ne!(second.password.as_str(), plan.password.as_str());
}

#[test]
fn restart_contract_rejects_local_plus_one_and_wrong_active() {
    ensure_test_identity_wrap();
    // 把 active 当 to-1：旧候选=2，但 relay 仍返回 new_to=2 → 必须拒绝。
    let wrong = restart_response(
        OLD_ROTATION,
        RESTART_ROTATION,
        EPOCH,
        OLD_CANDIDATE_EPOCH,
        OLD_CANDIDATE_EPOCH,
        10,
        CONTROL_REV,
        1,
    );
    let error = RestartContract::from_relay_response(&wrong).expect_err("禁止复用旧候选");
    assert!(matches!(error, menghunote_sync::SyncError::RelayWire(_)));

    // active 当旧候选（previous=active）→ 拒绝。
    let wrong = restart_response(
        OLD_ROTATION,
        RESTART_ROTATION,
        EPOCH,
        EPOCH,
        EPOCH + 1,
        10,
        CONTROL_REV,
        1,
    );
    assert!(RestartContract::from_relay_response(&wrong).is_err());

    // relay 返回的 from_epoch 与本地 active 假设不一致 → 拒绝。
    let contract = RestartContract::from_relay_response(&valid_response(10)).expect("contract");
    let wrong_local_root = RootSecretV1::new(VAULT_ID, EPOCH + 1, Secret32::from_array([0x99; 32]));
    let error = contract
        .validate_local(&wrong_local_root, CONTROL_REV)
        .expect_err("relay/local epoch 不一致必须拒绝");
    assert!(matches!(error, menghunote_sync::SyncError::RelayWire(_)));
}

#[test]
fn restart_execution_advances_epoch_and_retires_old_secrets() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let revoked = second_device(0x62);
    // 控制链仍把 D 当 active（吊销尚未反映到缓存）：restart 必须靠 revoked 列表排除它。
    relay.certs.insert(revoked.device_id, revoked.cert());
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[revoked.device_id]);

    let mut management = RecordingManagement::with_response(valid_response(freeze_seq));
    let execution = execute_restart(
        &vault,
        &mut relay,
        &mut management,
        &request(
            &materials,
            &assets,
            &certs,
            &[revoked.device_id],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect("restart 执行链");

    // 1) epoch 全部来自 relay：active=1、旧候选=2、新候选=3。
    assert_eq!(execution.plan.from_epoch, EPOCH);
    assert_eq!(execution.plan.previous_candidate_epoch, OLD_CANDIDATE_EPOCH);
    assert_eq!(execution.plan.to_epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(execution.activated.epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(management.activations.len(), 1);
    assert_eq!(
        management.activations[0].expected_control_rev,
        CONTROL_REV + 1
    );

    // 1b) §34：activate 必须携带完整 T 与旧/新管理 key 的 T 双签；T 的字段与本次
    //     候选材料逐项一致（relay `bind_activation_t` 会再做一次逐字段比对）。
    let activation_input = &management.activations[0];
    let manager_payload = activation_input
        .new_manager
        .as_ref()
        .expect("activate 必须带 new_manager");
    let activation_t = ActivationTV1::decode(
        manager_payload
            .activation_t
            .as_deref()
            .expect("NewManagerV1 必须携带 activation T"),
    )
    .expect("activation T decode");
    assert_eq!(activation_t.rotation_id, RESTART_ROTATION);
    assert_eq!(activation_t.from_epoch, EPOCH);
    assert_eq!(activation_t.to_epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(activation_t.freeze_seq, freeze_seq);
    assert_eq!(activation_t.old_manager_rev, 1);
    assert_eq!(
        activation_t.new_manager_rev,
        u32::try_from(execution.plan.new_manager_rev).expect("rev")
    );
    assert_eq!(
        activation_t.new_manager_pubkey,
        execution.plan.slots.new_manager_public
    );
    assert_eq!(
        activation_t.expected_control_rev,
        activation_input.expected_control_rev + 1
    );
    assert_eq!(activation_t.primary_cid, execution.primary.checkpoint_id);
    assert_eq!(activation_t.fallback_cid, execution.fallback.checkpoint_id);
    assert_eq!(
        activation_t.member_cert_digest,
        execution.member_cert_digest
    );
    assert_eq!(activation_t.slot_set_digest, execution.slot_set_digest);
    assert_eq!(activation_t.grants_digest, execution.grants_digest);
    assert_eq!(
        activation_t.old_asset_set_digest,
        execution.asset_set_digest
    );
    let t_input = activation_t_signing_input(&activation_t).expect("T signing input");
    for (label, public, signature) in [
        (
            "旧管理 T 签名",
            materials.manager.public_key(),
            manager_payload.old_t_signature.clone().expect("old T sig"),
        ),
        (
            "新管理 T 签名",
            execution.plan.slots.new_manager.public_key(),
            manager_payload.new_t_signature.clone().expect("new T sig"),
        ),
    ] {
        menghunote_sync::device::verify_signature(
            &public,
            &t_input,
            &signature.as_slice().try_into().expect("64"),
        )
        .unwrap_or_else(|error| panic!("{label}: {error}"));
    }
    // 本地 PreparedCheckpoint 复算的 commit 摘要必须写进 T（与 relay signed_commit 同口径）。
    assert_eq!(
        activation_t.new_asset_set_digest,
        candidate_asset_set_digest(NEW_CANDIDATE_EPOCH, &execution.reencrypted_assets)
            .expect("new asset digest")
    );

    // 2) 被吊销设备：无 grant、明确记录原因；其它 active 设备都有 grant。
    assert!(execution
        .grants
        .iter()
        .all(|leaf| leaf.device_id != revoked.device_id));
    assert!(execution.skipped.iter().any(|skipped| {
        skipped.device_id == revoked.device_id && skipped.reason == SkipReason::Revoked
    }));
    assert!(execution
        .grants
        .iter()
        .all(|leaf| leaf.device_id == materials.device_id));
    assert_eq!(execution.grants[0].epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(
        management.staged.first().expect("已 stage").0,
        RESTART_ROTATION
    );

    // 3) 双基线独立、S 覆盖冻结点、资产集合等于冻结集合。
    assert_eq!(execution.primary.role, BaselineRole::Primary);
    assert_eq!(execution.fallback.role, BaselineRole::Fallback);
    let first = execution
        .primary
        .evidence
        .as_ref()
        .expect("primary evidence");
    let second = execution
        .fallback
        .evidence
        .as_ref()
        .expect("fallback evidence");
    assert_ne!(first.checkpoint_id, second.checkpoint_id);
    assert_ne!(first.manifest_object_id, second.manifest_object_id);
    assert!(first.covered_seq >= freeze_seq);
    assert!(second.covered_seq >= freeze_seq);
    let primary_prepared = execution
        .primary
        .prepared
        .as_ref()
        .expect("primary prepared");
    let fallback_prepared = execution
        .fallback
        .prepared
        .as_ref()
        .expect("fallback prepared");
    assert_eq!(
        activation_t.primary_commit_digest,
        checkpoint_commit_digest(VAULT_ID, materials.device_id, &primary_prepared.report)
            .expect("primary commit digest")
    );
    assert_eq!(
        activation_t.fallback_commit_digest,
        checkpoint_commit_digest(VAULT_ID, materials.device_id, &fallback_prepared.report)
            .expect("fallback commit digest")
    );
    let ids = snapshot_object_ids(primary_prepared);
    assert!(ids
        .iter()
        .all(|id| !snapshot_object_ids(fallback_prepared).contains(id)));
    assert_eq!(execution.frozen_asset_count, 2, "墓碑/无引用资产也要复制");
    assert_eq!(first.asset_count, 2);

    // 4) 旧根解不开新文档/新资产；新根可以。
    for doc in &primary_prepared.report.docs {
        assert!(
            fetch_object(
                &mut relay,
                &materials.root,
                ObjectType::Snapshot,
                &doc.object.object_id,
                doc.object.part_count,
                doc.object.total_plain_len,
                &doc.object.object_digest,
                &[],
            )
            .is_err(),
            "旧根绝不能解新 epoch 的 snapshot"
        );
    }
    for asset in &execution.reencrypted_assets {
        let total = asset.byte_len + 42 + asset.media_type.len() as u64;
        let parts = asset_part_count(asset.byte_len, &asset.media_type);
        assert!(
            fetch_object(
                &mut relay,
                &materials.root,
                ObjectType::Asset,
                &asset.object_id,
                parts,
                total,
                &asset.object_digest,
                &[],
            )
            .is_err(),
            "旧根绝不能解新 epoch 的资产"
        );
        let (_, plaintext) = fetch_object(
            &mut relay,
            &execution.plan.candidate_root,
            ObjectType::Asset,
            &asset.object_id,
            parts,
            total,
            &asset.object_digest,
            &[],
        )
        .expect("新根可解新资产");
        assert!(plaintext.len() as u64 >= asset.byte_len);
    }

    // 5) 旧 password / 旧 R 只能得到旧根：new password/新 R'/新管理 key 全是新秘密。
    let old_slot_aad = standard_slot_aad(materials.device_id);
    let old_root = materials
        .password_slot
        .unwrap_password(b"pw", &old_slot_aad)
        .expect("旧口令解旧槽");
    assert_eq!(old_root.key_epoch, EPOCH);
    assert_ne!(
        old_root.root.expose(),
        execution.plan.candidate_root.root.expose(),
        "旧口令解出的只能是旧根"
    );
    assert!(
        materials
            .password_slot
            .unwrap_password(execution.plan.password.as_bytes(), &old_slot_aad)
            .is_err(),
        "新口令打不开旧槽"
    );
    assert!(
        materials
            .recovery_slot
            .unwrap_recovery(&execution.plan.slots.recovery_seed, &old_slot_aad)
            .is_err(),
        "新 R' 打不开旧恢复槽"
    );
    let old_manager =
        ManagementKey::from_recovery(&materials.recovery_seed, &VAULT_ID).expect("旧 R 派管理 key");
    assert_eq!(old_manager.public_key(), materials.manager_pub());
    assert_ne!(
        execution.plan.slots.new_manager_public,
        old_manager.public_key(),
        "旧 R 推不出新管理 key"
    );
    assert_ne!(
        execution.plan.slots.password.descriptor.slot_id,
        materials.password_slot.descriptor.slot_id
    );
    assert_eq!(
        execution.plan.slots.password.descriptor.key_epoch,
        NEW_CANDIDATE_EPOCH
    );
    assert_eq!(execution.plan.slots.password.descriptor.manager_rev, 2);
    assert_ne!(
        execution.plan.slots.password.wrapped_bytes(),
        materials.password_slot.wrapped_bytes(),
        "新槽槽体必须是新密文"
    );

    // 6) 双签管理转换绑定全部字段，缺一把钥匙验签失败。
    let new_manager = execution
        .activation
        .new_manager
        .as_ref()
        .expect("activate 必须带 new_manager（§27.2）");
    let transition = ManagerTransitionV1::decode(&new_manager.transition).expect("转换 CDE");
    assert_eq!(transition.vault_id, VAULT_ID);
    assert_eq!(transition.instance_id, INSTANCE_ID);
    assert_eq!(transition.rotation_id, RESTART_ROTATION);
    assert_eq!(transition.from_epoch, EPOCH);
    assert_eq!(transition.to_epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(transition.old_rev, 1);
    assert_eq!(transition.new_rev, 2);
    assert_eq!(
        transition.new_pubkey,
        execution.plan.slots.new_manager_public
    );
    assert_eq!(transition.covered_seq, freeze_seq);
    let old_signature = fixed64(new_manager.old_signature.as_deref().expect("旧签"));
    let new_signature = fixed64(new_manager.new_signature.as_deref().expect("新签"));
    verify_signature(
        &materials.manager.public_key(),
        &new_manager.transition,
        &old_signature,
    )
    .expect("旧管理 key 签名");
    verify_signature(
        &execution.plan.slots.new_manager_public,
        &new_manager.transition,
        &new_signature,
    )
    .expect("新管理 key 签名");
    assert!(
        verify_signature(&[0x11; 32], &new_manager.transition, &old_signature).is_err(),
        "错管理公钥必须验签失败"
    );

    // 7) 旧写仍冻结：屏障保持、游标未推进、pending 意图未动。
    assert_eq!(
        vault
            .get_meta("sync.repack_required")
            .expect("meta")
            .as_deref(),
        Some(b"1".as_slice())
    );
    assert_eq!(
        vault
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(b"c0".as_slice())
    );
    assert_eq!(pending_encryption(&vault, 10).expect("pending").len(), 1);
    assert_eq!(
        sync_cursor_state(&vault)
            .expect("cursor state")
            .integrated_seq,
        0
    );
    assert!(
        vault
            .get_meta("sync.rotation_apply_phase")
            .expect("phase")
            .is_none(),
        "restart 不应用基线、不推广身份"
    );

    // 8) durable journal：同一 root/password/固定槽密文，phase=RestartActivated。
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("restart journal");
    assert_eq!(journal.phase, RotationPhase::RestartActivated);
    assert_eq!(journal.previous_candidate_epoch, Some(OLD_CANDIDATE_EPOCH));
    assert_eq!(journal.to_epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(journal.rotation_id, Some(RESTART_ROTATION));
    assert_eq!(journal.activated_epoch, Some(NEW_CANDIDATE_EPOCH));
    assert!(journal.recovery_backup_confirmed);
    assert_eq!(
        journal
            .candidate_root
            .as_ref()
            .expect("journal root")
            .encode()
            .expect("root encode"),
        execution
            .plan
            .candidate_root
            .encode()
            .expect("execution root")
    );
    assert_eq!(
        journal.restart_password.as_deref(),
        Some(execution.plan.password.as_bytes())
    );
    assert_eq!(
        journal.new_slot_bodies,
        vec![
            execution.plan.slots.password.wrapped_bytes(),
            execution.plan.slots.recovery.wrapped_bytes()
        ],
        "固定槽密文必须原样记录"
    );
}

#[test]
fn restart_retry_reuses_journal_secrets_and_ciphertext() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    let response = valid_response(freeze_seq);

    let mut failing = RecordingManagement {
        fail_stage: Some("模拟 stage 失败"),
        ..RecordingManagement::with_response(response.clone())
    };
    let error = execute_restart(
        &vault,
        &mut relay,
        &mut failing,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect_err("stage 失败必须返回错误");
    assert!(matches!(error, menghunote_sync::SyncError::Transport(_)));
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartBaselinesReady);
    assert_eq!(journal.previous_candidate_epoch, Some(OLD_CANDIDATE_EPOCH));
    let stored_root = journal.candidate_root.clone().expect("stored root");
    let stored_password = journal.restart_password.clone().expect("stored password");
    let stored_bodies = journal.new_slot_bodies.clone();

    // 重试：必须从 journal 恢复同一 root/password/固定密文，不重新生成根。
    // 真实 relay 在丢响应重试时，旧 rotation view 的 control_rev 已是 restart 后的全局值。
    let mut response_after_restart = response;
    response_after_restart.previous.control_rev = response_after_restart.restarted.control_rev;
    let mut succeeding = RecordingManagement::with_response(response_after_restart);
    let execution = execute_restart(
        &vault,
        &mut relay,
        &mut succeeding,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect("journal 续跑");
    assert_eq!(
        execution.plan.candidate_root.encode().expect("root"),
        stored_root.encode().expect("stored root")
    );
    assert_eq!(
        execution.plan.password.as_bytes(),
        stored_password.as_slice()
    );
    assert_eq!(
        execution.plan.slots.password.wrapped_bytes(),
        stored_bodies[0]
    );
    assert_eq!(
        execution.plan.slots.recovery.wrapped_bytes(),
        stored_bodies[1]
    );
    assert_eq!(
        execution.slot_set_digest,
        slot_set_digest(&[
            execution.plan.slots.password.descriptor.clone(),
            execution.plan.slots.recovery.descriptor.clone()
        ])
        .expect("slot digest")
    );
    assert_eq!(succeeding.activations.len(), 1);
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartActivated);
}

#[test]
fn restart_refuses_activate_without_recovery_backup_confirmation() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    let response = valid_response(freeze_seq);

    let mut management = RecordingManagement::with_response(response.clone());
    let error = execute_restart(
        &vault,
        &mut relay,
        &mut management,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            false,
        ),
    )
    .expect_err("未确认备份必须拒绝 activate");
    assert!(matches!(
        error,
        menghunote_sync::SyncError::Unsupported("新恢复码未确认备份：拒绝 activate（保持冻结）")
    ));
    assert!(management.activations.is_empty());
    assert_eq!(management.staged.len(), 1, "备份确认前已 stage grant");
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartGrantsStaged);
    assert!(!journal.recovery_backup_confirmed);
    let stored_root = journal.candidate_root.clone().expect("root");
    assert_eq!(
        vault
            .get_meta("sync.repack_required")
            .expect("meta")
            .as_deref(),
        Some(b"1".as_slice())
    );

    // 确认备份后按 journal 续跑，同一 root 才允许 activate。
    let mut management = RecordingManagement::with_response(response);
    let execution = execute_restart(
        &vault,
        &mut relay,
        &mut management,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect("确认备份后续跑");
    assert_eq!(
        execution.plan.candidate_root.encode().expect("root"),
        stored_root.encode().expect("stored root")
    );
    assert_eq!(management.activations.len(), 1);
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartActivated);
    assert!(journal.recovery_backup_confirmed);
}

#[test]
fn restart_activate_failure_or_timeout_keeps_frozen_and_resumes() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    let response = valid_response(freeze_seq);

    // (a) activate 传输失败：journal 必须停在 RestartActivating，旧写仍冻结。
    let mut failing = RecordingManagement {
        fail_activate: Some("模拟 activate 超时"),
        ..RecordingManagement::with_response(response.clone())
    };
    let error = execute_restart(
        &vault,
        &mut relay,
        &mut failing,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect_err("activate 失败必须返回错误");
    assert!(matches!(error, menghunote_sync::SyncError::Transport(_)));
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartActivating);
    assert_eq!(journal.activated_epoch, None);
    assert_eq!(
        vault
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(b"c0".as_slice())
    );
    assert_eq!(pending_encryption(&vault, 10).expect("pending").len(), 1);
    assert_eq!(
        vault
            .get_meta("sync.repack_required")
            .expect("meta")
            .as_deref(),
        Some(b"1".as_slice())
    );

    // (b) 重启按 journal 续跑：同一 root/密文，重新 activate 成功。
    let mut succeeding = RecordingManagement::with_response(response.clone());
    let execution = execute_restart(
        &vault,
        &mut relay,
        &mut succeeding,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect("activate 失败后续跑");
    assert_eq!(execution.activated.epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(succeeding.activations.len(), 1);

    // 已激活的 journal：只查询收尾，绝不重发 relay restart/activate。
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartActivated);
    let found = ActivatedOutcome {
        epoch: journal.to_epoch,
        control_rev: journal.control_rev + 1,
        auth_rev: 2,
        member_set_hash: [0x5A; 32],
        primary_checkpoint: journal.baselines[0].checkpoint_id,
        fallback_checkpoint: journal.baselines[1].checkpoint_id,
        activation_commit: journal.activation_commit.clone().expect("激活回执"),
    };
    succeeding.activated_lookup = Some(found);
    let restart_calls = succeeding.restart_calls.len();
    let activation_calls = succeeding.activations.len();
    let resumed = execute_restart(
        &vault,
        &mut relay,
        &mut succeeding,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect("已激活 resume 只查询/收尾");
    assert_eq!(resumed.activated.epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(
        succeeding.restart_calls.len(),
        restart_calls,
        "已激活后不得再发 restart 请求"
    );
    assert_eq!(
        succeeding.activations.len(),
        activation_calls,
        "已激活后不得重复 activate"
    );
    assert_eq!(
        succeeding.adopted_manager_public, journal.new_manager_public,
        "run_restart 必须从 journal manager_seed 重建新管理 key 并注入"
    );

    // (c) 超时后 relay 明确 activated：只信回执，不再重复 activate。
    RotationJournal::clear(&vault).expect("clear journal");
    let mut timeout = RecordingManagement {
        fail_activate: Some("模拟 activate 丢响应"),
        timeout_reports_activated: true,
        ..RecordingManagement::with_response(response)
    };
    let execution = execute_restart(
        &vault,
        &mut relay,
        &mut timeout,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect("超时后查到 activated");
    assert_eq!(execution.activated.epoch, NEW_CANDIDATE_EPOCH);
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartActivated);
}

#[test]
fn restart_resumes_from_journal_after_activated_receipt_lost() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    let response = valid_response(freeze_seq);

    // 第一次：activate 丢响应，journal 停在 RestartActivating。
    let mut failing = RecordingManagement {
        fail_activate: Some("模拟 activate 丢响应"),
        ..RecordingManagement::with_response(response.clone())
    };
    execute_restart(
        &vault,
        &mut relay,
        &mut failing,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect_err("activate 丢响应");
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartActivating);

    // relay 侧实际已 activated；续跑只信回执，不重复 activate。
    let found = ActivatedOutcome {
        epoch: journal.to_epoch,
        control_rev: journal.control_rev + 1,
        auth_rev: 2,
        member_set_hash: [0x5A; 32],
        primary_checkpoint: journal.baselines[0].checkpoint_id,
        fallback_checkpoint: journal.baselines[1].checkpoint_id,
        activation_commit: vec![0xA1, 0x02],
    };
    let mut recovered = RecordingManagement {
        activated_lookup: Some(found),
        stored_activation: failing.attempted_stored(),
        ..RecordingManagement::with_response(response)
    };
    let execution = execute_restart(
        &vault,
        &mut relay,
        &mut recovered,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect("activated 回执续跑");
    assert_eq!(execution.activated.epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(
        execution.primary.checkpoint_id,
        journal.baselines[0].checkpoint_id
    );
    assert!(recovered.activations.is_empty(), "不得重复 activate");
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartActivated);
    assert_eq!(journal.activated_epoch, Some(NEW_CANDIDATE_EPOCH));
}

#[test]
fn restart_rejects_activate_epoch_mismatch_from_relay() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    let mut management = RecordingManagement {
        reported_epoch: Some(NEW_CANDIDATE_EPOCH + 1),
        ..RecordingManagement::with_response(valid_response(freeze_seq))
    };
    let error = execute_restart(
        &vault,
        &mut relay,
        &mut management,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect_err("activate 回执 epoch 不是候选 epoch 必须拒绝");
    assert!(matches!(error, menghunote_sync::SyncError::RelayWire(_)));
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartActivating);
    assert_eq!(
        vault
            .get_meta("sync.repack_required")
            .expect("meta")
            .as_deref(),
        Some(b"1".as_slice())
    );
}

#[test]
fn restart_refuses_revoked_self_or_stale_control_chain() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let revoked = second_device(0x64);
    relay.certs.insert(revoked.device_id, revoked.cert());
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    let response = valid_response(freeze_seq);

    // 本机被吊销：不能发起 restart，且不落 fresh 秘密。
    let mut management = RecordingManagement::with_response(response.clone());
    let error = execute_restart(
        &vault,
        &mut relay,
        &mut management,
        &request(
            &materials,
            &assets,
            &certs,
            &[materials.device_id],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect_err("本机吊销必须拒绝");
    assert!(matches!(
        error,
        menghunote_sync::SyncError::Unsupported("本机已被吊销，不能发起 restart")
    ));
    assert!(management.activations.is_empty());
    assert!(RotationJournal::load(&vault).expect("journal").is_none());

    // 控制链缓存仍含已吊销设备：拒绝（否则会把 grant 封给被吊销设备）。
    let mut management = RecordingManagement::with_response(response);
    let error = execute_restart(
        &vault,
        &mut relay,
        &mut management,
        &request(
            &materials,
            &assets,
            &certs,
            &[revoked.device_id],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect_err("active 证书集合含吊销设备必须拒绝");
    assert!(matches!(error, menghunote_sync::SyncError::Object(_)));
    assert!(management.activations.is_empty());
    assert!(RotationJournal::load(&vault).expect("journal").is_none());
    assert_eq!(
        vault
            .get_meta("sync.repack_required")
            .expect("meta")
            .as_deref(),
        Some(b"1".as_slice())
    );
}

#[test]
fn second_restart_never_reuses_candidate_material_or_grants() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let revoked = second_device(0x65);
    relay.certs.insert(revoked.device_id, revoked.cert());
    relay.revoked.insert(revoked.device_id);
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    let first = execute_restart(
        &vault,
        &mut relay,
        &mut RecordingManagement::with_response(valid_response(freeze_seq)),
        &request(
            &materials,
            &assets,
            &certs,
            &[revoked.device_id],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect("第一次 restart");
    // 本地推广/清理由阶段 H 接；这里模拟已推广，允许再次 restart。
    RotationJournal::clear(&vault).expect("clear journal");

    // 激活后：旧 epoch 的资产以新根驻留，下一次冻结分页看到的是它们。
    relay.frozen_assets = first
        .reencrypted_assets
        .iter()
        .map(|asset| FrozenAssetRef {
            object_id: asset.object_id,
            generation: 0,
            key_epoch: first.plan.to_epoch,
            part_count: asset_part_count(asset.byte_len, &asset.media_type),
            total_plain_len: asset.byte_len + 42 + asset.media_type.len() as u64,
            object_digest: asset.object_digest,
        })
        .collect();
    relay.frozen_asset_digest = Some(frozen_set_digest(&relay.frozen_assets));
    relay.epoch = first.plan.to_epoch + 2;

    // 第二次 restart：active=第一次新 epoch，旧候选=+1，relay 再分配 +2。
    let second_response = restart_response(
        RESTART_ROTATION,
        SECOND_ROTATION,
        first.plan.to_epoch,
        first.plan.to_epoch + 1,
        first.plan.to_epoch + 2,
        freeze_seq,
        first.activated.control_rev,
        first.plan.new_manager_rev,
    );
    let mut management = RecordingManagement::with_response(second_response);
    let second = execute_restart(
        &vault,
        &mut relay,
        &mut management,
        &request(
            &materials,
            &assets,
            &certs,
            &[revoked.device_id],
            first.plan.candidate_root.clone(),
            &first.plan.slots.new_manager,
            first.plan.new_manager_rev,
            RESTART_ROTATION,
            first.activated.control_rev,
            freeze_seq,
            true,
        ),
    )
    .expect("第二次 restart");

    assert_eq!(second.plan.from_epoch, first.plan.to_epoch);
    assert_eq!(
        second.plan.previous_candidate_epoch,
        first.plan.to_epoch + 1
    );
    assert_eq!(second.plan.to_epoch, first.plan.to_epoch + 2);
    // 候选材料全部 fresh：root/password/R'/槽 id/管理 key 都不复用。
    assert_ne!(
        second.plan.candidate_root.root.expose(),
        first.plan.candidate_root.root.expose()
    );
    assert_ne!(second.plan.password.as_str(), first.plan.password.as_str());
    assert_ne!(
        second.plan.slots.recovery_seed.expose(),
        first.plan.slots.recovery_seed.expose()
    );
    assert_ne!(
        second.plan.slots.password.descriptor.slot_id,
        first.plan.slots.password.descriptor.slot_id
    );
    assert_ne!(
        second.plan.slots.new_manager_public,
        first.plan.slots.new_manager_public
    );
    assert_ne!(second.slot_set_digest, first.slot_set_digest);
    assert_ne!(
        second.activation.grants_digest,
        first.activation.grants_digest
    );
    assert_ne!(
        second.activation.asset_set_digest,
        first.activation.asset_set_digest
    );
    assert_ne!(second.activation.rotation_id, first.activation.rotation_id);

    // 旧 grant 不能当新 epoch 的 grant 复用（AAD 绑定 epoch/recipient）。
    let old_leaf = &first.grants[0];
    let mut old_aad =
        GrantAadV1::decode(&old_leaf.signed_header[..old_leaf.signed_header.len() - 64])
            .expect("旧 grant AAD");
    assert_eq!(old_aad.epoch, first.plan.to_epoch);
    old_aad.epoch = second.plan.to_epoch;
    assert!(
        menghunote_sync::hpke::open_epoch_grant(
            materials.hpke_key.secret(),
            &old_leaf.hpke_enc,
            &old_aad,
            &old_leaf.ciphertext,
        )
        .is_err(),
        "旧 grant 换了 epoch 必须解不开"
    );
    // 新 grant 的 grant_id 也都是新的。
    assert_ne!(second.grants[0].grant_id, first.grants[0].grant_id);
    assert_eq!(management.activations.len(), 1);
    assert_eq!(
        management.activations[0].candidate_root.key_epoch,
        second.plan.to_epoch
    );
    assert_eq!(
        vault
            .get_meta("sync.repack_required")
            .expect("meta")
            .as_deref(),
        Some(b"1".as_slice())
    );
}

#[test]
fn grant_then_revoke_forces_higher_epoch_and_old_device_cannot_open_final_data() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();

    // D 在第一次 restart 时仍是 active 成员：必须拿到候选根 grant（离线也能收到）。
    let d = second_device(0x66);
    relay.certs.insert(d.device_id, d.cert());
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    let first = execute_restart(
        &vault,
        &mut relay,
        &mut RecordingManagement::with_response(valid_response(freeze_seq)),
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect("第一次 restart（D 仍 active）");

    // 证明 D 确实拿到并解开了候选根：旧私钥 + 保留的 grant 包可以还原 first root。
    let d_leaf = first
        .grants
        .iter()
        .find(|leaf| leaf.device_id == d.device_id)
        .expect("active 成员必须收到 grant");
    let aad = GrantAadV1::decode(&d_leaf.signed_header[..d_leaf.signed_header.len() - 64])
        .expect("grant AAD");
    let opened = open_epoch_grant(
        d.hpke_key.secret(),
        &d_leaf.hpke_enc,
        &aad,
        &d_leaf.ciphertext,
    )
    .expect("D 能打开自己的候选根 grant");
    assert_eq!(
        opened.encode().expect("opened root"),
        first.plan.candidate_root.encode().expect("first root"),
        "D 保留的候选包对应第一次候选根"
    );

    // grant 已发出之后才吊销 D：下一次 restart 必须分配**更高** epoch + 全新根，
    // 且不得再给 D 封包（F：已为设备封包后追加吊销 → 废弃候选根）。
    RotationJournal::clear(&vault).expect("clear journal");
    relay.revoked.insert(d.device_id);
    relay.frozen_assets = first
        .reencrypted_assets
        .iter()
        .map(|asset| FrozenAssetRef {
            object_id: asset.object_id,
            generation: 0,
            key_epoch: first.plan.to_epoch,
            part_count: asset_part_count(asset.byte_len, &asset.media_type),
            total_plain_len: asset.byte_len + 42 + asset.media_type.len() as u64,
            object_digest: asset.object_digest,
        })
        .collect();
    relay.frozen_asset_digest = Some(frozen_set_digest(&relay.frozen_assets));
    relay.epoch = first.plan.to_epoch + 2;
    let second_response = restart_response(
        RESTART_ROTATION,
        SECOND_ROTATION,
        first.plan.to_epoch,
        first.plan.to_epoch + 1,
        first.plan.to_epoch + 2,
        freeze_seq,
        first.activated.control_rev,
        first.plan.new_manager_rev,
    );
    let certs_without_d = member_certs(&relay, &[d.device_id]);
    let second = execute_restart(
        &vault,
        &mut relay,
        &mut RecordingManagement::with_response(second_response),
        &request(
            &materials,
            &assets,
            &certs_without_d,
            &[d.device_id],
            first.plan.candidate_root.clone(),
            &first.plan.slots.new_manager,
            first.plan.new_manager_rev,
            RESTART_ROTATION,
            first.activated.control_rev,
            freeze_seq,
            true,
        ),
    )
    .expect("吊销后第二次 restart");

    assert!(
        second.plan.to_epoch > first.plan.to_epoch,
        "restart 必须用 relay 分配的更高 epoch，而不是复用被泄露的候选"
    );
    assert_ne!(
        second.plan.candidate_root.root.expose(),
        first.plan.candidate_root.root.expose(),
        "候选根必须重新随机"
    );
    assert!(
        first
            .grants
            .iter()
            .any(|leaf| leaf.device_id == d.device_id),
        "第一次 grant 已为 D 封包（前置事实）"
    );
    assert!(
        second
            .grants
            .iter()
            .all(|leaf| leaf.device_id != d.device_id),
        "已吊销设备不得再收到新 epoch 的 grant"
    );

    // D 仍持有第一次候选包与私钥：它可以继续解第一次候选根（历史事实），
    // 但绝不能用它解第二次（最终）epoch 的数据。
    let reopened_first = open_epoch_grant(
        d.hpke_key.secret(),
        &d_leaf.hpke_enc,
        &aad,
        &d_leaf.ciphertext,
    )
    .expect("D 仍可重放自己的旧包");
    assert_eq!(
        reopened_first.encode().expect("old root"),
        first.plan.candidate_root.encode().expect("first root")
    );
    assert_ne!(
        reopened_first.encode().expect("old root"),
        second.plan.candidate_root.encode().expect("second root"),
        "旧包/旧私钥不得等价于最终候选根"
    );

    // 第二次 epoch 的资产密文只能用第二次根解；D 已知的第一次根必须失败。
    let asset = second
        .reencrypted_assets
        .first()
        .expect("第二次 epoch 必须有重加密资产");
    let part_count = asset_part_count(asset.byte_len, &asset.media_type);
    let total_plain = asset.byte_len + 42 + asset.media_type.len() as u64;
    assert!(
        fetch_object(
            &mut relay,
            &first.plan.candidate_root,
            ObjectType::Asset,
            &asset.object_id,
            part_count,
            total_plain,
            &asset.object_digest,
            &[],
        )
        .is_err(),
        "D 保留的旧候选根不得解开最终 epoch 的数据密文"
    );
    fetch_object(
        &mut relay,
        &second.plan.candidate_root,
        ObjectType::Asset,
        &asset.object_id,
        part_count,
        total_plain,
        &asset.object_digest,
        &[],
    )
    .expect("最终候选根可解自己的资产密文");
    assert_eq!(
        vault
            .get_meta("sync.repack_required")
            .expect("meta")
            .as_deref(),
        Some(b"1".as_slice()),
        "restart 必须保持 repack 屏障"
    );
}

// ---------------------------------------------------------------- 管理 realm HTTP fake

/// 测试用 relay 基址（token 签名绑定 origin，必须与实际请求 base 一致）。
fn fake_api() -> RelayApi {
    RelayApi::new(FAKE_BASE, RelayApiVersion::V1)
}

const FAKE_BASE: &str = "http://relay.test";
/// fake 时钟（`management_token` 的 `now`）。
const HTTP_NOW: u64 = 1_700_000_900;
/// fake challenge nonce / id（`management_token` 每次都会重新问，无需唯一）。
const FAKE_NONCE: [u8; 32] = [0x33; 32];
const FAKE_CHALLENGE: [u8; 16] = [0x34; 16];
/// `/checkpoints/{cid}` 的 `signed_commit` 原始字节（fake 对所有 cid 返回同一份）。
const FAKE_SIGNED_COMMIT: &[u8] = b"fake-checkpoint-signed-commit";

fn subject_of(public_key: &[u8; 32]) -> [u8; 16] {
    Sha256::digest(public_key)[..16]
        .try_into()
        .expect("管理 subject")
}

fn cbor_http_response(fields: cbor::Fields) -> HttpResponse {
    HttpResponse {
        status: 200,
        body: cbor::encode(fields).expect("测试响应编码"),
    }
}

fn management_denied() -> HttpResponse {
    HttpResponse {
        status: 403,
        body: cbor::encode(vec![
            (0, cbor::text("MANAGEMENT_REQUIRED")),
            (1, cbor::boolean(false)),
        ])
        .expect("错误体编码"),
    }
}

/// `(request_id, expected_control_rev, unsigned_body, signature)`。
type SignedManagementParts = ([u8; 16], u64, Vec<u8>, [u8; 64]);

/// 内存版管理 realm relay：`manager_rev` 在 activate 后 +1，旧 key 的 token 从
/// challenge 起就被 403 拒绝（与 `crates/relay/src/auth.rs` 一致）。
///
/// 它只实现 restart 执行链用到的端点，并**真的验证**管理签名：因此测试通过即证明
/// restart/stage/activate 由旧 key 签名，而 activate 之后的查询只可能由新 key 完成。
struct FakeManagerRelay {
    base: String,
    vault_id: [u8; 16],
    instance_id: [u8; 16],
    now: u64,
    manager_rev: u64,
    old_public: [u8; 32],
    new_public: Option<[u8; 32]>,
    previous_rotation_id: [u8; 16],
    current_rotation_id: [u8; 16],
    state: &'static str,
    from_epoch: u64,
    to_epoch: u64,
    freeze_seq: u64,
    control_rev: u64,
    auth_rev: u64,
    primary_checkpoint: [u8; 16],
    fallback_checkpoint: [u8; 16],
    activation_commit: Vec<u8>,
    member_set_hash: [u8; 32],
    /// `(prev_hash, record_hash, signed_record)`，rev = 下标 + 1。
    control_chain: Vec<([u8; 32], [u8; 32], Vec<u8>)>,
    /// activate 已生效但回包丢失（模拟 relay 成功、客户端读超时）。
    fail_activate_response: bool,
    /// activate 生效后 `/rotations/current` 一直失败（模拟进程在恢复前退出）。
    fail_current_after_activation: bool,
    /// `/rotations/current?request_id=` 返回的幂等回放体（restart POST 的原始响应字节）。
    restart_replay: Option<Vec<u8>>,
    /// 幂等回放的 scope（真实 relay 跨 scope 查，必须由客户端拒绝非 restart）。
    replay_scope: String,
    restart_calls: usize,
    activate_calls: usize,
    staged_calls: usize,
    requests: Vec<HttpRequest>,
    /// 通过鉴权的 token subject（旧/新管理 key 的选择证据）。
    accepted_token_subjects: Vec<[u8; 16]>,
}

impl FakeManagerRelay {
    fn new(old_public: [u8; 32], freeze_seq: u64) -> Self {
        // 与 restart 契约一致的 8 条控制链（raw 记录即可：activation_facts 只用
        // rev/prev/hash 连续性，typed 记录只在激活后被追加）。
        let mut control_chain = Vec::new();
        let mut prev = [0u8; 32];
        for rev in 1..=CONTROL_REV + 1 {
            let signed_record = format!("fake-raw-control-{rev}").into_bytes();
            let mut hasher = Sha256::new();
            hasher.update(prev);
            hasher.update(&signed_record);
            let hash: [u8; 32] = hasher.finalize().into();
            control_chain.push((prev, hash, signed_record));
            prev = hash;
        }
        Self {
            base: FAKE_BASE.to_owned(),
            vault_id: VAULT_ID,
            instance_id: INSTANCE_ID,
            now: HTTP_NOW,
            manager_rev: 1,
            old_public,
            new_public: None,
            previous_rotation_id: OLD_ROTATION,
            current_rotation_id: RESTART_ROTATION,
            state: "frozen",
            from_epoch: EPOCH,
            to_epoch: NEW_CANDIDATE_EPOCH,
            freeze_seq,
            control_rev: CONTROL_REV + 1,
            auth_rev: 1,
            primary_checkpoint: [0x51; 16],
            fallback_checkpoint: [0x52; 16],
            activation_commit: vec![0xA1; 32],
            member_set_hash: [0x5A; 32],
            control_chain,
            fail_activate_response: false,
            fail_current_after_activation: false,
            restart_replay: None,
            replay_scope: "management/restart".to_owned(),
            restart_calls: 0,
            activate_calls: 0,
            staged_calls: 0,
            requests: Vec::new(),
            accepted_token_subjects: Vec::new(),
        }
    }

    fn current_manager_public(&self) -> [u8; 32] {
        if self.manager_rev >= 2 {
            self.new_public.unwrap_or(self.old_public)
        } else {
            self.old_public
        }
    }

    fn current_subject(&self) -> [u8; 16] {
        subject_of(&self.current_manager_public())
    }

    /// 校验 Bearer token 属于**当前** manager_rev 对应的 key（旧 key 激活后必然失败）。
    fn check_token(&mut self, request: &HttpRequest) -> Result<[u8; 16], SyncError> {
        let header = request
            .headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.as_str())
            .ok_or(SyncError::RelayWire("fake: 缺 authorization"))?;
        let encoded = header
            .strip_prefix("Bearer tok-")
            .ok_or(SyncError::RelayWire("fake: token 前缀非法"))?;
        let subject: [u8; 16] = base64url::decode_array(encoded)?;
        if subject != self.current_subject() {
            return Err(SyncError::RelayApi {
                code: "MANAGEMENT_REQUIRED".to_owned(),
                retryable: false,
                current_epoch: None,
                current_control_rev: None,
                bootstrap_hint: None,
            });
        }
        self.accepted_token_subjects.push(subject);
        Ok(subject)
    }

    fn verify_signed_management(
        &self,
        request: &HttpRequest,
        manager_public: &[u8; 32],
    ) -> Result<SignedManagementParts, SyncError> {
        let path = request
            .path
            .strip_prefix(self.base.as_str())
            .unwrap_or(&request.path);
        let fields = cbor::decode(&request.body)?;
        let signed = cbor::value_map(cbor::field(&fields, 0)?)?;
        let unsigned_body = cbor::bytes_field(&signed, 0)?;
        let signature: [u8; 64] = cbor::bytes_field(&signed, 1)?
            .try_into()
            .map_err(|_| SyncError::RelayWire("fake: 管理签名长度非法"))?;
        let request_id: [u8; 16] = cbor::bytes_fixed(&fields, 1)?;
        let expected_control_rev = cbor::uint_field(&fields, 2)?;
        let body_digest: [u8; 32] = Sha256::digest(menghunote_sync_proto::management_request_cde(
            HttpMethod::Post,
            path,
            &unsigned_body,
        )?)
        .into();
        verify_signature(
            manager_public,
            &menghunote_sync_proto::management_signing_input(
                &self.instance_id,
                &self.vault_id,
                &request_id,
                expected_control_rev,
                &body_digest,
            )?,
            &signature,
        )?;
        Ok((request_id, expected_control_rev, unsigned_body, signature))
    }

    fn view_map(&self, previous: bool) -> cbor::CdeValue {
        let (rotation_id, state, to_epoch, control_rev, manager_rev) = if previous {
            (
                self.previous_rotation_id,
                "frozen",
                OLD_CANDIDATE_EPOCH,
                CONTROL_REV,
                1u64,
            )
        } else {
            (
                self.current_rotation_id,
                self.state,
                self.to_epoch,
                self.control_rev,
                self.manager_rev,
            )
        };
        let mut fields = vec![
            (0, cbor::bytes(rotation_id.to_vec())),
            (1, cbor::text(state)),
            (2, cbor::uint(self.from_epoch)),
            (3, cbor::uint(to_epoch)),
            (4, cbor::uint(self.freeze_seq)),
            (5, cbor::uint(0)),
            (7, cbor::uint(1)),
            (9, cbor::uint(0)),
            (10, cbor::uint(control_rev)),
            (11, cbor::uint(self.auth_rev)),
            (12, cbor::uint(manager_rev)),
            (13, cbor::uint(0)),
            (15, cbor::uint(0)),
        ];
        if state == "activated" {
            fields.push((6, cbor::bytes(self.member_set_hash.to_vec())));
            fields.push((18, cbor::bytes(self.primary_checkpoint.to_vec())));
            fields.push((19, cbor::bytes(self.fallback_checkpoint.to_vec())));
            fields.push((20, cbor::bytes(self.activation_commit.clone())));
        }
        cbor::CdeValue::Map(fields)
    }

    fn handle_challenge(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
        let fields = cbor::decode(&request.body)?;
        let realm = cbor::uint_field(&fields, 0)?;
        let subject: [u8; 16] = cbor::bytes_fixed(&fields, 1)?;
        if realm != u64::from(AuthRealm::Management as u8) || subject != self.current_subject() {
            return Ok(management_denied());
        }
        Ok(cbor_http_response(vec![
            (0, cbor::bytes(FAKE_CHALLENGE.to_vec())),
            (1, cbor::uint(u64::from(AuthRealm::Management as u8))),
            (2, cbor::bytes(subject.to_vec())),
            (3, cbor::bytes(FAKE_NONCE.to_vec())),
            (4, cbor::uint(self.now + 600)),
            (5, cbor::bytes(self.vault_id.to_vec())),
            (6, cbor::bytes(self.instance_id.to_vec())),
        ]))
    }

    fn handle_token(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
        let fields = cbor::decode(&request.body)?;
        let challenge_id: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
        let message = cbor::bytes_field(&fields, 1)?;
        let signature = cbor::bytes_field(&fields, 2)?;
        let challenge = Challenge::decode(&message)?;
        let origin: [u8; 32] = Sha256::digest(self.base.as_bytes()).into();
        if challenge.challenge_id != challenge_id
            || challenge.subject != self.current_subject()
            || challenge.realm != AuthRealm::Management
            || challenge.vault_id != self.vault_id
            || challenge.relay_instance_id != self.instance_id
            || challenge.origin_sha256 != origin
            || challenge.expires_at <= self.now
        {
            return Ok(management_denied());
        }
        let signature: [u8; 64] = signature
            .try_into()
            .map_err(|_| SyncError::RelayWire("fake: token 签名长度非法"))?;
        verify_signature(
            &self.current_manager_public(),
            &auth_signing_input(&challenge)?,
            &signature,
        )?;
        Ok(cbor_http_response(vec![
            (
                0,
                cbor::text(format!("tok-{}", base64url::encode(&challenge.subject))),
            ),
            (1, cbor::uint(self.now + 600)),
            (2, cbor::uint(self.now)),
            (3, cbor::uint(u64::from(AuthRealm::Management as u8))),
            (4, cbor::uint(1)),
        ]))
    }

    fn handle_rotation_view(
        &mut self,
        request: &HttpRequest,
        path: &str,
    ) -> Result<HttpResponse, SyncError> {
        self.check_token(request)?;
        let previous = path.ends_with(&base64url::encode(&self.previous_rotation_id));
        Ok(cbor_http_response(vec![(0, self.view_map(previous))]))
    }

    fn handle_restart(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
        self.check_token(request)?;
        let (_, expected_control_rev, _, _) =
            self.verify_signed_management(request, &self.old_public)?;
        if expected_control_rev != CONTROL_REV {
            return Ok(HttpResponse {
                status: 409,
                body: cbor::encode(vec![
                    (0, cbor::text("CAS_CONFLICT")),
                    (1, cbor::boolean(false)),
                ])
                .expect("冲突体"),
            });
        }
        self.restart_calls += 1;
        self.control_rev = CONTROL_REV + 1;
        Ok(cbor_http_response(vec![
            (0, self.view_map(false)),
            (1, cbor::boolean(true)),
        ]))
    }

    fn handle_activate(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
        self.check_token(request)?;
        // activate 必须由旧管理 key 签名（activate 之前 relay 的 manager_rev 仍是旧值）。
        let (request_id, expected_control_rev, unsigned_body, signature) =
            self.verify_signed_management(request, &self.old_public)?;
        let activation = RotationActivateBodyV1::decode(&unsigned_body)?;
        let new_manager = activation
            .new_manager
            .clone()
            .ok_or(SyncError::RelayWire("fake: activate 缺 new_manager"))?;
        let activation_t_bytes = new_manager
            .activation_t
            .clone()
            .ok_or(SyncError::RelayWire("fake: activate 缺 activation T"))?;
        let activation_t = ActivationTV1::decode(&activation_t_bytes)?;
        if expected_control_rev != self.control_rev
            || activation_t.rotation_id != self.current_rotation_id
            || activation_t.from_epoch != self.from_epoch
        {
            return Ok(HttpResponse {
                status: 409,
                body: cbor::encode(vec![
                    (0, cbor::text("CAS_CONFLICT")),
                    (1, cbor::boolean(false)),
                ])
                .expect("冲突体"),
            });
        }
        // apply：manager_rev 推进到新 key，旧 key 从此在 challenge 就被 403。
        self.new_public = Some(activation_t.new_manager_pubkey);
        self.manager_rev = u64::from(activation_t.new_manager_rev);
        self.state = "activated";
        self.to_epoch = activation_t.to_epoch;
        self.freeze_seq = activation_t.freeze_seq;
        self.primary_checkpoint = activation
            .primary_checkpoint
            .ok_or(SyncError::RelayWire("fake: activate 缺 primary cid"))?;
        self.fallback_checkpoint = activation
            .fallback_checkpoint
            .ok_or(SyncError::RelayWire("fake: activate 缺 fallback cid"))?;
        self.control_rev = expected_control_rev + 1;
        self.auth_rev = 2;
        self.activation_commit = vec![0xA1; 32];
        // 把 activation T 与双签写进控制链：resume 必须能从 `/control` 复算同一份 T。
        let event = EpochActivated {
            vault_id: activation_t.vault_id,
            instance_id: activation_t.instance_id,
            rotation_id: activation_t.rotation_id,
            from_epoch: activation_t.from_epoch,
            to_epoch: activation_t.to_epoch,
            covered_seq: activation_t.freeze_seq,
            primary_checkpoint: activation_t.primary_cid,
            fallback_checkpoint: activation_t.fallback_cid,
            activation_commit: [0xA1; 32],
            member_set_hash: self.member_set_hash,
            grants_digest: activation_t.grants_digest,
            control_rev: activation_t.expected_control_rev,
            auth_rev: 2,
            activation_t: Some(activation_t_bytes),
            old_t_signature: new_manager.old_t_signature.clone(),
            new_t_signature: new_manager.new_t_signature.clone(),
        };
        let record = SignedControlRecord {
            kind: ControlKind::EpochActivated,
            request_id,
            manager_rev: u64::from(activation_t.old_manager_rev),
            expected_control_rev,
            signed_body: unsigned_body,
            signature: signature.to_vec(),
            event: Some(event.encode()?),
        };
        let signed_record = record.encode()?;
        let prev = self
            .control_chain
            .last()
            .map(|(_, hash, _)| *hash)
            .unwrap_or([0u8; 32]);
        let mut hasher = Sha256::new();
        hasher.update(prev);
        hasher.update(&signed_record);
        let hash: [u8; 32] = hasher.finalize().into();
        self.control_chain.push((prev, hash, signed_record));
        self.activate_calls += 1;
        if self.fail_activate_response {
            // 关键：**回包丢失**，但 relay 侧的 activate 已经生效。
            return Err(SyncError::Transport("模拟 activate 回包丢失"));
        }
        Ok(cbor_http_response(vec![
            (0, cbor::uint(self.to_epoch)),
            (1, cbor::uint(self.control_rev)),
            (2, cbor::uint(self.auth_rev)),
            (3, cbor::bytes(self.member_set_hash.to_vec())),
            (4, cbor::bytes(self.primary_checkpoint.to_vec())),
            (5, cbor::bytes(self.fallback_checkpoint.to_vec())),
            (6, cbor::bytes(self.activation_commit.clone())),
        ]))
    }

    fn handle_current(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
        self.check_token(request)?;
        if self.fail_current_after_activation {
            return Err(SyncError::Transport("模拟恢复查询时进程退出"));
        }
        let mut fields = vec![(0, self.view_map(false))];
        // 真实 relay：带 `request_id` 时额外回放幂等结果（键 1）与 scope（键 2）。
        if request.path.contains("request_id=") {
            if let Some(replay) = &self.restart_replay {
                fields.push((1, cbor::bytes(replay.clone())));
                fields.push((2, cbor::text(self.replay_scope.clone())));
            }
        }
        Ok(cbor_http_response(fields))
    }

    fn handle_control(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
        self.check_token(request)?;
        let commits = self
            .control_chain
            .iter()
            .enumerate()
            .map(|(index, (prev, hash, signed_record))| {
                cbor::CdeValue::Map(vec![
                    (0, cbor::uint(index as u64 + 1)),
                    (1, cbor::bytes(prev.to_vec())),
                    (2, cbor::bytes(hash.to_vec())),
                    (3, cbor::bytes(signed_record.clone())),
                ])
            })
            .collect::<Vec<_>>();
        Ok(cbor_http_response(vec![
            (0, cbor::array(commits)),
            (1, cbor::uint(self.control_chain.len() as u64)),
            (2, cbor::uint(self.from_epoch)),
        ]))
    }

    fn handle_checkpoint(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
        self.check_token(request)?;
        Ok(cbor_http_response(vec![(
            0,
            cbor::CdeValue::Map(vec![(6, cbor::bytes(FAKE_SIGNED_COMMIT.to_vec()))]),
        )]))
    }
}

impl HttpTransport for FakeManagerRelay {
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
        self.requests.push(request.clone());
        let path = request
            .path
            .strip_prefix(self.base.as_str())
            .unwrap_or(&request.path)
            .to_owned();
        match request.method {
            "POST" if path == "/v1/auth/challenges" => self.handle_challenge(request),
            "POST" if path == "/v1/auth/tokens" => self.handle_token(request),
            "GET" if path.contains("/rotations/current") => self.handle_current(request),
            "GET" if path.contains("/rotations/") => self.handle_rotation_view(request, &path),
            "POST" if path.ends_with("/restart") => self.handle_restart(request),
            "POST" if path.ends_with("/activate") => self.handle_activate(request),
            "PUT" if path.contains("/grants/") => {
                self.check_token(request)?;
                self.staged_calls += 1;
                Ok(cbor_http_response(vec![(
                    0,
                    cbor::CdeValue::Map(Vec::new()),
                )]))
            }
            "GET" if path.contains("/control") => self.handle_control(request),
            "GET" if path.contains("/checkpoints/") => self.handle_checkpoint(request),
            _ => Err(SyncError::Unsupported(
                "fake management relay: 未实现的请求",
            )),
        }
    }
}

/// 先用 `RecordingManagement` 把 journal 推到 `RestartBaselinesReady`（stage 失败）。
///
/// 这一步之后续跑没有本地 `PreparedCheckpoint`：activation T 的 commit 摘要来自
/// relay 事实，fake 才能在激活后逐字节复现同一份 T。
fn pending_restart_journal(
    vault: &Vault,
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
    assets: &StaticAssets,
    certs: &[MemberCertDigestInputV1],
    freeze_seq: u64,
) {
    let mut failing = RecordingManagement {
        fail_stage: Some("模拟 stage 失败"),
        ..RecordingManagement::with_response(valid_response(freeze_seq))
    };
    execute_restart(
        vault,
        relay,
        &mut failing,
        &request(
            materials,
            assets,
            certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect_err("stage 失败必须返回错误");
    let journal = RotationJournal::load(vault)
        .expect("load journal")
        .expect("restart journal");
    assert_eq!(journal.phase, RotationPhase::RestartBaselinesReady);
}

/// 只带旧管理 key 跑一次 `execute_restart`（新 key 由 `run_restart` 从 journal 重建）。
fn http_execute_restart(
    vault: &Vault,
    relay: &mut FakeRelay,
    fake: &mut FakeManagerRelay,
    materials: &genesis::GenesisMaterials,
    assets: &StaticAssets,
    certs: &[MemberCertDigestInputV1],
    freeze_seq: u64,
) -> Result<menghunote_sync::rotation_restart::RestartExecution, SyncError> {
    let api = fake_api();
    let mut management = HttpRestartManagement::new(
        fake,
        &api,
        VAULT_ID,
        INSTANCE_ID,
        &materials.manager,
        HTTP_NOW,
    );
    execute_restart(
        vault,
        relay,
        &mut management,
        &request(
            materials,
            assets,
            certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
}

#[test]
fn restart_activation_lost_response_resumes_with_new_manager_key() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    pending_restart_journal(&vault, &mut relay, &materials, &assets, &certs, freeze_seq);

    // relay 侧 activate 生效但回包丢失；且新 key 的 `/rotations/current` 也失败
    // （模拟进程在恢复前退出，journal 停在 RestartActivating）。
    let mut fake = FakeManagerRelay::new(materials.manager.public_key(), freeze_seq);
    fake.fail_activate_response = true;
    fake.fail_current_after_activation = true;
    let error = http_execute_restart(
        &vault, &mut relay, &mut fake, &materials, &assets, &certs, freeze_seq,
    )
    .expect_err("activate 回包丢失 + 恢复查询失败必须返回错误");
    assert!(
        matches!(error, SyncError::Transport("模拟恢复查询时进程退出")),
        "必须是传输错误而不是误判成功：{error:?}"
    );
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartActivating);
    assert_eq!(fake.activate_calls, 1);
    let restart_calls = fake.restart_calls;
    let old_subject = subject_of(&materials.manager.public_key());
    let new_public = journal.new_manager_public.expect("journal 新管理公钥");
    let new_subject = subject_of(&new_public);
    assert!(
        fake.accepted_token_subjects.contains(&old_subject),
        "activate 之前必须用旧 key"
    );
    assert!(
        fake.accepted_token_subjects.contains(&new_subject),
        "activate 丢回包后的读回必须用新 key"
    );

    // 新进程续跑：只传旧 key；run_restart 必须从 journal manager_seed 重建新 key，
    // 只用新 key 查询 activated 并收尾——不得重发 restart/activate。
    fake.fail_activate_response = false;
    fake.fail_current_after_activation = false;
    let execution = http_execute_restart(
        &vault, &mut relay, &mut fake, &materials, &assets, &certs, freeze_seq,
    )
    .expect("已激活续跑必须用新 key 成功");
    assert_eq!(execution.activated.epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(
        fake.restart_calls, restart_calls,
        "已激活后不得再发 restart 请求"
    );
    assert_eq!(fake.activate_calls, 1, "已激活后不得重复 activate");
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartActivated);
    assert_eq!(journal.activated_epoch, Some(NEW_CANDIDATE_EPOCH));
}

#[test]
fn restart_activated_journal_resumes_with_caller_supplied_new_key_only() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    pending_restart_journal(&vault, &mut relay, &materials, &assets, &certs, freeze_seq);

    let mut fake = FakeManagerRelay::new(materials.manager.public_key(), freeze_seq);
    let execution = http_execute_restart(
        &vault, &mut relay, &mut fake, &materials, &assets, &certs, freeze_seq,
    )
    .expect("先正常激活一次");
    assert_eq!(execution.activated.epoch, NEW_CANDIDATE_EPOCH);
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartActivated);
    let new_manager = ManagementKey::from_seed(Secret32::from_array(
        journal.manager_seed.expect("manager_seed"),
    ));
    assert_eq!(
        new_manager.public_key(),
        journal.new_manager_public.expect("new manager public")
    );

    let restart_calls = fake.restart_calls;
    let activate_calls = fake.activate_calls;
    fake.accepted_token_subjects.clear();
    // 已激活状态直接 resume：调用方显式传入 journal 派生的新 key。
    let api = fake_api();
    let mut management = HttpRestartManagement::new(
        &mut fake,
        &api,
        VAULT_ID,
        INSTANCE_ID,
        &materials.manager,
        HTTP_NOW,
    )
    .with_new_manager(&new_manager);
    let resumed = execute_restart(
        &vault,
        &mut relay,
        &mut management,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect("已激活 journal 只查询/收尾");
    assert_eq!(resumed.activated.epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(fake.restart_calls, restart_calls, "不得重发 restart");
    assert_eq!(fake.activate_calls, activate_calls, "不得重发 activate");
    let new_subject = subject_of(&new_manager.public_key());
    assert!(
        !fake.accepted_token_subjects.is_empty(),
        "resume 必须实际查询 relay"
    );
    assert!(
        fake.accepted_token_subjects
            .iter()
            .all(|subject| *subject == new_subject),
        "已激活 resume 只能用新管理 key"
    );
}

#[test]
fn restart_http_outcome_queries_exact_request_and_rejects_other_scope() {
    ensure_test_identity_wrap();
    let (_dir, _vault, materials, _relay) = setup();
    let mut fake = FakeManagerRelay::new(materials.manager.public_key(), 10);
    let replay = cbor::encode(vec![(0, fake.view_map(false)), (1, cbor::boolean(true))])
        .expect("restart replay encode");
    fake.restart_replay = Some(replay);

    let api = fake_api();
    let mut management = HttpRestartManagement::new(
        &mut fake,
        &api,
        VAULT_ID,
        INSTANCE_ID,
        &materials.manager,
        HTTP_NOW,
    );
    let found = management
        .restart_outcome(REQUEST_ID, OLD_ROTATION)
        .expect("exact request 查询")
        .expect("幂等回放必须返回 restart 结果");
    assert_eq!(found.restarted.rotation_id, RESTART_ROTATION);
    assert_eq!(found.restarted.to_epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(found.restarted.control_rev, CONTROL_REV + 1);
    assert_eq!(found.previous.rotation_id, OLD_ROTATION);
    assert_eq!(found.previous.to_epoch, OLD_CANDIDATE_EPOCH);
    drop(management);

    // 幂等表跨全部管理 scope：同 request_id 若命中 begin/activate 等其它端点，必须拒绝。
    fake.replay_scope = "management/begin".to_owned();
    let api = fake_api();
    let mut management = HttpRestartManagement::new(
        &mut fake,
        &api,
        VAULT_ID,
        INSTANCE_ID,
        &materials.manager,
        HTTP_NOW,
    );
    let error = management
        .restart_outcome(REQUEST_ID, OLD_ROTATION)
        .expect_err("非 restart scope 必须拒绝");
    assert!(
        matches!(&error, SyncError::RelayWire(message) if message.contains("restart 端点")),
        "必须明确拒绝其它管理 scope：{error:?}"
    );
}

#[test]
fn restart_before_activation_uses_old_manager_key() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    pending_restart_journal(&vault, &mut relay, &materials, &assets, &certs, freeze_seq);

    let mut fake = FakeManagerRelay::new(materials.manager.public_key(), freeze_seq);
    let execution = http_execute_restart(
        &vault, &mut relay, &mut fake, &materials, &assets, &certs, freeze_seq,
    )
    .expect("activate 之前的管理步骤必须用旧 key 成功");
    let old_subject = subject_of(&materials.manager.public_key());
    let new_subject = subject_of(&execution.plan.slots.new_manager_public);
    // journal 已越过 intent：resume 不得重发 restart（旧候选不可复用）。
    assert_eq!(fake.restart_calls, 0, "已有分配后不得重发 restart");
    assert_eq!(fake.activate_calls, 1);
    assert!(fake.staged_calls >= 1);
    assert!(
        fake.accepted_token_subjects
            .iter()
            .all(|subject| *subject == old_subject),
        "restart/stage/activate/activation facts 都必须用旧 key（fake 真验签）"
    );
    assert!(!fake.accepted_token_subjects.contains(&new_subject));
}

#[test]
fn restart_rejects_new_manager_public_mismatch_before_any_management_call() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    pending_restart_journal(&vault, &mut relay, &materials, &assets, &certs, freeze_seq);

    // 模拟 journal 损坏：manager_seed 与 new_manager_public 不再匹配。
    let mut journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    journal.new_manager_public = Some([0xEE; 32]);
    journal.save(&vault).expect("保存损坏 journal");

    let mut management = RecordingManagement::with_response(valid_response(freeze_seq));
    let error = execute_restart(
        &vault,
        &mut relay,
        &mut management,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect_err("新管理 key 公钥不匹配 journal 必须拒绝");
    assert!(
        matches!(
            error,
            SyncError::LocalCorrupt("restart journal 管理 key 与 new_manager_public 不匹配")
        ),
        "必须是明确错误：{error:?}"
    );
    assert!(
        management.restart_calls.is_empty(),
        "必须在任何管理网络调用（尤其 restart）之前拒绝"
    );
}

#[test]
fn restart_old_manager_key_rejected_after_activation_reports_explicit_error() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    pending_restart_journal(&vault, &mut relay, &materials, &assets, &certs, freeze_seq);

    let mut fake = FakeManagerRelay::new(materials.manager.public_key(), freeze_seq);
    http_execute_restart(
        &vault, &mut relay, &mut fake, &materials, &assets, &certs, freeze_seq,
    )
    .expect("先正常激活");
    assert!(
        fake.manager_rev >= 2,
        "relay 必须已把 manager_rev 推到新 key"
    );

    // 只带旧 key 的管理对象：旧 key 换 token 被 relay 403 拒绝。
    let api = fake_api();
    let mut old_only = HttpRestartManagement::new(
        &mut fake,
        &api,
        VAULT_ID,
        INSTANCE_ID,
        &materials.manager,
        HTTP_NOW,
    );
    let error = old_only
        .restart(&RestartRelayRequest {
            previous_rotation_id: OLD_ROTATION,
            request_id: REQUEST_ID,
            expected_control_rev: CONTROL_REV,
        })
        .expect_err("激活后旧 key 必须被 relay 拒绝");
    assert!(
        matches!(&error, SyncError::RelayApi { code, .. } if code == "MANAGEMENT_REQUIRED"),
        "必须明确 relay 拒绝：{error:?}"
    );
    // 缺新 key 的读回必须是明确错误，不能返回 Ok(None)/Ok(Some) 冒充成功。
    let error = old_only
        .activated_outcome(RESTART_ROTATION)
        .expect_err("缺新 key 时必须明确报错");
    assert!(
        matches!(error, SyncError::Unsupported(message) if message.contains("新管理 key")),
        "必须说明缺新管理 key：{error:?}"
    );
    let error = old_only
        .activate(&dummy_activation_input())
        .expect_err("旧 key 被拒不能误判为 activate 成功");
    assert!(
        matches!(error, SyncError::Unsupported(message) if message.contains("新管理 key")),
        "必须给出可重试的明确错误：{error:?}"
    );
}

/// 所有失败/恢复路径的共同不变量：旧写仍冻结、cursor 不推进、身份不推广。
fn assert_old_writes_frozen(vault: &Vault) {
    assert_eq!(
        vault
            .get_meta("sync.repack_required")
            .expect("repack meta")
            .as_deref(),
        Some(b"1".as_slice()),
        "restart 失败/恢复期间必须保持冻结屏障"
    );
    assert_eq!(
        vault
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor meta")
            .as_deref(),
        Some(b"c0".as_slice()),
        "restart 不得推进 relay cursor"
    );
    assert_eq!(
        pending_encryption(vault, 10).expect("pending").len(),
        1,
        "旧 epoch 的本地写必须仍在 pending（未重包/未推广）"
    );
}

#[test]
fn restart_intent_recovers_exact_request_after_post_response_lost() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    let response = valid_response(freeze_seq);

    // 第一次：POST 已在 relay 提交（幂等表里有 exact request 结果），但响应丢失、
    // 进程随后被杀。此时本地只允许留下 POST 前的公开 intent。
    let mut lost = RecordingManagement {
        fail_restart: Some("模拟 POST 已提交但响应丢失"),
        restart_replay: Some(response.clone()),
        ..RecordingManagement::with_response(response.clone())
    };
    let error = execute_restart(
        &vault,
        &mut relay,
        &mut lost,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect_err("POST 丢响应必须返回错误");
    assert!(matches!(error, SyncError::Transport(_)));
    assert_eq!(lost.restart_calls.len(), 1, "首次必须真的发 POST");
    assert!(
        lost.restart_outcome_calls.is_empty(),
        "首发 POST 之前不做 exact request 查询"
    );

    // intent 只含公开契约/请求事实：没有 relay 分配、没有任何 fresh 秘密。
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartIntentRecorded);
    assert_eq!(journal.to_epoch, 0, "分配前绝不能本地 +1 写候选 epoch");
    assert!(journal.rotation_id.is_none(), "分配前不能有 rotation id");
    assert!(journal.candidate_root.is_none(), "intent 不能有候选根");
    assert!(journal.restart_password.is_none(), "intent 不能有新口令");
    assert!(journal.recovery_seed.is_none(), "intent 不能有 R'");
    assert!(journal.manager_seed.is_none(), "intent 不能有管理私钥种子");
    let intent = journal.restart_intent.clone().expect("durable intent");
    assert_eq!(intent.previous_rotation_id, OLD_ROTATION);
    assert_eq!(intent.request_id, REQUEST_ID);
    assert_eq!(intent.from_epoch, EPOCH);
    assert_eq!(intent.previous_candidate_epoch, OLD_CANDIDATE_EPOCH);
    assert_eq!(intent.expected_control_rev, CONTROL_REV);
    assert_eq!(intent.manager_rev, 1);
    assert_eq!(intent.freeze_seq, freeze_seq);
    assert_eq!(intent.instance_id, INSTANCE_ID);
    assert!(intent.freeze_barrier, "intent 必须记录冻结屏障=1");
    assert_old_writes_frozen(&vault);

    // 重启：按 intent 的 request_id 查回 exact restart 结果，拿到 relay 分配的
    // epoch 后才生成 fresh 秘密；绝不重发 POST、绝不本地 +1。
    let mut recovered = RecordingManagement {
        restart_replay: Some(response.clone()),
        ..RecordingManagement::with_response(response)
    };
    let execution = execute_restart(
        &vault,
        &mut relay,
        &mut recovered,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect("按 exact request 恢复");
    assert!(
        recovered.restart_calls.is_empty(),
        "已有 durable intent 时绝不能再 POST restart"
    );
    assert_eq!(
        recovered.restart_outcome_calls,
        vec![(REQUEST_ID, OLD_ROTATION)],
        "恢复必须按 exact request 查询"
    );
    assert_eq!(execution.plan.from_epoch, EPOCH);
    assert_eq!(execution.plan.previous_candidate_epoch, OLD_CANDIDATE_EPOCH);
    assert_eq!(execution.plan.to_epoch, NEW_CANDIDATE_EPOCH);
    assert_eq!(execution.plan.rotation_id, RESTART_ROTATION);
    assert_eq!(execution.plan.control_rev, CONTROL_REV + 1);
    assert_eq!(execution.activated.epoch, NEW_CANDIDATE_EPOCH);
    assert_old_writes_frozen(&vault);

    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartActivated);
    assert_eq!(journal.to_epoch, NEW_CANDIDATE_EPOCH);
    assert!(journal.candidate_root.is_some(), "分配后才允许生成候选根");
    assert!(journal.restart_password.is_some(), "分配后才允许生成新口令");
    assert_eq!(
        journal.restart_intent,
        Some(intent),
        "intent 不得被分配覆盖"
    );
}

#[test]
fn restart_fallback_retry_reuses_durable_primary_baseline() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    let response = valid_response(freeze_seq);
    let mut management = RecordingManagement::with_response(response);

    // primary 成功并即时 durable；fallback 创建时失败（等价于 fallback 阶段被杀）。
    let (error, primary_taken) = {
        let mut guarded = RoleGuardRelay::new(&mut relay, true, None);
        let error = execute_restart(
            &vault,
            &mut guarded,
            &mut management,
            &request(
                &materials,
                &assets,
                &certs,
                &[],
                materials.root.clone(),
                &materials.manager,
                1,
                OLD_ROTATION,
                CONTROL_REV,
                freeze_seq,
                true,
            ),
        )
        .expect_err("fallback 失败必须返回错误");
        (error, guarded.primary_created)
    };
    assert!(matches!(error, SyncError::Transport(_)));
    assert!(primary_taken.is_some(), "primary 必须已在 relay 占住 role");

    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(
        journal.phase,
        RotationPhase::RestartPrimaryBaselineReady,
        "primary 一完成就必须单独 durable"
    );
    assert_eq!(journal.baselines.len(), 1, "fallback 尚未落盘");
    let stored_primary = journal.baselines[0].clone();
    assert!(stored_primary.verified);
    assert_ne!(stored_primary.commit_digest, [0u8; 32]);
    assert_ne!(stored_primary.asset_set_digest, [0u8; 32]);
    assert!(
        journal.candidate_root.is_some(),
        "新根在 fallback 之前已 durable"
    );
    assert!(journal.restart_password.is_some());
    assert_old_writes_frozen(&vault);
    let checkpoints_before = relay.next_checkpoint;

    // 重跑：只重建 fallback，primary 复用 journal 的 cid/密文/commit receipt；
    // role guard 会让"重建 random primary"像真实 relay 一样撞 ID_CONFLICT。
    let (execution, retry_roles) = {
        let mut guarded = RoleGuardRelay::new(&mut relay, false, primary_taken);
        let execution = execute_restart(
            &vault,
            &mut guarded,
            &mut management,
            &request(
                &materials,
                &assets,
                &certs,
                &[],
                materials.root.clone(),
                &materials.manager,
                1,
                OLD_ROTATION,
                CONTROL_REV,
                freeze_seq,
                true,
            ),
        )
        .expect("primary 复用后 fallback 重试必须成功");
        (execution, guarded.role_calls.clone())
    };
    assert_eq!(
        execution.primary.checkpoint_id, stored_primary.checkpoint_id,
        "重跑必须复用同一 primary cid"
    );
    assert_eq!(
        execution.primary.commit_digest, stored_primary.commit_digest,
        "重跑必须复用同一 durable commit receipt"
    );
    assert_eq!(
        retry_roles,
        vec![(NEW_CANDIDATE_EPOCH, "fallback".to_owned())],
        "重跑不得再创建 primary（role CAS）"
    );
    assert_eq!(
        relay.next_checkpoint,
        checkpoints_before + 1,
        "重跑只允许创建 fallback 一个 checkpoint"
    );

    let final_journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(final_journal.phase, RotationPhase::RestartActivated);
    assert_eq!(final_journal.baselines.len(), 2);
    let manager = execution
        .activation
        .new_manager
        .as_ref()
        .expect("activate 必须带 new_manager");
    let activation_t = ActivationTV1::decode(
        manager
            .activation_t
            .as_deref()
            .expect("NewManagerV1 必须携带 activation T"),
    )
    .expect("activation T decode");
    assert_eq!(
        activation_t.primary_commit_digest, stored_primary.commit_digest,
        "T 的 primary commit digest 必须来自 durable 本地 receipt"
    );
    assert_eq!(
        activation_t.fallback_commit_digest, final_journal.baselines[1].commit_digest,
        "T 的 fallback commit digest 必须来自 durable 本地 receipt"
    );
    assert_old_writes_frozen(&vault);
}

#[test]
fn restart_journal_without_intent_fails_closed() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    let certs = member_certs(&relay, &[]);

    // 模拟旧版本留下的 restart journal：phase 已越过 intent，但没有 durable intent。
    let mut legacy = RotationJournal::freeze(&vault, EPOCH, REQUEST_ID, CONTROL_REV, [0x11; 32])
        .expect("legacy journal");
    legacy.phase = RotationPhase::RestartPrepared;
    legacy.from_epoch = EPOCH;
    legacy.to_epoch = NEW_CANDIDATE_EPOCH;
    legacy.rotation_id = Some(RESTART_ROTATION);
    legacy.previous_candidate_epoch = Some(OLD_CANDIDATE_EPOCH);
    legacy.freeze_seq = Some(freeze_seq);
    legacy.restart_previous_rotation_id = Some(OLD_ROTATION);
    legacy.control_rev = CONTROL_REV + 1;
    legacy.restart_intent = None;
    legacy.save(&vault).expect("save legacy journal");

    let mut management = RecordingManagement::with_response(valid_response(freeze_seq));
    let error = execute_restart(
        &vault,
        &mut relay,
        &mut management,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect_err("无 intent 的旧 journal 必须 fail-closed");
    assert!(
        matches!(&error, SyncError::Unsupported(message) if message.contains("durable intent")),
        "必须是明确的缺 intent 错误：{error:?}"
    );
    assert!(
        management.restart_calls.is_empty(),
        "缺 intent 时绝不能发 POST"
    );
    assert!(
        management.restart_outcome_calls.is_empty(),
        "缺 intent 时也不能按 request_id 猜结果"
    );
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(
        journal.phase,
        RotationPhase::RestartPrepared,
        "journal 不得被改写"
    );
    assert!(journal.restart_intent.is_none());
    assert_old_writes_frozen(&vault);
}

#[test]
fn restart_legacy_journal_without_baseline_intent_fails_closed() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    // 先产生一份字段自洽的新协议 journal，再把它改造成"旧版本停在基线创建前"。
    pending_restart_journal(&vault, &mut relay, &materials, &assets, &certs, freeze_seq);
    let mut legacy = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    legacy.phase = RotationPhase::RestartSlotsUploaded;
    legacy.baselines.clear();
    legacy.save(&vault).expect("save legacy journal");
    // 旧版本没有 per-role create intent：无法证明 relay create 是否已落。
    RestartBaselineJournal::clear(&vault).expect("clear baseline journal");

    let mut management = RecordingManagement::with_response(valid_response(freeze_seq));
    let error = execute_restart(
        &vault,
        &mut relay,
        &mut management,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect_err("无 per-role intent 的旧 journal 必须 fail-closed");
    assert!(
        matches!(&error, SyncError::Unsupported(message) if message.contains("per-role create intent")),
        "必须是明确的缺 per-role intent 错误：{error:?}"
    );
    assert!(
        management.restart_calls.is_empty(),
        "缺 per-role intent 时绝不能重发 restart"
    );
    assert!(
        management.staged.is_empty() && management.activations.is_empty(),
        "fail-closed 必须发生在任何 grant/activate 之前"
    );
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartSlotsUploaded);
    assert!(journal.baselines.is_empty(), "journal 不得被改写");
    assert_old_writes_frozen(&vault);
}

#[test]
fn restart_intent_without_relay_result_fails_closed() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    let response = valid_response(freeze_seq);

    // 首跑：POST 未提交（传输失败），只留下 durable intent。
    let mut lost = RecordingManagement {
        fail_restart: Some("模拟 POST 未提交"),
        ..RecordingManagement::with_response(response.clone())
    };
    execute_restart(
        &vault,
        &mut relay,
        &mut lost,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect_err("POST 传输失败必须返回错误");
    assert_eq!(
        RotationJournal::load(&vault)
            .expect("load")
            .expect("journal")
            .phase,
        RotationPhase::RestartIntentRecorded
    );

    // 重启：relay 明确没有该 request_id 的结果 -> fail-closed，绝不盲目重发 POST。
    let mut unresolved = RecordingManagement::with_response(response);
    let error = execute_restart(
        &vault,
        &mut relay,
        &mut unresolved,
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect_err("有 intent 但 relay 无结果必须 fail-closed");
    assert!(
        matches!(&error, SyncError::Unsupported(message) if message.contains("request_id")),
        "必须是明确的 exact request 无结果错误：{error:?}"
    );
    assert!(
        unresolved.restart_calls.is_empty(),
        "有 intent 时绝不能盲目重发 POST"
    );
    assert_eq!(
        unresolved.restart_outcome_calls,
        vec![(REQUEST_ID, OLD_ROTATION)]
    );
    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartIntentRecorded);
    assert!(journal.candidate_root.is_none());
    assert!(journal.restart_password.is_none());
    assert_old_writes_frozen(&vault);
}

#[allow(clippy::too_many_arguments)]
fn execute_restart_with_crash(
    vault: &Vault,
    relay: &mut FakeRelay,
    state: &mut CrashRelayState,
    management: &mut RecordingManagement,
    materials: &genesis::GenesisMaterials,
    assets: &StaticAssets,
    certs: &[MemberCertDigestInputV1],
    freeze_seq: u64,
    kill: Option<(String, KillPoint)>,
) -> Result<menghunote_sync::rotation_restart::RestartExecution, SyncError> {
    let mut crash = CrashRelay::new(relay, state, kill);
    execute_restart(
        vault,
        &mut crash,
        management,
        &request(
            materials,
            assets,
            certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
}

/// 正常 restart（无 crash）：两个 role 都必须留下 create intent + 各阶段 receipt。
#[test]
fn restart_writes_per_role_intent_and_stage_receipts() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    let mut state = CrashRelayState::default();
    let mut management = RecordingManagement::with_response(valid_response(freeze_seq));
    let execution = execute_restart_with_crash(
        &vault,
        &mut relay,
        &mut state,
        &mut management,
        &materials,
        &assets,
        &certs,
        freeze_seq,
        None,
    )
    .expect("正常 restart");

    let journal = RestartBaselineJournal::load(&vault)
        .expect("load baseline journal")
        .expect("baseline journal");
    assert_eq!(journal.entries.len(), 2, "primary/fallback 各一条 intent");
    for (role, expected_cid) in [
        ("primary", execution.primary.checkpoint_id),
        ("fallback", execution.fallback.checkpoint_id),
    ] {
        let entry = journal.entry(role).expect("per-role entry");
        assert_eq!(entry.stage, RestartBaselineStage::Promoted);
        assert_eq!(entry.intent.epoch, NEW_CANDIDATE_EPOCH);
        assert_eq!(entry.intent.rotation_id, RESTART_ROTATION);
        assert_eq!(entry.intent.expected_control_rev, CONTROL_REV + 1);
        assert_eq!(
            entry.intent.request_id,
            baseline_create_request_id(&VAULT_ID, role, NEW_CANDIDATE_EPOCH),
            "request_id 必须与 HTTP 层同一派生入口"
        );
        assert_eq!(
            entry.lease.as_ref().map(|lease| lease.checkpoint_id),
            Some(expected_cid),
            "lease 必须指向实际基线 cid"
        );
        assert!(entry.prepared.is_some(), "upload 后必须持久 prepared");
        assert!(entry.verified.is_some(), "validate 后必须持久 verified");
        assert!(
            entry.commit_digest.is_some(),
            "promote 后必须持久 commit receipt"
        );
        assert!(entry.published_at.is_some());
    }
    assert_eq!(state.new_creates, 2, "两个 role 各 create 一次");
    assert_eq!(state.role_conflicts, 0);
    assert_eq!(
        RotationJournal::load(&vault)
            .expect("load rotation journal")
            .expect("rotation journal")
            .phase,
        RotationPhase::RestartActivated
    );
    assert_old_writes_frozen(&vault);
}

/// 每个 role 分别在 create/upload/validate/promote 后被杀：重启必须复用同一
/// request_id/cid，不得 fresh create 撞 relay 的每 epoch role 唯一 CAS。
#[test]
fn restart_recovers_same_request_and_cid_after_crash_at_each_baseline_stage() {
    ensure_test_identity_wrap();
    let points = [
        ("create", KillPoint::Create, RestartBaselineStage::Intent),
        (
            "upload",
            KillPoint::Upload,
            RestartBaselineStage::ObjectsUploaded,
        ),
        (
            "validate",
            KillPoint::Validate,
            RestartBaselineStage::Validated,
        ),
        (
            "promote",
            KillPoint::Promote,
            RestartBaselineStage::Validated,
        ),
    ];
    for role in ["primary", "fallback"] {
        for (label, point, expected_stage) in points {
            let (_dir, vault, materials, mut relay) = setup();
            seed_cursor(&vault);
            let assets = assets();
            let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
            relay.allow_candidate_epoch = true;
            let certs = member_certs(&relay, &[]);
            let mut state = CrashRelayState::default();

            // 首跑：在指定 role/阶段注入"进程被杀"。
            let mut management = RecordingManagement::with_response(valid_response(freeze_seq));
            let error = execute_restart_with_crash(
                &vault,
                &mut relay,
                &mut state,
                &mut management,
                &materials,
                &assets,
                &certs,
                freeze_seq,
                Some((role.to_owned(), point)),
            )
            .expect_err("注入点必须返回错误");
            assert!(
                matches!(error, SyncError::Transport(_)),
                "{role}/{label}: 必须是模拟进程死亡的传输错误，实际 {error:?}"
            );

            let before = RestartBaselineJournal::load(&vault)
                .expect("load baseline journal")
                .expect("baseline journal");
            let entry = before.entry(role).expect("被杀 role 的 durable intent");
            assert_eq!(
                entry.stage, expected_stage,
                "{role}/{label}: 本地 durable 阶段不符"
            );
            let request_id = entry.intent.request_id;
            let cid_before = entry
                .lease
                .as_ref()
                .map(|lease| lease.checkpoint_id)
                .or_else(|| {
                    state
                        .leases
                        .get(&request_id)
                        .map(|lease| lease.checkpoint_id)
                })
                .expect("relay 侧已有 cid");
            assert_eq!(
                request_id,
                baseline_create_request_id(&VAULT_ID, role, NEW_CANDIDATE_EPOCH)
            );
            let expected_creates = if role == "primary" { 1 } else { 2 };
            assert_eq!(
                state.new_creates, expected_creates,
                "{role}/{label}: 首跑只允许每个已准备 role create 一次"
            );
            assert_eq!(state.role_conflicts, 0);

            // 重启：新进程只有 vault/relay 的 durable 状态，执行同一 restart。
            let mut management = RecordingManagement::with_response(valid_response(freeze_seq));
            let execution = execute_restart_with_crash(
                &vault,
                &mut relay,
                &mut state,
                &mut management,
                &materials,
                &assets,
                &certs,
                freeze_seq,
                None,
            )
            .unwrap_or_else(|error| panic!("{role}/{label}: 重启必须恢复，实际 {error:?}"));

            let baseline = if role == "primary" {
                &execution.primary
            } else {
                &execution.fallback
            };
            assert_eq!(
                baseline.checkpoint_id, cid_before,
                "{role}/{label}: 重启必须复用同一 cid"
            );
            assert_eq!(
                state.new_creates, 2,
                "{role}/{label}: 重试不得为任何 role 再 fresh create"
            );
            assert_eq!(
                state.role_conflicts, 0,
                "{role}/{label}: 不得撞 relay 每 epoch role 唯一 CAS"
            );
            if point == KillPoint::Create {
                assert!(
                    state.dedup_hits >= 1,
                    "{role}/create: create 已落但本地未落 lease，必须按同一 request_id 命中 relay 幂等结果"
                );
            }
            if point == KillPoint::Promote {
                assert!(
                    state.republish_hits >= 1,
                    "{role}/promote: publish 已落但 receipt 未落，必须按同一 commit 幂等重放"
                );
            }

            let final_journal = RestartBaselineJournal::load(&vault)
                .expect("load baseline journal")
                .expect("baseline journal");
            let final_entry = final_journal.entry(role).expect("per-role entry");
            assert_eq!(final_entry.stage, RestartBaselineStage::Promoted);
            assert_eq!(final_entry.intent.request_id, request_id);
            assert_eq!(
                final_entry.lease.as_ref().map(|lease| lease.checkpoint_id),
                Some(cid_before)
            );
            let rotation = RotationJournal::load(&vault)
                .expect("load rotation journal")
                .expect("rotation journal");
            assert_eq!(rotation.phase, RotationPhase::RestartActivated);
            let index = if role == "primary" { 0 } else { 1 };
            assert_eq!(
                rotation
                    .baselines
                    .get(index)
                    .map(|baseline| baseline.checkpoint_id),
                Some(cid_before),
                "{role}/{label}: rotation journal 的基线 cid 必须与恢复的一致"
            );
            assert_old_writes_frozen(&vault);
        }
    }
}

fn dummy_activation_input() -> ActivationInput {
    ActivationInput {
        rotation_id: RESTART_ROTATION,
        request_id: REQUEST_ID,
        expected_control_rev: CONTROL_REV + 1,
        primary_checkpoint: [0x61; 16],
        fallback_checkpoint: [0x62; 16],
        member_cert_digest: [0x63; 32],
        slot_set_digest: [0x64; 32],
        grants_digest: [0x65; 32],
        asset_set_digest: [0x66; 32],
        new_manager: None,
        candidate_root: RootSecretV1::new(
            VAULT_ID,
            NEW_CANDIDATE_EPOCH,
            Secret32::from_array([0x67; 32]),
        ),
        new_manager_public: [0x68; 32],
        new_manager_rev: 2,
        pv: 1,
    }
}

/// 续跑安全门：journal 里的 `verified=true` 不是覆盖证据；缺新修订 durable verified
/// receipt 时，restart 快捷恢复必须 fail-closed（旧开发 vault 语义）。
#[test]
fn restart_resume_requires_new_revision_verified_receipt() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    seed_cursor(&vault);
    let assets = assets();
    let freeze_seq = freeze_assets(&vault, &mut relay, &materials, &assets);
    relay.allow_candidate_epoch = true;
    let certs = member_certs(&relay, &[]);
    pending_restart_journal(&vault, &mut relay, &materials, &assets, &certs, freeze_seq);

    let journal = RotationJournal::load(&vault)
        .expect("load journal")
        .expect("journal");
    assert_eq!(journal.phase, RotationPhase::RestartBaselinesReady);
    assert!(journal.baselines[0].verified, "journal 声称已验证");
    let primary_cid = journal.baselines[0].checkpoint_id;
    let key = format!(
        "sync.checkpoint.verified_receipt.{}",
        base64url::encode(&primary_cid)
    );
    assert_eq!(
        vault
            .exec("DELETE FROM meta WHERE key=?1", &[Value::Text(key)],)
            .expect("删 verified receipt"),
        1,
        "新修订 receipt 必须已存在"
    );

    let error = execute_restart(
        &vault,
        &mut relay,
        &mut RecordingManagement::with_response(valid_response(freeze_seq)),
        &request(
            &materials,
            &assets,
            &certs,
            &[],
            materials.root.clone(),
            &materials.manager,
            1,
            OLD_ROTATION,
            CONTROL_REV,
            freeze_seq,
            true,
        ),
    )
    .expect_err("缺新修订 verified receipt 不得按旧 verified=true 快捷恢复");
    assert!(matches!(error, SyncError::LocalCorrupt(_)), "{error}");
    assert!(format!("{error}").contains("verified receipt"), "{error}");
    assert_old_writes_frozen(&vault);
}
